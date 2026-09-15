# Differential tests

Each slice of the Python → Rust migration is verified by running both
implementations over the same inputs and comparing results, rather than
by trusting that the port reads correctly. This directory holds the
harness.

## Why not just port the tests?

Because a hand-written Rust test encodes what I *think* `auth.py` does.
If I misread it, the port and the test are wrong in the same direction
and both pass. The Python probe here imports the real functions and
`exec`s the claim-extraction block sliced out of `auth.py` by source
text, so the comparison is against the code that actually runs in
production.

## Claim extraction (slice 1)

```bash
tests/differential/run.sh        # add -v to see sample divergences
```

Resolves ~2,130 Clerk claim sets through both stacks and compares the
resulting `AuthUser` field by field, including `is_admin`. Current
result: **2,124 identical, 10 differing, all 10 expected** — see
`expected_divergences.md`.

The corpus is four parts:

* hand-picked shapes — real V1 and V2 layouts plus every edge found
  while reading `auth.py` (empty `pla`, a bare `o:` feature, an
  `org_permissions` that is present but empty, bitmaps with gaps);
* **every** bitmap over 3 permissions × 2 features, enumerated — 64
  cases that pin the reconstruction down completely;
* claims of the wrong JSON type, which the first version of this corpus
  missed entirely. That omission mattered: without them the harness
  reported a clean 2,105/2,105 while Rust was in fact *more permissive*
  than Python on four claim shapes, resolving a user where Python
  returned 401. `validate_claim_types` closed that, and these cases keep
  it closed;
* 2,000 randomised V2 claim sets, seeded for reproducibility.

`run.sh` fails if the corpus stops resolving enough users to be
meaningful, so it cannot quietly go vacuous. It also fails if a
divergence appears that is not in `expected_divergences.jsonl`, **or** if
one listed there stops diverging — a stale allowlist hides real
coverage just as effectively as a missing test.

### Does it have teeth?

Verified by mutation — each of these was introduced into the Rust
deliberately and the harness caught it:

| injected bug | cases caught |
| --- | --- |
| off-by-one in the permission bit index | 788 |
| org checked before subject (swaps 400 and 401) | 189 |
| `is_admin` drops the bare `admin` role (V2 spelling) | 149 |
| plan takes the first `:` segment instead of the last | 140 |
| permission names trimmed (Python does not) | 1 |
| key-presence instead of Python's truthiness chain | 1 |
| `validate_claim_types` removed entirely | 13 |

The last three are caught by one or a few hand-picked cases, which is
why those cases exist — the randomised corpus does not generate them.
The allowlist was checked in the other direction too: deleting the
`validate_claim_types` call makes ten listed divergences disappear, and
the run fails on the stale entries rather than passing quietly.

## Signature verification (slice 1)

Not differential: `tests/clerk_verifier.rs` runs the verifier against a
real JWKS server on localhost. Comparing to Python there would mean
standing up the Clerk SDK against a fake instance, and the properties
that matter (`alg: none`, algorithm confusion, tampered payloads, wrong
issuer, wrong `azp`, expiry) are absolute rather than relative — they
should be rejected whatever Python does.

Those were mutation-checked too. Removing issuer validation, the `azp`
check, or the clock leeway each breaks a test. Removing the explicit
`alg` pin in `verify()` breaks **nothing** — `Validation::new(RS256)`
already restricts algorithms, so that check is legibility, not the
barrier. The validation set is the line to leave alone.

## The `settings` table (slice 1)

`require_active_billing` reads `Setting.get(db, org_id, "payment_past_due")`.
The seven cases that matter — present/absent, `"true"`, `"false"`,
`"TRUE"`, a NULL value, and a duplicated `(org_id, key)` pair — were run
through both stacks against one Postgres and agreed on all seven. Two
are easy to get wrong in a port and are now pinned by
`tests/settings_db.rs`:

* a row whose `value` is NULL returns NULL, **not** the default — once a
  row exists, Python stops falling back;
* the past-due comparison is exact, so `"TRUE"` is *not* past due.
  Being generous there would lock paying customers out of their cameras.

That test is gated on `TEST_DATABASE_URL` so `cargo test` still passes
with no database. Verified in both directions: with the variable set a
deliberately wrong expectation fails, and without it the cases skip.

## Ported routes (slice 2 onward)

```bash
tests/differential/http_run.sh        # add -v to list every case
```

Sends identical requests to Rust (`:8000`) and Python (`:8001`), both
pointed at one Postgres, and compares status and JSON body. Current
result: **77/77 identical**, covering the camera reads, the settings
reads, and `/api/audit-logs` — the last with 36 query-string variants
including every FastAPI 422 shape.

Both stacks run with `AUTH_PROVIDER=local` so they share one HS256
secret and accept the same token — which makes the token itself a test,
since it is minted by Python's own `issue_token()` and verified by the
Rust port of `local_auth.py`.

Bodies are compared structurally, not textually: neither list route has
an `ORDER BY` (both emit SQLAlchemy's unordered `filter_by(...).all()`),
so lists are compared as multisets. Comparing by position would produce
flakes, not findings.

### Seeding is part of the run, deliberately

`effective_status` turns a camera offline 90 seconds after its last
heartbeat, so a fixture seeded once and reused later exercises only the
offline branch. That is not hypothetical: the first run of this harness
reported **31/31 identical while testing neither a live camera nor the
`last_error` surfacing**, because the fixture had aged out during an
unrelated debugging detour. `http_run.sh` therefore re-seeds before every
run, and `http_diff.py` refuses to report at all unless the fixture still
covers live cameras, offline cameras, surfaced errors, and both
timestamp shapes.

### Does it have teeth?

| injected bug | cases caught |
| --- | --- |
| `last_error` surfaced regardless of status | 4 / 31 |
| heartbeat window 900s instead of 90s | coverage guard fires |
| chrono `%.f` instead of Python's `isoformat()` | 2 / 31 |
| org filter dropped from the list query (tenant leak) | 1 / 31 |

(Counts are from the 31-case run at slice 2's first commit; the suite has
since grown to 77.)

The coverage guard was checked too: ageing the fixture past 90 seconds
makes the run exit 2 with "COVERAGE TOO THIN" rather than a false green.

### What it found

`GET /api/cameras/../nodes` returned **200 from Rust and 404 from
Python**. The cause was not the router: `reqwest` builds every request
through a `Url`, which applies RFC 3986 dot-segment removal, so the proxy
was silently rewriting `/api/cameras/../nodes` to `/api/nodes` (and
`/a/./b` to `/a/b`) before forwarding. Python normalises nothing, so the
two stacks answered different routes for the same request.

No privilege escalation — every endpoint involved requires the same auth
— but a proxy that rewrites paths is not a transparent proxy, and
transparency is the entire contract during a strangler migration. The
proxy now uses hyper directly and passes the URI through byte for byte;
`proxy.rs` has a regression test for it.

## Query-parameter validation

`src/query.rs` reproduces FastAPI's 422 responses — same status, same
`detail` envelope, same `errors` list, same ordering. Every rule in it
was measured against the running service rather than read out of the
Pydantic docs, which turned up several things worth knowing:

* a repeated parameter takes the **last** value, not the first;
* values are whitespace-stripped, so `?limit=%205%20` is 5;
* `"5.0"` parses as 5, but `"5.5"`, `"1e3"` and `"0x10"` do not;
* `"1_000"` parses as 1000 — Python's underscore digit separators reach
  query parsing;
* errors are reported in the order parameters are *declared* in the
  handler signature, not the order they appear in the query string, and
  `message` summarises only the first.

The differential covers all of these plus every failure mode
(`int_parsing`, `greater_than_equal`, `less_than_equal`,
`string_pattern_mismatch`) and both single- and multi-error responses.

## Latent crashes found in the Python

```bash
tests/differential/latent_crashes.sh
```

Two routes 500 on data their own columns permit. See
`expected_divergences.md`; neither is reachable in production today.

## Rate limiting

Porting a route to Rust **removes its rate limit** unless the limit is
ported too: the `@limiter.limit` decorators live on the Python handlers,
and once Rust owns a path Python never sees those requests. The slice-2
differential surfaced this as a wave of 429s that looked like port bugs —
five ported routes had silently lost their limits.

`src/ratelimit.rs` reproduces `app/core/limiter.py`: the same bucket key
(node-key hash → org from the *unverified* JWT → `Fly-Client-IP` →
`X-Forwarded-For` → peer address), the same fixed-window strategy, and
the same flat 429 body with `Retry-After: 60` — flat, note, not the
`{"detail": ...}` envelope the other errors use.

Verified against the running Python: both stacks first return 429 on
**request #61** of a 60/minute route, and the bodies are identical once
key order is normalised.

Because Rust owns a ported route exclusively, its counter does not need
to be shared with Python's — nothing else counts those requests. It does
need to be shared between Rust *instances*, which is what `REDIS_URL` is
for; without it the counters are per-process and a caller round-robining
across machines gets N× the limit. That is the same caveat the Python
module documents about itself.

### Running the harness with limits on

`http_run.sh` flushes the shared counters before each run, so repeated
runs are deterministic:

```bash
docker run -d --name cc-redis-test -p 16379:6379 redis:7-alpine
# then start both tiers with REDIS_URL=redis://127.0.0.1:16379/0
```

If a case does hit a limit anyway, `http_diff.py` reports the run
**INCONCLUSIVE** (exit 3) rather than counting 429s as diffs — a false
green and a false red are both worse than an honest "re-run me".

## Two fixture guards

Both exist because the thing they catch already happened once and read
like a port bug.

* **Stale fixture** — `effective_status` flips a camera offline after 90
  seconds, so an aged fixture tests only the offline path. The run aborts
  unless live cameras, offline cameras, surfaced errors and both
  timestamp shapes are all present.
* **Tied sort keys** — several routes page with `ORDER BY <timestamp>
  DESC` and no tiebreaker, so two rows sharing a sort key let Postgres
  return a different page of 150 on each run. The run aborts (exit 2) if
  the seeded data contains any tie.

Both were verified by breaking them deliberately: a stale fixture exits
2 with "COVERAGE TOO THIN", and a seed with tied timestamps exits 2 with
"FIXTURE DEFECT: 145 tied sort key(s)".

## Route capture

```bash
tests/differential/route_capture.py
```

A route registered as `/api/nodes/{node_id}` also matches
`/api/nodes/plan`. FastAPI is saved from this by declaration order —
`/plan` sits above `/{node_id}` in the same router — but axum has no
ordering between separately registered paths, so porting a
parameterised route silently takes over every literal path beside it and
answers 404.

It happened on the first run of slice 3: `GET /api/nodes/{node_id}`
captured `/plan`, `/ws-status`, `/validate`, `/register` and
`/heartbeat`. `app.rs` now pins each to the proxy with `still_python()`,
and this script proves none has been missed. Verified by deleting a pin:
it exits 1 naming the path.

The first version of the check walked `app.routes` and reported a clean
"(none)". This FastAPI version nests routes under `_IncludedRouter`
wrappers whose `path` is `None`, so the filter dropped every real route
and produced a false all-clear. It reads the OpenAPI schema now — a
check that cannot fail is worse than no check.

## What cannot be ported, and why

See `in_process_state.md`. Several modules keep state in the Python
process's memory with no shared store behind it, so the routes reading it
cannot move one at a time. `hls.py` — the plan's headline slice-3 target
— is the most thoroughly blocked, because `mcp/server.py` imports its
segment cache directly and MCP stays Python by plan.

## Writes: side effects, not just responses (slice 4)

```bash
tests/differential/write_diff.py "$TOKEN"
```

A write handler can return exactly the right JSON and still write the
wrong row, skip an audit entry, or leave `updated_at` untouched.
Response diffing cannot see any of that. So each case runs twice against
a freshly reseeded database — once per stack — and compares the response
**and** the resulting table contents:

```
reseed -> request to python -> snapshot
reseed -> request to rust   -> snapshot
```

Current result: **25/25 identical on response and side effects.**

Two things legitimately differ between the runs and are normalised:
timestamps written as "now" (anything within 10 minutes of the request
becomes `<recent>`, so an *older* `created_at` is still compared exactly
and "handler wrongly reset created_at" is still caught), and nothing
else. Row ids are compared as-is, which is why `seed_cameras.sql`
restarts every sequence — without that, ids climb on each reseed and
every case reads as a side-effect diff. That is what happened on the
first run.

`WATCHED` deliberately includes tables a case is not expected to touch.
A handler that writes a stray audit row, or fails to write an expected
one, is precisely the bug this exists to find.

### What it found

`PATCH /api/incidents/{id}` with a body that changes nothing. SQLAlchemy
emits **no UPDATE at all** when no attribute actually changed, so
`updated_at` — an `onupdate` column — keeps its old value. An
unconditional `UPDATE` in Rust bumped it on every no-op patch, and the
dashboard sorts and badges on that field. The port now compares the
computed values against the current row and skips the write when they
match, which also covers patching a field to the value it already holds.

Invisible to response diffing: the response body was identical, because
it is re-read from the row after the write.

### Teeth

| injected bug | caught |
| --- | --- |
| always UPDATE (bumps `updated_at` on a no-op) | 5 / 25 |
| re-stamp `resolved_at` when re-resolving | 2 / 25 |
| `NULL` report serialised as `null` rather than `""` | 9 / 25 |
| reopening does not clear the resolution | 1 / 25 |
| ownership check dropped (cross-tenant read + delete) | 2 / 25 |

One mutation — dropping the org filter from the `DELETE` statement —
was **not** caught, correctly: `owned_incident()` already 404s first, so
that filter is defence-in-depth rather than the barrier. Removing the
ownership check itself is the mutation that matters, and it is caught.

## Path parameters

`Path<i32>` hands axum's own rejection to the caller —
`400 "Invalid URL: Cannot parse `abc` to a `i32`"` — where FastAPI
returns its 422 envelope with `loc: ["path", "<name>"]`. The SPA parses
that envelope. Handlers therefore take `Path<String>` and call
`query::path_int`, and the differential covers `abc`, `1.5` and `-1`.

## Body parsing

`Json<Value>` will not do: it requires `Content-Type: application/json`
and answers **415** otherwise, where FastAPI reads the bytes regardless
and returns its own 422. Handlers take `Bytes` and call
`query::parse_body`, which reproduces:

* an absent or empty body — `missing` at `loc: ["body"]`. The
  single-element location is what makes the summary read "Field required"
  with no field name;
* malformed JSON — `json_invalid` with a character offset;
* field-level errors through `BodyErrors`: `missing`, `string_type`,
  `string_too_long` (counted in **characters**, so an emoji icon is one),
  and `bool_parsing` with Pydantic v2's lax coercion.

Field lengths are enforced in the handler rather than left to the column
widths, because a varchar overflow is a 500 from Postgres where Pydantic
returns a 422 naming the field. The differential covers a 101-character
name and a 21-character colour.
