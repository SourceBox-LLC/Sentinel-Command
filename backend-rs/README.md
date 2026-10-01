# Command Center — the backend, and the agent

Rust, and the whole of it: one crate, four binaries. `sentinel-command`
is the web tier (axum) and was a Python FastAPI application until the
rewrite. `sentinel-agent` is the Sentinel AI agent (`src/agent/`, rig +
rmcp's client) and was a Python worker on LiteLLM; it runs as its own
Fly process group from the same image. The other two are operator
tools. There is no Python left in the repository.

**The route table in `src/app.rs` is the public surface.** Every route the
service answers is registered there, in one place. It used to be a
progress bar — anything not registered fell through to `proxy::forward`
and was still Python — and that is why the file reads like an inventory.

## How this was done, and how it was checked

A strangler: Rust took the listening port on day one and proxied what it
had not absorbed, slice by slice, with each slice verified against the
running Python before the route moved off the proxy.
`tests/differential/` is that apparatus. The final run before the Python
was deleted:

| | |
| --- | --- |
| reads | 592/592 identical |
| writes (response **and** table contents) | 729/729 |
| MCP (JSON-RPC) | 150/150 |
| background-loop bodies | 7/7 |
| SSE · HLS · WebSocket · plans | 29/29 · 49/49 · 20/20 · 34/34 |

Response diffing alone would not have been enough and the harness says so
in several places: the side-effect snapshots are what caught three loop
bodies that did nothing while the diff stayed green.

### After the cut

The Python is gone, so most of that apparatus cannot run. What holds the
line now:

* **`cargo test`** — 420 tests, including `tests/routing.rs` (the
  404/405/SPA answers the proxy used to give) and the `py*.rs` modules
  that reproduce CPython semantics the port depends on: `json.dumps`
  spacing, `round()` half-to-even, `float()` underscores,
  `fromisoformat`, `int()` coercion, `str()`. Each of those exists
  because a differential case failed on it.
* **`cargo test` with `TEST_DATABASE_URL`** — the same command, plus the
  database-gated integration tests, which skip themselves without it.
* **`cargo clippy --all-targets`** — kept at zero warnings, enforced in
  CI with `-D warnings`.
* **`openapi_drift.py`** — the harvested OpenAPI document vs the route
  table, both directions. Reads source only.
* **`tests/agent_contract.rs`** — the agent's `/complete` body vs what the
  handler reads. A renamed key there would silently record zero tool
  calls on every run, with no 422 and no log line.
* **`tests/differential/agent_run.sh`** — the agent's own differential,
  against the Python agent from the pre-cut worktree and a scripted
  model, on three provider wires.
* **`tests/differential/csv_run.sh`** — the one harness that still runs a
  real differential, by checking out the commit before the deletion as a
  git worktree and serving *that* Python against the same Postgres. It is
  how the three `?format=csv` exports were verified after the reference
  was deleted: 35/35 identical, byte for byte. The pattern is available
  to any later slice that needs it.

The seven static checkers whose source of truth was `backend/app/**`
refuse with exit 2 and the command that runs them against the parent
commit. `tests/differential/README.md` § "After the cut" has the whole
table.

## What this tier does NOT do

* **SQLite.** The Python branched on the `DATABASE_URL` scheme and gave
  self-hosted installs SQLite, with `sqlite:///./sentinel.db` as the
  documented default. sqlx here is built with the `postgres` feature
  only, so `config::unsupported_database_url` refuses a non-Postgres URL
  at startup with a sentence that says what to do — rather than letting
  the pool time out after ten seconds on a URL it was never going to
  open. Porting it means a dialect layer over 266 query sites plus a
  migration derived from `pg_dump`: a slice of its own, and the one
  thing the Python did that nothing here does.
* **Every LLM provider LiteLLM knew.** The agent speaks three wires —
  Ollama, Anthropic, and OpenAI Chat Completions (which, with
  `LLM_API_BASE`, reaches any compatible endpoint). Another provider
  prefix in `LLM_MODEL` is refused at startup with that list.
  `docs/SENTINEL_AGENT.md` § "What the port changed" has the other three
  places the agent deliberately differs from the Python.

## Schema

`migrations/0001_adopt_production_schema.sql` is `pg_dump --schema-only`
from production, not a transcription of the SQLAlchemy models — 21
tables, 62 indexes, 27 constraints, verified column-for-column (213/213)
against the live database. It is applied by `sqlx::migrate!`, embedded at
compile time.

## Running it

```bash
# Postgres, because that is the only engine this build opens.
docker run -d --name sentinel-pg -p 5432:5432 \
    -e POSTGRES_USER=sentinel -e POSTGRES_PASSWORD=sentinel \
    -e POSTGRES_DB=sentinel postgres:16-alpine

DATABASE_URL=postgresql://sentinel:sentinel@127.0.0.1:5432/sentinel \
    cargo run                       # http://localhost:8000
```

Two operator tools build from this crate and ship in the image on
`PATH`, because the documentation names both and the Python scripts they
replace went with the web tier:

```bash
cargo run --bin sentinel-hash-password          # LOCAL_ADMIN_PASSWORD_HASH
cargo run --bin sentinel-restore-from-cloud -- --list
```
