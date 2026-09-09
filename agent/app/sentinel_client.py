"""
HTTP client for talking back to Command Center's Sentinel API.

Authenticates via the shared X-Sentinel-Agent-Key header.  Endpoints
under ``/api/sentinel/runs`` on the Command Center side:
  - GET  /pending        — drain queue (across all orgs)
  - POST /{id}/start     — claim a run, transition to `running`
  - POST /{id}/complete  — terminal callback with outcome + trace

Multi-tenant by design: ``list_pending()`` returns runs across every
org and each row carries its own ``org_id``.  The processor uses that
``org_id`` to set ``X-Agent-Org-Override`` on subsequent MCP calls,
so per-call scoping happens at the MCP layer rather than via per-org
credentials.

Use as an async context manager so the underlying httpx connection
pool is reused across all calls in one wakeup AND torn down cleanly
on the way out:

    async with SentinelClient(base_url=..., agent_key=...) as sentinel:
        runs = await sentinel.list_pending()
        for r in runs:
            await sentinel.start(r["id"])
            ...
            await sentinel.complete(r["id"], outcome=..., ...)

A typical wakeup with N runs makes 1 + 2*N HTTP requests against CC;
without pool reuse that's 1 + 2*N TLS handshakes — material on a
serverless machine billed by the second.  With one shared pool it's
one handshake amortised across the whole drain.
"""

from __future__ import annotations

import logging
from typing import Any, Optional

import httpx

logger = logging.getLogger(__name__)


class SentinelClient:
    """Async HTTP client for Command Center's agent-facing endpoints."""

    def __init__(
        self, base_url: str, agent_key: str, timeout: float = 30.0
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self._headers = {
            "X-Sentinel-Agent-Key": agent_key,
            "Content-Type": "application/json",
        }
        # Single shared client → connection-pool + keep-alive across
        # every call in the wakeup.  Closed via aclose() / __aexit__.
        self._client = httpx.AsyncClient(
            timeout=httpx.Timeout(timeout),
            headers=self._headers,
        )

    async def __aenter__(self) -> "SentinelClient":
        return self

    async def __aexit__(self, exc_type, exc, tb) -> None:
        await self.aclose()

    async def aclose(self) -> None:
        """Close the underlying httpx client.  Safe to call multiple times."""
        try:
            await self._client.aclose()
        except Exception:  # noqa: BLE001
            logger.debug("sentinel_client: aclose ignored error", exc_info=True)

    async def list_pending(self, limit: int = 20) -> list[dict[str, Any]]:
        """Fetch up to ``limit`` pending runs across all orgs.  Returns
        a list of run dicts, oldest-first (FIFO).  Each run includes
        ``org_id`` so the caller knows which org's MCP key to use.
        """
        resp = await self._client.get(
            f"{self.base_url}/api/sentinel/runs/pending",
            params={"limit": limit},
        )
        resp.raise_for_status()
        data = resp.json()
        return list(data.get("runs", []))

    async def start(self, run_id: str) -> Optional[dict[str, Any]]:
        """Claim a pending run — transitions outcome to 'running'.

        Idempotent on the CC side: a second call returns the same row.
        Returns None on 404 (someone deleted the row, or it was never
        ours) so the caller can skip cleanly.
        """
        resp = await self._client.post(
            f"{self.base_url}/api/sentinel/runs/{run_id}/start",
        )
        if resp.status_code == 404:
            logger.warning("sentinel_client: run %s not found on /start", run_id)
            return None
        resp.raise_for_status()
        return resp.json()

    async def complete(
        self,
        run_id: str,
        *,
        outcome: str,
        summary: str = "",
        tool_call_count: int = 0,
        tool_trace: Optional[list[dict[str, Any]]] = None,
        severity: Optional[str] = None,
        incident_id: Optional[int] = None,
    ) -> Optional[dict[str, Any]]:
        """Mark a run terminal with its final outcome.

        ``outcome`` is one of: incident | no_action | error.
        ``severity`` (low|medium|high|critical) is required when
        outcome=incident.
        ``incident_id`` is the Command Center incident the agent filed
        (if any).
        """
        body = {
            "outcome": outcome,
            "summary": summary[:8000],
            "tool_call_count": tool_call_count,
            "tool_trace": tool_trace or [],
        }
        if severity is not None:
            body["severity"] = severity
        if incident_id is not None:
            body["incident_id"] = incident_id

        resp = await self._client.post(
            f"{self.base_url}/api/sentinel/runs/{run_id}/complete",
            json=body,
        )
        if resp.status_code == 404:
            logger.warning("sentinel_client: run %s not found on /complete", run_id)
            return None
        resp.raise_for_status()
        return resp.json()
