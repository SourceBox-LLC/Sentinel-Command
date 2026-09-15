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
result: **31/31 identical**.

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
