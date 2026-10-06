"""Resolve Clerk claims to an AuthUser using the REAL production code.

Reads one JSON claims object per line on stdin, writes one JSON result
per line on stdout.

The point of this probe is to avoid re-implementing the Python in order
to test the Rust against it — a re-implementation would just encode
whatever I think auth.py says. So: `decode_v2_permissions` and `AuthUser`
are imported from the live module, and the claim-extraction block (which
is inline inside an async function behind the Clerk SDK, and so cannot be
called directly) is sliced out of auth.py *by source text* and exec'd.
If someone edits those lines, this probe picks up the edit.
"""

import json
import sys
import textwrap
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
BACKEND = Path(__file__).resolve().parents[2].parent / "backend"
sys.path.insert(0, str(BACKEND))

from app.core.auth import AuthUser, decode_v2_permissions  # noqa: E402

SOURCE = (BACKEND / "app/core/auth.py").read_text()

# Slice the extraction block out of the real file, between these two
# anchors. Both are distinctive single occurrences; if either stops
# matching, fail loudly rather than silently testing nothing.
START = "        # Decode permissions: try V1 format first, then V2 format"
END = '        org_role = claims.get("org_role", "") or o_claim.get("rol", "")'

start_idx = SOURCE.index(START)
end_idx = SOURCE.index(END) + len(END)
BLOCK = textwrap.dedent(SOURCE[start_idx:end_idx])

# Sanity-check that we grabbed what we meant to, so a future edit that
# moves these lines turns into an error instead of a vacuous pass.
for needle in ("decode_v2_permissions", "pla", "fea", "org_id", "org_role"):
    assert needle in BLOCK, f"extraction block is missing {needle!r}"

CODE = compile(BLOCK, "auth.py-extraction-block", "exec")


def resolve(claims):
    ns = {"claims": claims, "decode_v2_permissions": decode_v2_permissions}
    exec(CODE, ns)  # noqa: S102 - the whole point: run production's own code

    user_id = ns["user_id"]
    org_id = ns["org_id"]

    # These two checks are the lines immediately after the block; they
    # decide the status code, so they are reproduced here rather than
    # sliced (they are trivial and unambiguous, unlike the extraction).
    if not user_id:
        return {"error": "NotAuthenticated"}
    if not org_id:
        return {"error": "NoOrganization"}

    user = AuthUser(
        user_id=user_id,
        org_id=org_id,
        org_role=ns["org_role"],
        org_permissions=ns["org_permissions"],
        email=ns["email"],
        username=ns["username"],
        plan=ns["active_plan"],
        features=ns["active_features"],
    )
    return {
        "user_id": user.user_id,
        "org_id": user.org_id,
        "org_role": user.org_role,
        "org_permissions": list(user.org_permissions),
        "email": user.email,
        "username": user.username,
        "plan": user.plan,
        "features": list(user.features),
        "is_admin": user.is_admin,
        "can_view_cameras": user.can_view_cameras,
    }


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        claims = json.loads(line)
        try:
            out = resolve(claims)
        except Exception as exc:
            # A raise here is not a crash of the test — it is what the
            # production service does on a malformed claim, and its
            # blanket handler turns it into a 401. Rust reports its own
            # Malformed rejection under the same name so the two can be
            # compared; the exception detail goes to stderr rather than
            # into the compared value, which would otherwise differ on
            # the message text alone.
            print(
                f"EXCEPTION {type(exc).__name__}: {exc} <- {json.dumps(claims)}",
                file=sys.stderr,
            )
            out = {"error": "EXCEPTION"}
        print(json.dumps(out, sort_keys=True))


if __name__ == "__main__":
    main()
