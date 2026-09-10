# Command Center docs

Supplementary documentation for Sentinel Command Center — the SaaS we operate at <https://sentinel-command.com>. The top-level `README.md` is the engineer-facing setup + reference for anyone reading or running the source locally (for audit or fixes); `AGENTS.md` is the developer / LLM-facing architecture reference. End users sign up at the live app; they don't deploy Command Center themselves. The docs in this tree cover the things that don't fit cleanly in either of those two files — operator launch checklist, ADRs, runbooks, and legal templates.

## [LAUNCH_HANDOFF.md](LAUNCH_HANDOFF.md) — what you need to do before paying customers

Twelve user-only items (Clerk prod keys, backup restore test, lawyer signoff, status page vendor, etc.) that every code-side launch blocker has been closed against. Start here if you're driving toward launch.

## [ARCHITECTURE.md](ARCHITECTURE.md) — how the whole system fits together

Start here if you're new. Every repository, every deployed service, and the paths between them — the signal path from camera to browser, the six credential types, where data lives, and how code ships. `README.md` is this repo's setup; `AGENTS.md` is Command Center's internals; ARCHITECTURE is the level above both.

## [SENTINEL_AGENT.md](SENTINEL_AGENT.md) — the AI agent

How the Sentinel AI agent works, how to run one yourself, and every environment variable it reads. It lives in this repo at `backend/app/sentinel_agent/` and deploys as the `agent` process group of the `sentinel-command` Fly app — not, as older references may suggest, a separate repository or app.

## Architecture Decision Records (`docs/adr/`)

One decision per file, numbered in order. ADRs capture the *why* behind a non-obvious choice so future maintainers don't re-litigate it. Format follows Michael Nygard's template (Context / Decision / Consequences).

- [0001-sync-schema-vs-alembic.md](adr/0001-sync-schema-vs-alembic.md) — why we don't use Alembic for backend schema migrations
- [0002-viewer-hour-billing.md](adr/0002-viewer-hour-billing.md) — why monthly viewer-hours, not camera count, are the binding tier limit

## Runbooks (`docs/runbooks/`)

Step-by-step responses to incidents that recur often enough to be
worth writing down. Optimised for being read under pressure — short
sections, command-oriented, not narrative.

- [ON_CALL.md](runbooks/ON_CALL.md) — ten scenarios (A–J) covering
  Sentry alerts, customer-reported camera/stream outages,
  Postgres/database issues, multi-customer incidents, suspected
  breaches, deletion requests, Resend email transport failures, CI
  deploy failures, and pre-deploy sanity checks. Has an append-only
  incident log section to populate as we respond to real ones.
- [DISASTER_RECOVERY.md](runbooks/DISASTER_RECOVERY.md) — the one
  `ON_CALL.md` deliberately doesn't cover: not "the app is slow" but
  "the data is gone." Backup production, the restore procedure, what to
  do when the whole machine/volume is lost, RPO/RTO, and a rehearsal
  drill. Also covers **self-hosted** installs, whose recovery story is
  completely different — no Fly volume or S3 bucket, but a cloud
  mirror they restore from with
  `backend/scripts/restore_from_cloud.py`, including what that
  deliberately does *not* bring back (node API keys, evidence blobs).

## Legal templates (`docs/legal/`)

Working drafts of customer-facing legal documents. Each is marked `DRAFT — NOT FOR EXECUTION` at the top and **must** be reviewed by counsel before it is sent to a customer or relied on as binding. They live in source control so the engineering reality and the legal text don't drift apart unnoticed.

- [DPA.md](legal/DPA.md) — Data Processing Agreement template, including SCC parameter annexes for EEA / UK transfers.
- [SUB_PROCESSORS.md](legal/SUB_PROCESSORS.md) — public sub-processor list with notice policy.

## One fact, one home

The failure mode for a doc set this size isn't missing information, it's the same number written in three places and updated in one. On 2026-09-09 the reasoning behind the agent's isolation existed in three files in three phrasings, and "23 tools" appeared five ways.

The rule:

- **Reference docs state a value once.** Whichever doc owns the subject owns the number. `AGENTS.md` owns Command Center's internals; `SENTINEL_AGENT.md` owns the agent's; `ARCHITECTURE.md` owns facts that only make sense *across* services (like the Fly proxy's bind budget, which is why two services scale to zero and two don't). Everything else links.
- **ARCHITECTURE.md carries structure, not values.** Relationships change rarely; numbers drift constantly. If you're about to add a figure there, check whether the doc that owns the subject should carry it instead.
- **Operational docs may inline a value** where stopping to look it up would make them unusable. `LAUNCH_HANDOFF.md` saying "kill a CameraNode for >90s" is correct; it's an instruction, not a specification.
- **Prefer pointing at code.** A value with a good comment beside it (`fly.toml`'s `[env]` block, `plans.py`) is more durable than the same value copied into prose, because the next person to change it is already looking at it.

## Writing new docs

- **ADR** — when you make a decision that was hard to make, or that someone else will almost certainly re-argue. Write it *while the tradeoffs are fresh*, not six months later.
- **Runbook** — when you catch yourself pasting the same sequence of commands into more than one support thread. Cheap to write, saves time forever. Two exist (`ON_CALL.md`, `DISASTER_RECOVERY.md`); add to those before starting a third file.
- **Legal templates** — `docs/legal/` is for drafts that capture engineering truth; the lawyer-reviewed binding version lives elsewhere (a signed PDF in your records system). Update the draft *whenever* the underlying processing changes (new sub-processor, new data category, new retention window) so the lawyer review stays small.
- **README / AGENTS** — these two are the primary docs and get updated in-place with every feature. Don't fork them into `docs/`.
