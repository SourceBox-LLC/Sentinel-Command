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

## 2. `jsonable_encoder` missing in the 422 handler

Every custom Pydantic validator 500s instead of returning its 422.
Reachable from the UI by typing `8:30` into the schedule field.

## 3. `AuditLog.to_dict()` calls `.isoformat()` on a nullable column

One row with a NULL timestamp 500s a whole page of audit logs.

## 4. `/settings/motion-ingestion` calls `.lower()` on `None`

500 rather than a 422.

## 5. A JSON body sent with `Content-Type: text/plain` 500s

FastAPI reads the body for a declared Pydantic model but raises when the
media type is not JSON.

## 6. An out-of-range path integer 500s

`incident_id` is unbounded in Python and `Integer` in the column, so
anything past int32 is a `NumericValueOutOfRange` rather than a 404.

## 7. `has_permission` is a substring test when `org_permissions` is a string

`"org:sys_memberships:manage" in claims["org_permissions"]` is a
substring match on a string, so a crafted single-permission claim can
satisfy an admin check. Only reachable if Clerk ever emits
`org_permissions` as a string rather than a list.

---

Cases 2-6 are reproduced every run by
`tests/differential/latent_crashes.sh`, which asserts that Rust serves
through each one. Case 1 has no harness — it is a concurrency property,
not a response.
