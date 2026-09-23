"""HTTP differential: the Rust tier vs the Python it replaces.

Sends identical requests to both stacks — which are pointed at one
Postgres — and compares status code and JSON body.

Bodies are compared structurally, not as text: Python's json separators
differ from serde_json's, and neither list route has an ORDER BY (both
emit SQLAlchemy's unordered `filter_by(...).all()`), so list responses
are compared as multisets keyed on a stable field rather than by
position. Anything order-dependent would be a flake, not a finding.

Usage:
    http_diff.py <token> [-v]
"""

import json
import os
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
import urllib.error
import urllib.request

from diffutil import value_diff

RUST = "http://127.0.0.1:8000"
PYTHON = "http://127.0.0.1:8001"

TOKEN = sys.argv[1]
# A non-admin caller. Without one, every is_admin() branch and every
# require_admin 403 goes untested, because issue_token() always mints
# org:admin.
MEMBER_TOKEN = sys.argv[2] if len(sys.argv) > 2 and not sys.argv[2].startswith("-") else None

# The raw agent key behind seed row 1 (self-host, not revoked). The
# hash is in seed_cameras.sql.
AGENT_KEY = "osa_00000000000000000000000000000001"
VERBOSE = "-v" in sys.argv

# (method, path, expect_auth) — expect_auth False sends no credential.

# DIFF_ONLY=<substring> restricts the run to matching cases. For mutation
# runs, where a full sweep per injected bug costs minutes and only the
# slice's own cases can move. A filtered run says so in its result line,
# so a filtered green is never mistaken for a full one.
DIFF_ONLY = os.environ.get("DIFF_ONLY", "")

CASES = [
    ("GET", "/api/cameras", True),
    ("GET", "/api/cameras", False),
    ("GET", "/api/camera-groups", True),
    ("GET", "/api/camera-groups", False),
    # every seeded camera, by id
    *[("GET", f"/api/cameras/{c}", True) for c in [
        "cam-live", "cam-stale", "cam-boundary", "cam-failed", "cam-failedold",
        "cam-restart", "cam-error", "cam-offline", "cam-neverseen", "cam-orphan",
        "cam-nocaps", "cam-emptycaps", "cam-spacecaps", "cam-nullstatus",
        "cam-capped", "cam-ts-whole", "cam-ts-tenth", "cam-ts-micro",
    ]],
    # cross-tenant: belongs to another org, must 404 not 200
    ("GET", "/api/cameras/cam-theirs", True),
    # nonexistent
    ("GET", "/api/cameras/does-not-exist", True),
    # url-encoded and awkward ids
    ("GET", "/api/cameras/../nodes", True),
    ("GET", "/api/cameras/cam%20space", True),
    ("GET", "/api/cameras/", True),
    # --- settings ----------------------------------------------------
    ("GET", "/api/settings", True),
    ("GET", "/api/settings", False),
    ("GET", "/api/settings/notifications", True),
    ("GET", "/api/settings/motion-ingestion", True),

    # --- audit logs: pagination, filters and every 422 shape ----------
    ("GET", "/api/audit-logs", True),
    ("GET", "/api/audit-logs", False),
    *[("GET", f"/api/audit-logs?{q}", True) for q in [
        "limit=1", "limit=5", "limit=500", "limit=3&offset=10",
        "offset=0", "offset=1000000", "offset=999999",
        "event=camera_created", "event=node_registered", "event=nope", "event=",
        # the LIKE-escaping cases: an underscore and a percent that the
        # caller typed literally and must not be treated as wildcards
        "username=clerk_user_alpha", "username=beta%25user", "username=_",
        "username=%25", "username=CLERK_USER_ALPHA", "username=nobody",
        "event=camera_created&username=clerk_user_alpha&limit=2",
        # parsing edges measured against the running service
        "limit=1_000", "limit=5.0", "limit=%205%20", "limit=05", "limit=1&limit=2",
        # every validation failure
        "limit=0", "limit=501", "limit=abc", "limit=", "limit=5.5", "limit=1e3",
        "limit=0x10", "limit=true", "offset=-1", "offset=1000001",
        "format=xml", "format=json", "offset=-1&limit=0", "limit=0&offset=-1",
        "unknown=x",
    ]],
    # csv still belongs to Python; validation must still run in Rust first
    ("GET", "/api/audit-logs?format=csv&limit=2", True),
    ("GET", "/api/audit-logs?format=csv&limit=0", True),

    # --- stream access logs (plan-gated on the "admin" feature) -------
    ("GET", "/api/audit/stream-logs", True),
    ("GET", "/api/audit/stream-logs", False),
    *[("GET", f"/api/audit/stream-logs?{q}", True) for q in [
        "limit=5", "limit=3&offset=7", "offset=140",
        "camera_id=cam-1", "camera_id=cam-5", "camera_id=nope",
        # NOT escaped on this route, unlike its two siblings: an
        # underscore here really is a LIKE wildcard, and the port has to
        # reproduce that rather than tidy it up.
        "user_id=user_1", "user_id=user%5F1", "user_id=_", "user_id=%25",
        "user_id=USER_1", "user_id=example.com", "user_id=nobody",
        "camera_id=cam-2&user_id=user_2&limit=4",
        "limit=0", "limit=501", "offset=-1", "format=xml",
    ]],
    ("GET", "/api/audit/stream-logs?format=csv&limit=2", True),
    ("GET", "/api/audit/stream-logs/stats", True),
    ("GET", "/api/audit/stream-logs/stats", False),
    *[("GET", f"/api/audit/stream-logs/stats?{q}", True) for q in [
        "days=1", "days=7", "days=30", "days=0",
        # no `ge` on this one in the Python, so a negative window is
        # accepted and simply returns zeroes
        "days=-5", "days=31", "days=abc",
    ]],

    # --- motion ------------------------------------------------------
    ("GET", "/api/motion/events", True),
    ("GET", "/api/motion/events", False),
    *[("GET", f"/api/motion/events?{q}", True) for q in [
        "hours=1", "hours=24", "hours=168", "limit=5", "limit=3&offset=4",
        "camera_id=cam-1", "camera_id=cam-4", "camera_id=nope",
        "hours=48&camera_id=cam-2&limit=6",
        "hours=0", "hours=169", "limit=0", "offset=-1", "hours=abc",
    ]],
    ("GET", "/api/motion/events/stats", True),
    *[("GET", f"/api/motion/events/stats?{q}", True) for q in [
        "hours=1", "hours=24", "hours=168", "hours=-5", "hours=169", "hours=abc",
    ]],

    # --- mcp activity: the DB-backed routes only ----------------------
    ("GET", "/api/mcp/activity/logs", True),
    ("GET", "/api/mcp/activity/logs", False),
    *[("GET", f"/api/mcp/activity/logs?{q}", True) for q in [
        "limit=5", "limit=4&offset=3",
        "tool_name=tool_1", "tool_name=tool_3", "tool_name=nope",
        "status=ok", "status=error", "status=nope",
        # escaped on this route: the underscore and percent are literal
        "key_name=key_alpha_one", "key_name=key%25beta", "key_name=_", "key_name=%25",
        "tool_name=tool_2&status=error&limit=3",
        "limit=0", "limit=501", "offset=-1", "format=xml",
    ]],
    ("GET", "/api/mcp/activity/logs?format=csv&limit=2", True),
    ("GET", "/api/mcp/activity/logs/stats", True),
    *[("GET", f"/api/mcp/activity/logs/stats?{q}", True) for q in [
        "days=1", "days=7", "days=30", "days=-5", "days=31", "days=abc",
    ]],
    # these three read an in-memory tracker in the Python process and
    # must still be proxied, not answered from an empty Rust one
    ("GET", "/api/mcp/activity/recent", True),
    ("GET", "/api/mcp/activity/sessions", True),
    ("GET", "/api/mcp/activity/stats", True),

    # --- nodes: only the single-node read is ported -------------------
    ("GET", "/api/nodes/node-aaaa1111", True),
    ("GET", "/api/nodes/node-bbbb2222", True),
    ("GET", "/api/nodes/node-aaaa1111", False),
    # another tenant's node must 404, not leak
    ("GET", "/api/nodes/node-cccc3333", True),
    ("GET", "/api/nodes/does-not-exist", True),
    # these stay with Python; they read in-process state
    ("GET", "/api/nodes", True),
    ("GET", "/api/nodes/plan", True),
    ("GET", "/api/nodes/ws-status", True),

    # --- incidents (reads) --------------------------------------------
    ("GET", "/api/incidents", True),
    ("GET", "/api/incidents", False),
    ("GET", "/api/incidents/counts", True),
    ("GET", "/api/incidents/1", True),
    ("GET", "/api/incidents/2", True),
    ("GET", "/api/incidents/3", True),
    # another tenant's, and a missing one
    ("GET", "/api/incidents/4", True),
    ("GET", "/api/incidents/9999", True),
    # path param that is not an integer — FastAPI 422, not axum's 400
    ("GET", "/api/incidents/abc", True),
    ("GET", "/api/incidents/1.5", True),
    ("GET", "/api/incidents/-1", True),
    *[("GET", f"/api/incidents?{q}", True) for q in [
        "limit=1", "limit=200", "limit=2&offset=1",
        "status=open", "status=resolved", "status=acknowledged", "status=dismissed",
        "severity=low", "severity=high", "severity=critical",
        "camera_id=cam-live", "camera_id=nope",
        "status=open&severity=high",
        # handler-validated enums: 400 with a plain message, not the 422
        "status=nonsense", "severity=nonsense",
        # Pydantic-validated bounds: the 422 envelope
        "limit=0", "limit=201", "offset=-1", "limit=abc",
    ]],

    # --- adversarial but plausible input ------------------------------
    # Unicode, encoding and separator handling on a path parameter.
    ("GET", "/api/cameras/caf%C3%A9", True),
    ("GET", "/api/cameras/%F0%9F%8E%A5", True),
    ("GET", "/api/cameras/cam%2Flive", True),
    ("GET", "/api/cameras/cam%00live", True),
    ("GET", "/api/cameras/%2e%2e%2f%2e%2e", True),
    ("GET", "/api/cameras/" + "x" * 300, True),
    ("GET", "/api/incidents/%31", True),
    ("GET", "/api/incidents/+1", True),
    ("GET", "/api/incidents/1%20", True),
    # Trailing slashes and doubled separators.
    ("GET", "/api/settings/", True),
    ("GET", "/api//cameras", True),
    ("GET", "/api/incidents//counts", True),
    # Query-string oddities on routes that parse one.
    ("GET", "/api/audit-logs?limit=5&limit=", True),
    ("GET", "/api/audit-logs?LIMIT=5", True),
    ("GET", "/api/audit-logs?limit[]=5", True),
    ("GET", "/api/audit-logs?username=%00", True),
    ("GET", "/api/audit-logs?username=%E2%80%8B", True),
    ("GET", "/api/motion/events?hours=24&hours=1", True),
    ("GET", "/api/incidents?status=open&status=resolved", True),
    # A filter value that looks like SQL.
    ("GET", "/api/audit-logs?event=%27%20OR%20%271%27%3D%271", True),
    ("GET", "/api/incidents?camera_id=%27%3B%20DROP%20TABLE%20incidents%3B--", True),
    # Header oddities.
    ("GET", "/api/cameras", True, {"Accept": "text/html"}),
    ("GET", "/api/cameras", True, {"Accept-Encoding": "br"}),
    ("GET", "/api/cameras", True, {"X-Forwarded-For": "1.2.3.4, 5.6.7.8"}),
    # Authorization header shapes.
    ("GET", "/api/cameras", False, {"Authorization": "Bearer"}),
    ("GET", "/api/cameras", False, {"Authorization": "bearer lowercase.token.here"}),
    ("GET", "/api/cameras", False, {"Authorization": "Bearer  double.space.token"}),
    ("GET", "/api/cameras", False, {"Authorization": "Basic dXNlcjpwYXNz"}),

    # --- the sentinel agent data plane, read side ----------------------
    #
    # Authenticated on X-Sentinel-Agent-Key rather than a bearer token,
    # so these pass a 4th element: extra headers. The shared
    # SENTINEL_AGENT_KEY is unset in both tiers on purpose — an unset
    # shared key must not disable the per-org scoped path, which is the
    # only one a self-hosted install has.
    *[("GET", p, False, {"X-Sentinel-Agent-Key": AGENT_KEY}) for p in [
        "/api/sentinel/runs/pending",
        "/api/sentinel/runs/pending?limit=1",
        "/api/sentinel/runs/pending?limit=100",
        # bounds and parsing, all 422 shapes
        "/api/sentinel/runs/pending?limit=0",
        "/api/sentinel/runs/pending?limit=101",
        "/api/sentinel/runs/pending?limit=abc",
        "/api/sentinel/runs/pending?limit=",
        "/api/sentinel/runs/pending?limit=1.5",
        "/api/sentinel/runs/pending?limit=2&limit=3",
    ]],
    # Every way of failing agent auth, each of which must be the same
    # 401 with the same body — telling a caller which of the two key
    # types they got wrong is a hint they should not have.
    ("GET", "/api/sentinel/runs/pending", False),
    ("GET", "/api/sentinel/runs/pending", True),
    ("GET", "/api/sentinel/runs/pending", False,
     {"X-Sentinel-Agent-Key": "osa_00000000000000000000000000000002"}),
    ("GET", "/api/sentinel/runs/pending", False,
     {"X-Sentinel-Agent-Key": "osa_ffffffffffffffffffffffffffffffff"}),
    ("GET", "/api/sentinel/runs/pending", False, {"X-Sentinel-Agent-Key": ""}),
    # A header byte above 0x7F: latin-1 decodable, and the reason the
    # Python hashes bytes rather than the decoded str.
    ("GET", "/api/sentinel/runs/pending", False,
     {"X-Sentinel-Agent-Key": "osa_\u00ff\u00fe"}),
    # Another tenant's scoped key: valid auth, empty queue.
    ("GET", "/api/sentinel/runs/pending", False,
     {"X-Sentinel-Agent-Key": "osa_00000000000000000000000000000003"}),

    # A single run, read by the operator rather than the agent — session
    # auth, org-scoped, and the only route that returns tool_trace.
    *[("GET", f"/api/sentinel/runs/{r}", True) for r in [
        "run0000000000000000000000000001",  # pending
        "run0000000000000000000000000004",  # running
        "run0000000000000000000000000005",  # error
        "run0000000000000000000000000006",  # incident, with a real trace
        "run0000000000000000000000000007",  # trace column is not JSON
        "run0000000000000000000000000008",  # trace parses but is not a list
        "run0000000000000000000000000003",  # another tenant's
        "nosuchrun",
        "",
    ]],
    ("GET", "/api/sentinel/runs/run0000000000000000000000000001", False),
    ("GET", "/api/sentinel/runs/run0000000000000000000000000001", "member"),
    # Not ported, and must stay on the proxy: the list reads the plan
    # cache and the licence client.
    ("GET", "/api/sentinel/runs", True),
    # A static sibling under the ported `{run_id}` route. POST reaches
    # Python through the method fallback; GET matches `{run_id}` on both
    # stacks and 404s. route_capture.py asserts the first statically —
    # this asserts it end to end.
    ("GET", "/api/sentinel/runs/manual", True),

    # GET /config is a read that writes: it creates the org's row on
    # first call. require_view, so a member sees it too.
    ("GET", "/api/sentinel/config", True),
    ("GET", "/api/sentinel/config", "member"),
    ("GET", "/api/sentinel/config", False),
    ("GET", "/api/sentinel/agent-keys", True),
    ("GET", "/api/sentinel/agent-keys", False),
    ("GET", "/api/sentinel/agent-keys", "member"),

    # --- Home Assistant integration: Bearer integration keys -----------
    #
    # The key is hashed as UTF-8 of the latin-1-decoded header and trimmed
    # with Python's str.strip(), which removes a trailing U+00A0 — one
    # byte on the wire. kind='integration' is the boundary: an MCP key
    # with a valid hash must not get in.
    *[("GET", path, False, {"Authorization": auth})
      for path in ("/api/integration/cameras", "/api/integration/status")
      for auth in (
          "Bearer osi_live_integration_key",
          "bearer osi_live_integration_key",
          "BEARER   osi_live_integration_key  ",
          "Bearer osi_live_integration_key\u00a0",
          "Bearer osi_revoked_integration",
          "Bearer osc_mcp_kind_key",
          "Bearer ",
          "Bearer    ",
          "Basic osi_live_integration_key",
          "Bearer\tosi_live_integration_key",
          "osi_live_integration_key",
      )],
    ("GET", "/api/integration/cameras", False),
    # A dashboard session token is not an integration key.
    ("GET", "/api/integration/cameras", True),

    # --- install + MCP setup scripts: public, header-heavy -------------
    #
    # The bodies are read straight off disk, so a diff here is almost
    # always a header diff: the media types differ between the three
    # (text/x-shellscript vs text/plain), only the two mcp-setup routes
    # carry Cache-Control, and Starlette appends "; charset=utf-8" to
    # every one of them because they all start with "text/".
    ("GET", "/install.sh", False),
    ("GET", "/mcp-setup.sh", False),
    ("GET", "/mcp-setup.ps1", False),
    # A signed-in browser hits the same routes; auth changes nothing.
    ("GET", "/install.sh", True),
    # Not ported, and must stay on the proxy: it resolves the newest
    # GitHub release through an in-process cache.
    ("GET", "/downloads/linux/x86_64", False),
    ("GET", "/downloads/nope/x86_64", False),

    # --- incident evidence blobs and their synthetic playlists ---------
    #
    # One case per fixture row (see seed_cameras.sql ids 2, 5-15): each
    # pins a branch of the MIME handling or the duration parse.
    *[("GET", f"/api/incidents/1/evidence/{e}", True) for e in
      (2, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15)],
    *[("GET", f"/api/incidents/1/evidence/{e}/playlist.m3u8", True) for e in
      (2, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15)],
    # No such evidence row, but the incident is the caller's: the 404
    # detail is the evidence one.
    ("GET", "/api/incidents/1/evidence/9999", True),
    ("GET", "/api/incidents/1/evidence/9999/playlist.m3u8", True),
    # The incident is missing or another tenant's, so Python raises out
    # of _get_owned_incident first and the detail is "Incident not
    # found" — a different string, from a different check.
    ("GET", "/api/incidents/9999/evidence/1", True),
    ("GET", "/api/incidents/9999/evidence/1/playlist.m3u8", True),
    ("GET", "/api/incidents/4/evidence/4", True),
    ("GET", "/api/incidents/4/evidence/4/playlist.m3u8", True),
    # Evidence 2 belongs to incident 1, so asking for it under incident
    # 3 must 404 rather than leak across incidents.
    ("GET", "/api/incidents/3/evidence/2", True),
    # Path integers: FastAPI 422s these before the handler runs.
    ("GET", "/api/incidents/1/evidence/abc", True),
    ("GET", "/api/incidents/abc/evidence/1", True),
    ("GET", "/api/incidents/1/evidence/5.0", True),
    ("GET", "/api/incidents/1/evidence/", True),
    # Unauthenticated and non-admin: both must be refused identically.
    ("GET", "/api/incidents/1/evidence/5", False),
    ("GET", "/api/incidents/1/evidence/5/playlist.m3u8", False),

    # --- health: three endpoints, three audiences -----------------------
    #
    # `ready` answers 503 when a critical probe fails, which the
    # harness's own tiers do reach: the email worker's interval is
    # pinned out of the way on BOTH, so neither ticks and both go
    # critical once past the startup grace. That is the interesting
    # case, and it is only comparable because the pinning is symmetric.
    ("GET", "/api/health", False),
    ("GET", "/api/health/ready", False),
    ("GET", "/api/health/ready?nocache=1", False),
    ("GET", "/api/health/ready?nocache=true", False),
    ("GET", "/api/health/ready?nocache=0", False),
    ("GET", "/api/health/ready?nocache=yes", False),
    ("GET", "/api/health/ready?nocache=banana", False),
    ("GET", "/api/health/ready?nocache=", False),
    # Repeated: FastAPI takes the last.
    ("GET", "/api/health/ready?nocache=0&nocache=1", False),
    ("GET", "/api/health/detailed", False),

    # --- security.txt: public, both locations --------------------------
    ("GET", "/.well-known/security.txt", False),
    ("GET", "/security.txt", False),
    # authenticated callers get the same file
    ("GET", "/.well-known/security.txt", True),

    # --- a NON-ADMIN caller --------------------------------------------
    # require_admin must 403, and the notification inbox must hide
    # audience="admin" rows. Neither was covered until a mutation
    # scored 296/296 with the audience filter deleted.
    ("GET", "/api/notifications", "member"),
    ("GET", "/api/notifications?limit=200", "member"),
    ("GET", "/api/notifications/unread-count", "member"),
    ("GET", "/api/cameras", "member"),
    ("GET", "/api/camera-groups", "member"),
    ("GET", "/api/settings", "member"),
    ("GET", "/api/incidents", "member"),
    ("GET", "/api/incidents/counts", "member"),
    ("GET", "/api/incidents/1", "member"),
    # Both evidence routes are require_admin, so a member must get the
    # same 403 from each stack — the case that a differential run as
    # admin cannot see at all.
    ("GET", "/api/incidents/1/evidence/5", "member"),
    ("GET", "/api/incidents/1/evidence/5/playlist.m3u8", "member"),
    ("GET", "/api/audit-logs", "member"),
    ("GET", "/api/audit/stream-logs", "member"),
    ("GET", "/api/audit/stream-logs/stats", "member"),
    ("GET", "/api/mcp/activity/logs", "member"),
    ("GET", "/api/mcp/keys", "member"),
    ("GET", "/api/nodes/node-aaaa1111", "member"),
    ("GET", "/api/motion/events", "member"),
    ("GET", "/api/notifications/email/preferences", "member"),

    # --- notifications -------------------------------------------------
    ("GET", "/api/notifications", True),
    ("GET", "/api/notifications", False),
    ("GET", "/api/notifications/unread-count", True),
    ("GET", "/api/notifications/email/preferences", True),
    *[("GET", f"/api/notifications?{q}", True) for q in [
        "limit=5", "limit=200", "limit=3&offset=10", "offset=0",
        "hours=1", "hours=168", "hours=720",
        # bounds: limit has ge/le, hours has only le
        "limit=0", "limit=201", "offset=-1", "hours=721", "hours=-5", "hours=abc",
    ]],

    # --- local auth: login and refresh ---------------------------------
    # POST cases carry a body, so they live in write_diff; these cover
    # the rejection paths, which have no side effects.
    ("POST", "/api/auth/local/login", False),
    ("POST", "/api/auth/local/refresh", False),
    ("GET", "/api/auth/local/login", False),

    # --- api keys ------------------------------------------------------
    ("GET", "/api/mcp/keys", True),
    ("GET", "/api/mcp/keys", False),
    ("GET", "/api/integration/keys", True),
    ("GET", "/api/integration/keys", False),

    # --- CORS: a ported route must carry the same headers Python does --
    ("GET", "/api/cameras", True, {"Origin": "http://localhost:5173"}),
    ("GET", "/api/cameras", True, {"Origin": "http://localhost:8000"}),
    # a disallowed origin still gets Starlette's "simple headers", but no
    # allow-origin and no Vary
    ("GET", "/api/cameras", True, {"Origin": "https://evil.test"}),
    ("GET", "/api/cameras", True, {"Origin": "https://app.example.com.evil.test"}),
    ("GET", "/api/settings", True, {"Origin": "http://localhost:5173"}),
    ("GET", "/api/incidents", True, {"Origin": "http://localhost:5173"}),
    # a proxied route must not end up with the headers twice
    ("GET", "/api/nodes", True, {"Origin": "http://localhost:5173"}),
    # unauthenticated responses carry them too
    ("GET", "/api/cameras", False, {"Origin": "http://localhost:5173"}),

    # --- request id: honoured when plausible, replaced when not -------
    ("GET", "/api/cameras", True, {"X-Request-Id": "client-supplied-1234"}),
    ("GET", "/api/cameras", True, {"X-Request-Id": "a-b-c-d-e-f-g-h"}),
    ("GET", "/api/cameras", True, {"X-Request-Id": "short"}),
    ("GET", "/api/cameras", True, {"X-Request-Id": "has space in it"}),
    ("GET", "/api/cameras", True, {"X-Request-Id": "semi;colon;injection"}),
    ("GET", "/api/cameras", True, {"X-Request-Id": "x" * 200}),
    # proxied, for contrast: the id must come from Python, not be
    # overwritten on the way back out
    ("GET", "/api/nodes", True, {"X-Request-Id": "client-supplied-5678"}),

    # HEAD is generated for every served path below rather than listed,
    # because hand-picking the sample is what hid the bug: the five paths
    # originally listed here all happened to go through `ported()`, and
    # the four routes registered by hand kept answering HEAD with 200
    # while this reported 218/218.

    # --- binary downloads ----------------------------------------------
    # The refusals need no network. The redirects do: both stacks ask
    # GitHub, and if it is unreachable both answer 503 — equal either
    # way, and equal for a good reason when it is reachable, because
    # the asset each picks is the same one.
    *[("GET", f"/downloads/{path}", False) for path in [
        "linux/x86_64", "linux/aarch64", "linux/armv7",
        "macos/aarch64", "windows/x86_64",
        "LINUX/X86_64",              # the lookup lowercases both halves
        "plan9/x86_64", "linux/sparc", "linux/", "windows/x86_64/extra",
    ]],

    # --- the node list -------------------------------------------------
    # Each row carries what its build compares to. Both stacks answer
    # from the environment fallback here, because neither has fetched
    # GitHub — that agreement is worth exactly as much as it sounds, and
    # the cache itself is covered by unit tests instead.
    ("GET", "/api/nodes", True),
    ("GET", "/api/nodes", False),
    ("GET", "/api/nodes", "member"),

    # --- the plan panel ----------------------------------------------
    # `usage.viewer_hours_used` is normalised away: it reads the
    # in-process counter, which is Rust's now, so the two answers are
    # not comparable by construction. Everything else here is.
    ("GET", "/api/nodes/plan", True),
    ("GET", "/api/nodes/plan", False),
    ("GET", "/api/nodes/plan", "member"),

    # --- sentinel runs -----------------------------------------------
    # `since` goes through `datetime.fromisoformat`, whose C parser takes
    # a good deal more than ISO 8601 — a colon after the seconds, a bare
    # `.` for the fraction, any character at all in the separator
    # position, and offsets whose minutes run past 59. A ValueError there
    # is a 400; the OverflowError from the `astimezone` that follows is
    # not caught and is a 500. The org-timezone cases need a settings row
    # and so live in the write differential, which has per-case setup.
    ("GET", "/api/sentinel/runs", True),
    ("GET", "/api/sentinel/runs", False),
    *[("GET", f"/api/sentinel/runs?{q}", True) for q in [
        "limit=3", "limit=2&offset=1", "offset=7", "limit=200", "limit=1",
        "trigger=manual", "trigger=motion", "trigger=scheduled",
        "trigger=incident_opened", "trigger=nope", "trigger=",
        "limit=0", "limit=201", "offset=-1", "limit=abc", "offset=1.5",
        "limit=9223372036854775807", "offset=9223372036854775807",
        "offset=9223372036854775808", "offset=99999999999999999999",
        # what fromisoformat takes
        "since=2026-05-07T15:00:00", "since=2026-05-07T15:00:00Z",
        "since=2026-05-07T15:00:00%2B00:00", "since=2026-05-07T15:00-05:00",
        "since=2026-05-07T15:00:00.123456", "since=2026-05-07",
        "since=20260507", "since=20260507T150000", "since=2026-W19-4",
        "since=2026-05-07%2015:00:00", "since=2026-05-07T15:00:00:00",
        "since=2026-05-07T15.00", "since=2026-05-07X15:00:00",
        "since=2026-05-07T15:00%2B05:99", "since=2026-05-07T15:00%2B00:00:00.5",
        "since=2026-05-07T15:00:00.123456%00junk",
        # Where `.replace("Z", "+00:00")` is not the no-op it looks
        # like: a Z outside the zone position changes what parses at
        # all, in both directions. Without these the replacement can be
        # deleted and every case still passes.
        "since=2026-W01Z", "since=2026-05-07Z15:00", "since=20260507Z1500",
        # and what it does not
        "since=bogus", "since=", "since=2026-13-01", "since=2026-02-30",
        "since=2026-05-07T24:00", "since=2026-05-07T15:00%2B24:00",
        "since=%D9%A2%D9%A0%D9%A2%D9%A6-05-07",
        # overflow in astimezone, which the route does not catch
        "since=0001-01-01T00:00%2B01:00", "since=9999-12-31T23:00-01:00",
        "since=2026-05-07T15:00:00&trigger=manual&limit=2&offset=1",
    ]],

    # --- integers at the edges ---------------------------------------
    # Every one of these was a real difference. The window routes cap
    # `days`/`hours` on one side only, so a large negative value asks for
    # a window past year 9999, where Python's datetime overflows into a
    # 500 — chrono reaches year 262143 and answered 200, and further out
    # `Duration::days` panicked and dropped the connection. Beyond i64,
    # Pydantic still parses a Python int and reports the *bound* it
    # broke, and past 4,300 digits reports a size error, where a naive
    # i64 parse says "not an integer". And an id too large for an
    # `integer` column is a 500 from Postgres, because SQLAlchemy types
    # the bind from the column — not the 404 that comparing it as a
    # bigint would give.
    *[("GET", f"/api/mcp/activity/logs/stats?days={v}", True) for v in [
        "-1000000000000", "-3000000", "-2000000", "-2914000",
        "-99999999999999999999", "99999999999999999999",
        "9223372036854775807", "-9223372036854775808",
        "1" + "0" * 4300, "1_" + "0" * 4300, "0" * 4300 + "5",
    ]],
    *[("GET", f"/api/audit/stream-logs/stats?days={v}", True) for v in [
        "-1000000000000", "-3000000", "-99999999999999999999",
    ]],
    *[("GET", f"/api/motion/events/stats?hours={v}", True) for v in [
        "-100000000000000", "-70000000", "-99999999999999999999",
    ]],
    *[("GET", f"/api/notifications?hours={v}", True) for v in [
        "-99999999999999999999", "-100000000000000",
    ]],
    *[("GET", f"/api/audit-logs?{q}", True) for q in [
        "offset=99999999999999999999", "limit=-99999999999999999999",
        "limit=9223372036854775808", "offset=" + "1" + "0" * 4300,
    ]],
    *[("GET", f"/api/incidents/{v}", True) for v in [
        "3000000000", "-3000000000", "2147483648", "-2147483649",
        "2147483647", "99999999999999999999", "1" + "0" * 4300,
    ]],
    *[("GET", f"/api/incidents/1/evidence/{v}", True) for v in [
        "3000000000", "99999999999999999999",
    ]],

    # methods Rust has NOT ported on a path it HAS — these must still
    # reach Python rather than being answered with 405 by axum.
    ("POST", "/api/cameras", True),
    ("DELETE", "/api/cameras/cam-live", True),
    ("POST", "/api/camera-groups", True),
    ("PUT", "/api/cameras/cam-live", True),
]


# Headers compared alongside status and body. CORS is here because a
# ported route leaves Python's CORSMiddleware behind and came back with
# none of these — the preflight still passed (OPTIONS falls through to
# the proxy) so the browser failed only on the real response.
COMPARED_HEADERS = (
    "access-control-allow-origin",
    "access-control-allow-credentials",
    "access-control-expose-headers",
    "vary",
    "retry-after",
    # The security set main.py stamps on every response. A ported route
    # left the middleware behind and shipped none of it — including
    # X-Frame-Options, whose absence is what made the dashboard
    # clickjackable the last time this regressed.
    "x-content-type-options",
    "x-frame-options",
    "referrer-policy",
    "permissions-policy",
    "x-request-id",
    # The response's own headers, not the middleware's. These went
    # uncompared until the install-script routes landed, where the
    # media type *is* the behaviour: /mcp-setup.ps1 is text/plain and
    # /mcp-setup.sh is text/x-shellscript, both with a charset Starlette
    # appends rather than the handler setting it, and only those two
    # carry Cache-Control. Body-only diffing scores all three identical
    # while a client sniffing the type gets the wrong answer.
    "content-type",
    "content-disposition",
    "cache-control",
)

# A freshly minted request id is random, so only its shape can be
# compared. An echoed-back inbound id is compared literally, because
# honouring the client's id is the behaviour under test.
MINTED_ID = re.compile(r"^[0-9a-f]{16}$")


def served_paths():
    """Every path app.rs serves, with a concrete value for each {param}.

    Read out of the route table rather than listed here, so a newly
    ported route is covered the day it lands.
    """
    src = (HERE.parent.parent / "src" / "app.rs").read_text()
    concrete = {
        "{camera_id}": "cam-live",
        "{incident_id}": "1",
        "{node_id}": "node-aaaa1111",
        "{group_id}": "1",
    }
    out = []
    for m in re.finditer(r'\.route\(\s*"([^"]+)"\s*,', src):
        i = src.rindex("(", 0, m.end())
        depth, j = 0, i
        while j < len(src):
            if src[j] == "(":
                depth += 1
            elif src[j] == ")":
                depth -= 1
                if depth == 0:
                    break
            j += 1
        if "still_python" in src[m.end():j]:
            continue
        path = m.group(1)
        for placeholder, value in concrete.items():
            path = path.replace(placeholder, value)
        if "{" in path:
            continue
        out.append(path)
    return sorted(set(out))


def fetch(base, method, path, with_auth, extra_headers=None):
    req = urllib.request.Request(base + path, method=method)
    if with_auth == "member":
        req.add_header("Authorization", f"Bearer {MEMBER_TOKEN}")
    elif with_auth:
        req.add_header("Authorization", f"Bearer {TOKEN}")
    for k, v in (extra_headers or {}).items():
        req.add_header(k, v)
    try:
        with urllib.request.urlopen(req, timeout=15) as r:
            return r.status, r.read(), _headers(r.headers)
    except urllib.error.HTTPError as e:
        return e.code, e.read(), _headers(e.headers)
    except Exception as e:  # noqa: BLE001
        return None, str(e).encode(), {}


def _headers(msg):
    out = {}
    for k in COMPARED_HEADERS:
        v = msg.get(k)
        if v is None:
            continue
        if k == "x-request-id" and MINTED_ID.match(v):
            v = "<minted>"
        out[k] = v
    return out


def normalise(body, path=""):
    """Parse JSON if possible; sort lists by a stable key.

    `/api/nodes/plan` reports one field this harness cannot compare:
    `usage.viewer_hours_used` comes from the in-memory counter the
    segment route maintains, and that counter lives in whichever
    process serves segments. Since that became Rust, Python's copy only
    ever reads zero — which is precisely why the route had to move, and
    precisely why the two answers are not comparable. The counter is
    covered by tests/hls_db.rs against a real database, and the
    arithmetic around it by `round_two_places_like_python`.

    security.txt is plain text with an `Expires` field regenerated on
    every request, so the two stacks differ by however many seconds
    apart they were called. The field is asserted separately — that it
    parses, and that it lands inside RFC 9116's one-year cap — by unit
    tests in src/api/well_known.rs.
    """
    if body.startswith(b"# Sentinel by SourceBox"):
        return "\n".join(
            ln for ln in body.decode("utf-8", "replace").splitlines()
            if not ln.startswith("Expires:")
        )
    try:
        v = json.loads(body)
    except Exception:  # noqa: BLE001
        return body.decode("utf-8", "replace")
    if path.startswith("/api/health/") and isinstance(v, dict):
        # Three families of value here vary between two calls a
        # millisecond apart, and none of them is a port decision:
        # measured latencies, the process's own uptime and its clock.
        # Everything else — every status, every threshold, every cache
        # and queue count, and the disk figures — is compared as it is.
        for field in ("uptime_seconds", "started_at", "time"):
            if field in v:
                v[field] = f"<{field}>"
        checks = v.get("checks")
        if isinstance(checks, dict):
            for name, probe in checks.items():
                if not isinstance(probe, dict):
                    continue
                for field in ("latency_ms", "tick_age_seconds", "uptime_seconds"):
                    if field in probe and probe[field] is not None:
                        probe[field] = f"<{field}>"
                # The disk fills and drains while the suite runs — a
                # cargo build between the two calls moves it by
                # megabytes — so a live reading here is either flaky or
                # meaningless. The STATUS is compared, which is what
                # the thresholds decide, and the arithmetic behind it
                # is compared exactly by health_run.sh, which feeds
                # both stacks the same injected reading.
                if name == "disk":
                    for field in ("bytes_free", "bytes_used", "bytes_total",
                                  "percent_used"):
                        if field in probe:
                            probe[field] = f"<{field}>"
    if path.startswith("/api/nodes/plan") and isinstance(v, dict):
        usage = v.get("usage")
        if isinstance(usage, dict) and "viewer_hours_used" in usage:
            usage["viewer_hours_used"] = "<in-process counter>"
    if isinstance(v, list):
        def key(item):
            if isinstance(item, dict):
                for k in ("camera_id", "id", "name"):
                    if k in item:
                        return (0, str(item[k]))
            return (1, json.dumps(item, sort_keys=True))
        return sorted(v, key=key)
    return v


def check_coverage():
    """Fail if the fixture has aged out of the interesting states.

    `effective_status` turns a camera offline 90 seconds after its last
    heartbeat. A fixture seeded once and reused later therefore exercises
    only the offline branch — and an earlier run of this harness reported
    31/31 identical while doing exactly that, testing neither a live
    camera nor the last_error surfacing. Re-seed, then run.
    """
    status, body, _ = fetch(RUST, "GET", "/api/cameras", True)
    if status != 200:
        print(f"COVERAGE: /api/cameras returned {status}, expected 200")
        return False
    cams = json.loads(body)
    live = [c for c in cams if c["status"] not in ("offline", None)]
    errs = [c for c in cams if c["last_error"]]
    stale = [c for c in cams if c["status"] == "offline"]
    fracs = [c["last_seen"] for c in cams if c["last_seen"] and "." in c["last_seen"]]
    whole = [c["last_seen"] for c in cams
             if c["last_seen"] and "." not in c["last_seen"]]

    print(f"coverage: {len(cams)} cameras, {len(live)} live, {len(stale)} offline, "
          f"{len(errs)} surfacing last_error, "
          f"{len(fracs)} fractional / {len(whole)} whole-second timestamps")

    ok = True
    for label, got, want in [
        ("live cameras", len(live), 3),
        ("offline cameras", len(stale), 3),
        ("cameras surfacing last_error", len(errs), 3),
        ("fractional timestamps", len(fracs), 1),
        ("whole-second timestamps", len(whole), 1),
    ]:
        if got < want:
            print(f"COVERAGE TOO THIN: {label} = {got}, need >= {want}. Re-seed first.")
            ok = False
    return ok


def main():
    # Generated, not listed — see the note in CASES.
    CASES.extend(("HEAD", p, True) for p in served_paths())

    if not check_coverage():
        print("\nrun seed_cameras.sql against the test database, then re-run")
        return 2

    bad = 0
    rate_limited = []
    wanted = [w for w in DIFF_ONLY.split("|") if w]
    cases = [c for c in CASES if not wanted or any(w in c[1] for w in wanted)]
    for case in cases:
        method, path, auth = case[0], case[1], case[2]
        extra = case[3] if len(case) > 3 else None
        rs_status, rs_body, rs_head = fetch(RUST, method, path, auth, extra)
        py_status, py_body, py_head = fetch(PYTHON, method, path, auth, extra)
        rs, py = normalise(rs_body, path), normalise(py_body, path)

        same = (rs_status == py_status) and (rs == py) and (rs_head == py_head)
        label = f"{method} {path}" + ("" if auth else "  (no auth)")

        # Python's 500s carry no headers at all: an unhandled exception
        # is caught by Starlette's outermost ServerErrorMiddleware, above
        # the middleware that stamps the security headers and the request
        # id. Rust keeps them, which is strictly safer and which no
        # client can depend on the absence of. Compare status and body on
        # a 500, not headers.
        if rs_status == 500 and py_status == 500:
            rs_head = py_head = {}
            same = (rs_status == py_status) and (rs == py)

        # A 429 on either side means the run itself exhausted a limit, not
        # that the port is wrong. Counting it as a diff would be a false
        # positive; counting it as a pass would hide a real one.
        if 429 in (rs_status, py_status) and not path.startswith("/api/_ratelimit"):
            rate_limited.append(label)
            continue
        if same:
            if VERBOSE:
                print(f"  ok      {label:<46} {rs_status}")
        else:
            bad += 1
            print(f"  DIFFER  {label:<46} rust={rs_status} python={py_status}")
            if rs_head != py_head:
                print(f"            headers rust  : {json.dumps(rs_head, sort_keys=True)}")
                print(f"            headers python: {json.dumps(py_head, sort_keys=True)}")
            if rs_status == py_status and rs != py:
                # The paths that differ, not two dumps with the same
                # first three hundred characters — a list of twenty-one
                # cameras differing in one field printed as two
                # identical prefixes eleven times over.
                for line in value_diff(py, rs):
                    print(f"            {line}")

    if rate_limited:
        print(f"\nINCONCLUSIVE: {len(rate_limited)} case(s) hit a rate limit and were "
              f"not compared, e.g. {rate_limited[0]}")
        print("Flush the limiter (docker exec cc-redis-test redis-cli FLUSHDB) and re-run.")
        return 3

    scope = f" [DIFF_ONLY={DIFF_ONLY!r}: {len(cases)} of {len(CASES)} cases]" if DIFF_ONLY else ""
    print(f"\n{len(cases) - bad}/{len(cases)} identical, {bad} differing{scope}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
