"""
Pending-run processor — the orchestration layer that's called from
the /wakeup webhook handler.

Flow:
    1. Fetch pending runs from Command Center (FIFO, all orgs).
    2. For each run:
        a. POST /start to claim it.
        b. Connect to OpenSentry MCP with the multi-tenant agent key
           AND an X-Agent-Org-Override header set to run.org_id.
        c. Run the Agent.
        d. POST /complete with the structured outcome.
        e. Disconnect from MCP.

Multi-tenancy:
    The agent serves every org through one deployment. The MCP layer
    enforces per-call org scoping via the override header — the
    server uses run.org_id as the authoritative scope and rejects if
    the org doesn't have Sentinel enabled or isn't on Pro Plus.

Idempotency:
    - /start is idempotent on the CC side; safe to call again if a
      previous wakeup processed the run halfway.
    - /complete is also idempotent (terminal runs reject a second
      complete cleanly).
    - The agent itself is stateless per-run, so retries are clean.

Concurrency:
    Sequential per wakeup. Multiple pending runs in one wakeup
    invocation are processed one after another. Each run gets a
    fresh MCP client (built and torn down) — no cross-run state.
    Async fan-out is a future optimization.
"""

from __future__ import annotations

import asyncio
import logging
from typing import Any

from app.agent import Agent
from app.config import Settings
from app.llm import LLMProvider
from app.mcp_client import MCPClientManager
from app.sentinel_client import SentinelClient

logger = logging.getLogger(__name__)


def _build_mcp_servers_config(settings: Settings, org_id: str) -> dict:
    """Build the MCP servers config for a specific org.

    The agent uses ONE shared bearer (the multi-tenant agent key)
    across every org and tells the MCP server which org each call is
    on behalf of via the X-Agent-Org-Override header.  Per-call
    scoping happens server-side based on the header value, not the
    bearer.

    Defensive coercion on `org_id`: `str(...).strip()` so a bad
    pending row (int instead of str, leading/trailing whitespace
    from a migration glitch) can't either crash httpx (which rejects
    non-str header values) or silently match a different org's
    override on the server.  The MCP server's override-read at
    `mcp/server.py:_resolve_via_agent_key` also strips, so this is
    belt-and-suspenders agreement on shape at both ends.
    """
    safe_org = str(org_id or "").strip()
    return {
        "opensentry": {
            "transport": "streamable_http",
            "url": settings.mcp_url,
            "headers": {
                "Authorization": f"Bearer {settings.opensentry_mcp_agent_key}",
                "X-Agent-Org-Override": safe_org,
            },
        },
    }


async def process_pending_runs(
    settings: Settings,
    llm: LLMProvider,
    sentinel: SentinelClient,
    *,
    max_runs: int = 20,
    in_flight: dict | None = None,
) -> dict[str, Any]:
    """Drain pending runs from Command Center, processing each in
    turn.  Returns a per-run summary for the webhook handler to log.

    Never raises — every failure is captured and surfaced in the
    response payload so the caller (Fly's request scheduler) sees a
    clean 200 even when individual runs error out.

    ``in_flight`` is an optional mutable dict the wrapper uses to learn
    which run was being processed when a wall-clock timeout fires.
    Without it a cancelled run is stranded in `running` state on CC
    forever (start is idempotent but list_pending won't return rows
    already past pending).
    """
    summary: dict[str, Any] = {
        "fetched": 0,
        "processed": 0,
        "errored": 0,
        "skipped_no_org": 0,
        "results": [],
    }

    # Drain until CC reports empty.  Wakeups that land while a drain is
    # running are acknowledged-and-dropped by main.py's _drain_lock
    # (`already_draining`) — without this re-list loop, a run created
    # mid-drain (second camera's motion 30s into a long drain) sat
    # unprocessed until CC's ~5-minute stale-pending re-fire.  The
    # wall-clock in process_with_timeout bounds the whole loop.
    #
    # ``failed_ids``: runs that already took the error path THIS drain.
    # A row can fail and still be `pending` on CC (e.g. /start raised
    # and the best-effort error-complete also failed — a partial outage
    # where reads work but writes don't).  It would then re-list every
    # pass, and because each failure increments `errored`, the forward-
    # progress guard below would never fire — ~40 passes of hammering
    # an already-unhappy CC inside one wall-clock.  One attempt per
    # drain per run; CC's re-fire/reaper own the retries.
    failed_ids: set = set()
    while True:
        try:
            pending = await sentinel.list_pending(limit=max_runs)
        except Exception as exc:  # noqa: BLE001
            logger.exception("processor: failed to fetch pending runs")
            summary["errored"] += 1
            summary["fetched_error"] = str(exc)
            return summary

        if not pending:
            return summary
        summary["fetched"] += len(pending)

        # Forward-progress guard for the re-list loop: rows we can only
        # skip (no org_id, lost claims) stay `pending`/unclaimable and
        # would otherwise be re-listed forever.
        terminal_before = summary["processed"] + summary["errored"]

        for run in pending:
            run_id = run.get("id", "?")
            run_org = run.get("org_id")

            # Already errored once this drain and evidently still
            # pending on CC — don't retry within the same drain.
            if run_id in failed_ids:
                continue

            # Defensive — every pending row should carry org_id, but if
            # one slips through without it we have no way to scope the
            # MCP call. Skip rather than risk acting on a corrupted row.
            if not run_org:
                logger.warning("processor: run %s has no org_id, skipping", run_id)
                summary["skipped_no_org"] += 1
                continue

            # Stamp the in-flight handle BEFORE awaiting so the wrapper
            # sees a current run id even if cancellation lands during
            # _process_one_run's first network round-trip.
            if in_flight is not None:
                in_flight["run_id"] = run_id
                in_flight["org_id"] = run_org

            try:
                await _process_one_run(run, settings, llm, sentinel)
                summary["processed"] += 1
                summary["results"].append({"id": run_id, "status": "ok"})
            except Exception as exc:  # noqa: BLE001
                logger.exception("processor: run %s failed", run_id)
                failed_ids.add(run_id)
                summary["errored"] += 1
                summary["results"].append({
                    "id": run_id,
                    "status": "error",
                    "error": str(exc),
                })
                # Best-effort: tell CC that this run errored so the UI
                # can show it instead of leaving it pending forever.
                try:
                    await _complete_with_retry(
                        sentinel,
                        run_id,
                        outcome="error",
                        summary=f"Agent harness failure: {exc}",
                        tool_call_count=0,
                    )
                except Exception:  # noqa: BLE001
                    logger.exception("processor: complete() also failed for %s", run_id)

            # Note: we deliberately DO NOT clear in_flight in a finally
            # block here.  asyncio.wait_for cancellation runs all finally
            # blocks during stack unwind before re-raising TimeoutError,
            # which would clear the handle BEFORE process_with_timeout
            # sees it.  Letting in_flight retain the most recent run_id
            # is safe — if it points at an already-terminal row, the
            # wrapper's complete() call is an idempotent no-op on CC's
            # is_terminal check.

        if summary["processed"] + summary["errored"] == terminal_before:
            # A full pass moved nothing to a terminal state — every row
            # was skip-only.  Bail; CC's reaper owns those.
            return summary


async def _complete_with_retry(
    sentinel: SentinelClient,
    run_id: str,
    *,
    attempts: int = 3,
    **payload: Any,
) -> None:
    """POST /complete with short backoff.

    The completion is the single most valuable byte the agent sends —
    losing it to a transient CC hiccup orphans the whole run (and any
    incident it filed).  Raises only after every attempt failed.
    """
    delay = 2.0
    for attempt in range(1, attempts + 1):
        try:
            await sentinel.complete(run_id, **payload)
            return
        except Exception:  # noqa: BLE001
            if attempt == attempts:
                raise
            logger.warning(
                "processor: complete() attempt %d/%d failed for %s — retrying in %.0fs",
                attempt, attempts, run_id, delay,
            )
            await asyncio.sleep(delay)
            delay *= 2


async def _process_one_run(
    run: dict,
    settings: Settings,
    llm: LLMProvider,
    sentinel: SentinelClient,
) -> None:
    """Inner loop for a single run.  Raises on unrecoverable failure;
    the caller catches and best-effort marks the run as errored.
    """
    run_id = run["id"]
    org_id = run["org_id"]
    logger.info(
        "processor: start run id=%s org=%s trigger=%s camera=%s",
        run_id, org_id, run.get("trigger_type"), run.get("camera_id"),
    )

    # Claim the run.  If CC says "not found" we bail (nothing to do).
    started = await sentinel.start(run_id)
    if started is None:
        return
    # claimed=False → another drain (overlapping wakeup) won the claim.
    # CC's /start is idempotent and used to return an indistinguishable
    # 200 either way — both drains then ran the full LLM loop and filed
    # DUPLICATE incidents at double spend.
    if isinstance(started, dict) and started.get("claimed") is False:
        logger.info(
            "processor: run %s already claimed by another drain — skipping",
            run_id,
        )
        return

    # Connect to OpenSentry MCP scoped to THIS run's org via the
    # X-Agent-Org-Override header.  Per-run client + per-run scope
    # — no cross-org state can leak because the connection is built
    # and torn down inside this function.
    mcp_config = _build_mcp_servers_config(settings, org_id)
    mcp = MCPClientManager(tool_timeout_seconds=settings.mcp_tool_timeout_seconds)
    try:
        await mcp.connect(mcp_config)
    except Exception:
        await mcp.disconnect()
        raise

    try:
        agent = Agent(llm, mcp, max_iterations=settings.max_agent_iterations)
        result = await agent.run(run)
    finally:
        # MCP teardown is on the failure path too — don't leak the
        # streamable-HTTP connection back to CC.  Bounded: on wall-clock
        # cancellation this finally runs during unwind, and a hung
        # streamable-HTTP teardown here would block wait_for from ever
        # returning — hanging the whole wakeup until Fly kills the VM.
        try:
            await asyncio.wait_for(mcp.disconnect(), timeout=10)
        except Exception:  # noqa: BLE001
            logger.exception("processor: MCP disconnect failed for run %s", run_id)

    # Post the result back — WITH retries.  complete() was single-shot:
    # a transient CC 5xx/redeploy at exactly this moment threw away the
    # real result (the outer fallback posted a generic harness error,
    # orphaning any incident the run actually filed).
    await _complete_with_retry(
        sentinel,
        run_id,
        outcome=result["outcome"],
        summary=result.get("summary", ""),
        tool_call_count=result.get("tool_call_count", 0),
        tool_trace=result.get("tool_trace") or [],
        severity=result.get("severity"),
        incident_id=result.get("incident_id"),
    )
    logger.info(
        "processor: completed run id=%s outcome=%s incident=%s",
        run_id, result["outcome"], result.get("incident_id"),
    )


# ── helper for /wakeup to optionally bound total processing time ────


async def process_with_timeout(
    settings: Settings,
    llm: LLMProvider,
    sentinel: SentinelClient,
    *,
    max_runs: int = 20,
    # MUST stay comfortably under fly.toml's kill_timeout=300 (Fly's
    # maximum).  CC's wakeup client disconnects at 5s, so Fly's idle
    # auto-stop SIGINTs the machine ~30s into every drain; uvicorn's
    # graceful shutdown then keeps the in-flight handler alive for at
    # most kill_timeout before SIGKILL.  A 540s budget meant drains in
    # the 335-540s band died at SIGKILL with the TimeoutError cleanup
    # below unreachable — the stranded run sat `running` until CC's
    # 20-minute reaper.  At 270s the cleanup always beats SIGKILL and
    # leftover pending work chains onto a fresh machine via CC's
    # stale-pending re-fire.
    timeout_seconds: float = 270.0,
) -> dict[str, Any]:
    """Wrap process_pending_runs with a hard timeout so a runaway
    agent loop can't keep the machine alive forever (and so cleanup
    runs BEFORE Fly's SIGKILL — see timeout_seconds above).

    On timeout we know exactly which run was in-flight (via the
    in_flight handle) and best-effort POST /complete with
    outcome=error.  Without this, the run is stranded in `running`
    state on CC: list_pending won't return it on the next wakeup
    (it's no longer pending) and start() doesn't re-claim running
    rows, so it'd sit there forever.
    """
    in_flight: dict = {}
    try:
        return await asyncio.wait_for(
            process_pending_runs(
                settings, llm, sentinel,
                max_runs=max_runs,
                in_flight=in_flight,
            ),
            timeout=timeout_seconds,
        )
    except asyncio.TimeoutError:
        stranded_id = in_flight.get("run_id")
        logger.warning(
            "processor: hit %.0fs wall-clock timeout (stranded run=%s)",
            timeout_seconds, stranded_id,
        )
        if stranded_id:
            try:
                await _complete_with_retry(
                    sentinel,
                    stranded_id,
                    outcome="error",
                    summary=(
                        f"Agent hit the {timeout_seconds:.0f}s wall-clock "
                        f"timeout — investigation incomplete."
                    ),
                    tool_call_count=0,
                )
                logger.info(
                    "processor: marked stranded run %s as errored", stranded_id,
                )
            except Exception:  # noqa: BLE001
                logger.exception(
                    "processor: failed to clean up stranded run %s", stranded_id,
                )
        return {
            "timeout": True,
            "timeout_seconds": timeout_seconds,
            "stranded_run_id": stranded_id,
        }
