# Expected divergences

Ten claim sets where Rust deliberately does not match Python. Each is
listed in `expected_divergences.jsonl`; the harness fails on any
divergence *not* in that file, and also on any entry in it that has
stopped diverging, so the list cannot quietly go stale.

Every one is a claim of the wrong JSON type. Clerk signs session tokens,
so none of these is forgeable — reaching one means Clerk changed its wire
format or something upstream is broken.

## The rule

Python has no type checking on claims. It therefore does one of two
things with a wrong-typed claim:

* **Raises** — `pla`, `fea`, `o.per`, `o.fpm` of the wrong type, or a
  non-object `o`, all reach a `.split()` or `.get()` that blows up. The
  blanket `except Exception` in `_get_current_user_clerk` turns that into
  `401 Authentication failed`. **Rust matches these exactly**, via
  `ClaimError::Malformed` → `AuthError::Failed` → the same 401. They are
  not in the divergence list.

* **Silently coerces** — `sub`, `org_id`, `org_role`, `email`,
  `username`, `o.id`, `o.rol` and the permission lists are used as
  whatever they happen to be, and an `AuthUser` is built around them.
  **Rust refuses instead.** These are the ten.

## Why refusing is the right side to be on

`AuthUser.has_permission` is `permission in self.org_permissions`. When
`org_permissions` is a list that is a membership test. When it is a
*string* — which nothing stops it being — it silently becomes a substring
test:

```
org_permissions = "xxorg:cameras:manage_cameras"
"org:cameras:manage_cameras" in org_permissions   ->  True
```

That claim set is in the corpus, and the harness records what each stack
does with it: Python resolves an `AuthUser` with **`is_admin: true`**.
Rust rejects the token.

This is a latent privilege escalation in the Python, not merely an
untidy coercion, and it is the reason the divergence is worth keeping
rather than "fixing" by copying Python's behaviour. It is not currently
reachable — Clerk sends `org_permissions` as an array — so it is a
hardening note for the Python service, not an incident.

## The list

| claims | Python | Rust |
| --- | --- | --- |
| `o: {id: 5, rol: 6}` | resolves, `org_id: 5` | 401 |
| `org_permissions: "abc"` | resolves, perms `['a','b','c']` | 401 |
| `org_permissions: "xxorg:cameras:manage_cameras"` | resolves, **`is_admin: true`** | 401 |
| `permissions: "abc"` | resolves, perms `['a','b','c']` | 401 |
| `org_permissions: [1, 2, 3]` | resolves, perms `[1,2,3]` | 401 |
| `org_permissions: {"a": 1}` | resolves, perms `['a']` | 401 |
| `sub: 12345` | resolves, `user_id: 12345` | 401 |
| `org_id: 999` | resolves, `org_id: 999` | 401 |
| `org_role: 7` | resolves, `org_role: 7` | 401 |
| `email: 5, username: true` | resolves with those values | 401 |

# Ported routes (slice 2)

Two more deliberate divergences, both cases where the Python raises and
returns 500. They are asserted by `latent_crashes.sh` rather than living
in the main HTTP differential, because each makes Python 500 for *every*
request to its route — leaving the rows in the fixture would turn every
other case on that route red and hide real regressions.

Neither is reachable in production today (every writer sets the columns
involved), so both are hardening notes for `backend/`, not incidents.

## `AuditLog.to_dict()` crashes on a NULL timestamp

```python
"timestamp": self.timestamp.isoformat(),   # column is nullable
```

`timestamp` has no `nullable=False`, so one odd row 500s an entire page
of audit history — up to 500 rows fail because of one. Rust serves
`"timestamp": null` for that row and the rest of the page normally.

Reproduced: `GET /api/audit-logs?limit=500` → rust 200, python 500.

## `/settings/motion-ingestion` crashes on a NULL value

```python
enabled = Setting.get(db, org_id, "motion_ingestion_enabled", "true").lower() == "true"
```

`Setting.get` returns `setting.value` when a row exists, which is `None`
for a NULL value — and `.lower()` on it raises. The sibling routes
(`/settings`, `/settings/notifications`) compare the same shape of data
with a bare `==` and do not crash, so this is an inconsistency between
three routes reading the same table rather than a considered choice.

Rust answers `{"motion_ingestion_enabled": false}`. Deliberately
**disabled**, not enabled: this is a kill switch for a runaway sensor
flooding events, and silently re-opening it because its stored value
became unreadable is the wrong direction. Defaulting to `true` would
have matched the route's *documented* default while defeating its
purpose in exactly the case where it matters.

Reproduced: `GET /api/settings/motion-ingestion` → rust 200, python 500.

## One inconsistency deliberately preserved

`/settings/motion-ingestion` lowercases before comparing; `/settings` and
`/settings/notifications` do not. So a stored `"TRUE"` reads as *enabled*
for motion ingestion and as *off* for the notification toggles. The
fixture contains exactly that value for both, and both stacks agree.
This is copied rather than fixed: the Python still serves the write path
for these settings, and a looser read on one side would disagree with it.

## A JSON body with a non-JSON Content-Type (slice 4)

```
POST /api/camera-groups
Content-Type: text/plain

{"name": "x"}
```

Python returns **500**. FastAPI reads the body for the declared Pydantic
model but raises when the media type is not JSON. Rust parses the bytes
and accepts it.

Reproduced by `latent_crashes.sh`. Diverging here rather than copying the
500 is deliberate: an unhandled exception is not a contract, and the
Python's *own* handling of a genuinely absent or malformed body is a
clean 422, which Rust matches exactly (`parse_body` in `src/query.rs`,
including the single-element `loc: ["body"]` that makes the summary read
"Field required" with no field name).

## Every custom Pydantic validator 500s instead of 422 (slice 4)

The most consequential of these, because it is reachable through normal
use rather than by malformed data.

```
PATCH /api/cameras/{id}/recording-settings
{"scheduled_start": "8:30"}          ->  python 500, rust 422
```

`CameraRecordingPolicy._validate_hhmm` raises `ValueError`. Pydantic v2
puts the **exception object** into `ctx["error"]`, and
`validation_exception_handler` in `main.py` passes the error list
straight to `JSONResponse(content=...)`. `json.dumps` cannot serialise a
`ValueError`, so the handler itself raises and the request becomes a 500:

```
File "app/main.py", line 457, in validation_exception_handler
TypeError: Object of type ValueError is not JSON serializable
```

This is not specific to `HH:MM`. It fires for **any** custom
`field_validator` in the codebase — currently two:

| validator | reached by |
| --- | --- |
| `CameraRecordingPolicy._validate_hhmm` | typing `8:30` instead of `08:30` in the schedule UI |
| `McpKeyCreate` `scope_tools` | creating a scoped MCP key |

The built-in constraints (`max_length`, `bool` coercion) are unaffected —
their `ctx` holds plain values — which is why `{"scheduled_start":
"123456"}` correctly returns 422 while `{"scheduled_start": "25:00"}`
returns 500.

Rust emits the 422 the validator was written to produce. The two cases
are listed in `write_diff.py`'s `EXPECTED_DIVERGENCES`, so the run fails
if they stop diverging — which is what will happen when the Python is
fixed.

**The fix is one line in `main.py`**: run the error list through
`fastapi.encoders.jsonable_encoder` before handing it to `JSONResponse`.

## ~~An out-of-range path integer 500s instead of 422~~ (closed)

Rust used to answer 422 here, on the reasoning that a value no
`incidents.id` could hold is not a valid id for the column and deserved
the same shape a non-numeric one gets. That was wrong as a *port*: the
Python accepts the value — `incident_id: int` is unbounded — and it is
Postgres that refuses it, because SQLAlchemy types the bind from the
column:

```
psycopg.errors.NumericValueOutOfRange: integer out of range
sqlalchemy.exc.DataError
```

Both stacks now 500. The same shape reaches `camera_groups.id`,
`mcp_api_keys.id` and the agent's `tool_call_count`, and all of them are
matched rather than diverged; `query::int4` is where each one narrows,
at the query and not at the parameter, so everything Python checks in
between still comes first. It stays recorded as a Python bug — see
`PYTHON_BUGS.md` #8 — and the read differential sends the values on
either side of the boundary.

## A 500 carries the security headers (slice 4 audit)

Python's 500 responses have **no** `X-Content-Type-Options`,
`X-Frame-Options`, `Referrer-Policy`, `Permissions-Policy` or
`X-Request-Id`. An unhandled exception is caught by Starlette's
outermost `ServerErrorMiddleware`, which sits above the middleware that
stamps them, so the error response skips the lot. Verified on two
unrelated 500s — the NULL-setting crash and a NUL byte in a path
parameter — both bare.

Rust keeps the headers on a 500. Diverging deliberately: an error page
without `nosniff` is worse than one with it, nothing can depend on their
absence, and copying the gap would mean writing code to strip them.

The HTTP differential compares status and body on a 500 but not headers,
for this reason.


## `POST /api/cameras/{camera_id}/codec` with a list `audio_codec`

| input | Python | Rust |
| --- | --- | --- |
| `{"video_codec": "avc1.64001f", "audio_codec": ["a", "b"]}` | 200, stores `{a,b}` | 500, stores nothing |

Deliberate. Python's length and newline checks inspect the list rather
than the text psycopg later derives from it, and the resulting array
literal lands in a column that is written into an HLS `CODECS`
attribute, where its comma splits one codec into two. See
`PYTHON_BUGS.md` #2. Every other non-string `audio_codec` already 500s
in Python (`len(5)` raises outside any `try`), so Rust extends that
behaviour to lists instead of reproducing a corrupting write.

Listed in `write_diff.py`'s `EXPECTED_DIVERGENCES`, so the run fails if
the two ever start agreeing — a divergence that silently disappears is
as untrustworthy as one that silently appears.

## JSON Python accepts and a `serde_json::Value` cannot hold

| body | Python | Rust |
| --- | --- | --- |
| `{"name": NaN}`, `Infinity`, `-Infinity` | decodes, then validates the float | 400 "There was an error parsing the body" |
| a lone surrogate escape, `"\ud800"` | decodes to a str with a lone surrogate | 400 |
| an exponent past f64, `1e400` | decodes to `inf` | 400 |

`src/pyjson.rs` reproduces CPython's decoder *errors* exactly — message
and code-point position, checked against a corpus generated by the
backend's own interpreter — because both appear in FastAPI's 422. What
it cannot do is represent the handful of values CPython accepts that
JSON does not. Rather than invent a Value for them, Rust refuses the
body. None is producible by `JSON.stringify` in the SPA or by
CameraNode's serde.

Nesting past Python's recursion limit raises `RecursionError`, which
FastAPI also reports as that 400. The exact depth depends on Python's
stack at the time; Rust approximates it at 900.
