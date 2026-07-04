# ADR 0002: Viewer-hours per month is the real tier differentiator

- **Status:** Accepted (April 2026, shipped in commit `a94ef35`)
- **Date:** 2026-04
- **Deciders:** Command Center maintainers + product

## Context

Until April 2026 the tiered pricing was:

| Plan | Cameras | Nodes |
|------|---------|-------|
| Free | 2 | 1 |
| Pro | 10 | 5 |
| Business (later renamed Pro Plus) | 50 | unlimited |

Hardware caps were the only differentiator. The bill scaled with how many cameras a customer plugged in.

Two problems with that model:

1. **Hardware caps don't predict our cost.** A camera that's plugged in but never watched costs us almost nothing — a heartbeat, occasional metadata. A camera that's watched 24/7 by a five-person team chews through egress (HLS segments served same-origin from the in-memory cache). The Pro tier could buy us a customer with 5 idle cameras (low cost) or one with 5 wall-mounted live-watched cameras (high cost). Same price, very different unit economics.
2. **The price points are too low to absorb that variance.** Pro at $12/mo means a customer who watches a single 1080p stream 24/7 burns ~30× the bandwidth that an idle Pro customer does. We can't afford to silently subsidize the heavy users on plans that low.

We considered three responses:
- **Raise prices.** Penalises the 90% of customers who don't drive a lot of viewing.
- **Bill for overage.** Surprise charges erode the "private, predictable" pitch on `/security` and the pricing page. Most consumer-camera competitors do this and customers hate it.
- **Cap the metered feature.** Hard cap on viewing hours per month. When you hit it, live playback pauses; recording, motion events, and the CameraNode itself keep running. Predictable bill, no surprises, the heavy users self-select into the higher tier.

We picked option three.

## Decision

**Monthly viewer-hours per organization is the binding cap. Hardware counts are kept as abuse rails set well above what any realistic customer needs.**

| Plan | Cameras | Nodes | **Viewer-hours / month** | SSE conn. | Log retention |
|------|---------|-------|---|---|---|
| Free | 5 | 2 | 30 | 10 | 30 days |
| Pro | 25 | 10 | **300** | 30 | 90 days |
| Pro Plus | 200 | unlimited | **1,500** | 100 | 365 days |

Source of truth: `backend/app/core/plans.py::PLAN_LIMITS`.

### How it's enforced

- **Counter:** Every successful `GET /api/cameras/{id}/segment/{filename}` calls `record_viewer_second(org_id)`, which increments an in-memory `dict[(org_id, "YYYY-MM"), int]`. One segment ≈ 1 second of video, so the counter is a viewer-second tally.
- **Flush:** A 60-second background task (`_viewer_usage_flush_loop` in `backend/app/main.py`) snapshots the dict, clears it, then UPSERTs each entry into `OrgMonthlyUsage`. One DB write per minute per *active* org. The hot serve path never touches SQLite.
- **Cap check:** Before serving a segment, `get_hls_segment` calls `_warm_cached_viewer_seconds(org_id)` which returns the cached DB total + pending in-memory delta (O(1) after first call per org per process). If `used_seconds >= max_hours * 3600`, return HTTP 429 with `Retry-After: 3600` and an upgrade-prompt message body.
- **Plan resolution for the cap:** Uses `effective_plan_for_caps(db, org_id)`, not `user.plan` from the JWT — see `0001-sync-schema-vs-alembic.md` precedent for "DB-resolved truth beats stale token claims."

### What's not metered

- **Recordings on the CameraNode** — unlimited and free. Local SQLite, the CameraNode's disk is the cap, and the customer owns the hardware.
- **Motion events** — the cap doesn't fire on the motion event channel even if the dashboard is closed.
- **Heartbeats / segment uploads** — push-segment is gated by `disabled_by_plan` (camera-cap, not viewer-cap).
- **Snapshots** — captured + stored locally on the CameraNode, never billed.

## Consequences

**Positive:**
- The bill is exactly the plan price, every month. "We cap, we don't charge extra" is now a real claim.
- Heavy viewers self-select into Pro Plus. Light viewers pay light prices.
- Hardware caps decouple from cost — we can raise the camera ceiling without raising the price ceiling.
- The flush loop costs one UPSERT per active org per minute — totally trivial database load.
- The cap is visible to the customer in the dashboard (live gauge on `DashboardPage.jsx` reading `/api/nodes/plan` → `usage.viewer_hours_used / viewer_hours_limit`).

**Negative:**
- Customer who needs more than 1,500h/mo on Pro Plus needs an out. The pricing page directs them to email us; we'd raise their bucket manually rather than lose them to a wall.
- The in-memory counter is per-process. Multi-worker / multi-VM deployments need either Redis-backed counters or accept that the count is per-replica (acceptable today on Fly's single-replica setup, would need rethinking before going multi-region).
- Year-month rollover at UTC 00:00 — segments served around midnight UTC could feel like the cap reset slightly off the customer's local-clock month boundary. A documentation issue, not a billing issue.
- Cap enforcement is at *segment serve time*, not at *cap-exceeded time*. A user playing a single long stream past their cap will hit a 429 mid-playback. The hls.js layer handles it gracefully (player pauses, surfaces error), but the failure mode is "playback stops without warning" rather than a graceful pre-flight check.

**Neutral:**
- Tier-scaled SSE concurrency caps and per-tier MCP daily call caps shipped in the same commit (`a94ef35`). They share the same enforcement pattern (resolve plan → look up cap → 429 if exceeded) but live in different modules.
- The earlier `business` plan slug was renamed to `pro_plus` during the Clerk-side reorg.  The transitional alias documented here was carried for a short window and removed once every known org had rolled over.

## Revisiting this decision

Re-evaluate if any of the following become true:

- A customer segment emerges that genuinely wants overage billing (security firms running 24/7 multi-site). Build it as opt-in, don't make it the default.
- Egress pricing on Fly (or wherever we host) changes meaningfully. The 30/300/1500 ladder was sized against current egress cost; if that doubles we have to either raise prices or tighten caps.
- The in-memory counter becomes a multi-replica problem before we expect it to. Move the flush to a Redis-backed counter; the existing snapshot+UPSERT pattern translates cleanly.
- Someone wires the CameraNode to push viewer-hours back from the node side as well (defense in depth against a backend bug under-counting). Today the count is backend-only — the CameraNode doesn't know how much its segments are watched.

## References

- `backend/app/core/plans.py` — `PLAN_LIMITS`, `effective_plan_for_caps`, `resolve_org_plan`.
- `backend/app/api/hls.py` — `record_viewer_second`, `_warm_cached_viewer_seconds`, `flush_viewer_usage`, `get_hls_segment`'s 429 path.
- `backend/app/api/nodes.py` — the `/api/nodes/plan` endpoint that surfaces `usage.viewer_hours_used` to the dashboard.
- `backend/app/main.py` — `_viewer_usage_flush_loop` background task.
- `backend/app/models/models.py::OrgMonthlyUsage` — the persistence shape.
- Frontend `DashboardPage.jsx` — the live gauge with green/amber/red thresholds.
- Frontend `PricingPage.jsx` — the customer-facing copy that this ADR justifies.
