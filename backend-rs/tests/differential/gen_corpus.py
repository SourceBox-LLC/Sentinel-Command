"""Generate the claim corpus for the auth differential test.

Two halves:

* Hand-picked shapes — the real V1 and V2 layouts, plus every edge I
  could find while reading auth.py: empty strings, the `o:` prefix on its
  own, a plan claim that is bare or colon-only, an org_permissions that
  is present but empty (which Python's `or` chain falls through), and
  bitmaps with gaps.
* Randomised combinations — the point is to catch what I did *not* think
  to hand-pick, particularly in the bitmap reconstruction where an
  off-by-one in either index silently shifts permissions onto the wrong
  feature.
"""

import itertools
import json
import random

random.seed(20260914)  # reproducible: a flaky corpus is not evidence

HAND_PICKED = [
    # --- nothing at all ------------------------------------------------
    {},
    {"sub": ""},
    {"org_id": "org_1"},                      # no subject
    {"sub": "user_1"},                        # no org
    {"sub": "user_1", "org_id": ""},
    # --- V1 -------------------------------------------------------------
    {"sub": "u1", "org_id": "o1", "org_role": "org:admin"},
    {"sub": "u1", "org_id": "o1", "org_role": "org:member"},
    {"sub": "u1", "org_id": "o1", "org_role": "org:member",
     "org_permissions": ["org:cameras:manage_cameras"]},
    {"sub": "u1", "org_id": "o1", "org_role": "org:member",
     "org_permissions": ["org:cameras:read"]},
    {"sub": "u1", "org_id": "o1", "org_role": "", "org_permissions": []},
    # `permissions` as the alias, including the case Python's `or` chain
    # falls through to because org_permissions is present but empty.
    {"sub": "u1", "org_id": "o1", "permissions": ["org:cameras:manage_cameras"]},
    {"sub": "u1", "org_id": "o1", "org_permissions": [],
     "permissions": ["org:cameras:manage_cameras"]},
    {"sub": "u1", "org_id": "o1", "org_permissions": ["a"], "permissions": ["b"]},
    # --- V2 -------------------------------------------------------------
    {"sub": "u2", "o": {"id": "o2", "rol": "admin"}},
    {"sub": "u2", "o": {"id": "o2", "rol": "member"}, "pla": "o:pro",
     "fea": "o:cameras,o:admin"},
    {"sub": "u2", "o": {"id": "o2", "rol": "member", "per": "read,write", "fpm": "1,2"},
     "fea": "o:cameras,o:billing"},
    {"sub": "u2", "o": {"id": "o2", "rol": "member",
                        "per": "read,manage_cameras", "fpm": "3"},
     "fea": "o:cameras"},
    # bitmap with a gap: second feature has no fpm entry
    {"sub": "u2", "o": {"id": "o2", "per": "read,write", "fpm": "3"},
     "fea": "o:cameras,o:billing"},
    # more fpm entries than features
    {"sub": "u2", "o": {"id": "o2", "per": "read", "fpm": "1,1,1"},
     "fea": "o:cameras"},
    # zero bitmap grants nothing
    {"sub": "u2", "o": {"id": "o2", "per": "read,write", "fpm": "0,0"},
     "fea": "o:cameras,o:billing"},
    # --- claim-shape edges ---------------------------------------------
    {"sub": "u", "org_id": "o", "pla": ""},
    {"sub": "u", "org_id": "o", "pla": "pro"},         # no colon
    {"sub": "u", "org_id": "o", "pla": ":"},           # colon only
    {"sub": "u", "org_id": "o", "pla": "o:e:deep"},    # several colons
    {"sub": "u", "org_id": "o", "fea": ""},
    {"sub": "u", "org_id": "o", "fea": ","},
    {"sub": "u", "org_id": "o", "fea": "o:"},          # prefix with nothing after
    {"sub": "u", "org_id": "o", "fea": " o:cameras , admin "},   # whitespace
    {"sub": "u", "org_id": "o", "fea": "cameras,,admin"},        # empty middle
    # V1 org_id present but empty falls through to the V2 `o` claim
    {"sub": "u", "org_id": "", "o": {"id": "o2", "rol": "admin"}},
    {"sub": "u", "org_role": "", "org_id": "o", "o": {"rol": "admin"}},
    # `o` present but empty — falsy in Python
    {"sub": "u", "org_id": "o", "o": {}, "fea": "o:cameras"},
    # fpm values that are not integers
    {"sub": "u", "o": {"id": "o", "per": "read", "fpm": "nope"}, "fea": "o:c"},
    {"sub": "u", "o": {"id": "o", "per": "read", "fpm": "1,oops,4"},
     "fea": "o:a,o:b,o:c"},
    {"sub": "u", "o": {"id": "o", "per": "read", "fpm": ""}, "fea": "o:c"},
    {"sub": "u", "o": {"id": "o", "per": "", "fpm": "1"}, "fea": "o:c"},
    # a per list with an empty name — indices must not shift
    {"sub": "u", "o": {"id": "o", "per": "read,,write", "fpm": "7"}, "fea": "o:c"},
    # whitespace inside per: NOT trimmed by Python, so the key keeps it
    {"sub": "u", "o": {"id": "o", "per": " read , write ", "fpm": "3"}, "fea": "o:c"},
    # large and negative bitmaps
    {"sub": "u", "o": {"id": "o", "per": "a,b,c", "fpm": "-1"}, "fea": "o:c"},
    {"sub": "u", "o": {"id": "o", "per": "a,b,c", "fpm": "999999999"}, "fea": "o:c"},
    # email/username presence
    {"sub": "u", "org_id": "o", "email": "a@b.c", "username": "someone"},
]

PERM_POOL = ["read", "write", "manage_cameras", "delete", "invite"]
FEATURE_POOL = ["cameras", "billing", "admin", "audit"]


def random_v2():
    """A V2 claim set with a randomly shaped bitmap."""
    perms = random.sample(PERM_POOL, random.randint(1, len(PERM_POOL)))
    features = random.sample(FEATURE_POOL, random.randint(1, len(FEATURE_POOL)))
    # Sometimes emit fewer fpm values than features, sometimes more —
    # both are index-alignment traps.
    n_fpm = max(0, len(features) + random.choice([-1, 0, 0, 1]))
    fpm = [random.randint(0, (1 << len(perms)) - 1) for _ in range(n_fpm)]

    o = {"id": random.choice(["", "org_x", "org_y"]),
         "rol": random.choice(["", "admin", "org:admin", "member", "org:member"]),
         "per": ",".join(perms)}
    if fpm:
        o["fpm"] = ",".join(str(v) for v in fpm)

    claims = {"sub": random.choice(["", "user_a", "user_b"]), "o": o,
              "fea": ",".join(random.choice(["o:", ""]) + f for f in features)}
    if random.random() < 0.3:
        claims["pla"] = random.choice(["", "o:pro", "pro", "o:free_org"])
    if random.random() < 0.2:
        claims["org_id"] = random.choice(["", "org_v1"])
    if random.random() < 0.2:
        claims["org_role"] = random.choice(["", "org:admin"])
    if random.random() < 0.15:
        claims["org_permissions"] = random.choice(
            [[], ["org:cameras:manage_cameras"], ["org:cameras:read"]]
        )
    return claims


def exhaustive_bitmaps():
    """Every bitmap over 3 permissions x 2 features.

    Small enough to enumerate completely, and this is exactly where an
    index error hides: 64 cases pin the reconstruction down entirely.
    """
    for a, b in itertools.product(range(8), repeat=2):
        yield {
            "sub": "u",
            "o": {"id": "o", "rol": "member", "per": "read,write,manage_cameras",
                  "fpm": f"{a},{b}"},
            "fea": "o:cameras,o:billing",
        }


def main():
    out = list(HAND_PICKED)
    out.extend(exhaustive_bitmaps())
    out.extend(random_v2() for _ in range(2000))
    for claims in out:
        print(json.dumps(claims, sort_keys=True))


if __name__ == "__main__":
    main()
