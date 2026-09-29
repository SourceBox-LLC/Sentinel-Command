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

## What happened next: option 2, started (2026-09-20)

The four cache routes are Rust's now — `push-segment`, `playlist`,
`stream.m3u8` and `segment/{filename}` — along with the caches
themselves (`src/hls.rs`) and the two background loops that keep them
honest. `POST /motion` stays behind, because it shares `hls.py` with
them but not their state: it reaches the WebSocket module's motion
handling instead.

That is option 2 from the list below, and it commits this branch to
finishing it. **From this commit the branch is not deployable as a
strangler**, and that is not a regression — it is the shape the slice
was always going to have. Rust owns the segment bytes, so every other
reader of them is now reading the wrong process's memory:

| still in Python | what it does to the cache | what breaks until it moves |
| --- | --- | --- |
| ~~`mcp/server.py` `attach_clip`~~ | `snapshot_recent_segment_bytes` | **closed** — the MCP surface is Rust's, see below |
| `cameras.py` delete camera | `cleanup_camera_cache` | a deleted camera keeps serving until the stale sweep |
| `nodes.py` delete, decommission, register | `cleanup_camera_cache` | same |
| `webhooks.py` `organization.deleted` | `cleanup_camera_cache` | same |
| `settings.py` full reset | `cleanup_camera_cache` | same |
| `nodes.py` `/plan` | `get_viewer_seconds_used` | the usage gauge reads zero while Rust counts |

Every row above is closed as of the loops slice. The order out was the
one this document predicted — the MCP surface, then the WebSocket
manager, then the routes, then the loops in `main.py` — and the last of
those is what closed the table.

**All twelve loops are Rust's now**, which matters to this document
specifically because two of them own in-process state of their own: the
disk check's six-hour re-emit debounce, and the email worker's last-tick
stamp that the readiness probe reads. Both live in whichever process
answers the probe, so neither could have moved separately from it — the
same rule the rest of this file is about, applied to a loop rather than
a route.


None of that is hypothetical and none of it is visible to a
differential, because both processes' caches are cold in a test
environment — which is exactly the failure mode this document was
written about. The order out is: the MCP surface (which is what
`attach_clip` needs), then the WebSocket manager, then the routes in the
table, then the loops in `main.py`.

What *is* verified is the part that moved:
`tests/differential/hls_diff.py` compares scenarios rather than
requests, because a segment is only readable from the process that was
pushed it. Forty-seven of them, covering the round trip, all three
eviction policies, the playlist rewriter, and every refusal.

## The MCP surface moved with it (2026-09-28)

Option 2, which the list below calls a contradiction of the plan: the
MCP server is Rust's now, all 23 tools, on `rmcp` instead of `fastmcp`.
That closes the first row of the table above, and it closes it in the
only way the rule in this document allows — `attach_clip` reads the
segment cache, so it had to move to the process that owns the segment
cache, not merely be taught to reach it.

The clip path is verified the same way `hls.py` is, by scenario rather
than by request: `mcp_diff.py` pushes the same segments into EACH tier's
own cache and then calls `attach_clip` against each, because a cache
that lives in a process cannot be shared between two of them. That is
the shape any test of in-process state has to take here, and the reason
the naive form of this test would have passed on two empty caches.

Option 1 would still be the better architecture if the segment cache
ever needs to outlive one machine. Nothing in this slice forecloses it.

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

## The plan cache is invisible to this harness (2026-09-16)

`resolve_org_plan` opens with:

```python
if settings.is_local_auth():
    return "self_host"
```

and `effective_plan_for_caps` short-circuits the same way before it
reads `payment_past_due`. Both differential tiers run
`AUTH_PROVIDER=local`, because that is what lets them share one HS256
secret and accept each other's tokens — so **every plan lookup in every
harness here returns the constant `"self_host"`**, and neither the
Setting fast path, the throttled live Clerk lookup, the 30-second
effective-plan cache, nor the seven-day past-due grace window is
exercised at all.

That matters for the ~20 routes blocked on `core.plans`. Porting them
against this harness would produce a run that is green because the
interesting code never executes — the same shape as the claims corpus
that scored 2,105/2,105 while Rust was more permissive than Python on
four claim shapes, and the camera fixture that reported 31/31 after
ageing out of every state it was meant to cover.

Two things follow, and neither is optional:

1. The hosted billing path needs its own verification, on the model of
   `tests/clerk_verifier.rs`: a fake Clerk billing endpoint on
   localhost, both stacks pointed at it with `AUTH_PROVIDER=clerk`, and
   cases for the entitlement rules that have no local-auth equivalent —
   the first `active` item winning, a `canceled` item whose
   `period_end` is still in the future counting as entitled,
   `period_end` arriving as epoch milliseconds or as a datetime, and a
   failed lookup keeping the cached value rather than downgrading.
2. Whatever runs the local-auth cases must **refuse to report** unless
   it can show the short-circuit was not taken — a coverage guard, not
   a comment.

Until both exist, a plan-gated route moving off the proxy is unproven
no matter what the case count says.

## Two caches, one strangler (2026-09-16)

While both tiers run, each holds its **own** copy of every in-process
cache: the 30-second effective-plan cache, the 60-second resolve
throttle, and the GitHub release cache. A Clerk webhook handled by
Python calls `invalidate_effective_plan_cache()` in the Python process
only, so Rust keeps serving the stale plan for up to its own TTL.

Bounded and self-correcting, and it disappears when the proxy does —
but it is a divergence no differential can see, because both caches are
cold in a test environment. Same class as the release cache already
documented above for `GET /api/nodes`.

## What is left is one connected component, not more slices (2026-09-16)

`blockers.py` reports the remaining routes grouped by primitive, which
makes them look like six independent slices. They are not. The
in-process caches are reached from far outside the modules that own
them:

```
api.hls  (segment cache, playlist cache, viewer-usage accumulator)
   <- api/cameras.py     delete a camera      -> cleanup_camera_cache
   <- api/nodes.py       decommission, delete, plan
   <- api/webhooks.py    organization.deleted
   <- main.py            three background loops
   <- mcp/server.py      snapshot_recent_segment_bytes

api.ws  (ConnectionManager)
   <- api/nodes.py       ws-status, decommission
   <- api/cameras.py     snapshot
   <- api/integration.py snapshot
   <- api/settings.py    danger/full-reset

mcp.activity  (tracker)
   <- api/mcp_activity.py  recent, sessions, stats, stream
   <- mcp/server.py        every tool call
```

A cache is only correct if exactly one process owns it. The moment Rust
owns the segment cache, every call site above has to be in Rust too —
otherwise Python's `cleanup_camera_cache` clears a cache Rust is not
serving from, and a deleted camera keeps streaming out of Rust's copy
until its own eviction runs. That is a correctness bug no differential
can see, because both caches are cold in a test environment.

Those call sites drag in `core.plans`, `core.email` and
`core.license_client` in turn. So the honest shape of the remaining
work is:

* **route-by-route, still safe:** anything that touches none of the
  above. `blockers.py` lists what is left of that, and it is thinning.
* **one atomic slice, ~4,200 lines:** `api/hls.py` (969),
  `api/ws.py` (729), `mcp/server.py` (2,262) and `mcp/activity.py`
  (250), together with the call sites that reach into them and the
  background loops in `main.py`. This is also where the memory argument
  lands — the segment cache is capped at 384 MB on a 985 MB machine,
  and the idle tiers measure 23 MB (Rust, debug) against 143 MB
  (Python).

The strangler bought the first 42 routes cheaply. It does not divide
the last group, and pretending otherwise would mean shipping a
two-process cache split.
