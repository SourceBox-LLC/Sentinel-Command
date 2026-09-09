"""
Agent loop — runs an LLM ↔ MCP tool conversation for a single
sentinel_run task and returns the structured outcome.

Each call to ``Agent.run()`` is a single agent invocation that runs to
completion (terminal outcome OR tool-budget exhausted).  No
conversation persistence — the agent is stateless across calls.
That's intentional: the run record on Command Center is the source of
truth, and the agent's job is to read the trigger, do the
investigation, and post a single complete result.

Design choices that matter:

- **Structured task input.**  Slice 2 of the rollout removed the
  free-form chat shape; runs come in as ``{trigger_type, camera_id,
  manual_prompt, ...}`` and the agent picks the right system prompt.
- **Structured outcome output.**  Returns a dataclass-shaped dict
  the caller can pass directly to ``SentinelClient.complete()``.
- **Outcome inference.**  When the LLM's terminal message contains
  signal that an incident was filed (an ``incident_id`` mentioned by
  the model OR returned from a ``create_incident`` / ``finalize_incident``
  tool call), we surface it.  Otherwise the run is no_action.
- **Bounded trace.**  Tool trace is truncated at the model's hard
  iteration limit; per-tool result is truncated at 800 chars in the
  trace (the LLM still gets the full result via the messages array).
"""

from __future__ import annotations

import json
import logging
from typing import Any

from app.sentinel_agent.llm import (
    LLMProvider,
    assistant_message,
    has_images,
    image_message,
    prune_images,
    tool_call_arguments,
    tool_result_message,
)
from app.sentinel_agent.mcp_client import MCPClientManager
from app.sentinel_agent.prompts import initial_user_message, system_prompt_for_trigger

logger = logging.getLogger(__name__)


# Match "incident #423 created" / "Incident #1234" / "incident: 17" etc.
# MCP tools whose return value carries the canonical incident_id.
# Both write tools return the incident dict via Incident.to_dict()
# (server.py:1283-1284 and :1372-1373); the dict's "id" field is the
# row primary key.
_INCIDENT_ID_BEARING_TOOLS = frozenset({"create_incident", "finalize_incident"})


class Agent:
    """Stateless run-handler. Each call processes ONE sentinel run."""

    def __init__(
        self,
        llm: LLMProvider,
        mcp: MCPClientManager,
        max_iterations: int = 10,
    ) -> None:
        self.llm = llm
        self.mcp = mcp
        self.max_iterations = max_iterations

    async def run(self, run: dict) -> dict[str, Any]:
        """Process a single run task. Returns a result dict shaped for
        SentinelClient.complete() — caller passes it through.

        Result shape:
            {
                "outcome":  "incident" | "no_action" | "error",
                "summary":  str,
                "tool_call_count": int,
                "tool_trace":   list[{tool, args, result}],
                "severity":  Optional[str],     # only when outcome=incident
                "incident_id": Optional[int],   # only when outcome=incident
            }
        """
        trigger_type = run.get("trigger_type", "manual")
        system_prompt = system_prompt_for_trigger(trigger_type)
        first_user_msg = initial_user_message(run)

        messages: list[dict[str, Any]] = [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": first_user_msg},
        ]
        tools = self.mcp.get_tools_for_llm()
        tool_trace: list[dict[str, Any]] = []
        incident_id: int | None = None
        severity: str | None = None

        for iteration in range(self.max_iterations):
            try:
                response_msg = await self.llm.chat(messages, tools=tools)
            except TimeoutError:
                logger.warning(
                    "agent: LLM call timed out at iter %d (%.0fs)",
                    iteration, self.llm.timeout_seconds,
                )
                return _truncated_result(
                    incident_id,
                    severity,
                    (
                        f"LLM call timed out after "
                        f"{self.llm.timeout_seconds:.0f}s at iteration "
                        f"{iteration}. Investigation incomplete."
                    ),
                    tool_trace,
                )
            except Exception as exc:
                logger.exception("agent: LLM call failed at iter %d", iteration)
                return _truncated_result(
                    incident_id, severity, f"LLM call failed: {exc}", tool_trace,
                )

            messages.append(assistant_message(response_msg))

            # Terminal: model responded without calling a tool.
            if not response_msg.tool_calls:
                summary = (response_msg.content or "").strip()
                # NO free-text incident-id recovery here.  The regex
                # fallback let prose corrupt outcomes: "…incident #7…"
                # (a camera literally named that, injected on-screen
                # text, or hallucination) turned clean no_action runs
                # into outcome=incident pointing at an unrelated id —
                # or a CC 400 that downgraded the run to error.  The
                # authoritative signal is _parse_id_from_tool_json on
                # write-tool returns, captured below.

                outcome, severity_out = _classify_outcome(
                    incident_id=incident_id,
                    severity=severity,
                    summary=summary,
                )
                return {
                    "outcome": outcome,
                    "severity": severity_out,
                    "incident_id": incident_id if outcome == "incident" else None,
                    "summary": summary or "(agent returned no summary)",
                    "tool_call_count": len(tool_trace),
                    "tool_trace": tool_trace,
                }

            # Otherwise, fan out the tool calls.  Everything appended
            # from this index on is the CURRENT iteration's batch — the
            # pruning pass below must not touch it (the LLM hasn't seen
            # any of it yet).
            batch_start = len(messages)
            for tc in response_msg.tool_calls:
                # OpenAI-shaped providers hand arguments back as a JSON
                # string, Ollama as a dict; the helper normalises both and
                # degrades to {} on malformed JSON rather than killing the
                # run — the tool then fails its own validation with a
                # message the model can react to.
                args = tool_call_arguments(tc)
                logger.info("agent: tool %s(%s)", tc.function.name, args)

                result = await self.mcp.call_tool(tc.function.name, args)

                # Snapshot tool-call into the trace.  Keep result short
                # so the trace fits inside CC's 50-entry × ~JSON budget.
                tool_trace.append({
                    "tool": tc.function.name,
                    "args": _sanitize_args(args),
                    "result": (result.get("text") or "")[:800],
                })

                # Sniff for incident filing signal — track the most
                # recent severity the model selected on a write tool.
                if tc.function.name in ("create_incident", "update_incident"):
                    sev = (args or {}).get("severity")
                    if sev in ("low", "medium", "high", "critical"):
                        severity = sev

                # Authoritative incident_id capture — JSON-parse the
                # return of create_incident / finalize_incident.  The
                # MCP server returns the full Incident.to_dict() on
                # success, so `id` is reliable.  Most recent write
                # wins (e.g. finalize after create returns the same id
                # but with the report body filled in — either way the
                # id is the same row).  Only mutate on success
                # (error tool results don't carry an id field).
                if tc.function.name in _INCIDENT_ID_BEARING_TOOLS:
                    parsed_id = _parse_id_from_tool_json(
                        result.get("text", "")
                    )
                    if parsed_id is not None:
                        incident_id = parsed_id

                # Feed the tool result back to the LLM.
                messages.append(
                    tool_result_message(tc, result.get("text", ""))
                )
                if result.get("images"):
                    # Separate user message: an OpenAI-shaped `tool`
                    # message cannot carry an image at all.
                    messages.append(
                        image_message(tc.function.name, result["images"])
                    )

            # Image pruning: every base64 frame appended above is
            # otherwise RE-SENT on every subsequent LLM call — O(N²)
            # retransmission that can blow the context window mid-run
            # (→ run error) and scales token spend with loop length.
            # Prune only PREVIOUS iterations' frames (indices below
            # batch_start) — the model ingested those on an earlier
            # call.  The current batch stays INTACT: a multi-camera
            # fan-out (e.g. the scheduled sweep's view_camera-per-
            # camera) appends several image messages in one iteration,
            # and pruning to "newest only" here would make the model
            # assess cameras whose frames it never saw.
            for stale in messages[:batch_start]:
                if not has_images(stale):
                    continue
                prune_images(stale)

        # Iteration budget exhausted.
        return _truncated_result(
            incident_id,
            severity,
            (
                f"Reached max iterations ({self.max_iterations}) without a "
                "terminal answer. Investigation may be incomplete."
            ),
            tool_trace,
        )


# ── helpers ──────────────────────────────────────────────────────────


def _truncated_result(
    incident_id: int | None,
    severity: str | None,
    summary: str,
    tool_trace: list,
) -> dict:
    """Terminal result for a run cut short (timeout / LLM failure /
    iteration budget).

    If the run already FILED an incident before being truncated, report
    outcome=incident with that id — the old blanket outcome=error
    dropped the id on the floor, orphaning a real incident from its run
    record (and CC stores no id for error outcomes).
    """
    if incident_id is not None:
        return {
            "outcome": "incident",
            "severity": severity or "low",
            "incident_id": incident_id,
            "summary": f"{summary} An incident was filed before the cutoff.",
            "tool_call_count": len(tool_trace),
            "tool_trace": tool_trace,
        }
    return {
        "outcome": "error",
        "summary": summary,
        "tool_call_count": len(tool_trace),
        "tool_trace": tool_trace,
    }


def _parse_id_from_tool_json(text: str) -> int | None:
    """Extract `id` from a JSON-shaped MCP tool return.

    The server returns Incident.to_dict() for create_incident and
    finalize_incident, so a clean JSON parse with an int `id` field
    is the canonical signal that an incident was filed.  Returns
    None for malformed payloads, error envelopes (`{"error": ...}`),
    or non-int id values.
    """
    if not text:
        return None
    try:
        payload = json.loads(text)
    except (TypeError, ValueError):
        return None
    if not isinstance(payload, dict):
        return None
    # Reject explicit error envelopes — these come from
    # mcp_client.py:121-122 when a tool call raised on the server.
    if "error" in payload and "id" not in payload:
        return None
    raw_id = payload.get("id")
    if isinstance(raw_id, bool):  # bool is a subclass of int in Python
        return None
    if isinstance(raw_id, int):
        return raw_id
    return None


def _classify_outcome(
    *,
    incident_id: int | None,
    severity: str | None,
    summary: str,
) -> tuple[str, str | None]:
    """Decide the run's terminal outcome from the available signals."""
    if incident_id is not None:
        return ("incident", severity or "low")
    return ("no_action", None)


def _sanitize_args(args: dict | None) -> dict:
    """Best-effort small-dict-only args for the trace.  Strips
    enormous values (base64 blobs, raw frame payloads) so the trace
    stays under CC's row size budget.
    """
    if not isinstance(args, dict):
        return {}
    out = {}
    for k, v in args.items():
        if isinstance(v, str) and len(v) > 200:
            out[k] = v[:200] + "…"
        elif isinstance(v, (dict, list)) and len(json.dumps(v, default=str)) > 200:
            out[k] = "<truncated>"
        else:
            out[k] = v
    return out


