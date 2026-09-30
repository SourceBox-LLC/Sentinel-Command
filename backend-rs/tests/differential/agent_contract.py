#!/usr/bin/env python3
"""Hold the Sentinel agent's `/complete` body to what Command Center reads.

Replaces `backend/tests/test_agent_contract.py`, which did this by
importing Command Center's Pydantic model — and which therefore could not
survive the web tier being deleted. Its docstring said the contract test
was the concrete reason the agent moved into this repository, so losing it
was not an option.

**The failure it guards against is silent, which is the whole point.**
The agent hand-builds the JSON body in
`app/sentinel_agent/sentinel_client.py`. Pydantic ignored unknown fields
by default and the Rust handler ignores them too: if Command Center
renamed `tool_call_count`, the agent would keep sending the old key,
Command Center would drop it on the floor and default the column to 0,
and nothing would surface — no 422, no exception, no log line. Every run
would silently record zero tool calls.

So this reads both sides as source and compares the key sets:

  * the agent's keys, from the `body = {...}` literal and the two
    conditional `body[...] =` assignments after it;
  * Command Center's, from the `errors.<reader>(&body, "name", …)` calls
    in `post_run_complete`.

Neither side is imported. The agent is still Python and Command Center is
Rust, so there is no process that could hold both — which is exactly the
situation the original test was written to escape and the reason this one
parses text instead.

Usage: agent_contract.py
"""
from __future__ import annotations

import pathlib
import re
import sys

HERE = pathlib.Path(__file__).resolve().parent
RS = HERE.parent.parent
REPO = RS.parent
AGENT = REPO / "backend" / "app" / "sentinel_agent" / "sentinel_client.py"
HANDLER = RS / "src" / "api" / "sentinel.rs"


def agent_keys() -> set[str]:
    """Every key the agent puts in the `/complete` body.

    Read out of `complete()`, not the whole file: the client also posts
    to `/start`, and mixing the two bodies would compare a union against
    one handler and pass while hiding a mismatch in either.
    """
    source = AGENT.read_text()
    start = source.find("async def complete(")
    if start < 0:
        print("REFUSING: could not find the client's complete() in the agent",
              file=sys.stderr)
        raise SystemExit(2)
    # Up to the POST, which is the end of the body construction.
    end = source.find("self._client.post", start)
    block = source[start:end if end > 0 else len(source)]

    keys = set(re.findall(r'^\s+"(\w+)":', block, re.M))
    # `body["severity"] = …` and `body["incident_id"] = …`, the two the
    # agent sends only when it has them.
    keys.update(re.findall(r'body\["(\w+)"\]\s*=', block))
    if not keys:
        print("REFUSING: parsed no keys out of complete_run — the pattern has rotted",
              file=sys.stderr)
        raise SystemExit(2)
    return keys


def handler_keys() -> set[str]:
    """Every field `post_run_complete` reads out of the body."""
    source = HANDLER.read_text()
    start = source.find("pub async fn post_run_complete")
    if start < 0:
        print("REFUSING: could not find post_run_complete", file=sys.stderr)
        raise SystemExit(2)
    # The reads all happen before the outcome is validated; stop at the
    # end of the function to avoid picking up a later handler's.
    end = source.find("\npub async fn ", start + 10)
    block = source[start:end if end > 0 else len(source)]

    keys = set(re.findall(r'errors\.\w+\(&body,\s*"(\w+)"', block))
    if not keys:
        print("REFUSING: parsed no body reads out of post_run_complete",
              file=sys.stderr)
        raise SystemExit(2)
    return keys


def main() -> int:
    sent = agent_keys()
    read = handler_keys()

    print(f"agent sends {len(sent)}: {', '.join(sorted(sent))}")
    print(f"handler reads {len(read)}: {', '.join(sorted(read))}")

    bad = 0
    # The dangerous direction. A key the agent sends and Command Center
    # does not read is data thrown away in silence.
    ignored = sent - read
    if ignored:
        bad += 1
        print(f"\nFAIL  {len(ignored)} key(s) the agent sends are IGNORED:")
        for key in sorted(ignored):
            print(f"        {key}")
        print("      Command Center drops unknown fields without complaint, so this")
        print("      surfaces as a column silently holding its default — no 422, no")
        print("      exception, no log line. Rename on both sides or read it.")

    # The other direction is a weaker signal but still worth saying: a
    # field the handler reads and the agent never sends is either a
    # default that is load-bearing or a leftover.
    unsent = read - sent
    if unsent:
        print(f"\nnote: {len(unsent)} field(s) the handler reads that the agent "
              f"does not send: {', '.join(sorted(unsent))}")
        print("      Fine if each has a deliberate default (`summary`, `tool_trace`)")
        print("      or comes from another caller; worth a look otherwise.")

    if not bad:
        print("\nevery key the agent sends is read")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
