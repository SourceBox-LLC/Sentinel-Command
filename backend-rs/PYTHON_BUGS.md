# Bugs found in `backend/` while porting

> **Historical record.** Written during the Python → Rust port (September
> 2026), when the Rust had to reproduce the Python exactly and so could
> not fix these. The Python was deleted on 2026-09-30 (web tier) and 2026-10-01 (agent), and many of these
> bugs have since been fixed in the Rust backend; the code comments cite
> the entry number where they were (`PYTHON_BUGS.md #13`, …). For current
> behaviour, read [AGENTS.md](../AGENTS.md) and the code, not this file.

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

**Closed in the Rust, 2026-10-03**, once the Python was gone and there was nothing left to agree with: the codec check is the column's 50 characters, and a non-string codec is a 400 rather than a TypeError's 500 (`api/node_writes.rs`, `CODEC_MAX_CHARS`). SQLite and PostgreSQL now give the same answer.

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

**Closed in the Rust, 2026-10-03**, once the Python was gone and there was nothing left to agree with: `query::int4` answers an out-of-range id with a 422. `latent_crashes.sh` case 4 now expects 422 from Rust.

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

**Closed in the Rust, 2026-10-03**, once the Python was gone and there was nothing left to agree with: every `hours` / `days` window has a floor of 0, so a negative one is the same 422 any other out-of-range parameter gets, and the overflow path is unreachable.

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

**Closed in the Rust, 2026-10-03**, once the Python was gone and there was nothing left to agree with: the subscription no longer holds a spare sender. A subscriber dropped for falling behind drains what it had queued, its stream ends, and the browser's EventSource reconnects with a fresh one (`sse.rs`, pinned by `a_dropped_subscriber_drains_what_it_had_and_then_ends`).

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

## 13. A scoped agent key bypasses the agent tool allowlist entirely

**This one is an escalation, not a quirk.** Everything else in this
file is a behaviour worth knowing about. This is a hole.

`_AGENT_ALLOWED_TOOLS` exists for a stated reason, and the comment
above it says it plainly: the agent's LLM is steered by content an
attacker can influence — camera names, and text held up to a lens —
and "disable recording, then report all clear" is the canonical
injection against a camera product. So the agent gets reads plus
incident authoring, and `set_camera_recording_policy` is excluded.

`ScopeMiddleware._lookup_allowed` enforces that for exactly one
credential: the shared, multi-tenant `SENTINEL_AGENT_MCP_KEY`. It then
falls through to `McpApiKey` by hash. A **scoped per-org agent key**
lives in `sentinel_agent_keys`, a different table, so it matches
neither — `_lookup_allowed` returns `None`, and `on_call_tool`'s gate
is skipped:

```python
allowed = self._lookup_allowed()
if allowed is not None and name not in allowed:   # allowed IS None
    raise ToolError(...)
```

`_resolve_org`'s path 1b then authenticates the same key perfectly
well. The tool runs.

Verified against the running Python, not inferred. With
`osa_…0001` from the fixture and Sentinel enabled for the org:

```
before: continuous_24_7 = t
tools/call set_camera_recording_policy {camera_id: cam-live,
                                        continuous_24_7: false}
  -> {"success": true, ...}, isError: false
after:  continuous_24_7 = f
```

`tools/list` on the same key returns all 23 tools, including the one
the allowlist excludes.

The credential this affects is the one given to customers running the
agent themselves — the case the scoped key was introduced FOR. So the
customer-hosted agent, on hardware SourceBox does not control, has
strictly more tool access than SourceBox's own multi-tenant agent,
which is backwards.

The fix is one lookup: `_lookup_allowed` should consult
`sentinel_agent_keys` the same way `_resolve_org` does, and return
`_AGENT_ALLOWED_TOOLS` when it matches. `compute_agent_allowed_tools`
already takes its inputs as parameters, so the allowlist itself needs
no change.

**Reproduced in the port, and it should not stay that way.** The two
stacks have to agree while both are serving, and a port that quietly
closed this would diverge on the one case that matters. It is
reproduced, it is pinned by a differential case that asserts the
CURRENT behaviour, and that case is marked so it fails loudly when
master is fixed — which is the signal to change both together.

**Closed in the Rust, 2026-10-03**, once the Python was gone and there was nothing left to agree with: `mcp::auth::lookup_allowed` recognises a scoped `osa_` key and returns the agent allowlist for it, exactly as for the shared key. `tests/mcp_scope_db.rs` asserts it on both databases and fails against the old lookup; `mcp_diff.py` now expects the allowlist and a refused `set_camera_recording_policy` for the scoped key.

## 14. Three MCP tools validate the same `camera_id` three different ways

Severity: **low** — a cosmetic inconsistency, not a security or data
problem. Recorded because it is the kind of thing a port has to decide
about deliberately, and all three behaviours are reproduced.

`camera_id` is the same optional string argument on all three, and the
empty string reaches three different answers:

| tool | the check | `camera_id: ""` |
| --- | --- | --- |
| `create_incident` | `if camera_id:` | accepted, **and stored as `""`** |
| `add_observation` | `if camera_id is not None:` | looked up and refused, "not found" |
| `get_stream_logs` | `if camera_id:` | no filter at all — every row comes back |

The middle one is defensible and the first is not: `create_incident`
skips the existence check for a falsy value and then assigns the column
the argument anyway, so the incident row ends up naming a camera id that
matches no camera. Nothing reads it back for a join, so nothing breaks —
it just means an incident can carry `camera_id = ''` where every other
"no camera" incident carries NULL, and a `GROUP BY camera_id` over
incidents shows an extra bucket.

`get_stream_logs` is the surprising one to a caller rather than a bug:
passing an empty filter widens the result instead of narrowing it.

The fix, if it is ever worth making, is to normalise the empty string to
`None` at the argument boundary for all three, which makes `""` mean
"absent" everywhere. That changes `add_observation`'s refusal into a
success, so it is a behaviour change and not a tidy-up.

**Reproduced in the port**, all three, and pinned: `mcp_diff.py` carries
an empty-`camera_id` case for each, with a comment on each pair saying
which way it goes and why. The pairs were not chosen for coverage — I had
written the Rust with the falsy reading applied uniformly, and the cases
exist because writing them down is what showed the three tools disagree.

**Closed in the Rust, 2026-10-03**, once the Python was gone and there was nothing left to agree with: one reading everywhere — an empty `camera_id` is absent (`mcp::tools::opt_camera_id`). `create_incident` stores NULL, `add_observation` accepts it, `list_incidents` and `get_stream_logs` do not filter on it.

## 15. The dashboard document ships with no `X-Request-Id`

Severity: **low** — a support-and-debugging gap, not a correctness or
security one. It is here because it is the *same bug, in the same place*,
as one already fixed beside it.

`app/main.py`'s `request_context` middleware stamps `X-Request-Id` on the
response, and its own comment says why: "returned in the response header
so a customer can quote it in a support ticket and we can find their
exact request in seconds."

The SPA middleware is registered LAST, which makes it the OUTERMOST
middleware, and it returns a `FileResponse` directly without calling down
the stack. So `request_context` never runs for it, and every response it
serves — the dashboard HTML document, `/assets/*` — carries no request
id. Measured on both stacks: Python omits it on `/`, `/dashboard`,
`/incidents`, `/mcp`, `/mcp/`; Rust emits it on all five.

The document is the one response a customer is actually looking at when
they open a ticket, so it is the worst one to be missing.

This is the exact failure `_apply_security_headers` exists to fix. Its
docstring:

> Factored out because the SPA middleware below is registered LAST —
> making it the OUTERMOST middleware — and it returns FileResponse
> objects directly, without calling down through this middleware.
> Result before the factor-out: the dashboard HTML document and every
> /assets/* file shipped with NO X-Frame-Options / nosniff / HSTS

The security headers were given an explicit call from the SPA paths.
`X-Request-Id` was not, and nothing pointed at it — which is how one half
of a two-part bug survives its own fix. The fix is the same shape: set it
on the SPA responses too, from the same place the security headers are
set.

**NOT reproduced in the port**, deliberately, on the same grounds as the
500-headers divergence already recorded in `expected_divergences.md`:
nothing can depend on the header's absence, and matching it would mean
writing code whose only purpose is to strip it. `http_diff.py` therefore
does not compare `x-request-id` on an SPA response, and says so at
`is_spa_response`.

Found by adding `GET /mcp` to the read differential — the first case in
the suite to compare an SPA response's headers at all.

## 16. Four of the nine synced tables never reach the cloud mirror

Severity: **high** — no local data is lost, but the backup this feature
exists to be is missing the cameras, the camera groups, the nodes and
the Sentinel run history. A restore from it rebuilds an install with
incidents and motion events attached to cameras that do not exist.

Found by the loops differential: the port pushed nine tables and the
Python pushed five.

`app/core/sync_client.py::_push_table` builds each row's envelope
inline:

```python
"rows": [
    {
        "id": str(row.id),
        "updated_at": getattr(row, spec.cursor_attr).isoformat(),
        "data": _row_payload(row),
    }
    for row in rows
],
```

`getattr(...)` returns `None` for a row whose cursor column is NULL, and
`None.isoformat()` raises `AttributeError`. That happens while building
the payload — **before the POST** — so the caller's per-table `except`
catches it, logs a warning, and moves on having sent *nothing at all*
for that table. Not a partial batch: nothing.

Measured against the differential fixture:

| table | rows with a NULL cursor | reaches the mirror |
| --- | --- | --- |
| `cameras` | 22 of 37 | **no** |
| `camera_groups` | some | **no** |
| `camera_nodes` | some | **no** |
| `sentinel_runs` | 7 of 16 | **no** |
| `incidents`, `incident_evidence`, `motion_events`, `sentinel_config`, `notifications` | none | yes |

The NULLs are legitimate. `updated_at` has no `server_default` on these
models, so any row written by a path that does not set it explicitly
keeps NULL — and ONE such row is enough to block the entire table
forever, because the failure is per-table and every cycle hits it again.

The warning it logs is indistinguishable from a transient 5xx, which is
why this has never surfaced: the loop looks like it is working, and the
only way to notice is to attempt a restore.

Three fixes, and the first two are both needed:

1. Exclude rows whose cursor column is NULL in the query — a row with no
   cursor cannot participate in an incremental sync meaningfully, and
   including one poisons the table.
2. Give those columns a `server_default` so the NULLs stop being
   created. `scripts/restore_from_cloud.py` should then be run against a
   real mirror to find out what is actually in it, since the table above
   says the answer today is "less than anyone would assume".
3. Separately, one unserialisable row should not discard a whole batch
   of good ones.

### Measured through the restore tool

Demonstrated end to end rather than argued, once
`sentinel-restore-from-cloud` existed to ask the mirror what it holds:

```
$ sentinel-restore-from-cloud --list
table                        rows  deleted
incident_evidence              16        0
incidents                       4        0
motion_events                 108        0
notifications                 143        0
sentinel_config                 1        0
sentinel_runs                  16        0
```

No `cameras`. No `camera_groups`. No `camera_nodes`. Asking it to restore
them answers `No mirrored data for table "camera_nodes"` — **the
documented disaster-recovery path cannot bring back a single camera or
node**, because the mirror was never sent one.

Filling the NULLs by hand and re-syncing makes all eight appear, which
pins the mechanism to the NULLs and nothing else:

```
$ UPDATE cameras SET updated_at = now() WHERE updated_at IS NULL;   -- 22 rows
$ UPDATE camera_nodes SET updated_at = now() WHERE updated_at IS NULL;  -- 6
$ UPDATE camera_groups SET updated_at = now() WHERE updated_at IS NULL; -- 4
$ sentinel-restore-from-cloud --list
camera_groups                   4        0
camera_nodes                    6        0
cameras                        30        0
…
```

That one-off UPDATE is also the immediate mitigation for any install
already running: it does not fix the bug, but it unblocks the tables
until fix (1) or (2) lands.

**Reproduced in the port**, deliberately and loudly: `src/sync.rs`
rejects the batch before the push, with a comment pointing here. The two
stacks have to agree while both are serving, and this is the single most
important thing in that file to fix on master.

**Closed in the Rust, 2026-10-03**, once the Python was gone and there was nothing left to agree with: a NULL cursor sorts and compares as the epoch, so such a row is pushed on the first pass and again whenever it is next updated; no table is skipped. Every Rust insert into the four tables sets `updated_at`, so only rows the Python wrote can be NULL.

## 17. A new user's first page load can be a 500

**Where:** `backend/app/api/notifications.py::_get_or_create_state`

```python
state = db.query(UserNotificationState).filter(...).first()
if state is None:
    state = UserNotificationState(clerk_user_id=..., org_id=..., last_viewed_at=now)
    db.add(state)
    db.commit()
```

Check-then-insert against `uq_user_notif_state_user_org`. On first load
the dashboard asks for the inbox, the unread count and the notification
SSE stream at the same instant, and all three call this for a user with
no row yet. Two of them see "absent", both insert, and the loser raises
`IntegrityError` — an unhandled 500 on the first page a new user ever
sees.

**How it was found:** not by the differential, which could not have
found it. Every harness here sends one request at a time, so this path
scored identical for the whole port; it takes two requests in flight.
It surfaced when a real browser was pointed at a fresh self-hosted
install and the console showed a 500, with
`duplicate key value violates unique constraint` in the server log.

Python is likelier to get away with it — its sync handlers run on a
threadpool and the window is narrower — but nothing prevents it, and it
needs only a fresh user, which is every user once.

**Not reproduced in the port.** This is the one entry in this file where
the Rust deliberately differs, because the Python is no longer serving
and there is nothing left to agree with: `get_or_init_state` uses
`INSERT … ON CONFLICT DO NOTHING` and reads the row back, so the loser
returns the winner's cursor. `tests/notification_state_db.rs` fires
twenty first-requests at once; it fails without the conflict clause.

**Fix on master, if the Python is ever served again:** the same —
`INSERT … ON CONFLICT DO NOTHING` (`on_conflict_do_nothing()` on the
Postgres dialect), or catch `IntegrityError`, roll back and re-query.

## 18. The Python agent cannot make a tool call under the locked `mcp` 2.x

`backend/uv.lock` resolves `mcp` to **2.2.0** — on this branch and on
`master`, since the Dependabot bump of `fastmcp` to 4.0.3 (`c958b4c`).
`app/sentinel_agent/mcp_client.py` handles one of the 2.x renames, the
import, and misses two others:

```python
from mcp.client.streamable_http import streamable_http_client as streamablehttp_client
...
streamablehttp_client(url=url, headers=headers)   # 2.x takes no `headers`
...
"parameters": tool.inputSchema                    # renamed in 2.x
```

Under 2.x the client takes `http_client=create_mcp_http_client(headers=…)`
and yields a 2-tuple, not the 3-tuple the `async with` unpacks. Every run
fails at connect, before the model is asked anything, and is reported as
`Agent harness failure: …`.

**What that means in production:** `mcp_client.py` is identical on
`master`, and so is the lock. If the deployed image was built after that
bump, Sentinel AI has not completed a run since. I could not confirm
which lock the running machine was built from — worth checking the run
history for a wall of `error` outcomes starting 2026-09-11.

**How it was found:** building the reference side of the agent
differential. The Python agent would not run a single scenario from the
repository's own lock; the reference venv had to pin `mcp==1.28.1`
(`tests/differential/agent_run.sh`) before there was anything to compare
against. The 16 agent tests pass throughout, because none of them opens
an MCP connection.

**Not reproduced in the port** — there is no Python SDK in it. The Rust
agent uses `rmcp`'s client.

**Fix on master:** either pin `mcp<2` in `backend/pyproject.toml` (the
range is `>=1.6.0,<3` on purpose, and that purpose is not met), or build
the headers into an `http_client`, unpack two values, and read
`tool.input_schema` with a fallback.

## 19. Frames are appended between two tool results of the same turn

`agent.py` appends a tool's result, then — if the tool returned images —
a `user` message carrying them, inside the loop over the turn's tool
calls. A turn with two calls where the first returns frames therefore
produces:

```
assistant(tool_calls=[A, B])
tool(A)
user(images from A)
tool(B)
```

The OpenAI and Anthropic APIs both require every tool result for an
assistant turn to follow it before any other message. LiteLLM reorders
this for Anthropic and passes it through unchanged for OpenAI (seen at
the fake provider; the rejection itself was not tested against the live
API).
Ollama accepts anything, which is why it never surfaced: Ollama is the
production wire.

**How it was found:** the agent differential's frames scenario, which
makes two calls in one turn on all three wires.

**Not reproduced in the port.** `agent/run.rs` appends every tool result
for the batch and then the frames. `agent_diff.py::tools_before_frames`
removes exactly that reordering from the comparison and nothing else.

**Fix on master:** collect the image messages and extend `messages` with
them after the `for tool_call in …` loop.

## 20. A restore from the cloud mirror loses every evidence row on a fresh database

`scripts/restore_from_cloud.py` restores tables in the order the mirror
lists them (`for summary in wanted`, where `wanted` is the service's
`/v1/sync/tables` response). Three foreign keys cross the mirrored
tables — `cameras.node_id`, `cameras.group_id` and
`incident_evidence.incident_id` — and nothing orders parents first. With
an alphabetical listing, `incident_evidence` is restored before
`incidents`, and every row is refused:

```
insert or update on table "incident_evidence" violates foreign key
constraint "incident_evidence_incident_id_fkey"
```

The per-row SAVEPOINT that makes "one bad row does not sink the restore"
true also makes this quiet: the run finishes, reports the failures, and
restores everything else. Running it a SECOND time works, because the
incidents now exist. The scenario the tool is for — a new machine after
a lost disk — is exactly the one where the database is empty and the
first run is the one that matters.

**How it was found:** the mirror round trip added for the SQLite build
(`tests/differential/dialect_restore.sh`), which restores into an EMPTY
database. PostgreSQL to PostgreSQL failed 16 of 288 rows before SQLite
was involved at all. The earlier restore check had gone into the
database the rows were pushed from.

**Not reproduced in the port.** `sentinel-restore-from-cloud` sorts the
tables parents-first (`restore_rank`) and a unit test names each foreign
key.

**Fix on master:** the same — order `wanted` so `camera_groups`,
`camera_nodes` and `incidents` come before `cameras` and
`incident_evidence`.
