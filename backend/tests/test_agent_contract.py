"""
Contract tests between the Sentinel AI agent and Command Center.

The agent (agent/app/sentinel_client.py) hand-builds the JSON body that
Command Center's ``RunCompleteBody`` validates. Until both lived in one
repo the two sides agreed only by coincidence — nothing checked them, and
nothing could, because a test in either repo could not see the other.
That is the concrete reason the agent moved in here.

The failure this guards against is SILENT, which is what makes it worth
testing rather than eyeballing. Pydantic ignores unknown fields by
default, so if Command Center renamed ``tool_call_count`` the agent would
happily keep sending the old key, CC would drop it on the floor and
default the column to 0, and no error would surface anywhere: no 422, no
exception, no log line. Every run would just silently record zero tool
calls. ``test_every_key_the_agent_sends_is_a_real_field`` is the check
that turns that into a red test.

These tests drive the agent's REAL client through a mock transport rather
than restating its payload shape. A copied dict would drift from the
source it is supposed to be pinning, which is the bug, not the test.
"""

from __future__ import annotations

import json
from typing import Any

import httpx
import pytest
from pydantic import ValidationError

from app.api.sentinel import _VALID_TERMINAL_OUTCOMES, RunCompleteBody
from app.sentinel_agent.sentinel_client import SentinelClient

# Both sides are now one package, so this is a plain import. It used to
# be an importlib load-by-path: the agent lived in its own project whose
# top-level package was also called `app`, and putting it on sys.path
# would have made `import app` ambiguous for every other test here. That
# collision is gone with the agent under app/sentinel_agent/.


async def _capture_complete_body(**kwargs: Any) -> dict:
    """Run the agent's real ``complete()`` and return the JSON it sent."""
    captured: dict = {}

    def handler(request: httpx.Request) -> httpx.Response:
        captured.update(json.loads(request.content))
        return httpx.Response(200, json={"ok": True})

    client = SentinelClient(base_url="https://cc.example", agent_key="osa_test")
    # Swap in a mock transport but keep the client's own headers/timeout
    # so we exercise the object the agent actually constructs.
    await client._client.aclose()
    client._client = httpx.AsyncClient(transport=httpx.MockTransport(handler))
    async with client:
        await client.complete("run_abc", **kwargs)

    assert captured, "agent sent no body"
    return captured


# ── The contract ─────────────────────────────────────────────────────


async def test_minimal_complete_body_validates():
    """The body the agent sends with only `outcome` set must validate."""
    body = await _capture_complete_body(outcome="no_action")
    RunCompleteBody(**body)  # raises if the contract broke


async def test_incident_complete_body_validates():
    """The fullest body the agent can send — an incident with a trace."""
    body = await _capture_complete_body(
        outcome="incident",
        summary="Person detected at the side door.",
        tool_call_count=3,
        tool_trace=[{"tool": "view_camera", "args": {"camera_id": 1}}],
        severity="high",
        incident_id=42,
    )
    parsed = RunCompleteBody(**body)
    # Values must survive the round trip, not merely be accepted.
    assert parsed.outcome == "incident"
    assert parsed.severity == "high"
    assert parsed.incident_id == 42
    assert parsed.tool_call_count == 3
    assert parsed.tool_trace == [{"tool": "view_camera", "args": {"camera_id": 1}}]


async def test_every_key_the_agent_sends_is_a_real_field():
    """Guards the silent-drop failure described in the module docstring.

    Pydantic ignores unknown keys, so a field rename on the CC side would
    cost the agent data with no error anywhere. Comparing key sets is the
    only way that surfaces.
    """
    body = await _capture_complete_body(
        outcome="incident",
        summary="x",
        tool_call_count=1,
        tool_trace=[],
        severity="low",
        incident_id=1,
    )
    unknown = set(body) - set(RunCompleteBody.model_fields)
    assert not unknown, (
        f"agent sends {sorted(unknown)}, which RunCompleteBody does not declare. "
        "Pydantic drops these silently — the run would record wrong data with no error."
    )


@pytest.mark.parametrize("outcome", sorted(_VALID_TERMINAL_OUTCOMES))
async def test_each_terminal_outcome_round_trips(outcome: str):
    """Every outcome CC accepts must survive the agent's builder.

    `severity` is required by CC only for incidents, so send it there.
    """
    body = await _capture_complete_body(
        outcome=outcome,
        severity="low" if outcome == "incident" else None,
    )
    assert RunCompleteBody(**body).outcome in _VALID_TERMINAL_OUTCOMES


async def test_summary_truncation_matches_cc_max_length():
    """The agent truncates summaries with `summary[:8000]`; CC declares
    `max_length=8000`. These are two independent 8000s in two files, and
    if CC's ever drops below the agent's, every long summary starts
    422-ing in production — after the LLM has already been paid for.
    """
    body = await _capture_complete_body(outcome="no_action", summary="a" * 20_000)
    RunCompleteBody(**body)  # must not raise

    # And the agent's ceiling is not merely under a much larger CC limit:
    # 8000 is genuinely where CC cuts off, so the two are actually pinned
    # to each other rather than coincidentally compatible.
    with pytest.raises(ValidationError):
        RunCompleteBody(outcome="no_action", summary="a" * 8_001)
