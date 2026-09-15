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
