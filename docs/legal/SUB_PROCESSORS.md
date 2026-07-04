# Sub-processors

> **STATUS: WORKING DRAFT.** Same caveat as the DPA — this file
> describes the engineering reality and must be reviewed by counsel
> before being held out to customers as a binding sub-processor list.

This is the public list of third-party services that SourceBox LLC
engages to process Customer Personal Data on behalf of customers of
**Sentinel Command Center**.

The list mirrors what's stated in `/security` on the Command Center
website; if the two ever drift, treat this file as the authoritative
technical record and `/security` as the customer-facing summary.

**Notice policy.** New sub-processors and replacements are announced
by:

1. Updating this file in the public repository (master branch); and
2. Emailing the Customer's billing contact at least **14 days** before
   the new sub-processor begins processing Customer Personal Data.

Customers who reasonably object on data-protection grounds may
terminate the affected Service per Section 4.4 of the
[DPA](./DPA.md).

---

## Current sub-processors

### Clerk
- **Service provided:** User authentication, organization management,
  session tokens, billing subscription management.
- **Personal Data processed:** Name, email, password hash (Clerk-
  managed; SourceBox never sees plain or hashed passwords), org
  membership, role, session tokens, subscription metadata (plan,
  status, billing dates).
- **Location of processing:** United States.
- **Cross-border transfers:** Yes (US-based; SCC reliance for EEA/UK
  data).
- **Privacy policy:** https://clerk.com/legal/privacy
- **DPA reference:** https://clerk.com/legal/dpa
- **Notes:** Clerk in turn engages its own sub-processors, including
  Stripe (see below) for billing. Clerk's published sub-processor
  list governs that downstream chain.

### Stripe (via Clerk)
- **Service provided:** Payment processing for Sentinel
  subscriptions. Collected and processed by Clerk; SourceBox never
  receives card numbers, CVVs, or other payment-instrument data.
- **Personal Data processed:** Card number and other payment
  instrument data (collected directly by Stripe), billing address,
  email associated with payment.
- **Location of processing:** United States.
- **Cross-border transfers:** Yes.
- **Privacy policy:** https://stripe.com/privacy
- **Notes:** Treated here as a Clerk sub-processor, but called out
  separately so customers know where card data physically lives.

### Fly.io
- **Service provided:** Application hosting, persistent volume
  storage (holds the SourceBox SQLite database file), global edge
  network, TLS termination.
- **Personal Data processed:** All metadata SourceBox stores about
  Customer (account identity, audit logs, stream access logs, motion
  event metadata, settings rows). Fly.io does not see or process
  video content (live segments are RAM-only; recordings live on
  Customer's CameraNode hardware off Fly's network).
- **Location of processing:** United States, with edge network in
  multiple regions.
- **Cross-border transfers:** Yes (US-based; edge regions in EU,
  Asia-Pacific).
- **Privacy policy:** https://fly.io/legal/privacy-policy/
- **DPA reference:** https://fly.io/legal/data-processing-agreement/
- **Notes:** Volumes encrypted at rest by Fly.io. SourceBox runs in a
  single primary region with edge proxies forwarding to it.

### Sentry — *optional, off by default*
- **Service provided:** Application error tracking and performance
  monitoring.
- **Personal Data processed:** Exception stack traces, request URLs,
  user_id (where present in the trace), IP address (truncated by
  Sentry's PII scrubber). 10% trace sample rate. **No video, no
  request body content, no MCP arguments.**
- **Location of processing:** United States.
- **Cross-border transfers:** Yes.
- **Privacy policy:** https://sentry.io/privacy/
- **DPA reference:** https://sentry.io/legal/dpa/
- **Notes:** Disabled when the `SENTRY_DSN` environment variable is
  unset. SourceBox's own deployment runs with Sentry enabled; an
  AGPL-3.0 fork running without a DSN does not engage Sentry as a
  sub-processor for that deployment.

### Resend — *optional, off by default*
- **Service provided:** Transactional email delivery for operator-
  critical alerts (camera offline + recovered, CameraNode offline +
  recovered, AI-agent-created incidents, MCP API key audit events,
  CameraNode host disk approaching full, organization membership
  lifecycle — added / role-changed / removed, and motion detection
  with per-camera cooldown + digest). All settings are opt-in
  per-org; six default ON for new orgs, motion defaults OFF.
- **Personal Data processed:** Recipient email address (for org
  members opted-in to email alerts), subject line and body content
  (which may include camera names, node names, and incident
  summaries — all operator-controlled strings, never video or
  motion-detection imagery), delivery status (sent / delivered /
  bounced / complained) returned via webhook.
- **Location of processing:** United States.
- **Cross-border transfers:** Yes.
- **Privacy policy:** https://resend.com/legal/privacy-policy
- **DPA reference:** https://resend.com/legal/dpa
- **Notes:** Disabled when the `EMAIL_ENABLED` environment variable
  is unset or `false`. Resend is also bypassed when `RESEND_API_KEY`
  is unset. An AGPL-3.0 fork that hasn't configured both does not
  engage Resend as a sub-processor for that deployment. Recipient
  emails are derived from Clerk org membership at send time — no
  separate mailing-list state is held by SourceBox or Resend.

### Ollama Cloud — *optional; engaged only by the AI Sentinel agent*
- **Service provided:** Cloud large-language-model inference for the
  optional **Sentinel** AI investigation agent. When an organization
  uses Sentinel, the agent captures camera snapshots and reads
  incident context, then sends them to a vision-capable LLM to
  describe what is happening and draft an incident report.
- **Personal Data processed:** **Camera image snapshots (JPEG
  frames), which may contain images of people, vehicles, license
  plates, and property**, plus incident text and camera/metadata
  strings the agent reads or writes. This is the one sub-processor
  that receives actual imagery — unlike motion detection (which is
  on-device) and the rest of this list (which never see video).
- **Location of processing:** United States (Ollama Cloud,
  ollama.com; model `qwen3.5:cloud`).
- **Cross-border transfers:** Yes.
- **Privacy policy:** https://ollama.com/privacy
- **Notes:** Engaged **only** when an organization runs the Sentinel
  agent (a paid-tier feature with a per-organization on/off toggle in
  Settings). Organizations that never enable or trigger Sentinel send
  no imagery to Ollama. The agent runs as a separate SourceBox-operated
  service; an AGPL-3.0 fork that does not deploy the agent does not
  engage Ollama as a sub-processor.

---

## If you fork and run your own copy

Command Center is a SaaS operated by SourceBox LLC; the sub-processor
relationships above describe *our* deployment. If you exercise your
AGPL-3.0 right to run a fork on your own infrastructure, those
relationships do not apply to your deployment — you become responsible
for the relationships you create with your own hosting provider, auth
provider, and any error-monitoring service. SourceBox LLC has no
visibility into and no responsibility for forks running outside our
infrastructure.

---

## Update history

A short, append-only history of changes to this list. Customers can
diff this file in the repository for the full record.

- **2026-04-25** — Initial draft consolidated from the engineering
  source-of-truth (`/security` page + actual codebase
  configuration).
- **2026-05-02** — Added Resend (transactional email) as an
  optional sub-processor. Engaged only when the operator sets both
  `EMAIL_ENABLED=true` and `RESEND_API_KEY`. v1 covers four
  operator-critical alert kinds; motion-event emails deferred to
  v1.1.
- **2026-06-29** — Added Ollama Cloud as an optional sub-processor
  for the Sentinel AI investigation agent. This is the first
  sub-processor that receives camera imagery (JPEG snapshots), and
  only for organizations that use the agent. Disclosed here, in the
  Privacy Policy §1/§4, and on `/security`.

---

## Reporting a sub-processor concern

If you have a concern about a sub-processor on this list — for
example, a privacy-relevant change in their service or a new
investigation that affects their data-handling — please contact
SourceBox at the support address in your Order Form, or open a
GitHub issue at
<https://github.com/SourceBox-LLC/Sentinel-Command/issues>
(public; do not include sensitive information).
