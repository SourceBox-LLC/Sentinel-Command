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

## Where the differentials cannot reach: the Clerk path

Both HTTP harnesses run with `AUTH_PROVIDER=local`, because that is the
only way the two stacks can accept the *same* token. That is a real
strength — the token is minted by Python's own `issue_token()` and
verified by the Rust port of `local_auth.py` — but it means **no
differential exercises the Clerk verifier at all**. Production runs
Clerk.

`tests/clerk_verifier.rs` covers it against a synthetic keypair, and the
claim differential covers everything downstream of verification. What
neither covers is whether the *verification options* match the SDK's.
Those have to be read.

Reading `clerk_backend_api` 7.0.0 turned up one bug and confirmed three
guesses:

| behaviour | source | verdict |
| --- | --- | --- |
| a **missing `azp` is rejected** when `authorized_parties` is set | `verifytoken.py::_decode_token` | **was wrong** — fixed |
| `azp` compared by exact string membership, no normalisation | same | **was wrong** — fixed |
| clock skew allowance 5000 ms | `VerifyTokenOptions.clock_skew_in_ms` | matched (5 s) |
| `o.rol` carries no `org:` prefix | Clerk session-token docs | matched |
| `pla` / `fea` carry `u:` or `o:` scope prefixes | same | matched |

The `azp` one is worth dwelling on. Clerk's own documentation says the
claim "could be omitted if, for privacy-related reasons, `Origin` is
empty or null" — which reads like a reason to treat it as optional, and
is exactly why the port had a passing test asserting that a token
without `azp` is *accepted*. The SDK does the opposite:

```python
if options.authorized_parties is not None:
    azp = payload.get("azp")
    if azp is None or azp not in options.authorized_parties:
        raise TokenVerificationError(...)
```

Command Center always passes `authorized_parties`, so an absent `azp` is
a 401 in production. The port was the more permissive of the two tiers —
the one direction that is never acceptable.

The lesson generalises: for anything the differential cannot reach, the
dependency's **source** outranks its documentation, and a test written
from the documentation can encode the bug.

One deliberate difference remains. The SDK passes
`options={'verify_iss': False}` and does not check the issuer at all;
this port does, and also requires `exp` and `iss` to be present. Both are
stricter, both are unreachable in practice (the signature already ties a
token to one instance's JWKS), and stricter is the safe direction.

## Streaming protocols (WebSocket and SSE)

```bash
tests/differential/streaming_diff.py "$TOKEN"
```

Neither protocol fits a request/response differential, and the proxy was
broken for **both** in ways nothing else in this suite would have caught.
Found by asking what no harness reaches, not by a failing test.

### `/ws/node` — the entire node fleet

`Connection` and `Upgrade` are hop-by-hop headers, so the proxy stripped
them. That is correct for an ordinary request and catastrophic for a
handshake: Python saw a plain GET to a WebSocket-only route and answered
**404**. Every CameraNode would have failed to connect.

The tell was subtle — the 404 carried `server: uvicorn`, so the request
*had* reached Python; it just no longer looked like an upgrade.

The proxy now forwards those headers on an upgrade request, and on a 101
takes `hyper::upgrade::on` for both sides and copies bidirectionally.
Verified with a real authenticated handshake: 101 plus a full
client→server→client round trip returning an identical `ack`.

### SSE — the motion feed and the Home Assistant integration

The proxy called `.collect()` on the response body before returning it,
so an endpoint that never ends never responded. Measured before the fix:
Python emitted its first event immediately, Rust emitted **nothing** for
the full six-second timeout. The body is streamed through now.

### Teeth

Reverting each fix individually is caught:

| reverted fix | result |
| --- | --- |
| strip `Upgrade`/`Connection` again | no successful handshake, exit 2 |
| `.collect()` the response body again | no SSE event, exit 2 |

Both surface through the coverage guard rather than as a diff, because
when a stream fails on both stacks the comparison itself is meaningless —
two identical failures would otherwise read as a pass. The guard's
message names both plausible causes (proxy vs stale fixture) and says
which log line tells them apart.

## Request bodies: correctness and memory

```bash
tests/differential/upload_diff.py "$TOKEN"
```

`push-segment` is the highest-volume route in the service — up to 20/s
per node — and the one place a proxy's body handling has teeth.

`_read_capped_body` in `hls.py` rejects an oversized push on its
`Content-Length` before reading it, and its docstring says why: *"the
lever that makes a 10 GB attempted upload cost zero memory at the
server"*. The proxy called `to_bytes(body, usize::MAX)`, buffering the
whole upload before forwarding a byte — which defeats that lever
completely, because the bytes land in Rust's memory before Python ever
sees the header.

Measured on a single 400 MB upload:

| proxy | RSS |
| --- | --- |
| buffering (`to_bytes`) | 21 MB → **731 MB** |
| streaming (now) | 21 MB → 22 MB |

The machine has 985 MB and a 384 MB segment cache to fit beside it, so
two concurrent uploads were an OOM. One unauthenticated request could
have taken the service down — the 400 MB is buffered *before* the
handler ever checks the node key.

The script asserts three things: a real 300 KB segment survives the hop
byte-for-byte (pushed as a node, fetched back as a viewer, SHA-256
compared), an 8 MB push is refused 413 by both stacks, and a 400 MB
upload grows RSS by less than 64 MB. Reintroducing the buffer is caught:
the ceiling reports 763 MB and names the cause.

One probe detail worth keeping: the oversized push is sent with `curl`,
not `urllib`. The server answers 413 and closes while the client is
still sending, which `urllib` surfaces as `ConnectionResetError` rather
than as the response — and that reset is the *desired* behaviour, so the
probe has to be able to see past it.

## Rate-limit parity

```bash
tests/differential/ratelimit_parity.py
```

Porting a route removes its `@limiter.limit` decorator, because the
decorator is on the Python handler and Python never sees the request once
Rust owns the path. Slice 2 shipped five routes with their limits
silently dropped before that was noticed — by accident, through a wave of
429s that looked like port bugs.

This compares the two tables directly: every method+path Rust serves,
against the decorator on the same route in Python. Currently **26 pairs,
0 mismatches**. Verified by mutation: dropping a limit or changing its
value is named exactly.

It also refuses to run when its own view of `app.rs` is incomplete, which
is not hypothetical — the first version's regex excluded `:` before the
verb and so skipped every route written as `axum::routing::delete(...)`.
Four ported routes went unchecked and it still printed "0 mismatches".
The guard compares the *set* of served paths against `app.rs`; comparing
counts was itself wrong first, because `still_python()` appears in its
own function definition as well as at every call site.

The **window** is compared as well as the number, which is the point: a
30/hour route ported with a minute window is sixty times the intended
budget, and the count alone looks correct. The limiter takes a window
now (`PerMinute<N>` / `PerHour<N>`), and the 429 body renders
`"30 per 1 hour"` byte-identically to slowapi's.

This is not theoretical. `DELETE /api/integration/keys/{key_id}` was
ported in this commit, the checker failed the run because the route is
`30/hour`, and the window support exists because of that failure. The
checker refusing to pass is what made it a five-minute fix instead of a
production finding.

## Response headers

The HTTP differential compares a fixed set of response headers alongside
status and body — not just on the CORS cases, on every case. Two
defects were invisible without it:

### Ported routes carried no CORS headers

Python wraps every response in Starlette's `CORSMiddleware`; a ported
route leaves that wrapper behind. The preflight still succeeded, because
`OPTIONS` is not a method any ported route claims and so falls through to
the proxy — and then the browser blocked the *actual* response. Every
cross-origin caller would have broken: the Vite dev server on :5173, any
separately-hosted frontend, the `CORS_ALLOWED_ORIGINS` deployments.

`src/cors.rs` reproduces the three cases, all measured rather than
reasoned about, because reasoning got it wrong twice:

| request | headers |
| --- | --- |
| allowed origin | `allow-credentials`, `allow-origin`, `expose-headers`, `Vary: Origin` |
| **disallowed** origin | `allow-credentials` and `expose-headers` only — no `allow-origin`, and **no `Vary`** |
| no `Origin` header | nothing at all |

The middle row is the surprising one. A disallowed origin still gets
Starlette's "simple headers", and does *not* get `Vary: Origin` even
though the response genuinely varies by it. My first implementation
added `Vary` there on the reasoning that it was correct HTTP; the second
dropped the simple headers because a narrower `grep` had hidden them.

Preflight stays with Python deliberately — a second implementation of it
is a second thing to keep in sync.

### HEAD

axum answers `HEAD` from a `GET` handler automatically; FastAPI does not
and returns 405. So every ported route silently started accepting `HEAD`
where Python refused it, `/api/health` included. `ported()` routes `HEAD`
to the proxy.

### Teeth

| injected bug | caught |
| --- | --- |
| CORS layer removed | 7 / 211 |
| `allow-origin` echoed for any origin | 2 / 211 |
| `Vary: Origin` dropped | 5 / 211 |
| `HEAD` answered from the GET handler | 4 / 211 |

## Security headers and the request id

`main.py` stamps two more things onto every response through middleware,
and a ported route left both behind. The Rust tier was answering with
`content-type`, `content-length` and `date`, and nothing else.

```
x-request-id: 9b681f7596b74624
x-content-type-options: nosniff
x-frame-options: DENY
referrer-policy: strict-origin-when-cross-origin
permissions-policy: camera=(), microphone=(), geolocation=()
```

The security set is not decoration. `_apply_security_headers` carries a
comment about a previous regression where the SPA document shipped
without them, and "the one response where frame-ancestors actually
matters (the document) was the one being skipped, leaving the dashboard
clickjackable". Porting routes reintroduced the same shape of gap on the
API surface.

The request id is honoured from the client when it is 8–128 characters
of alphanumerics and hyphens, and replaced otherwise — the Python
comment gives the reason, which is that an unvalidated header injects
arbitrary text into log lines and Sentry tags. Note that Python's
`str.isalnum()` is Unicode-aware, so an ASCII-only check here would
reject ids the Python tier accepts; the port matches the looser rule
deliberately.

`COMPARED_HEADERS` in the differential covers all five, with a minted id
normalised to `<minted>` so only its shape is compared.

### One property the differential cannot see

"Do not restamp a proxied response" is real — restamping would hand the
client an id that appears in no log line — but no response comparison can
catch it. When the client supplies an id, both stacks echo the same one;
when it does not, Python's minted id and Rust's are both sixteen hex
characters and indistinguishable.

So it is tested directly instead: `headers::stamp` is split out of the
layer, and a unit test asserts that a response already carrying an id
keeps it. Verified by deleting the guard — the differential stays at
218/218 and the unit test fails.

### Teeth

| injected bug | caught by | cases |
| --- | --- | --- |
| headers middleware removed | differential | 197 / 218 |
| `X-Frame-Options` weakened to `SAMEORIGIN` | differential | 197 / 218 |
| inbound request id never honoured | differential | 2 / 218 |
| no length check on the inbound id | differential | 2 / 218 |
| proxied responses restamped | **unit test** | differential sees nothing |

## Generated cases beat hand-picked ones

The HEAD cases are generated from `app.rs`'s route table, not listed.
The five originally listed all happened to go through `ported()`, and
`ported()` was the only place the HEAD fix had been applied — so four
routes registered by hand kept answering HEAD with 200 where Python
returns 405, while the suite reported 218/218.

Anything that should hold for *every* served route should be generated
from the route table for the same reason. A hand-picked sample tests the
routes you remembered, which are the ones you already fixed.

## Adversarial input

A batch of odd-but-plausible requests is fired at every ported route:
Unicode and percent-encoded path parameters, `%00`, `%2F`, traversal
sequences, a 300-character id, doubled query parameters, SQL-looking
filter values, awkward `Authorization` header shapes, doubled path
separators, trailing slashes.

It found two things, both in the same run.

### `%2F` in a path parameter

Starlette percent-decodes the path **before** matching, so
`/api/cameras/cam%2Flive` becomes three segments and matches no route —
the router's 404, `{"detail": "Not Found"}`. axum matches on the raw
path, so the same request reached the handler with
`camera_id = "cam/live"` and produced the handler's own
`{"detail": "Camera not found"}`. Same status, different body; the same
divergence covered `%2e%2e%2f` traversal attempts.

`query::path_segment` rejects a decoded slash, restoring Starlette's
answer.

### Internal errors had the wrong shape entirely

There is **not one deliberate `HTTPException(status_code=500)`** in the
Python service — every 500 over there is an unhandled exception, which
Starlette renders as `text/plain; charset=utf-8` with the body
`Internal Server Error`. The port was answering `{"detail": "database
error"}` as JSON: a shape no Python 500 has, and a hint about what broke
into the bargain. `ApiError::internal` now renders exactly what
Starlette does, and the sqlx message goes to the log only.

### One divergence kept

Python's 500s carry **no** security headers and no request id —
Starlette's outermost `ServerErrorMiddleware` sits above the middleware
that stamps them, so an error response skips the lot. Verified on two
unrelated 500s. Rust keeps them; an error page without `nosniff` is
worse than one with it, and copying the gap would mean writing code to
strip them. The differential compares status and body on a 500, not
headers.

## Local admin login

`POST /api/auth/local/login` and `/refresh` are ported, and registered
**conditionally** — the Python mounts that router only in the `else`
branch of `is_clerk_auth()`, so under Clerk the paths do not exist.
Claiming them unconditionally would answer 503 where Python answers 404,
and would advertise a self-hosted login on a hosted deployment.

The strongest evidence here is not the differential. Both stacks share
one `APP_SECRET_KEY` and one argon2 hash, so:

* each stack's minted token is accepted by the **other** for API access
  *and* for refresh;
* the claims are identical apart from `iat`/`exp`.

### Two properties the differential cannot see

Mutation testing found both, by scoring 102/102 on changes that are
plainly wrong:

**Short-circuiting on a wrong username.** Both paths return the same 401
with the same body — only the clock differs. The Python comments the
reason: returning in microseconds for a bad username while a good one
costs ~100ms hands an attacker the valid username by response timing
before they ever guess at the password. Covered by a timing test that
asserts a wrong username still pays for the argon2 verify; with an early
return it fails at **9.5µs**.

**A malformed stored hash accepting.** The fixture has a valid hash, so
that branch never runs in the differential. Covered by a unit test.

Both are cases where "the responses are identical" is true and
irrelevant.

## The non-admin caller

Every differential ran as an **admin** until slice 4's notification work,
because `issue_token()` hardcodes `org_role: "org:admin"`. So every
`is_admin()` branch and every `require_admin` 403 went untested across
thirty routes.

Mutation testing surfaced it: deleting the notification audience filter
— which leaks admin-only inbox rows to ordinary members — scored
**296/296 identical**.

`http_run.sh` now mints a second token by hand, signed with the same
HS256 secret but carrying `org_role: "org:member"`. Both stacks read the
role straight from the claims, so both accept it and both treat the
caller as a non-admin. Cases marked `"member"` use it.

The difference is visible and real: on `/api/audit-logs` the admin gets
200 and the member 403; on the inbox the admin sees 116 rows including
29 admin-audience, the member sees 87 and none.

## Fixtures that must straddle a threshold

`unread-count` reports `capped: count > 99`. With one caller at 117
unread, `> 99` and `> 50` agree on every request, so a wrong threshold is
invisible — and was. The member's read-state is seeded to land their
count at **66**, between the two, which is what makes the branch
testable.

The general rule, learned three times on this branch: a fixture has to
put a value on *both* sides of every boundary the code tests, not merely
exercise the code path.

## Relative timestamps: the recurring fixture trap

Three times now a fixture seeded relative to `now()` has broken the write
differential, which reseeds twice seconds apart:

| column | symptom |
| --- | --- |
| `cameras.last_seen` | every write case differed |
| `notifications.created_at` | every write case differed |
| `user_notification_state.last_viewed_at` | 2/111 identical |

The read differential *needs* them relative — live-vs-offline cameras, an
unread count in a specific band. So `write_diff.py` has a `FREEZE` block
that pins each one after seeding, to distinct fixed values where
ordering matters. Any new `now()`-relative column in the fixture needs an
entry there.
