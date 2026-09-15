# In-process state: what the strangler cannot cross

The plan assumed routes are stateless handlers over a shared database.
Most are. The ones that are not cannot be ported one at a time, because
the state they read lives in the Python process's memory and there is no
shared store behind it.

This was found the hard way in slice 3: `hls.py` is the plan's headline
target for the "memory argument", and it is the module most thoroughly
blocked.

## The map

| state | module | read by | blocks |
| --- | --- | --- | --- |
| `_segment_cache`, `_playlist_cache` | `api/hls.py` | hls routes, **`mcp/server.py`**, `cameras.py`, `nodes.py`, `webhooks.py`, `main.py` loops | all of `hls.py` |
| `_pending_viewer_seconds`, `_cached_viewer_seconds` | `api/hls.py` | segment serving, `nodes.py` `/plan`, the flush loop | `GET /api/nodes/plan` |
| `manager` (`ConnectionManager`) | `api/ws.py` | `ws.py`, `cameras.py` snapshot, `nodes.py` ws-status + delete, `mcp/server.py` | WebSockets and several node/camera routes |
| `tracker` | `mcp/activity.py` | `/api/mcp/activity/recent`, `/sessions`, `/stats` | those three |
| `_cached_release` | `core/release_cache.py` | `check_node_version` | `GET /api/nodes`, register, heartbeat |
| `_effective_cache`, `_last_resolve_at` | `core/plans.py` | plan resolution, cap enforcement | `/api/nodes/plan` |
| `_transition_debounce` | `api/notifications.py` | notification fan-out | notification writes |
| `_cache` | `core/recipients.py` | email recipient lookup | email |

## Why `hls.py` is the hard one

`mcp/server.py:33` does:

```python
from app.api.hls import snapshot_recent_segment_bytes
```

The MCP server reads recent segment bytes straight out of the in-process
cache to build clips. The plan puts MCP out of scope — it stays Python
behind the proxy at `/mcp`, and `fastmcp` is never replaced. So moving
the segment cache into Rust breaks a component the plan promised not to
touch.

That makes `hls.py` all-or-nothing in a second sense too: segments are
written by `POST /push-segment` and read by `GET /segment/{filename}`
from the same dict, so splitting those two across stacks would serve 404
to every viewer.

This is **not** a latent scaling bug. `fly.toml` pins the `app` process
group to one machine (the volume has a single attachment slot, which is
also why the deploy strategy is `immediate`), and the comments there say
so. The in-process cache is consistent with that constraint.

## The release-cache trap

`GET /api/nodes` looks portable — it is one query plus `to_dict()`. It is
not. It decorates each row with `latest_node_version` and
`update_available` from `check_node_version`, which reads
`release_cache`, an in-process 600-second cache of the newest GitHub
release tag, falling back to `settings.LATEST_NODE_VERSION` when cold.

A Rust port with no such cache would serve the env fallback. Both caches
are cold in a test environment, so **the differential would pass and
production would diverge** — the worst shape of bug this project can
produce. Ported routes must be checked for this class of dependency by
reading, not by testing.

## Options for unblocking `hls.py`

Listed in the order I would consider them, not recommended blindly:

1. **Move segments to shared storage** (Redis, or the attached volume).
   Both stacks then read the same bytes and `hls.py` becomes portable in
   pieces. This is an architecture change to the Python first, provable
   on its own, and it would also lift the one-machine constraint. It is
   the only option that leaves the plan's MCP promise intact.
2. **Port `hls.py` and the MCP clip tool together.** Contradicts the
   plan's explicit scope, and drags `fastmcp` along with it.
3. **Expose the cache from Rust over localhost HTTP** so Python's MCP can
   read it. Cheap to build, but it makes the Python depend on the Rust
   for a core feature — the strangler is supposed to run the other way,
   and it would have to be unwound at slice 8.
4. **Leave `hls.py` to Python permanently.** Defensible: it is the one
   module where the measured benefit was never latency, and the plan's
   own note says the honest target there was a *fix*, not a rewrite.

Nothing here is blocked on a decision to keep making progress — slices 4
and 7 contain plenty of database-backed routes. But slice 3 as written
cannot be completed, and pretending otherwise by porting half of
`hls.py` would break live video.
