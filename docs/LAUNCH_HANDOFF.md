# Launch handoff — what only you can do

> **Audience:** Sb (you, the operator).
> **Goal:** every launch blocker that code could close is closed. What's
> left needs a credit card, a signature, hardware or a human decision.

This list is sequenced by *order-of-operations*, not by importance.
Tackle the dependencies first (auth, transports) so the later items
(legal, support process) have something to point at.

> **Last refreshed: 2026-10-05**, after the Rust backend went live. Items marked ✅ have shipped. Items
> marked 🟡 are partially done. Unmarked items are open.
>
> **Five remain, and none of them is code:**
>
> | # | Item | Blocked on |
> | - | ---- | ---------- |
> | 1 | Clerk production keys | you — swap at the last minute before launch |
> | 3 | Status page vendor | you — optional, recommended |
> | 6 | Legal: Terms of Service + Privacy Policy published, DPA reviewed | you + counsel — **no Terms or Privacy Policy exists yet**, though sign-up asks users to agree to both |
> | 8 | Pi performance benchmark | hardware access |
> | 11 | On-call rotation | you — a process, not a change |
>
> Item 12 (day-before go/no-go) is a checklist to *run*, not to close.
>
> Items 2, 4, 5, 7, 9 and 10 are closed. The genuine blocker is legal:
> a Terms of Service and Privacy Policy have to be written and published
> (sign-up already links them), and the DPA reviewed.

---

## 1. Clerk production keys

**State now.** The dashboard is using Clerk's test keys (`pk_test_*`,
`sk_test_*`). These work end-to-end for auth and billing in the
sandbox, but they're scoped to the test environment: real Stripe
charges don't post, and the sign-in card says "Development mode". This
is intentional until launch. The Clerk application is also still
named "SourceBox Sentry", which is what the sign-in card shows; rename
it in the Clerk dashboard.

**What you need to do.**
1. In the Clerk dashboard, switch the application to production
   mode (or create a separate production app and copy the
   user/organization schema across — Clerk has a one-click clone for
   this).
2. Configure the production Stripe account in Clerk's billing tab.
   Billing runs entirely through Clerk, which uses Stripe underneath;
   there is no direct Stripe integration in the code.
3. Update Fly secrets:
   ```
   fly secrets set \
     CLERK_PUBLISHABLE_KEY=pk_live_... \
     CLERK_SECRET_KEY=sk_live_... \
     -a sentinel-command
   ```
4. Verify the Clerk webhook endpoint
   `https://app.sentinel-command.com/api/webhooks/clerk` is
   registered in the production Clerk app and signing secret is set
   (`CLERK_WEBHOOK_SECRET`). Test by upgrading a test org and
   confirming its `settings` row `org_plan` becomes `pro`.

   **Subscribe it to every event the handler reads**, not just billing:
   `organization.created`/`.deleted`, `organizationMembership.created`/
   `.updated`/`.deleted`, `paymentAttempt.updated`, `subscription.*`
   (`created`, `updated`, `active`, `pastDue`) and `subscriptionItem.*`
   (`active`, `canceled`, `ended`, `freeTrialEnding`, `pastDue`).

   > **2026-10-05:** the development instance's only endpoint still
   > pointed at `https://opensentry-command.fly.dev/…` — a hostname that
   > no longer resolves — and Svix had disabled it, so production had
   > received no Clerk webhook since the rename: org deletions never ran
   > the GDPR wipe, plan changes arrived only through the hourly
   > reconcile, and the membership events were not subscribed at all.
   > Repointed to the URL above, re-enabled, given the event list above,
   > and proven by resending an `organization.deleted` for a test org:
   > the signature verified and the org's rows were erased. Re-enabling
   > does not replay what was missed while it was disabled.

**Verification.** After the secret swap, sign out and sign back in.
The dev-mode badge in the corner should disappear.

---

## 2. Notification transport — Resend email ✅ DONE (live)

> **2026-07-05:** Resend is configured and `EMAIL_ENABLED=true` in
> production. The operator walkthrough is kept below for reference.

**State now.** Email is built and deployed. Each notification kind has
a per-org email toggle. Motion email **defaults off** and has a
per-camera 15-minute cooldown with a digest, to keep volume down.
Transport is Resend; the code is `backend-rs/src/email*.rs` and
`recipients.rs`, and the 46 templates in `backend-rs/templates/emails/`
are compiled into the binary and checked against a golden corpus by
`cargo test`. Bounces and complaints arrive at `/api/webhooks/resend`
and land in `email_suppression`. The sub-processor disclosure is in
`SUB_PROCESSORS.md` and `DPA.md`.

**SMS and mobile push are out of scope.** For those, wire your own MCP
agent to Twilio or PagerDuty.

**Operator action to activate email:**
1. Sign up at resend.com (free tier covers 3K emails/month — comfortably
   above realistic volume for the operator-critical kinds).
2. Verify a sending domain — Resend gives you 4 DNS records (SPF TXT,
   DKIM CNAMEs ×3, optional DMARC). 15-60 min for DNS to propagate.
   **This step is already done:** `sentinel-command.com` is verified,
   with DKIM and SPF/Return-Path on `send.sentinel-command.com`. This
   previously recommended `notifications.sourceboxsentry.com`, which is
   the pre-rename brand and is *not* a verified sending domain — setting
   `EMAIL_FROM_ADDRESS` to it would have failed every send.
3. Configure a webhook in Resend → endpoint
   `https://app.sentinel-command.com/api/webhooks/resend`. Copy the
   signing secret (starts with `whsec_`).
4. Set the four Fly secrets:
   ```
   fly secrets set \
     RESEND_API_KEY=re_... \
     RESEND_WEBHOOK_SECRET=whsec_... \
     EMAIL_FROM_ADDRESS=notifications@sentinel-command.com \
     EMAIL_ENABLED=true \
     -a sentinel-command
   ```
   (Done — sending is verified on `sentinel-command.com`; the code
   defaults already match these values so the secrets are only needed
   to override. `notifications@` is a no-reply sender by design —
   support is a separate channel at `support@sentinel-command.com`.)
5. Smoke test: kill a CameraNode for >90s, watch the test admin's inbox
   for the offline email, click the unsubscribe link, verify the
   toggle flipped off in `/settings`. Then repeat for motion: trigger a
   camera, confirm the first-motion email arrives, and confirm a second
   trigger inside the cooldown window produces a digest rather than a
   second email.

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
3. Link the status page from the website and from `SECURITY.md`.
4. Subscribe customers to status updates via the vendor's
   subscription widget — automatic for most.

---

## 4. Domain / DNS ✅ DONE

**State now.** Live and serving. `app.sentinel-command.com` (the app and
API) holds a Let's Encrypt certificate from Fly, valid to 2026-12-02;
the apex `sentinel-command.com` (marketing site, on GitHub Pages) is
valid to 2026-12-07. Both renew automatically. The app's own origin is
`FRONTEND_URL` (`app.sentinel-command.com`, set in `fly.toml`), which is
also the only origin Clerk sign-in tokens are accepted from.

Note the split, because it has bitten the docs twice: **the API lives on
`app.`, not the apex.** The apex has no `/api` and no `/docs`. Anything
pointing an integration, webhook or health probe at the bare domain is
wrong.

The rest of this section is a **procedure for changing domains later**,
not outstanding work.

**To switch.**
1. Buy a domain (e.g. `sentry.sourceboxlabs.com`).
2. Add a Fly cert via `fly certs add`.
3. Update `FRONTEND_URL` in `fly.toml`. It is the CORS origin, the
   only `azp` Clerk tokens are accepted with, and the base of email links.
   Extra origins go in `CORS_ALLOWED_ORIGINS` (comma-separated).
4. Update Clerk's allowed origins, and the Clerk webhook endpoint URL.
5. Update the Resend webhook endpoint URL.
6. Update `scripts/install.sh` (Linux/macOS) so it points at the new base URL
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
keeps us inside the free-tier event budget. `backend-rs/src/sentry.rs::init()`
no-ops gracefully when DSN is absent (local dev), so no extra config
needed there. Email alerting confirmed firing — you've received at
least one Sentry alert email (`OPENSENTRY-COMMAND-1`).

The disk check (`loops.rs::check_disk_critical`, at 95% full) raises
its alert as one `tracing::error!`, which Sentry turns into an event.
It deliberately does not notify customers: platform disk is the
operator's problem, not theirs (commit `594b86c`).

Dashboard: `fly ext sentry dashboard -a sentinel-command`.

---

## 6. Legal: terms, privacy policy and DPA review

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

**⚠️ Terms of Service and Privacy Policy do not exist anywhere yet.**
The sign-up page says "By creating an account you agree to our Terms of
Service and Privacy Policy" and links
`https://sentinel-command.com/legal/terms` and `…/legal/privacy`; the
footer of every email links the privacy one. Both URLs return 404 (the
old `/legal` page went with the marketing pages in July and was never
republished; the website's own "Terms" link 404s too). Write both, have
counsel review them, and publish them **at those two URLs** so the
existing links start working. Until then, users are agreeing to
documents that don't exist.

**Factual corrections the drafts need** are listed in editor's notes at
the top of `legal/DPA.md` and `legal/SUB_PROCESSORS.md`. The important
one: incident evidence (snapshots and short video clips) *is* stored in
Command Center's database, which the drafts deny. The website's "Privacy
by design" section says "We don't hold your video", which needs the same
qualification.

**Other legal documents you may need (not drafted yet).**
- Acceptable Use Policy (probably worth one, given the camera
  context — what users *cannot* point cameras at).

---

## 7. Backups and disaster recovery ✅ DONE

**State now.** The hosted database is `sentinel_command` on the shared
`sentinel-postgres` cluster. Backups are the cluster's daily snapshots
(kept 5 days) plus a daily portable `pg_dump` from
`.github/workflows/backup.yml` (kept 14 days). Both live on Fly, by
decision. Restores were rehearsed on 2026-07-06 and twice on 2026-09-07,
and the procedure is in
[DISASTER_RECOVERY.md](runbooks/DISASTER_RECOVERY.md).

**What to keep doing.**
1. Glance at the snapshots now and then:
   ```bash
   fly volumes list -a sentinel-postgres
   fly volumes snapshots list <volume_id> -a sentinel-postgres
   ```
2. Re-run the restore drill quarterly, and once more as soon as there is
   real customer data (every drill so far restored an empty pre-launch
   database).

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

## 9. GitHub repo settings ✅ DONE

**State now (2026-09-09).** Branch protection on `master`:

- `allow_force_pushes: false` · `allow_deletions: false` · `required_linear_history: true`
- **Required status checks:** `Backend tests (sqlite)`, `Backend tests (postgres)`, `Frontend audit + build`
- **Required PR reviews:** 1 approver, `dismiss_stale_reviews: true`
- `enforce_admins: false` — the sole admin can override via the UI. Defense against fat-finger, not against deliberate action.

This item previously read *"status-check + review enforcement deferred"*
on the grounds that there was no PR flow and checks "only kick in during
merges, so they're decorative in direct-push mode". Both premises are
gone: all work now lands through PRs, and the checks gate them.

`strict` (require branch up to date) is deliberately **off**. With it on,
every Dependabot PR needs a rebase whenever master moves, which stalls
auto-merge for no safety gain — the checks still run against the PR head.

**Two things this does NOT fix, both live:**

- **Dependabot auto-merge does not deploy.** GitHub doesn't fire
  `on: push` workflows for commits pushed with `GITHUB_TOKEN`, so an
  auto-merged bump lands on `master` without deploying, and CodeQL going
  green on it hides that convincingly. Needs a PAT
  (`DEPENDABOT_PAT`); `deploy.yml` has a `workflow_dispatch` trigger as
  the interim lever. See the notes at the top of
  `.github/workflows/dependabot-auto-merge.yml`.
- **Frontend lint warnings don't fail CI.** `npm run lint` runs and
  fails on errors only, so warnings accumulate. The backend gates on
  `cargo fmt --check` and `cargo clippy -D warnings`.

**Dependabot:** now configured on all four repos (Command Center,
CameraNode, License, Sync). Only Command Center has an auto-merge
workflow, deliberately — the other three deploy on push, so a
`GITHUB_TOKEN` merge there would silently skip their deploy too.

---

## 10. Customer support process ✅ DONE (inbox live)

**State now (2026-07-05).** Support + security mailboxes are live via
**ImprovMX** email forwarding on `sentinel-command.com` (MX →
`mx1/mx2.improvmx.com`), forwarding to the operator inbox:

- `support@sentinel-command.com` — customer support. Wired into the
  in-app `ErrorBoundary` crash screen, the `SUB_PROCESSORS.md`
  sub-processor-concern channel.
- `security@sentinel-command.com` — vulnerability reports. Published
  as the **primary** `Contact:` in `/.well-known/security.txt` (GitHub
  Security Advisories remains as the secondary channel), in
  `SECURITY.md`, and in the DPA's vulnerability-management section.

ImprovMX free tier covers this comfortably; note it handles *incoming*
mail only — outbound transactional email still goes through Resend
(item 2). Both coexist on the domain: MX points at ImprovMX, SPF/DKIM
(TXT) authorize Resend, so there's no conflict.

**Remaining (optional, not blocking).**
1. Define an internal first-response target (e.g. within 1 business
   day). Don't promise an SLA on the public site at the Free / Pro
   tiers (the security page already says "No formal SLA on Free or
   Pro").
2. Keep the website's documentation current
   (<https://sentinel-command.com/documentation/>) so customers can
   self-serve the common questions before they email.

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
[X] Backup restore tested (item 7) — rehearsed 2026-07-06 and 2026-09-07;
    Fly-only backups are an accepted decision
[ ] DPA + sub-processors PDF on file with lawyer signoff (item 6)
[ ] Status page live and pointed at /api/health/ready (item 3)
[X] Sentry alerts confirmed firing in production env (item 5)        — done 2026-05-03
[X] Custom domain live + Clerk allows it (item 4)                   — app.sentinel-command.com
[X] Branch protection enabled on master (item 9)                      — done 2026-05-04
[X] Support inbox configured and monitored (item 10)                 — done 2026-07-05 (ImprovMX: support@ + security@)
[X] Resend signup + EMAIL_ENABLED=true + smoke test (item 2)         — done
[ ] Run `cd backend-rs && cargo fmt --check && cargo test && cargo clippy --all-targets -- -D warnings`
    — green, zero warnings (the agent's tests included: same crate)
[ ] Run `cd frontend && npm run build && npm audit --omit=dev` — both clean
[ ] Browse the live app at 375px, 1024px, 1440px — nothing broken
[ ] Clerk webhook endpoint enabled, pointed at app.sentinel-command.com,
    subscribed to the full event list (item 1)
[ ] Hit https://app.sentinel-command.com/api/health/detailed — overall "healthy", DB latency < 50ms,
    disk.percent_used < 80%, resend.status either "ok" or
    "unconfigured" (intentional pre-launch)
```

When every box is checked, ship the launch announcement.

---

## What's shipped since the original draft (audit trail)

> A historical record. File names below from before October 2026 refer
> to the deleted Python backend (`backend/app/…`); the behaviour lives on
> in `backend-rs/`.

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
  the WebSocket (the HTTP limiter does not cover an upgrade).  Full first-touch UX
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
