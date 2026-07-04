# On-call runbook — Sentinel Command Center

> **Audience:** humans responding to a page or a customer report.
> **Goal:** find the actual problem fast, recover service, and write up
> what happened — without paging someone who can't help.

This runbook covers Command Center (the cloud service). For
CameraNode-side issues (a single customer's hardware misbehaving) see
the operator FAQ in `/docs#troubleshooting` — most of those are
self-serve.

The format is one section per scenario. Each section has the same
shape so you can scan it under pressure: **Symptoms**, **First
checks**, **Likely causes**, **Fix paths**, **When to escalate**.

---

## Quick reference

| Tool / link | Why |
|---|---|
| https://sentinel-command.com/api/health | Liveness — is the process up? |
| https://sentinel-command.com/api/health/detailed | DB ping latency, cache + queue depths |
| `fly logs -a opensentry-command` | Application stderr/stdout |
| `fly status -a opensentry-command` | Machine health + last deploy |
| `fly ssh console -a opensentry-command` | Shell into the live machine |
| Sentry: project **opensentry-command** | Alert origin, stack traces |
| Clerk dashboard | Auth issues, billing webhook deliveries |
| GitHub: `SourceBox-LLC/Sentinel-Command` | Source, deploy via push to master |

---

## Scenario A: Sentry alert fired

**Symptoms.** Inbound email or webhook from Sentry referencing a
specific issue ID (e.g. `OPENSENTRY-COMMAND-1`).

**First checks.**
1. Open the Sentry issue. Read the stack trace and the most recent
   event's request context.
2. Check the issue's *frequency* and *first seen* timestamp. A
   spike on a brand-new exception is more urgent than a slow trickle
   of a known one.
3. Hit `/api/health/detailed` — does the broken subsystem show up
   there? (DB error → unhealthy. Big viewer-usage queue → degraded.)

**Likely causes.**
- A code path with no test coverage was hit by real production data.
  (Example: `OPENSENTRY-COMMAND-1` — `_log_cleanup_loop` chained
  `.union()` calls hit a CompoundSelect that has no `.union()`. Fix:
  `union(a, b, c, ...)` function form. Tests now in
  `backend/tests/test_log_cleanup_union.py` + `test_log_cleanup.py`.)
- An external dependency (Clerk, Fly database) is degraded.
- A recent deploy introduced a regression. Check `git log master --since=24.hours`.

**Fix paths.**
- **Hotfix and roll forward.** If the failing code is in Python or
  JS, write a regression test in `backend/tests/` or
  `frontend/tests/` first, then fix, then push to master. CI deploys
  via GitHub Actions; do **not** `fly deploy` directly — the deploy
  workflow is documented in `MEMORY.md` and ensures the right env
  variables are set.
- **Roll back.** If the deploy that introduced the bug is recent
  and the fix isn't obvious, `git revert <sha>` and push to master.
  Faster than chasing the root cause at 3am.

**When to escalate.**
- The exception is in the auth path (Clerk integration broken) and
  every customer is locked out.
- The exception fires at boot and the app is in a crash loop. Check
  `fly logs` and consider a pinned previous image.

---

## Scenario B: Customer reports "all my cameras are offline"

**Symptoms.** Single customer ticket; their dashboard shows every
camera in their org as offline; node heartbeats not coming through.

**First checks.**
1. From `fly logs`, search for the customer's `org_id` (find it
   via Clerk dashboard). Look for `[OfflineSweep]` log lines —
   these fire when the sweep flips a node/camera to offline.
2. Hit `/api/health/detailed` — is the SSE subscriber count zero?
   Is anything else degraded? If the DB is fine and other customers
   are streaming, this is likely customer-side.
3. Ask the customer: did they restart their CameraNode? Is the host
   machine on a network with outbound HTTPS to
   `sentinel-command.com`?

**Likely causes.**
- Customer's home internet dropped and the node hasn't reconnected.
- Customer's CameraNode crashed and didn't auto-restart. Their host
  machine's process supervisor (systemd, etc.) needs to bring it
  back up.
- The node's API key was rotated but the local config wasn't
  updated. Customer's `node.db` would show the old key hash.
- Their entire org was rebased to Free tier after a payment failure
  exceeded the 7-day grace window — the cameras beyond cap (5 on
  Free) would be `disabled_by_plan=True`, which presents as offline.
  Check the org's billing status in Clerk.

**Fix paths.**
- For credential-out-of-sync, walk the customer through the
  re-auth flow documented in `/docs#troubleshooting`.
- For payment-grace expiry, ask the customer to update their card
  in the Clerk billing portal. The grace flag clears on next webhook.
- For genuine node hangs, the customer needs hands-on access to
  the host machine. We cannot fix this remotely.

**When to escalate.**
- Multiple unrelated customers report the same symptom in the same
  hour — that's a Command Center problem masquerading as customer
  problems. Pivot to Scenario E.

---

## Scenario C: Customer reports "stream won't play"

**Symptoms.** Single customer; the dashboard loads, the camera tile
is visible, but clicking play shows the spinner forever or shows an
error overlay.

**First checks.**
1. In the customer's browser dev tools, check the network tab — is
   `/api/cameras/{id}/stream.m3u8` returning 200 or an error?
2. From `fly ssh console`, hit `/api/health/detailed` and check
   `checks.hls_cache.playlists_cached` — is it nonzero? If so, at
   least one customer is streaming.
3. Tail `fly logs` for the customer's `camera_id` and look for
   `[HLS]` or `[Cleanup]` lines that mention it.

**Likely causes.**
- Stream segments aged out of the in-memory cache. The segment
  cache is RAM-only, evicted after 60s of inactivity per camera.
  Customer has to refresh their browser to re-trigger the
  CameraNode → playlist push.
- Customer hit their viewer-hour cap for the month. The playback
  endpoint returns a clear error in the body — check the response.
- Customer's CameraNode stopped pushing segments. Their UI says
  "online" because the heartbeat is still landing, but the segment
  pipeline stalled. Common causes: ffmpeg process crashed,
  USB camera disconnected, host machine I/O saturated.

**Fix paths.**
- Cap-related: customer needs to upgrade or wait until the next
  calendar month rolls. We do not extend caps without an Order
  Form / Pro Plus paid agreement.
- Cache-related: ask the customer to refresh. If the issue persists
  longer than 60s, the CameraNode is probably the problem.
- CameraNode-side: customer-side troubleshooting — see
  `/docs#troubleshooting`.

**When to escalate.**
- Cache size in `/api/health/detailed` is *zero* but multiple
  customers are reporting playback failures at the same time. The
  CameraNode → backend `POST /push-segment` path may be broken.
  Check Fly logs for `403`, `429`, or `500` on that endpoint.

---

## Scenario D: Database is slow, unresponsive, or out-of-disk

**Symptoms.** `/api/health/detailed` shows
`checks.database.status == "error"`, `latency_ms > 1000`, or
`checks.disk.status == "critical"`. Sentry firing `OperationalError`,
`SqliteError`, or our own `[DiskCheck] OPERATOR ALERT` event.

**Important context.** We use **SQLite on a Fly volume**, NOT Fly's
managed Postgres. There is no separate database app to check. The
DB lives at `/data/opensentry.db` on the same machine as the
FastAPI process. WAL mode + NullPool + busy_timeout=5000 (see
`backend/app/core/database.py`).

**First checks.**
1. `fly status -a opensentry-command` — is the machine itself up
   and healthy? Check the "events" timeline for recent restarts.
2. `curl https://sentinel-command.com/api/health/detailed` —
   look at:
   - `checks.database.status` and `latency_ms`
   - `checks.disk.percent_used` and `checks.disk.status`
   - `checks.viewer_usage.pending_writes` (high = flush loop wedged)
3. `fly logs -a opensentry-command` — search for `SqliteError`,
   `database is locked`, `no space left`, or `[DiskCheck]`.
4. `fly ssh console -a opensentry-command` then
   `df -h /data` to see actual volume usage.

**Likely causes.**

- **Disk full on `/data`.** Logs show `no space left on device` or
  the disk-check loop's `OPERATOR ALERT` Sentry event fired.
  Recordings + audit logs + email outbox all share this volume.
  Most common growth driver: an org with high motion-event volume
  (every event is a row in `MotionEvent` until the daily cleanup
  loop runs).
- **WAL file got huge.** SQLite's WAL grows during writes and
  collapses on read checkpoints. A long-running read transaction
  can prevent checkpointing → WAL grows unbounded → disk fills
  faster than expected.
- **`database is locked` errors.** Should be rare with WAL +
  busy_timeout, but a slow-running transaction (e.g. a 10K-row
  delete in `run_log_cleanup`) can briefly block writers.
- **Viewer-usage flush wedged.** If
  `checks.viewer_usage.pending_writes` is climbing, the 60s flush
  loop is failing. Could be a SQLite lock or an app-level
  exception.

**Fix paths.**

- **Disk full:**
  ```
  # Extend the volume (downtime: machine restart while resize completes)
  fly volumes extend <volume_id> --size <new_GB> -a opensentry-command
  ```
  Then trigger early log cleanup if needed:
  ```
  fly ssh console -a opensentry-command \
    -C "uv run python -c 'from app.main import run_log_cleanup; \
        from app.core.database import SessionLocal; \
        db = SessionLocal(); \
        print(run_log_cleanup(db))'"
  ```
- **WAL bloat:** force a checkpoint:
  ```
  fly ssh console -a opensentry-command \
    -C "sqlite3 /data/opensentry.db 'PRAGMA wal_checkpoint(TRUNCATE);'"
  ```
- **`database is locked`:** restart the app machine to clear any
  stuck readers. `fly machine restart <id> -a opensentry-command`.
- **Viewer-usage flush wedged:** restart the app. Root-cause via
  the app exception trail in Sentry.

**When to escalate.**

- After a restart the app comes back up but the disk fills
  again within hours — there's a runaway write somewhere (motion
  spam, broken background loop). Read recent commits + Sentry
  for clues before another restart.
- The volume is at its plan max (Fly volumes don't auto-extend
  past plan limits). Need a paid plan upgrade or migration to
  larger storage.

---

## Scenario E: Multiple unrelated customers reporting issues at once

**Symptoms.** Three or more independent tickets in a short window.

**First checks.**
1. `/api/health/detailed` — every check should be green. Anything
   yellow or red explains it.
2. Fly status page (https://status.flyio.net/) — is our region
   degraded?
3. Clerk status page — is the auth provider down?
4. Sentry dashboard — is one specific exception spiking?

**Fix paths.**
- If Fly is down: post a status update to customers (email + the
  `#status` channel if you have one); wait it out; do not deploy
  during a regional incident.
- If Clerk is down: same — every signed-in user starts seeing 401s
  but the underlying app is fine. Wait for Clerk recovery.
- If Sentry shows a spike: jump to Scenario A with the most-fired
  issue.

**When to escalate.**
- Both Fly and Clerk green, no Sentry spike, but customers still
  report failures. Time to read the actual error responses they're
  getting. Ask for screenshots and the `request-id` from the
  network tab if present.

---

## Scenario F: Suspected data breach or unauthorized access

**Symptoms.** Anomalous Sentry traces showing access from
unexpected IPs; customer reports a stream session they didn't
initiate; an audit log row your monitoring flagged.

**First checks.**
1. Read the audit log row(s) in question — `audit_log` table —
   for context. Who, when, where (IP), what action.
2. Check whether the access used a JWT (Clerk session) or an MCP
   API key. The `event` and `user_id` columns disambiguate.

**Containment.**
- If an MCP API key was compromised: revoke it immediately via the
  admin dashboard. The hash is invalidated; further calls 401.
- If a Clerk session was compromised: ask the customer to sign out
  of all sessions in their Clerk account settings (this rotates
  the underlying token). Force-rotate from the Clerk dashboard if
  the user can't.
- If the cause is a vulnerability in our code: revert the offending
  change and ship a fix. Do **not** disclose the vulnerability
  details publicly until a fix is shipped (responsible disclosure).

**Notification.**
- If Customer Personal Data of an org was actually accessed by
  someone unauthorized, we have a 72-hour notification clock under
  the DPA (Section 4.7). Start drafting the customer notice while
  containment is in flight.
- Notify counsel; the legal record of the incident lives outside
  this repo. Don't write breach details into a GitHub issue.

**When to escalate.**
- Anything past containment.

---

## Scenario G: Customer requests deletion (GDPR / CCPA right to erase)

**Symptoms.** Customer submits a deletion request via support.

**First checks.**
1. Confirm the request is from a verified org admin. Identity hijack
   for "delete this org" is a real attack — verify via Clerk-known
   email at minimum.
2. Identify the org_id.

**Fix paths.**
- Self-service: walk the admin through Settings → Delete
  Organization. This deletes every node, camera, group, MCP key,
  audit log, stream access log, motion event, and settings row in
  a single transaction. There is no soft-delete.
- Manual: if self-service fails (e.g. a stuck row), `fly ssh
  console` and run the same cascade via psql. Document what you
  ran in the runbook log below.

**Records.**
- Log the deletion in your DSAR record-keeping system (outside this
  repo). Keep a record that the deletion happened, by whom, and on
  what date — but not the data itself.

**When to escalate.**
- The customer requests a deletion that includes Clerk account
  metadata. We cannot delete that from here — they have to delete
  their Clerk account themselves. Provide the Clerk
  account-deletion link.

---

## Scenario H: Email isn't sending (Resend transport failures)

**Symptoms.** Customer reports they didn't get an expected camera-
offline email. `/api/health/detailed` shows `checks.resend.queue_depth`
climbing or `checks.resend.status == "error"`. Sentry firing
exceptions from `app.core.email_worker`.

**First checks.**
1. `/api/health/detailed` — `checks.resend.status` tells you the
   transport's overall state:
   - `"ok"` — sending fine
   - `"unconfigured"` — `EMAIL_ENABLED=false` OR `RESEND_API_KEY` unset
   - `"error"` — recent send attempts are failing
2. `fly logs -a opensentry-command | grep -E "\\[Email\\]|\\[Worker\\]"` —
   look for lines like `[Email] Resend send failed` or
   `[Worker] reclaimed N stuck sending rows`.
3. Resend dashboard — check the "Emails" tab for recent attempts.
   Are they marked sent / bounced / blocked? Resend's dashboard
   shows the SMTP-level reason.
4. SSH and query the outbox directly:
   ```
   fly ssh console -a opensentry-command \
     -C "sqlite3 /data/opensentry.db \
       'SELECT status, COUNT(*) FROM email_outbox GROUP BY status;'"
   ```

**Likely causes.**

- **Sender reputation hit.** A burst of complaints (users marking
  emails as spam) caused Resend to suspend our sending. Check the
  Resend dashboard for warnings. Most common trigger: someone with
  many flappy outdoor cameras opted INTO motion email + the digest
  cooldown wasn't enough. Recovery: investigate the suppression list
  for the affected addresses, possibly tighten cooldown via
  `Setting.email_motion_cooldown_minutes` for that org.
- **DNS / SPF / DKIM regression.** A registrar change broke the
  Resend domain verification. Resend dashboard → Domains tab will
  show "verification failed." Re-add the records and wait for
  propagation.
- **API key rotated / revoked.** `[Email] Resend send failed
  err=AuthenticationError` everywhere. Generate a new API key in
  Resend, update the Fly secret:
  ```
  fly secrets set RESEND_API_KEY=re_... -a opensentry-command
  ```
- **Resend platform incident.** Check status.resend.com. Just wait
  it out. The outbox queue will drain on its own when Resend
  recovers (rows stay `pending` and the worker keeps retrying up to
  `EMAIL_MAX_ATTEMPTS=3` per row).
- **Worker crash loop.** The `email_worker_loop` is supposed to
  swallow per-row exceptions but might be failing at the outer
  `SessionLocal()` open. Sentry traces will show the actual
  exception.

**Fix paths.**

- **Stuck rows:** rows in `status='sending'` for >60s are
  auto-reclaimed by the worker. If a lot are stuck, restart the
  app to force a fresh worker tick.
- **Drain queue manually after fixing root cause:** the worker
  resumes automatically once the underlying issue clears. No
  manual drain needed — just confirm `queue_depth` is decreasing
  on `/api/health/detailed`.
- **Kill switch as last resort:**
  ```
  fly secrets set EMAIL_ENABLED=false -a opensentry-command
  ```
  This silences the transport entirely. Outbox rows still queue
  but the worker logs "would have sent" and marks them skipped.
  Useful when a sender-reputation hit needs days to recover.

**When to escalate.**

- Resend suspended our domain entirely (not just a single bounce).
  This is a customer-facing trust issue and may require migrating
  to a fresh sending domain. Contact Resend support with the suspension
  notice.

---

## Scenario I: CI deploy is failing

**Symptoms.** A push to `master` succeeded the test + frontend jobs
but the "Deploy to Fly.io" job failed. The live site is fine — just
not yet on the latest commit.

**First checks.**
1. `gh run list --workflow="Test & Deploy" --limit 5 -R SourceBox-LLC/Sentinel-Command` —
   recent run history. Multiple failures in a row = real problem,
   single failure = could be transient flake.
2. `gh run view <run_id> --log-failed -R SourceBox-LLC/Sentinel-Command | tail -30` —
   show the failure tail.
3. Compare to the most recent successful run's log to spot what
   changed:
   ```
   gh run view <good_run_id> --log -R SourceBox-LLC/Sentinel-Command \
     | grep -E "WARN|Error|builder" | tail -10
   ```

**Likely causes.**

- **Fly remote builder auth flake.** WireGuard tunnel auth flap
  (we hit this 2026-05-04). Pattern in logs:
  `WARN Failed to start remote builder heartbeat: unauthorized`.
  Our current workflow (since `882780c`, 2026-05-04) builds locally
  on the GitHub runner via `docker/build-push-action` and pushes
  directly to `registry.fly.io` — this bypasses the remote builder
  entirely, so this should NOT recur in the current setup. If it
  does, we somehow regressed the workflow.
- **`docker/build-push-action` failure.** Buildkit issue, runner
  out of disk, or a Dockerfile syntax error. Should be obvious
  from the log.
- **Registry push 401.** Token in `FLY_API_TOKEN` is dead. Rotate:
  ```
  fly tokens create deploy -x 8760h -a opensentry-command \
    | gh secret set FLY_API_TOKEN -R SourceBox-LLC/Sentinel-Command
  fly tokens revoke <old_id>
  gh run rerun <failed_run_id> --failed -R SourceBox-LLC/Sentinel-Command
  ```
- **`fly machine update` failed.** The build + push succeeded but
  rolling the live machine hit an error. Check `fly status` and
  `fly logs` for what happened on the machine side.

**Fix paths.**

- **Single transient flake:** retry the failed jobs:
  ```
  gh run rerun <run_id> --failed -R SourceBox-LLC/Sentinel-Command
  ```
  If the next attempt succeeds, log the incident in this runbook
  and move on.
- **Persistent failure:** read the workflow's long comment block
  in `.github/workflows/deploy.yml` for context on the THREE prior
  builder regressions (depot timeout 2026-04-28; Fly remote builder
  WireGuard auth 2026-05-04; depot manifest namespace mismatch
  2026-05-04). Each was solved differently. The current local-
  Buildkit setup is the most resilient path we've found.
- **Deploy is BLOCKING a critical fix:** as a one-time emergency
  override, you can bypass CI and deploy manually:
  ```
  cd backend && uv run python -c "..."  # tests still must pass locally
  cd frontend && npm run build
  fly deploy -a opensentry-command  # USE WITH CAUTION
  ```
  This breaks the "every deploy goes through CI" guarantee, so use
  only when truly necessary and document in the runbook log.

**When to escalate.**

- Multiple distinct failure modes in a short window (e.g. tests
  flaking AND deploy flaking AND audit failing) — that's a
  GitHub Actions / Fly platform incident. Pivot to Scenario E.

---

## Scenario J: Pre-deploy sanity check before pushing master

> Not a fire — but if you're deploying at 11pm, walk through this
> checklist before pushing.

- `cd backend && python -m pytest` — must be green.
- `cd frontend && npm run build && npm run lint` — must be clean
  (lint warnings allowed; errors are not).
- `git log origin/master..HEAD` — read every commit message. If
  anything looks risky, postpone or coordinate.
- Hit `/api/health/detailed` on the live site — verify it's healthy
  *now* before you change it.
- Push. Watch GitHub Actions complete. Hit
  `/api/health/detailed` again.

---

## Runbook log

Append-only record of incidents we've actually responded to. Keep
each entry small — date, scenario, what we did, the take-away. The
goal is to build pattern recognition over time.

> No entries yet.

When you handle an incident, add it here. Future-you will thank
present-you.

---

## Updating this runbook

If you respond to something this runbook doesn't cover, add a new
scenario or extend an existing one *while it's fresh*. The runbook
is graded on whether it makes the next person's response faster,
not on whether it's pretty.
