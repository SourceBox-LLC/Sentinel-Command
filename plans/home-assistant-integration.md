# Home Assistant integration — implementation plan

## Status (updated 2026-05-30)

| Phase | What | Status |
|---|---|---|
| 1 | Integration API key + auth (`osi_`, `kind` column, cross-kind guard) | ✅ shipped + deployed |
| 1b | Dashboard `/integrations` page to mint/copy/revoke keys | ✅ shipped + deployed |
| 2 | `/api/integration/*` — cameras (LAN-direct URLs), snapshot, recording, status | ✅ shipped + deployed |
| 3 | Motion SSE (`/api/integration/motion/stream`, separate subscriber pool) | ✅ shipped + deployed |
| 4 | HACS custom component (config flow + camera/switch/binary_sensor/sensor) | ✅ built — separate repo `Sentinel-HomeAssistant`, pending publish + on-HA testing |
| 2b | Off-LAN proxy (integration-authed HLS proxy + viewer-hour metering) | ⬜ deferred |

The entire Command Center side (Phases 1–3 + 1b) is live; the HA component
(Phase 4) lives in its own repo and is validated on a real Home Assistant
instance. Notable post-build fix: `require_integration_org` uses a one-shot DB
session so the long-lived motion SSE doesn't pin a connection for its lifetime.

## Context

Sentinel's core design is **one Command Center org owning many CameraNodes**, each
with its own cameras. A node-level Home Assistant integration would force a user
to add every node to HA separately and re-add it whenever a node moves or a new
one joins — O(nodes) setup that breaks on churn. The Command Center already
aggregates every node's cameras into one org-scoped surface, so the integration
belongs **at the Command Center level**: one HA config, one credential, all
cameras across all nodes, auto-syncing as nodes come and go.

Two facts in the codebase make CC-level the clear choice (not just the convenient
one):

- **Motion is already aggregated at the CC, and basically only there.** Nodes
  push motion to the CC over WebSocket
  ([`ws.py::_handle_motion_event`](../backend/app/api/ws.py#L514) →
  `motion_broadcaster`), and the CC re-broadcasts org-wide via an SSE feed
  ([`motion.py::stream_motion_events`](../backend/app/api/motion.py#L178)). The
  CameraNode's *local* motion emit was removed in v0.1.61. Motion-triggered
  automations are the #1 HA use case, and they're a built primitive here.
- **The CC tracks each node's LAN IP.**
  [`CameraNode.local_ip` + `.http_port`](../backend/app/models/models.py#L237)
  are updated on every heartbeat / WS hello, and
  [`CameraNode.effective_status`](../backend/app/models/models.py#L275) tells us
  whether a node is currently reachable (online if a heartbeat landed in the last
  90s).

### Locked decision: CC control plane, LAN-direct data plane

The one tax of a pure CC-level design is **video**: streaming live feeds through
the Fly proxy ([`hls.py`](../backend/app/api/hls.py#L579)) adds cloud latency,
egress cost, and consumes the org's monthly viewer-hour cap
([`max_viewer_hours_per_month`](../backend/app/api/hls.py#L686)) — and a camera
that goes dark when the WAN hiccups is a poor security posture.

Because the CC already knows each node's `local_ip`, the integration hands HA a
**LAN-direct stream URL when HA and the node are co-located**, falling back to the
CC proxy only when HA is off-LAN. One setup, all cameras — but the heavy bytes
stay on the LAN and never touch the viewer-hour meter.

```
  Home Assistant ──(1 integration key)──▶ Command Center  ┐ CONTROL PLANE
      │                                   /api/integration/*  (camera list, motion
      │                                                        SSE, recording, status)
      │
      └──(LAN-direct HLS, co-located)───▶ CameraNode :8080/hls/…  ┐ DATA PLANE
                                          (proxy fallback when remote) ┘
```

---

## Architectural overview

A **HACS custom component** (Python) talks to a new `/api/integration/*`
REST + SSE surface on the Command Center, authenticated by a new per-org
**integration API key** that reuses the existing MCP-key infrastructure.

| HA entity | Backed by | Source |
|---|---|---|
| `camera` per camera | LAN-direct `/hls/{id}/stream.m3u8` (proxy fallback) + snapshot | `Camera` + `CameraNode.local_ip`; `_capture_snapshot_bytes` |
| `switch` (recording) | recording policy toggle | `Camera.continuous_24_7` / recording-settings path |
| `binary_sensor` (motion) | org-wide motion SSE | `motion_broadcaster` / `MotionEvent` |
| `binary_sensor` (online) | `Camera.effective_status` / `CameraNode.effective_status` | models |
| `sensor` (disk, version, viewer-hours) | node storage stats + plan usage | `CameraNode.storage_*`, nodes plan endpoint |

**Four phases, each independently shippable:**

| Phase | What ships | Standalone? |
|---|---|---|
| 1 | Integration API key + auth dependency + management UI | Yes |
| 2 | `/api/integration/*` discovery, stream-URL, snapshot, recording | Yes (curl-able) |
| 3 | Motion SSE for HA `binary_sensor`s | Yes |
| 4 | HACS custom component (config flow + entity platforms) + docs | Yes |

---

## Phase 1 — Integration API key + auth dependency

Goal: a revocable, org-scoped credential HA can carry, without entangling it with
the MCP key surface.

**Recommended approach: extend `McpApiKey` with a `kind` column** rather than a
parallel model — it reuses the already-hardened minting, hashing, revocation,
`last_used_at`, audit, and admin-notify machinery
([`mcp_keys.py`](../backend/app/api/mcp_keys.py)).

- [`models.py::McpApiKey`](../backend/app/models/models.py#L362) — add
  `kind = Column(String(20), nullable=False, default="mcp", server_default="mcp")`.
  Values: `"mcp"` (existing) / `"integration"` (HA). Picked up by `sync_schema()`
  on next boot.
- **Critical cross-auth guard:** the MCP auth path
  ([`mcp/server.py::_resolve_org`](../backend/app/mcp/server.py#L344) and
  [`ScopeMiddleware._lookup_allowed`](../backend/app/mcp/server.py#L267)) currently
  filters `filter_by(key_hash=…, revoked=False)` with no `kind`. Add
  `kind="mcp"` to BOTH so an integration key can't authenticate to MCP tools, and
  the integration dependency filters `kind="integration"` so an MCP key can't hit
  the HA API. Distinct prefixes (`osc_` vs `osi_`) keep them visually
  distinguishable; the cross-auth guard is the real boundary.
- **NEW** `backend/app/core/integration_auth.py` — `require_integration_org`
  FastAPI dependency:
  - Read `Authorization: Bearer osi_…`; SHA-256; look up
    `McpApiKey(kind="integration", revoked=False)`; 401 on miss.
  - Stamp `last_used_at`; return an `AuthUser`-shaped object (org_id + a synthetic
    role) so existing org-scoping helpers compose.
  - Rate-limit (reuse [`limiter`](../backend/app/core/limiter.py); a generous
    per-key cap — HA polls a coordinator, not per-frame).
  - **Plan-gate decision (call out):** gate on Pro/Pro+ like MCP, or allow on
    free? Recommend **allow on free** for the discovery/snapshot/recording control
    plane (it's a trust-building local feature), and let the *proxy* video path
    inherit the existing viewer-hour cap. Revisit if abused.
- Key-management endpoints mirroring [`mcp_keys.py`](../backend/app/api/mcp_keys.py):
  `POST/GET/DELETE /api/integration/keys` with prefix `osi_`, gated by
  `require_admin` (creating a credential is an admin action), audit-logged
  (`event="integration_key_created"`) + admin-notified.
- Frontend: an "Integrations" panel (or a tab on the existing
  [`McpPage`](../frontend/src/pages/McpPage.jsx)) to mint/copy/revoke the key —
  one-time reveal, same UX as MCP keys.

**Verification:** mint a key in the UI; `curl -H "Authorization: Bearer osi_…"`
against a Phase-2 endpoint returns org data; an `osc_` key is rejected by the
integration API and an `osi_` key is rejected by the MCP server; revoke → 401.
Unit tests: `require_integration_org` resolves org from a valid key, 401s on
revoked/unknown/wrong-kind, and the MCP path rejects `kind="integration"`.

---

## Phase 2 — Integration REST surface

Goal: a curl/HTTPie user can enumerate cameras and drive the node before the HA
component exists. New router **NEW** `backend/app/api/integration.py`, all routes
`Depends(require_integration_org)`, registered in
[`main.py`](../backend/app/main.py).

| Route | Returns / does |
|---|---|
| `GET /api/integration/cameras` | Every org camera with `{ id, name, status, video_codec, online, node_id, node_online, stream: { local_url, proxy_url }, snapshot_url, recording }`. `local_url` built only when `node.effective_status == "online"`. |
| `GET /api/integration/cameras/{id}/snapshot` | Live JPEG. Reuse [`_capture_snapshot_bytes`](../backend/app/mcp/server.py#L1234) (already org-scoped + node-online checked). |
| `POST /api/integration/cameras/{id}/recording` | Body `{recording: bool}`. Set `Camera.continuous_24_7`; the heartbeat reconciler drives the node (same path as the dashboard toggle). |
| `GET /api/integration/status` | Org system status: node/camera online split, per-node disk + version, viewer-hour usage. Reuse the [`nodes` plan/status](../backend/app/api/nodes.py#L775) shape. |

**LAN-direct URL builder** (shared helper):
`http://{node.local_ip}:{node.http_port}/hls/{camera_id}/stream.m3u8` when the
node is online and has a `local_ip`; always include `proxy_url`
(`/api/cameras/{id}/stream.m3u8`) so HA can fall back. The node's local HLS is
**unauthenticated by design** (LAN-trust threat model in
[`server/api.rs`](../../OpenSentry-CameraNode/src/server/api.rs#L22)) — acceptable
because HA runs on the same trusted LAN; documented in the integration's threat
notes.

**Verification:** `curl /api/integration/cameras` lists all cameras across nodes
with both URLs; `local_url` plays in VLC on-LAN; snapshot opens to a real JPEG;
recording toggle flips `continuous_24_7` and the node starts archiving within a
heartbeat. Unit tests: discovery payload shape; LAN URL omitted when node offline;
org-scoping (a key for org A never sees org B's cameras).

---

## Phase 3 — Motion SSE for HA `binary_sensor`s

Goal: HA receives real-time motion so automations ("turn on lights when motion in
the driveway") fire.

- **NEW** `GET /api/integration/motion/stream` — SSE that reuses
  [`motion_broadcaster.subscribe(org_id, cap)`](../backend/app/api/motion.py#L194),
  authed by the integration key. Emits the same per-camera
  [`MotionEvent`](../backend/app/models/models.py#L533) JSON
  (`{camera_id, node_id, score, timestamp}`) the dashboard already consumes.
- **SSE subscriber-cap interaction (call out):** the per-org cap
  ([`max_sse_subscribers`](../backend/app/api/motion.py#L193)) counts a persistent
  HA connection as one slot. Either bump the cap for integration keys or give the
  integration its own pool so a dashboard tab and HA don't contend.
- HA side: one `binary_sensor` per camera, flipped `on` on an event and
  auto-reset after a cooldown (configurable, default ~30s) since the feed is
  event-only, not state.

**Verification:** wave at a camera; the corresponding HA `binary_sensor` flips
`on` within ~1–2s and clears after cooldown; an HA automation triggers.

---

## Phase 4 — HACS custom component + docs

Goal: a one-screen HA setup that builds every entity from the integration API.

- **NEW** `custom_components/sentinel/` (separate HACS repo):
  - `config_flow.py` — user enters Command Center URL + integration key; validate
    against `GET /api/integration/status`.
  - `coordinator.py` — `DataUpdateCoordinator` polling `/api/integration/cameras`
    + `/status` (~30s); a background task on the motion SSE.
  - `camera.py` — `async_camera_image()` → `/snapshot`; `stream_source()` →
    `local_url` when reachable, else `proxy_url` (HA reads HLS via ffmpeg/go2rtc).
  - `switch.py` (recording), `binary_sensor.py` (motion + online), `sensor.py`
    (disk / version / viewer-hours).
- Docs: a Command Center docs page ("Connect Home Assistant") + the HACS README.
- Later: zeroconf — the CC could advertise nodes for HA auto-discovery (depends on
  the deferred mDNS work in the CameraNode Local-mode plan).

**Verification:** fresh HA install → add integration → enter URL + key → all
cameras across all nodes appear with live video, snapshots, recording switches,
motion sensors; adding a node in the CC surfaces its cameras in HA on the next
coordinator refresh with no HA reconfiguration.

---

## Engineering risks

1. **Live-stream interop** is the biggest unknown. The node emits h264 MPEG-TS HLS
   (`avc1.*` codec strings in `Camera.video_codec`); HA's stream/go2rtc must
   ingest it. Verify against go2rtc; if low-latency live view matters, an RTSP
   output from the node (bigger change) is the follow-up. HLS works for v1.
2. **LAN-direct over plain HTTP.** The node HLS is unauthenticated HTTP on the LAN.
   Fine for the trusted-LAN target; document it, and gate the proxy fallback
   behind the integration key for off-LAN.
3. **Viewer-hour metering of proxied streams.** LAN-direct dodges it; remote HA
   needs a rule (exempt / separately meter / count). Decide in Phase 2.
4. **Cross-auth between key kinds.** The `kind` filter on BOTH auth paths is
   load-bearing — an omission lets an integration key call MCP tools or vice
   versa. Covered by tests in Phase 1.
5. **SSE subscriber cap contention** between dashboard tabs and a persistent HA
   connection (Phase 3).

---

## Critical files (one-line index)

- [`models.py::McpApiKey`](../backend/app/models/models.py#L362) — add `kind`;
  the credential to reuse.
- [`models.py::Camera` / `CameraNode`](../backend/app/models/models.py#L21) —
  `camera_id`, `node_id` FK, `local_ip`, `http_port`, `effective_status`.
- [`mcp_keys.py`](../backend/app/api/mcp_keys.py) — `osc_` minting pattern to
  mirror as `osi_`.
- [`mcp/server.py`](../backend/app/mcp/server.py) — `_resolve_org` /
  `ScopeMiddleware` (add `kind="mcp"` guard); `_capture_snapshot_bytes` to reuse.
- [`motion.py`](../backend/app/api/motion.py) — `motion_broadcaster` + SSE to
  reuse for the HA motion feed.
- [`hls.py`](../backend/app/api/hls.py) — proxy stream + viewer-hour cap (fallback
  path).
- [`auth.py`](../backend/app/core/auth.py) — dependency pattern the new
  `require_integration_org` sits alongside.
- **NEW** `backend/app/core/integration_auth.py` — `require_integration_org`.
- **NEW** `backend/app/api/integration.py` — the `/api/integration/*` router.
- **NEW** `custom_components/sentinel/` — the HACS component (separate repo).
