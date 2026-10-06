# Slice 8: cutting the proxy

> **Historical record.** The plan for the last step of the Python → Rust
> port: removing the Python and its proxy. That step is done (the Python
> was deleted on 2026-09-30 and 2026-10-01; the rewrite merged to `master` on
> 2026-10-05). Kept for the reasoning; not current reference.

The plan says "delete `backend/`, drop the second process". Both halves
of that sentence turn out to be wrong, and the real scope is larger. This
file is the working list, written before the irreversible part starts.

## `backend/` cannot be deleted wholesale

`fly.toml` runs **two process groups from one image**:

```
app   = uvicorn app.main:app          -> becomes the Rust binary
agent = python -m app.sentinel_agent  -> stays Python, out of scope by plan
```

The plan says both "delete `backend/`" (slice 8) and "`sentinel_agent/`
… stays Python and deploys independently" (out of scope). Those
contradict. The agent wins: it owns the only LiteLLM import and was never
part of this rewrite.

Checked rather than assumed — the agent imports nothing from the rest of
the application:

```
$ grep -rhoE "^(from|import) app\.[a-z_.]+" app/sentinel_agent/*.py | sort -u
from app.sentinel_agent.agent
from app.sentinel_agent.config
from app.sentinel_agent.llm
from app.sentinel_agent.mcp_client
from app.sentinel_agent.processor
from app.sentinel_agent.prompts
from app.sentinel_agent.sentinel_client
```

So `backend/app/sentinel_agent/` and a dependency set for it survive;
everything else in `backend/app/` goes.

## Four routes still answer from Python

`/api-docs`, `/api-redoc`, `/api/openapi.json`, `/docs/oauth2-redirect`.
FastAPI generates the schema from its own route table and Pydantic
models, so there is nothing to port faithfully — a Rust document would
never match byte for byte.

Decision: **harvest Python's own document while it still exists**, bake
it in, and serve the two UI shells and the OAuth redirect page around it.
That gives the same schema rather than one I invented, and a checker
compares its paths against `app.rs`'s route table so the snapshot cannot
drift unnoticed. `AGENTS.md` lists `/api/openapi.json` as an API surface,
which rules out quietly dropping it.

## Two operator tools go with the Python, and both are documented

| tool | referenced by | why it cannot just go |
| --- | --- | --- |
| `scripts/restore_from_cloud.py` | `docs/runbooks/DISASTER_RECOVERY.md` (3 invocations), `README.md`, `docs/README.md` | it IS the documented recovery path for a self-hosted install |
| `scripts/hash_local_admin_password.py` | `AGENTS.md` self-host setup | without it there is no way to produce `LOCAL_ADMIN_PASSWORD_HASH` |

Both get ported to Rust binaries. `backup_db.sh` and `restore_db.sh` are
shell and only need relocating.

A detail on the password tool: Rust's `Argon2::default()` is **not**
python-argon2's default. The crate defaults to `m=19456, t=2, p=1`;
python-argon2's `PasswordHasher()` uses `m=65536, t=3, p=4`, which is
what every existing `LOCAL_ADMIN_PASSWORD_HASH` was written with.
Verification is unaffected (the PHC string carries its own parameters)
but the new tool must emit the stronger set, or self-hosters would
silently get weaker hashes than the installs before them.

## A production bug the differential cannot see

`app.rs::mcp_redirect` builds its absolute `Location` from
`request.uri().scheme_str()`, which is `None` for an origin-form request
and falls back to `http`. Behind Fly's edge that emits
`http://…/mcp/` on an HTTPS request — a downgrade strict MCP clients
refuse. It is exactly what uvicorn's `--forwarded-allow-ips=*` exists to
prevent on the Python side, and the harness cannot catch it because the
harness is plain HTTP. Must honour `X-Forwarded-Proto`.

## Order

1. ~~mutation run for the loops~~ (verification of already-committed code)
2. `X-Forwarded-Proto` in `mcp_redirect`
3. harvest the OpenAPI document, port the four docs routes + a drift checker
4. `hash-password` binary
5. `restore-from-cloud` binary
6. relocate `scripts/`
7. Dockerfile: Rust for `app`, Python for `agent`
8. `fly.toml` `[processes]`
9. delete `backend/app/` except `sentinel_agent/`, trim `pyproject.toml`
10. delete `proxy.rs` and the fallback's forward
11. update `AGENTS.md`, `README.md`, `docs/`
12. re-verify what still can be

All twelve are done. Step 12 found more than it was meant to, which is
recorded here rather than only in the commits, because the pattern is the
useful part:

| found | how |
| --- | --- |
| `?format=csv` on three routes was a 502 | `grep proxy::forward src/` — the deletion turned three documented deferrals into three broken downloads |
| the MCP pre-auth body cap had silently stopped applying, and its rate limit was never ported | reading `spa::fallback` and noticing `json_error` had a `429` branch nothing called |
| Sentry was never ported | listing the Python's `app/core/*` and asking which had no counterpart |
| `request_id` / `org_id` in logs were never ported | the same list: `logging_setup.py`, `request_context.py` |
| **all 46 email templates were missing from the repo and the image** | the same list again — and two tests that covered them had been skipping since the deletion |
| nothing scanned the Rust dependency tree | comparing the three advisory gates against the three dependency sets |
| `pip-audit --strict` was already failing | running the new agent job's commands by hand instead of trusting them |
| `tiers.sh` names the wrong password for its own hash | a login that should have worked, during the compose verification |

The common shape: **a check that cannot fail is indistinguishable from a
check that passes.** Two of these were guarded by `if not path.exists():
return`, one by a comparison of two stacks that both said no, one by a
`cargo audit` that was never wired up. Looking for green is not the same
as looking for *reachable*.

## What step 12 can and cannot cover

Every harness in `tests/differential/` diffs against the running Python
web tier. After step 9 they cannot run. That is the cost of this slice
and it is why it comes last.

The harnesses stay in the tree rather than being deleted with the Python.
They are the record of how equivalence was established, and they still
run against the commit before step 9.

**The claim this paragraph used to make about which ones keep working was
wrong**, and it named the wrong example. `column_defaults.py` parses
`backend/app/models/models.py` — it is one of SEVEN static checkers whose
source of truth is the Python, and every one of them dies after step 9.
They were parsing both sides on purpose; that is what made them worth
having, and it is why they cannot outlive one side. Each now refuses with
`deleted_python.require(...)` — exit 2 and a sentence naming the commit
and how to run it against the parent — rather than a
`FileNotFoundError` traceback that reads like a broken script.

What survives unchanged: `openapi_drift.py` and `agent_contract.py`
(written for this world), the generated corpora, and the mutation specs
whose harness does not need a Python tier.

What replaces the rest is the Rust test suite — 386 lib tests, the
database-gated integration tests, and `tests/routing.rs`, which pins the
404/405/SPA answers the proxy used to give.

And one harness came back. `csv_run.sh` serves the Python from a
**worktree of `2baabe6~1`** against the same Postgres as the current Rust
binary, which is how the three `?format=csv` exports — ported after the
cut, and therefore the only slice written with no reference — were still
verified byte for byte: 35/35 identical, 8 quoting probes present. The
same trick is available to any later slice that needs it, which makes
"the Python is gone" a cost rather than a wall.
