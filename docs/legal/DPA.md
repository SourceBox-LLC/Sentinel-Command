# Data Processing Agreement (DRAFT — NOT FOR EXECUTION)

> **STATUS: WORKING DRAFT.** This document is a starting-point template
> reflecting how Sentinel by SourceBox actually processes customer data.
> It has **not** been reviewed by counsel and **must not be sent to
> a customer or signed** until a qualified privacy lawyer has reviewed
> it for the jurisdictions you intend to operate in (at minimum: US
> state laws including CCPA/CPRA; EU/EEA GDPR; UK GDPR; and any
> sector-specific rules — HIPAA, FERPA, etc. — your customer base
> implicates).
>
> Maintain this file as the *technical truth* of what SourceBox does;
> the lawyer-reviewed binding version lives elsewhere (a signed PDF
> in your records system).

---

**Version:** Draft 0.1
**Last technical review:** 2026-04-25
**Author:** SourceBox LLC engineering
**Reviewers required before binding:** privacy counsel; security lead

---

## 1. Parties

This Data Processing Agreement ("**DPA**") is entered into between:

- **SourceBox LLC** ("**Processor**", "**we**", or "**us**"), the
  operator of Sentinel Command Center; and
- The customer organization identified in the executed Order Form or
  online subscription record ("**Controller**", "**Customer**", or
  "**you**").

Each a "**Party**", together the "**Parties**".

This DPA forms part of and is governed by the Sentinel Terms
of Service ("**Agreement**"). In the event of conflict between this
DPA and the Agreement with respect to processing of Personal Data,
this DPA controls.

## 2. Definitions

Terms not defined here have the meaning given in the Agreement, the
GDPR (Regulation (EU) 2016/679), or the UK GDPR as applicable.

- **Personal Data** — any information relating to an identified or
  identifiable natural person, as defined in Applicable Data
  Protection Law.
- **Applicable Data Protection Law** — all laws and regulations
  applicable to the Parties' respective processing of Personal Data
  under this Agreement, including without limitation the GDPR, the
  UK GDPR, the EU ePrivacy Directive, the CCPA/CPRA, and equivalent
  state, federal, or sectoral law in jurisdictions where Customer
  operates.
- **Sub-processor** — any third party engaged by Processor to process
  Personal Data on behalf of Customer.
- **Customer Personal Data** — Personal Data processed by Processor on
  behalf of Customer under this DPA. The categories are described in
  Annex 1.

## 3. Roles and scope

3.1 In relation to Customer Personal Data, Customer is the
**Controller** and SourceBox is the **Processor**.

3.2 SourceBox processes Customer Personal Data only as instructed by
Customer. The Agreement, this DPA, and Customer's documented use of
the Service constitute Customer's complete and final instructions.
Any additional or conflicting instructions require a written
agreement.

3.3 The subject-matter, duration, nature, and purpose of processing,
along with categories of data subjects and Personal Data, are
described in **Annex 1**.

## 4. Processor obligations

SourceBox shall:

4.1 **Process only on documented instructions** from Customer,
including with regard to international transfers (Section 8). If a
legal requirement compels SourceBox to process otherwise, we will
notify Customer in advance unless that law itself prohibits the
notification on important public-interest grounds.

4.2 **Maintain confidentiality.** All SourceBox personnel authorized
to process Customer Personal Data are subject to a contractual or
statutory confidentiality obligation.

4.3 **Implement appropriate technical and organisational measures**
("TOMs") to ensure a level of security appropriate to the risk,
described in **Annex 2**. Customer's primary technical reference for
TOMs is the public security disclosure at
`/security` on the Command Center website, which we keep current as
the implementation evolves.

4.4 **Engage Sub-processors only with general written authorization.**
The current list of authorized Sub-processors is maintained at
[`SUB_PROCESSORS.md`](./SUB_PROCESSORS.md). SourceBox will provide
Customer at least 14 days' prior notice of any new Sub-processor or
replacement, by updating that file in the public repository and
emailing the Customer's billing contact. If Customer reasonably
objects to a new Sub-processor on data-protection grounds, Customer
may terminate the affected Service and receive a pro-rated refund of
prepaid fees attributable to the unused period; this is the
exclusive remedy.

4.5 **Bind Sub-processors** to data-protection obligations no less
protective than those imposed on SourceBox under this DPA. SourceBox
remains liable to Customer for the acts and omissions of its
Sub-processors as if they were its own.

4.6 **Assist Customer in responding to data-subject requests** (access,
rectification, erasure, portability, objection) by providing the
in-product tooling described in the Agreement (admin export of stream
access logs, audit logs; org-level deletion via Settings → Delete
Organization). Where the Service does not provide self-service
tooling, SourceBox will assist Customer at Customer's reasonable cost
to the extent technically feasible.

4.7 **Notify Customer without undue delay, and in any case within 72
hours** after becoming aware of a Personal Data Breach affecting
Customer Personal Data. The notification will include, to the extent
known: the nature of the breach, categories and approximate number of
data subjects and records, likely consequences, and measures taken
or proposed.

4.8 **Assist Customer with DPIAs and prior consultation** under
Articles 35–36 GDPR (and equivalent provisions under other Applicable
Data Protection Law), to the extent reasonably required given the
nature of processing and information available to SourceBox.

4.9 **At Customer's choice, delete or return all Customer Personal
Data** at the end of the Agreement, and delete existing copies unless
retention is required by Applicable Data Protection Law. Customer's
self-service delete (Settings → Delete Organization) executes this
deletion in a single transaction with no soft-delete grace window.
Backups may persist for up to 30 days under our standard retention
policy and are then overwritten.

4.10 **Make available to Customer all information necessary to
demonstrate compliance** with the obligations laid down in Article 28
GDPR, and allow for and contribute to audits, including inspections,
conducted by Customer or another auditor mandated by Customer, on
reasonable prior notice and during business hours, no more than once
per twelve-month period (more frequently if required by Applicable
Data Protection Law or following a Personal Data Breach). Audit
expenses are borne by Customer except where the audit reveals material
non-compliance, in which case SourceBox bears its own cost.

## 5. Customer obligations

5.1 Customer warrants that it has all necessary rights, consents, and
lawful bases to instruct SourceBox to process Customer Personal Data
under this DPA.

5.2 Customer is responsible for ensuring that its end-users are
informed of the processing as required by Applicable Data Protection
Law, including in any deployment where Customer points cameras at
locations where individuals have a reasonable expectation of privacy.

5.3 Customer is the controller of the **video content** captured by
its CameraNode hardware. SourceBox does not store, copy, or have
remote access to that video; live segments transit Command Center in
RAM only and are not retained. See Annex 1 for the precise data-flow
boundary.

## 6. Personnel and sub-processors

6.1 SourceBox personnel access to Customer Personal Data is restricted
to staff with a need-to-know for service operation, debugging, or
support. Access is logged.

6.2 As of the date of this DPA the Sub-processors listed in
[`SUB_PROCESSORS.md`](./SUB_PROCESSORS.md) are deemed approved.
Updates to that list are governed by Section 4.4.

## 7. Security

The technical and organisational measures applied to Customer
Personal Data are described in **Annex 2** and on the public
`/security` page. SourceBox will maintain at least the level of
security described there for the duration of the Agreement and may
improve, but not materially weaken, those measures.

## 8. International transfers

8.1 SourceBox is established in the United States. Customer Personal
Data may be transferred to or accessed from the United States and
any country in which SourceBox or its authorized Sub-processors
operate.

8.2 Where the transfer of Customer Personal Data from the EEA, UK, or
Switzerland would otherwise be unlawful absent a transfer mechanism,
the Parties agree that the **Standard Contractual Clauses** (Module
Two: Controller-to-Processor) adopted by the European Commission in
Decision (EU) 2021/914, and the UK International Data Transfer
Addendum issued by the UK Information Commissioner's Office, are
incorporated into this DPA by reference and shall apply, with
SourceBox as "data importer" and Customer as "data exporter".

8.3 The Parties agree that the SCC modules and addenda shall be
populated as set out in **Annex 3**.

## 9. Liability

The liability of each Party under and in connection with this DPA is
subject to the limits and exclusions set out in the Agreement.

## 10. Term and termination

This DPA is effective from the date of execution and continues for
the duration of the Agreement plus any period during which SourceBox
processes Customer Personal Data after termination of the Agreement.
Sections 4.7–4.9, 6.1, and 9 survive termination.

## 11. Governing law

This DPA is governed by the law of the State in which SourceBox is
incorporated (Delaware), except that where Applicable Data Protection
Law (e.g. GDPR, UK GDPR) prescribes a different governing law for the
SCCs or equivalent transfer mechanism, that prescribed law applies to
those clauses.

## 12. Order of precedence

In the event of a conflict between the documents:

1. The SCCs (where they apply, by virtue of Section 8.2);
2. This DPA;
3. The Agreement;
4. Annexes to this DPA (in case of conflict among Annexes, the more
   specific Annex prevails over the more general).

---

# Annex 1 — Description of processing

## Subject-matter and duration

SourceBox provides a multi-tenant security-camera management platform.
Processing continues for the duration of the Agreement, plus the
data-retention windows described below for residual logs.

## Nature and purpose

- Authenticating users and authorizing access to Customer's cameras.
- Caching live video segments in Command Center RAM for stream
  delivery (no persistent storage of video).
- Logging metadata about authenticated stream views, audit-relevant
  account events, and operational events (motion, offline transitions).
- Operating billing via the Sub-processor Clerk (which uses Stripe).

## Categories of data subjects

- Customer's users (operators, viewers, admins of the Customer's
  organization in Command Center).
- Camera operators (where distinct from users).
- Individuals incidentally captured by Customer's cameras (whose
  video Customer alone controls; SourceBox does not process this
  video — see Section 5.3).

## Categories of Personal Data

The data SourceBox processes on behalf of Customer is **metadata
only** — never video content. Specifically:

- **Account / identity** (via Clerk): name, email, organization
  membership, role, password hash (Clerk-managed), session tokens.
- **Stream access logs**: viewer user_id, email, IP address,
  truncated user-agent string, camera_id, node_id, timestamp.
- **Audit logs**: event type, user_id, IP address, timestamp,
  free-form details (e.g. "camera created", "settings updated").
- **MCP activity logs**: tool name, key name, status, duration,
  argument summary, timestamp.
- **Motion events**: camera_id, node_id, score, segment sequence,
  timestamp.
- **Notifications**: kind, audience, title, body, severity, timestamp.
- **Billing metadata**: plan slug, subscription status, payment
  past-due timestamps. (Card details handled solely by Stripe; see
  Annex 4.)

## What SourceBox does not process

- **Video content.** Live segments transit Command Center in RAM and
  are evicted on a 15-second window; nothing is written to disk on
  SourceBox infrastructure. Recordings live exclusively on the
  Customer's CameraNode hardware in an encrypted SQLite database
  (AES-256-GCM, machine-derived key) — SourceBox has no copy and no
  remote read access.
- **Snapshot images** (same path as recordings — CameraNode-only).
- **Card numbers, CVVs, or other payment instrument data.** These are
  collected directly by Stripe and never traverse SourceBox systems.

## Retention

Per-tier metadata retention windows for log tables (`stream_access_logs`,
`mcp_activity_logs`, `audit_log`, `motion_events`, `notifications`):

- Free tier: 30 days
- Pro tier: 90 days
- Pro Plus tier: 365 days

After the retention window, rows are automatically purged by the
nightly cleanup loop (`backend/app/main.py::run_log_cleanup`).

Account / identity data is retained for the duration of the
Agreement and deleted on Customer's request or via Settings → Delete
Organization.

# Annex 2 — Technical and organisational measures

> The authoritative live description is at `/security` on the Command
> Center website. This Annex summarizes the measures as of the
> Agreement's effective date.

## Encryption

- **In transit.** TLS 1.2+ for all traffic between CameraNode, Command
  Center, and the Customer's browser / MCP client.
- **At rest (CameraNode).** AES-256-GCM with a key derived from a
  machine-id file on the host. Each blob uses a fresh random nonce
  and an authentication tag bound to the blob's identifying metadata.
- **At rest (Command Center).** Postgres volumes are encrypted at
  rest by the hosting provider (Fly.io). No video content is stored
  on Command Center disks.

## Access control

- Multi-tenant isolation by `org_id` enforced at every API layer.
- Role-based access (admin, viewer) within an org.
- Node API keys are hashed (SHA-256) at rest server-side and are
  encrypted on the node itself.
- Personnel access to production is restricted, logged, and reviewed.

## Resilience and availability

- Live video segments are cached in application RAM only; a process
  restart flushes the cache and recovers from the next CameraNode push.
- The CameraNode continues recording locally during Command Center
  outages; live streaming pauses until connectivity returns.
- Database backups are managed by the hosting provider with daily
  snapshots retained per their standard schedule.

## Monitoring and incident response

- Application errors are captured by Sentry (when `SENTRY_DSN` is
  configured) at a 10% trace sample rate. No video, body content, or
  user identifiers beyond what's required for triage are captured.
- Operator-critical alerts (camera offline + recovered, CameraNode
  offline + recovered, AI-agent-created incident, MCP API key audit
  events, CameraNode host disk approaching full, organization
  membership lifecycle, and motion detection with cooldown + digest)
  are delivered via email through Resend when
  `EMAIL_ENABLED=true` and per-org per-setting
  preference allows. Each email includes a one-click unsubscribe link
  scoped to the (org, kind) pair. Recipients are derived live from
  Clerk org membership at send time; no separate mailing list is
  maintained.
- Personal Data Breach response: documented internally; notification
  obligation in Section 4.7.

## Vulnerability management

- Security issues may be reported privately at the GitHub Security
  Advisories endpoint linked from `/security`.
- Dependency vulnerabilities are tracked via Dependabot and patched
  within reasonable timeframes proportional to severity.

# Annex 3 — Standard Contractual Clauses parameters

> To be completed only when the SCCs are operative under Section 8.2.

- **Module:** Two (Controller to Processor).
- **Clause 7 (docking):** Not applied.
- **Clause 9 (Sub-processor authorization):** Option 2 — General
  written authorization. Notice period: 14 days (Section 4.4 of this
  DPA).
- **Clause 11 (Redress):** Independent dispute resolution body not
  required.
- **Clause 17 (Governing law):** Law of the EU Member State in which
  the data exporter is established (default Ireland if the exporter is
  not in an EU Member State).
- **Clause 18 (Forum and jurisdiction):** Courts of the same Member
  State as Clause 17.
- **UK Addendum (where applicable):** Tables 1–4 populated from this
  Annex; Customer is the "Exporter" and SourceBox is the "Importer".

# Annex 4 — Sub-processors and their roles

The current binding list lives in [`SUB_PROCESSORS.md`](./SUB_PROCESSORS.md).
At the date of this DPA the engaged Sub-processors are:

| Sub-processor | Role | Personal Data |
|---|---|---|
| Clerk | Authentication, session, billing | Account identity, billing metadata |
| Stripe (via Clerk) | Payment processing | Card data (collected by Stripe directly; SourceBox never sees) |
| Fly.io | Application + database hosting | All metadata SourceBox stores |
| Sentry (optional) | Error monitoring | Exception stack traces; no video, no body content |
| Resend (optional) | Transactional email for operator-critical alerts | Recipient email + subject/body of alerts opted-in by the org |

---

> *End of draft.* Lawyer review required before this is sent to any
> customer.
