# backend-rs: Command Center and the Sentinel AI agent

One Rust crate, four binaries:

| Binary | What it is |
| --- | --- |
| `sentinel-command` | The web tier ([axum](https://github.com/tokio-rs/axum)): API, MCP server, live-video relay, background loops, and the built frontend. Fly's `app` process group. |
| `sentinel-agent` | The Sentinel AI agent (`src/agent/`, built on [rig](https://github.com/0xPlaygrounds/rig) and rmcp's client). Fly's `agent` process group. See [docs/SENTINEL_AGENT.md](../docs/SENTINEL_AGENT.md). |
| `sentinel-hash-password` | Makes `LOCAL_ADMIN_PASSWORD_HASH` for self-hosted installs. Also takes `--stdin`. |
| `sentinel-restore-from-cloud` | Restores a self-hosted install from its cloud mirror. See [DISASTER_RECOVERY.md](../docs/runbooks/DISASTER_RECOVERY.md#self-hosted-installs-restoring-from-the-cloud-mirror). |

`sentinel-command` and `sentinel-restore-from-cloud` are also built for SQLite (`…-sqlite`); see [Two databases](#two-databases).

**Start with `src/app.rs`.** It registers every route the service answers, in one table. The rest of the internals are documented in [AGENTS.md](../AGENTS.md).

## Run it

```bash
# PostgreSQL (the default build)
docker run -d --name sentinel-pg -p 5432:5432 \
    -e POSTGRES_USER=sentinel -e POSTGRES_PASSWORD=sentinel \
    -e POSTGRES_DB=sentinel postgres:16-alpine
DATABASE_URL=postgresql://sentinel:sentinel@127.0.0.1:5432/sentinel cargo run   # http://localhost:8000

# SQLite
DATABASE_URL=sqlite:///./sentinel.db cargo run --features sqlite
```

Copy `.env.example` to `.env` for the full list of settings. Sign-in needs either Clerk keys or the local-auth variables ([AGENTS.md › Configuration](../AGENTS.md#configuration)).

## Test it

```bash
cargo test                                  # unit and routing tests; DB tests skip themselves
TEST_DATABASE_URL=postgresql://…/test_db cargo test   # plus the PostgreSQL integration tests
cargo test --features sqlite                # the SQLite build; its DB tests always run
cargo fmt                                   # CI fails on unformatted code
cargo clippy --all-targets -- -D warnings   # and with --features sqlite; both kept at zero
```

Point `TEST_DATABASE_URL` at a database of its own: the tests insert rows.

What the tests cover:

- **`src/**`**: unit tests beside the code, including the `py*.rs` modules that reproduce Python behaviours the API depends on (`json.dumps` spacing, `round()` half-to-even, `int()` coercion, `fromisoformat`, `str()`).
- **`tests/routing.rs`**: 404, 405 and SPA answers, the MCP mount, and the docs switch.
- **`tests/*_db.rs`**: everything that touches the database: HLS, plans, loops, sync, MCP scope, races, node auth, notifications, settings.
- **`tests/agent_contract.rs`**: the agent's `/complete` body against the handler that reads it. A renamed field would otherwise make every run record zero tool calls, silently.
- **`tests/clerk_verifier.rs`**: Clerk JWT verification against a local JWKS (`tests/fixtures/` holds a throwaway key pair).

## Two databases

The database driver is chosen **at build time**, not run time: plain `cargo build` for PostgreSQL, `--features sqlite` for SQLite (`src/db.rs` explains why). Query sites are written once and compiled for each. The Docker image carries both builds, and `sentinel-command` replaces itself with `sentinel-command-sqlite` when `DATABASE_URL` is a `sqlite://` URL.

**Writing SQL that runs on both:**

- `$1` placeholders, `RETURNING`, `ON CONFLICT`, `FILTER (WHERE …)` and `NULLS LAST` work on both.
- `CAST(x AS TEXT)`, never `x::text`. `LIMIT n OFFSET m`, in that order.
- Every `ORDER BY` over a nullable column says `NULLS LAST` or `NULLS FIRST`. The engines put NULL at opposite ends, and a unit test enforces this.
- Lists: `format!("id {}", db::any(1))` with `.bind(db::list(&ids))`.
- Case-insensitive matching: `db::ILIKE`, always with `ESCAPE '\\'`.
- No alias on the table in `UPDATE … RETURNING`.
- Bind times from Rust (`now_naive()`); don't use SQL `now()` or `interval`.
- Give every aggregate an `ORDER BY`; the engines group differently.
- To close a check-then-write race, use a conditional `UPDATE`, or `db::lock_for_update(&mut tx, key)` inside a `db::begin_write` transaction.

## Schema

`migrations/0001_adopt_production_schema.sql` is production's `pg_dump --schema-only`, not a transcription: 21 tables, checked column for column against the live database. `migrations-sqlite/` is what the old Python models produced on SQLite, so an existing self-hosted `sentinel.db` opens unchanged. Both are embedded at compile time and applied at start-up by `sqlx::migrate!`. Add a new numbered file to **both** directories for any schema change.

## How the rewrite was checked

The backend and the agent were Python until October 2026. Rust took over gradually, slice by slice, as a "strangler": it served what it had ported and proxied the rest to the Python, and each slice was compared with the running Python before it moved.

`tests/differential/` is that apparatus. It ran both stacks against one database and compared responses **and** table contents. The final run before the Python was deleted:

| | |
| --- | --- |
| Reads | 592/592 identical |
| Writes (response and table contents) | 729/729 |
| MCP (JSON-RPC) | 150/150 |
| Background-loop bodies | 7/7 |
| SSE · HLS · WebSocket · plans | 29/29 · 49/49 · 20/20 · 34/34 |
| Agent, per provider wire | Ollama 20/20 · OpenAI 15/15 · Anthropic 12/12 |

Most of those harnesses need the Python and now run only against the commit before the deletion. `tests/differential/README.md` records how each one worked and what it found. The bugs the port uncovered in the Python are in `PYTHON_BUGS.md`. Both files are historical records.

After the merge to `master`, the code was also checked with:

- schema-driven fuzzing of every REST route and MCP tool;
- an authorization matrix for each credential type;
- a two-org isolation run;
- concurrency races;
- a live production check with a real Clerk user.
