# Command Center docs

Documentation that doesn't belong in the two main files. `README.md` introduces the project. `AGENTS.md` is the developer reference for Command Center's internals. Everything else lives here.

User-facing documentation (setting up cameras, recording, notifications) is on the website: <https://sentinel-command.com/documentation/>.

## Start here

| Doc | Read it when |
| --- | --- |
| [ARCHITECTURE.md](ARCHITECTURE.md) | You're new, or need to know which service or repo owns something. Every repository and deployed service, and how they connect. |
| [SENTINEL_AGENT.md](SENTINEL_AGENT.md) | You're working on the AI agent, or running one yourself. |
| [LAUNCH_HANDOFF.md](LAUNCH_HANDOFF.md) | You're preparing for paying customers. The remaining steps that need a person, not code. |

## Runbooks (`runbooks/`)

Written to be read under pressure: short sections and copy-paste commands.

- **[ON_CALL.md](runbooks/ON_CALL.md)**: "the app is broken". Sentry alerts, cameras or streams down, database trouble, suspected breaches, deletion requests, email failures, CI and deploy failures. Ends with an incident log.
- **[DISASTER_RECOVERY.md](runbooks/DISASTER_RECOVERY.md)**: "the data is gone". Production backups and restores, losing the whole machine, recovery targets, a rehearsal drill. It also covers self-hosted installs restoring from the cloud mirror with `sentinel-restore-from-cloud`, and what that can't bring back.

## Decision records (`adr/`)

One decision per file, in order, so nobody has to re-argue it. Context, decision, consequences.

- [0001: schema migrations](adr/0001-sync-schema-vs-alembic.md). Why there was no Alembic, and how the schema is managed now (sqlx migrations).
- [0002: viewer-hour billing](adr/0002-viewer-hour-billing.md). Why monthly viewer-hours, not camera count, are the real plan limit.

## Legal

The **Terms of Service** and **Privacy Policy** are published on the website, from the `sentinel-website` repo: <https://sentinel-command.com/legal/terms> and <https://sentinel-command.com/legal/privacy>. Update the Privacy Policy in the same change as any code that alters what is collected, where it goes, or how long it is kept.

### Drafts (`legal/`)

Marked `DRAFT — NOT FOR EXECUTION`. **Counsel must review them** before they're sent to a customer or relied on. They live here so the legal text and the engineering reality don't drift apart.

- [DPA.md](legal/DPA.md): data processing agreement template, with SCC annexes for EEA and UK transfers.
- [SUB_PROCESSORS.md](legal/SUB_PROCESSORS.md): the public sub-processor list and notice policy.

## Historical records

The backend and agent were Python until the Rust rewrite (merged 2026-10-05). These files record how the port was done and checked. They describe the past and are not maintained as current reference:

- `backend-rs/tests/differential/README.md`: the harnesses that compared Rust against the Python, and their results.
- `backend-rs/PYTHON_BUGS.md`: bugs the port found in the Python.
- `backend-rs/SLICE_8.md`: the plan for deleting the Python.
- `backend-rs/tests/differential/in_process_state.md`: the Python's in-memory state, and how each piece was carried over.

`image-specs/` holds prompts for generated marketing images. It is not documentation of the system.

## Keeping the docs right

**One fact, one home.** A doc set this size fails by saying the same number in three places and updating one. On 2026-09-09, "23 tools" appeared five different ways.

- **Each subject's own doc states its values.** `AGENTS.md` owns Command Center's internals; `SENTINEL_AGENT.md` owns the agent; `ARCHITECTURE.md` owns facts that only make sense across services. Everything else links.
- **`ARCHITECTURE.md` carries structure, not numbers.** Relationships change rarely; numbers drift.
- **Runbooks may repeat a value** when stopping to look it up would make them unusable under pressure.
- **Prefer pointing at code.** A value with a comment beside it (`fly.toml`, `plans.rs`) outlives the same value copied into prose, because whoever changes it is already looking at it.

**Writing new docs:**

- **ADR**: when a decision was hard, or someone will want to re-argue it. Write it while the trade-offs are fresh.
- **Runbook**: when you've pasted the same commands into a second support thread. Add to the two existing runbooks before starting a third.
- **Legal draft**: update it whenever the processing changes (a new sub-processor, data category or retention window), so the next legal review is small.
- **README / AGENTS**: update them in place with every feature. Don't fork them into `docs/`.
