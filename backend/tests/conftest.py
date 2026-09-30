"""Shared fixtures for the Sentinel AI agent's tests.

What this file used to be: 
FastAPI test client, Clerk bypass, a dialect-parametrised database — all
scaffolding for the web tier's 57 test files, which went when the web
tier did.

What remains needs none of it. `app/sentinel_agent/` is the only Python
in this repository now, it opens no database and serves no HTTP, and its
one surviving test exercises the provider layer with a stub. The file
stays rather than being deleted so `uv run pytest` still has a rootdir
and so the next person adding an agent test has somewhere obvious to put
a fixture.

The contract that used to live in `test_agent_contract.py` — every key the
agent sends to `/complete` being a field Command Center reads — moved to
`backend-rs/tests/differential/agent_contract.py`. It could not stay
here: it worked by importing Command Center's Pydantic model, and
Command Center is Rust now. The new one reads both sides as source,
because no process can hold both.
"""
