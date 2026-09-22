# Bugs found in `backend/` while porting

Found by the differential harnesses or by reading the code they made me
read. **None of these are fixed here.** This branch is a port; changing
the Python underneath it would mean the two stacks no longer agree, and
the whole method rests on them agreeing. They are written down so they
can be fixed on `master` deliberately, with their own tests.

Ordered by severity.

## 1. A Clerk outage wedges the event loop for up to an hour

`app/api/hls.py:696`, in `async def get_hls_segment`:

```python
from app.core.plans import effective_plan_for_caps, get_plan_limits
effective_plan = effective_plan_for_caps(db, user.org_id)
```

That call is synchronous and, on a cache miss for an org whose cached
`org_plan` is not in `PAID_PLAN_SLUGS`, reaches
`fetch_live_plan_slug` → `clerk.organizations.get_billing_subscription()`.

The Clerk SDK's default retry policy is
`BackoffStrategy(500, 60000, 1.5, 3600000)` with
`retry_connection_errors=True` — it retries a 5xx or a connection
failure for up to **one hour** before raising. There is no
`asyncio.to_thread` anywhere in `hls.py`.

So during a Clerk outage, one free-tier viewer requesting one HLS
segment blocks the uvicorn event loop — every request to the entire
Command Center, not just billing — for as long as that retry runs.
`fetch_live_plan_slug`'s own docstring says "SYNC + network-bound: call
via `asyncio.to_thread` from async contexts"; this call site does not.

`async def integration_status` (`app/api/integration.py:357`) has the
same shape via `resolve_org_plan`.

What limits it: the 30-second `_effective_cache` means it only happens
on a miss, and `_last_resolve_at` records the attempt *before* the call,
so other requests for the same org return early for 60 seconds. Neither
helps the caller already holding the loop.

Fix directions: wrap the call in `asyncio.to_thread`, and give the SDK
client a retry budget measured in seconds rather than an hour.

## 2. A list `audio_codec` is stored as `{a,b}` and bypasses validation

`POST /api/cameras/{camera_id}/codec` checks
`len(audio_codec) > 64 or "\n" in audio_codec or "\r" in audio_codec`.
For a JSON list those are the list's own length and element membership,
not the text that gets stored — then psycopg adapts the list to a
Postgres array and the column receives its text form. Measured:
`["a", "b"]` stores `{a,b}`, `["a b"]` stores `{"a b"}`, `[1, 2]`
stores `{1,2}`.

`audio_codec` is written into the HLS `CODECS` attribute, where a comma
separates codecs — so this is a malformed-playlist injection past a
check that exists specifically "to prevent playlist corruption". No
CameraNode sends a list; any authenticated node key can. The Rust port
refuses (500) rather than reproducing it, recorded in
`tests/differential/expected_divergences.md`.

## 3. The codec check allows 64 characters into a 50-character column

The same handler accepts `video_codec` and `audio_codec` up to 64
characters, and both columns are `String(50)`. A 51-64 character codec
passes validation and 500s on commit with
`value too long for type character varying(50)`. The port matches this
(same column, same error) rather than tightening it.

## 4. `jsonable_encoder` missing in the 422 handler

Every custom Pydantic validator 500s instead of returning its 422.
Reachable from the UI by typing `8:30` into the schedule field.

## 5. `AuditLog.to_dict()` calls `.isoformat()` on a nullable column

One row with a NULL timestamp 500s a whole page of audit logs.

## 6. `/settings/motion-ingestion` calls `.lower()` on `None`

500 rather than a 422.

## 7. A JSON body sent with `Content-Type: text/plain` 500s

FastAPI reads the body for a declared Pydantic model but raises when the
media type is not JSON.

## 8. An out-of-range path integer 500s

`incident_id` is unbounded in Python and `Integer` in the column, so
anything past int32 is a `NumericValueOutOfRange` rather than a 404.

SQLAlchemy types the bind parameter from the column, which is what makes
this a database error rather than a query that finds no row — raw
psycopg sends a bigint for the same value and Postgres compares the two
happily. The same shape reaches `camera_groups.id` through
`PUT /api/cameras/{camera_id}/group?group_id=…`, `mcp_api_keys.id`
through the key revoke routes, and `sentinel_runs.tool_call_count` and
`incidents.id` through the agent's run-complete body. A `ge=0` /
`le=<int32>` on each would turn all of them into 422s.

## 9. `has_permission` is a substring test when `org_permissions` is a string

`"org:sys_memberships:manage" in claims["org_permissions"]` is a
substring match on a string, so a crafted single-permission claim can
satisfy an admin check. Only reachable if Clerk ever emits
`org_permissions` as a string rather than a list.

## 10. A negative `days` or `hours` window 500s

`GET /api/audit/stream-logs/stats`, `/api/mcp/activity/logs/stats`,
`/api/motion/events/stats` and `/api/notifications` all declare their
window with an upper bound and no lower one:

```python
days: int = Query(7, le=30)
...
since = datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(days=days)
```

A negative value subtracts backwards, so `?days=-3000000` lands past
year 9999 and `datetime` raises OverflowError — an unhandled 500. Larger
magnitudes fail earlier, inside `timedelta`, and anything past i64 never
gets that far. `?days=-2000000` is fine and simply returns an empty
window, so the boundary moves with the clock.

A `ge=0` on each of the four would make it a 422. Reproduced by the read
differential, which sends the values on either side of the boundary.

---

Cases 4-8 are reproduced every run by
`tests/differential/latent_crashes.sh`, which asserts that Rust serves
through each one. Cases 8 and 10 are also sent by the read differential,
where Rust is required to 500 in exactly the same places — porting the
fault deliberately, because the whole method rests on the two stacks
agreeing. Case 1 has no harness — it is a concurrency property, not a
response.

## 11. An SSE subscriber dropped for being slow keeps a live, dead stream

`app/api/notifications.py`, `NotificationBroadcaster.notify` — and the
same code in `motion.py` and `mcp_activity.py`:

```python
try:
    q.put_nowait(event_data)
except asyncio.QueueFull:
    dead.append((q, is_admin))
```

Dropping a subscriber that has fallen 100 events behind is right: one
stalled browser tab must not hold up an alert to everyone else. What
follows is not. The generator on the other side is still awaiting
`queue.get()` on a queue that is now in nobody's subscriber list:

```python
event = await asyncio.wait_for(queue.get(), timeout=25.0)
```

so it goes on emitting `: keepalive` every 25 seconds, forever, over a
connection that can never deliver another event. The browser's
`EventSource` sees a healthy stream and never reconnects, so the bell
silently stops updating for that tab until the page is reloaded. The
socket, the task and the queue all stay allocated.

It is also self-perpetuating under load: the subscriber most likely to
be dropped is one on a slow link, and it is exactly the one that will
now never recover without a reload.

The fix is to close the queue — or push a sentinel — when dropping a
subscriber, so the generator returns and the client reconnects into a
fresh subscription.

**Reproduced in the port, on purpose.** `src/sse.rs` holds a clone of
the sender inside `Subscription` for no reason other than to keep the
channel open, because in Rust the drop would otherwise end the response
by itself. Fixing it here first would mean the two stacks disagree, and
the whole method rests on them agreeing. It should be fixed once, on
`master`, with a test — and then the `keepalive` field comes out.

## 12. `settings` allows duplicate `(org_id, key)` rows, and every read of one picks arbitrarily

`ix_settings_org_key` is a plain btree index, not a unique one, and
`Setting.set` is a read-then-write:

```python
setting = db.query(Setting).filter_by(org_id=org_id, key=key).first()
if setting: setting.value = value
else: db.add(Setting(org_id=org_id, key=key, value=value))
```

Two concurrent setters that both miss the SELECT both insert, and
nothing stops them. From then on the key has two rows, and every
reader is a `.first()` with no `order_by` — so which value an org's
plan, timezone, past-due flag or email toggle resolves to is whatever
the planner returns first, and it can differ from one query to the
next.

Found by a harness case that inserted a second `timezone` row for an
org the fixture already gave one: the two tiers answered a heartbeat's
`recording_state` differently about half the time, in both directions,
because each picked a different row. That was my case's bug, but the
nondeterminism it exposed is the schema's.

The writers most able to race here are the ones that fire per event
rather than per user action: the Clerk webhook (`org_plan`,
`payment_past_due`), the heartbeat's past-due sweep, and the motion
cooldown anchors, which are written per camera per event.

The fix is a unique index on `(org_id, key)` plus an upsert — 
`ON CONFLICT (org_id, key) DO UPDATE`. It needs a migration that
collapses any duplicates already present, which is why it is not a
one-line change.

**Not reproduced deliberately, and not fixable from this branch.** The
port's `settings::get` is the same `LIMIT 1` without an `ORDER BY`, so
it matches Python exactly — including the part where "matches" means
both are arbitrary. A port that added `ORDER BY id` here would be more
predictable than the thing it is replacing and would diverge from it,
which is the one thing this branch cannot do.
