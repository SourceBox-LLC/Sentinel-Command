# Launch handoff — what only you can do

> **Audience:** Sb (you, the operator).
> **Goal:** every code-side launch blocker I (Claude) could close has
> been closed and committed. What's left is everything that needs a
> credit card, a signature, hardware, or a human decision — none of
> which I can do for you.

This list is sequenced by *order-of-operations*, not by importance.
Tackle the dependencies first (auth, transports) so the later items
(legal, support process) have something to point at.

> **Last refreshed: 2026-05-05.** Items marked ✅ have shipped since
> the original draft. Items marked 🟡 are partially done (code-side
> ready, operator action remaining). Items still wide open are
> unmarked. The original "closed by Claude on 2026-04-25" stamp at
> the bottom no longer captures reality — significant code shipped
> in the SaaS-readiness sweep, the email v1+v1.1 work, the multi-
> tenant disk fix, the CI rewrite, branch protection, and the
> 2026-05-04→2026-05-05 SaaS launch-checklist closeout (see audit
> trail at the bottom).

---

## 1. Clerk production keys

**State now.** The dashboard is using Clerk's test keys (`pk_test_*`,
`sk_test_*`). These work end-to-end for auth and billing in the
sandbox, but they're scoped to the test environment — real Stripe
charges don't post, the dev-mode badge shows in the UI, and the
"Clerk dev keys in prod" memory note documents this is intentional
*for now*.

**What you need to do.**
1. In the Clerk dashboard, switch the application to production
   mode (or create a separate production app and copy the
   user/organization schema across — Clerk has a one-click clone for
   this).
2. Configure the production Stripe account via Clerk's billing tab
   (Clerk handles Stripe under the hood per
   `MEMORY.md::project_billing_stack`).
3. Update Fly secrets:
   ```
   fly secrets set \
     CLERK_PUBLISHABLE_KEY=pk_live_... \
     CLERK_SECRET_KEY=sk_live_... \
     -a sentinel-command
   ```
4. Verify the Clerk webhook endpoint
   `https://sentinel-command.com/api/webhooks/clerk` is
   registered in the production Clerk app and signing secret is set
   (`CLERK_WEBHOOK_SECRET`). Test by upgrading a test org and
   confirming the `Setting(org_plan="pro")` row shows up.

**Verification.** After the secret swap, sign out and sign back in.
The dev-mode badge in the corner should disappear.

---

## 2. Notification transport — Resend email 🟡 (code shipped, operator action remaining)

**State now.** Email v1 + v1.1 are fully built and deployed. **12
notification kinds gated by 7 per-org per-kind toggles.** Six default
ON for new orgs (camera offline/recovered, CameraNode offline/recovered,
AI-agent incidents, MCP key audit, CameraNode disk, member audit);
**motion defaults OFF** with a per-camera 15-min cooldown + digest
mechanism for volume control. Transport is Resend; integration lives
in `app/core/email.py`, `app/core/email_worker.py`, `app/core/recipients.py`,
`app/core/email_templates.py`, `app/core/email_unsubscribe.py`. 22
Jinja2 templates in `app/templates/emails/`. Webhook-driven bounce/
complaint handling at `/api/webhooks/resend` writes to `EmailSuppression`.
Sub-processor disclosure already in `SUB_PROCESSORS.md` + `DPA.md`.
Marketing copy already swept across SecurityPage / PricingPage / FAQ /
docs Notifications.

**SMS and mobile push are still explicitly out of scope** per the
`project_notification_channels` memory note. MCP-driven external
alerting (wire to Twilio / PagerDuty via your own MCP agent) is
the answer for those.

**Operator action to activate email:**
1. Sign up at resend.com (free tier covers 3K emails/month — comfortably
   above realistic volume for the operator-critical kinds).
2. Verify a sending domain — Resend gives you 4 DNS records (SPF TXT,
   DKIM CNAMEs ×3, optional DMARC). 15-60 min for DNS to propagate.
   Recommended subdomain: `notifications.sourceboxsentry.com` (keeps
   marketing-email reputation isolated from transactional).
3. Configure a webhook in Resend → endpoint
   `https://sentinel-command.com/api/webhooks/resend`. Copy the
   signing secret (starts with `whsec_`).
4. Set the four Fly secrets:
   ```
   fly secrets set \
     RESEND_API_KEY=re_... \
     RESEND_WEBHOOK_SECRET=whsec_... \
     EMAIL_FROM_ADDRESS=notifications@sourceboxsentry.com \
     EMAIL_ENABLED=true \
     -a sentinel-command
   ```
5. Smoke test: kill a CameraNode for >90s, watch the test admin's inbox
   for the offline email, click the unsubscribe link, verify the
   toggle flipped off in `/settings`. See plan file
   `~/.claude/plans/gentle-coalescing-teacup.md` for the full motion
   smoke test sequence.

**Code is safe to keep deployed indefinitely** with `EMAIL_ENABLED=false`
(the default). The worker still runs but the transport short-circuits
with a logged "would have sent" line.

---

## 3. Status page vendor (recommended)

**State now.** Two health endpoints live and ready for an external
monitor to poll:

- `/api/health/ready` (commit `6265b32`, 2026-05-04) — readiness
  check with DB + Clerk + disk + email-worker probes. Returns
  HTTP 200 when ready, **503 with detail body when any critical
  dependency is unhealthy**. 30s cached so a swarm of pollers
  doesn't hammer Clerk. **This is the better target for external
  status pages and uptime monitors** because it speaks HTTP status
  codes rather than nesting status in the body.
- `/api/health/detailed` — verbose status snapshot, always 200.
  Useful for dashboards that parse JSON; bad for vendors that
  only check status codes.

There's no public status page yet.

**Options.**
- **BetterStack** / **Better Uptime** — modern, generous free tier.
- **Instatus** ($20/mo for the smallest paid plan, free tier works
  for solo operations).
- **Statuspage.io** by Atlassian (more features, pricier).
- **UptimeRobot** — free tier, good for the "page me when it's
  actually down" minimum.

**What to do.**
1. Create a status page / synthetic monitor on the chosen vendor.
2. Point the synthetic monitor at `/api/health/ready` (NOT
   `/detailed`) every minute. The monitor only needs to look at
   HTTP status: 200 = up, 5xx = down. Body is for humans.
3. Link the status page from `/security` (replace the placeholder
   "No public status page yet" line in the "Honest gaps" section).
4. Subscribe customers to status updates via the vendor's
   subscription widget — automatic for most.

---

## 4. Domain / DNS

**State now.** Live on `sentinel-command.com`. CORS is
hard-coded for that origin (`backend/app/main.py::cors_origins`).

**To switch.**
1. Buy a domain (e.g. `sentry.sourceboxlabs.com`).
2. Add a Fly cert via `fly certs add`.
3. Update `cors_origins` in `app/main.py` to include the new
   domain.
4. Update `FRONTEND_URL` env var on Fly.
5. Update Clerk's allowed origins to include the new domain.
6. Update `install.sh` (Linux/macOS) so it points at the new base URL
   — this URL gets baked into customer CameraNodes at install time, so
   transitioning takes weeks. The Windows MSI doesn't need a parallel
   update because it's a static download from GitHub Releases (the
   MSI's URL doesn't change with the Command Center domain).

---

## 5. Sentry production setup ✅ DONE

**State now.** Sentry is fully wired and verified in production.
`SENTRY_DSN` is set in Fly secrets via the Sentry extension
(`fly ext sentry create -a sentinel-command` provisioned a sponsored
Team plan and auto-injected the DSN). `SENTRY_TRACES_SAMPLE_RATE=0.1`
keeps us inside the free-tier event budget. `app/core/sentry.py::init_sentry()`
no-ops gracefully when DSN is absent (local dev), so no extra config
needed there. Email alerting confirmed firing — you've received at
least one Sentry alert email (`OPENSENTRY-COMMAND-1`).

The disk-check loop that was added in the SaaS-readiness sweep
(`_check_and_emit_disk_critical` at 95% threshold) routes its alert
via `logger.error()` with structured `extra` fields, which Sentry
captures as a server-side event. This replaced an earlier (incorrect)
attempt to email customer admins about the platform disk — see ADR
in commit `594b86c` for the multi-tenant violation rationale.

Dashboard: `fly ext sentry dashboard -a sentinel-command`.

---

## 6. Lawyer review of legal templates

**State now.** I wrote `docs/legal/DPA.md` and
`docs/legal/SUB_PROCESSORS.md` as engineering-truth working drafts.
Both lead with `DRAFT — NOT FOR EXECUTION` so nobody can sign them
accidentally.

**What you need to do.**
1. Find a privacy lawyer. Many SaaS-friendly firms have flat-fee
   "starter DPA review" packages for early-stage companies in the
   $1.5–4K range.
2. Send them the markdown drafts. They will return a redlined PDF.
3. Save the lawyer-approved PDF in your records system (NOT in this
   repo — the markdown stays as the engineering record).
4. When sub-processors change, update `SUB_PROCESSORS.md` in master
   and email the billing contact (per the DPA's 14-day notice
   policy). The repo edit IS the public notice.

**Other legal templates you may need that I haven't drafted.**
- Terms of Service (the existing `/legal` page has an outline; have
  the lawyer review it).
- Privacy Policy (same — check `/legal`).
- Acceptable Use Policy (probably worth one, given the camera
  context — what users *cannot* point cameras at).

---

## 7. Backups and disaster recovery

**State now.** **We use SQLite on a Fly volume**, not Fly's managed
Postgres. `DATABASE_URL=sqlite:////data/sentinel.db` per `fly.toml`.
The volume is `sentinel_data` mounted at `/data`. Fly snapshots
the volume daily on their default schedule (5-day retention on the
Free plan, longer on paid).

**What you need to do.**
1. Verify the volume snapshot schedule:
   ```
   fly volumes snapshots list <volume_id> -a sentinel-command
   ```
   You should see daily snapshots going back 5+ days.
2. **Test a restore.** This is the only thing that turns "we have
   backups" from a claim into a fact. Do this at least once before
   you onboard the first paying customer:
   - Pick a recent snapshot and create a new volume from it:
     ```
     fly volumes create sentinel_data_restore_test \
       --snapshot-id <snap_id> -a sentinel-command
     ```
   - Spin up a temporary machine pointing at the restored volume
     (or detach prod, attach the restore, verify, swap back —
     riskier but cleaner).
   - Confirm SQLite opens cleanly + tables are intact:
     ```
     fly ssh console -a sentinel-command \
       -C "sqlite3 /data/sentinel.db '.tables'"
     ```
   - Sanity-check key tables have rows: `Camera`, `CameraNode`,
     `Setting`, `Notification`.
3. Document the restore procedure in
   `docs/runbooks/DISASTER_RECOVERY.md` (still unwritten — wait
   until you've done a real restore so you can capture what
   actually broke vs. what worked).

**Single-machine deploy caveat:** because we run a single Fly machine
with a single volume, "restore" means downtime. The deploy strategy
is `immediate` (also documented in `fly.toml`), so a deploy already
involves ~30-60s of unavailability. A restore would be similar but
with the additional manual swap step. Acceptable for current scale;
worth re-evaluating when usage warrants HA (LiteFS or migrating to
Postgres for clusterability).

---

## 8. Pi performance benchmark

**State now.** The CameraNode README and the `/docs` site describe
the node as running on "any Linux, macOS, or Windows machine,
including a Raspberry Pi". I haven't validated that actually works
under a realistic camera load.

**What you need to do.**
1. Get a Pi 4 (or Pi 5, increasingly common). Install Sentinel
   CameraNode via the install script.
2. Connect 1, 2, 4 USB cameras at 1080p / 30fps and watch:
   - CPU steady-state under load.
   - Memory steady-state.
   - Egress bandwidth to Command Center.
   - Whether motion detection completes within the segment window.
3. Document the result somewhere — at minimum in
   `Sentinel-CameraNode/README.md` under a "Performance reference"
   section. If a Pi 4 only handles 2 cameras at 1080p, that's
   useful for users to know upfront. If it handles 8, even better.

**If the Pi turns out to be too weak for the advertised use case,**
update the docs honestly. Better to say "Pi 5 recommended for 4+
cameras" than to lose a customer who tried it on a Pi 3.

---

## 9. GitHub repo settings 🟡 (branch protection done; status-check + review enforcement deferred)

**State now (2026-05-04).** Branch protection on `master` is enabled
with three rules:

- `allow_force_pushes: false` — defends against `git push --force` muscle memory at 2am
- `allow_deletions: false` — defends against `git push --delete origin master`
- `required_linear_history: true` — no merge commits, keeps `git log` readable

`enforce_admins: false` — you (the only admin) can override via the
GitHub UI if you genuinely need to fix something that requires
force-push. Defense against fat-finger, not against deliberate action.

**Deferred until you have a co-maintainer:**
- Required PR reviews (1 approver). No PR flow exists today; we push
  direct to `master` with CI as the safety net.
- Required status checks for merge. These only kick in during merges,
  so they're decorative in direct-push mode. Add when PR flow lands.

**Dependabot security updates:** already on (PR #8 was a Dependabot
PR for Clerk CVE GHSA-w24r-5266-9c3c, handled 2026-04-30). Worth
verifying the schedule annually.

---

## 10. Customer support process

**State now.** Support routes are described in the legal page and
implied in the DPA, but no actual support inbox is configured.

**What you need to do.**
1. Set up a `support@yourdomain.tld` mailbox. Forward to your
   personal email or use Front / Help Scout for triage if volume
   warrants.
2. Define an internal target: respond to first email within X
   business hours. Don't promise an SLA on the public site at the
   Free / Pro tiers (the security page already says "No formal SLA
   on Free or Pro").
3. Document common questions in `/docs#faq` (already pretty good)
   so customers can self-serve.

---

## 11. On-call rotation

**State now.** You're a one-person team. The runbook
(`docs/runbooks/ON_CALL.md`) is written as if any human can pick up
a page.

**What you need to do later.**
1. As soon as you have a second engineer / co-maintainer, define a
   PagerDuty (or alternative) rotation.
2. Update the runbook with rotation contact info.
3. The runbook itself doesn't change — it's already in the
   "scannable under pressure" shape.

---

## 12. Final go/no-go checklist (to run the day before launch)

```
[ ] Clerk production keys swapped (item 1)
[ ] Backup restore tested at least once (item 7)
[ ] DPA + sub-processors PDF on file with lawyer signoff (item 6)
[ ] Status page live and pointed at /api/health/detailed (item 3)
[X] Sentry alerts confirmed firing in production env (item 5)        — done 2026-05-03
[ ] Custom domain (if applicable) live + Clerk allows it (item 4)
[X] Branch protection enabled on master (item 9)                      — done 2026-05-04
[ ] Support inbox configured and monitored (item 10)
[ ] Resend signup + EMAIL_ENABLED=true + smoke test (item 2)
[ ] Run `cd backend && uv run pytest` — all green (450+ tests)
[ ] Run `cd frontend && npm run build && npm audit --omit=dev` — both clean
[ ] Browse the live site at 375px, 1024px, 1440px — nothing broken
[ ] Hit /api/health/detailed — overall "healthy", DB latency < 50ms,
    disk.percent_used < 80%, resend.status either "ok" or
    "unconfigured" (intentional pre-launch)
```

When all twelve check, ship the launch announcement.

---

## What's shipped since the original draft (audit trail)

- **2026-04-26 → 2026-05-01:** SaaS-readiness sweep — composite
  indexes on McpActivityLog + MotionEvent, disk-full alarm in
  `/api/health/detailed`, motion-ingestion per-org kill switch,
  HLS global byte-cap eviction. Tigris/AWS dead-secret cleanup.
  Marketing pass 1+2 (SEO meta + benefit-first hero copy + Clerk
  dark theme). Docs drift fixes (Postgres→SQLite, `~15s`→`~60s`
  cache buffer, MCP tool count corrections, SLA wording).
- **2026-05-02:** Verified Sentry production setup (item 5 done).
- **2026-05-03:** Email v1 — Resend transport + worker + recipient
  lookup + 3 new tables, `create_notification` email side-channel,
  `/api/webhooks/resend`, disk-check loop, templates + UI + copy
  sweep + DPA + sub-processor disclosure. Three review-fix commits
  (idempotency-key routing, rate-limit on unsubscribe, EmailLog +
  EmailOutbox retention).
- **2026-05-04:** Multi-tenant violation removed (`disk_critical`
  no longer routes to customers — operator-only Sentry path).
  Four new email kinds added (camera/node recovery, MCP key audit,
  CameraNode disk warning, member audit via Clerk webhook). Motion
  email v1.1 with per-camera cooldown + digest. CI workflow
  rewritten three times (Fly remote builder → depot.dev → local
  Buildkit on the runner) after WireGuard auth regression on Fly's
  side. Branch protection on master.
- **2026-05-04 → 2026-05-05 (SaaS launch-checklist closeout, 14
  commits):** Operator-debugging hygiene — per-request IDs in a
  contextvar, ContextFilter that injects `request_id`+`org_id`
  into every log line, Sentry tag, `X-Request-Id` response
  header, ruff in CI with conservative ruleset.  Multi-tenant
  rate-limit audit caught + closed 7 missing-decorator endpoints
  including 3 SSE streams, 4 admin DB endpoints, the incident-
  evidence proxy, and a custom in-memory connect-throttle for
  the WebSocket (slowapi only does HTTP).  Full first-touch UX
  pass — welcome email on `organization.created`, Help link in
  authenticated nav, in-app CameraNode install widget that
  auto-creates a node + bakes credentials into the displayed
  one-liner, contextual `?` tooltips on the three highest-
  confusion settings, member promotion-request button.  Audit-
  log CSV export across all three audit endpoints with shared
  streaming helper.  RFC 9116 `/.well-known/security.txt` with
  rolling 11-month Expires + full Vulnerability Disclosure
  Policy section on `/security` with CFAA safe-harbour
  language.  GDPR Article 17 cascade gap-fix (both
  `danger/full-reset` and the `organization.deleted` Clerk
  webhook were only clearing 5–7 of 14 org-scoped tables —
  fixed via `app/core/gdpr.py` as single source of truth) +
  Article 20 export endpoint streaming a ZIP per table.
  `/api/health/ready` with DB + Clerk + disk + email-worker
  probes returning 503 on critical failure (the existing
  `/api/health` and `/detailed` always returned 200, useless
  for external uptime monitors).  `pip-audit` in CI for backend
  deps; `vitest` wired into the frontend CI step (suite existed
  but wasn't running, gated nothing); 23 new frontend tests
  including one that caught a real interaction bug in
  HelpTooltip on touch devices.  Closer: built
  `OrgAuditLogPanel` admin component on top of the now-
  paginated `/api/audit-logs`, completing the admin
  dashboard's audit surface.  Backend tests 464 → 549.
- **2026-05-05 cleanup pass:** Pulled `drop_orphan_tables`
  and `sanitize_existing_codecs` out of the boot path (one-shot
  fixes that had been no-op'ing for weeks); kept as documented
  helpers for snapshot-restore.  Deleted dead `EmptyState`
  component + tests (superseded by `WelcomeHero`).  Renamed
  misleading "Recording toggled (legacy)" audit label.
  Replaced dead `legal@sourcebox.dev` contact in `LegalPage.jsx`
  with in-app self-serve (Settings → Privacy & Data) for
  Article 17 / 20 / CCPA + GitHub Issues for everything else
  until the `legal@` mailbox lands.

> *Originally closed by Claude on 2026-04-25. Refreshed
> 2026-05-05 after the SaaS launch-checklist closeout sweep
> (req-IDs, rate-limit audit, first-touch UX, audit CSV,
> security.txt, GDPR delete + export, /healthz/ready,
> pip-audit + vitest in CI, AuditLog UI, cleanup pass). The
> remaining items still all require you — credit cards,
> signatures, hardware, human decisions.*
