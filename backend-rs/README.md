# Command Center — Rust web tier

Mid-migration from Python, by strangler. This process owns the listening
port and serves what it has ported; everything else is forwarded to the
Python application on `PYTHON_UPSTREAM` (localhost, same container).

**The route table in `src/app.rs` is the progress bar.** Anything
registered there is Rust. Anything not registered falls through to
`proxy::forward` and is still Python. When the fallback forwards nothing,
`backend/`, `proxy.rs` and the second process are deleted together.

## Out of scope, deliberately

- `backend/app/sentinel_agent/` — runs as a separate Fly process group and
  owns the only LiteLLM import. Stays Python, deploys independently.
- `backend/app/mcp/server.py` — mounted at `/mcp` as a sub-app, so the
  proxy forwards it. `fastmcp` never has to be replaced.

## Schema

`migrations/0001_adopt_production_schema.sql` is `pg_dump --schema-only`
from production, not a transcription of the SQLAlchemy models — 21 tables,
62 indexes, 27 constraints, verified column-for-column (213/213) against
the live database. Both stacks share one schema for the whole migration;
neither may redefine it.

## Running both tiers locally

```bash
# Python, on the port the proxy forwards to
cd backend && DATABASE_URL=... uv run uvicorn app.main:app --port 8001

# Rust, owning the public port
cd backend-rs && DATABASE_URL=... PYTHON_UPSTREAM=http://127.0.0.1:8001 cargo run
```

Verify a slice by diffing the two: same request to `:8000` and `:8001`,
compare status, content type, body and database side effects.
