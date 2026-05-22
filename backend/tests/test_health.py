"""Basic health and security header tests."""


def test_health_endpoint(unauthenticated_client):
    """Health check should always return 200."""
    resp = unauthenticated_client.get("/api/health")
    assert resp.status_code == 200
    data = resp.json()
    assert data["status"] == "healthy"
    assert "version" in data


def test_security_headers(unauthenticated_client):
    """All responses should include security headers."""
    resp = unauthenticated_client.get("/api/health")
    assert resp.headers.get("X-Content-Type-Options") == "nosniff"
    assert resp.headers.get("X-Frame-Options") == "DENY"
    assert resp.headers.get("Referrer-Policy") == "strict-origin-when-cross-origin"
    assert "camera=()" in resp.headers.get("Permissions-Policy", "")


# ── /api/health/detailed ─────────────────────────────────────────────


def test_health_detailed_returns_full_shape(unauthenticated_client):
    """Status-page consumers depend on the top-level keys; pin them so a
    refactor that drops one (e.g. removing `uptime_seconds` to "simplify")
    breaks the test, not silently the status page."""
    resp = unauthenticated_client.get("/api/health/detailed")
    assert resp.status_code == 200
    data = resp.json()

    assert data["status"] in ("healthy", "degraded", "unhealthy")
    assert data["version"] == "2.1.2"
    assert isinstance(data["uptime_seconds"], (int, float))
    assert data["uptime_seconds"] >= 0
    assert "started_at" in data
    assert "time" in data
    # 8 keys: 4 dependency probes (shared with /api/health/ready) +
    # 4 in-process subsystem snapshots (HLS cache, viewer usage,
    # SSE, Resend transport).  Status-page consumers may render any
    # of these; pinning the set catches "someone added/removed a key
    # without updating the dashboard schema."
    assert set(data["checks"].keys()) == {
        "database", "clerk", "disk", "email_worker",
        "hls_cache", "viewer_usage", "sse", "resend",
    }


def test_health_detailed_resend_check_disabled_by_default(unauthenticated_client, monkeypatch):
    """When EMAIL_ENABLED=false (the default for local dev / tests), the
    resend check reports 'disabled' rather than 'unconfigured' or 'ok'.
    This split lets the operator distinguish "I forgot to set the
    secret" from "I left the kill-switch off intentionally."
    """
    from app.core.config import settings as app_settings
    monkeypatch.setattr(app_settings, "EMAIL_ENABLED", False)

    resp = unauthenticated_client.get("/api/health/detailed")
    data = resp.json()

    assert data["checks"]["resend"]["status"] == "disabled"
    # Queue depth is always reported as an int, never None — status-page
    # consumers should be able to render it without null-checks.
    assert isinstance(data["checks"]["resend"]["queue_depth"], int)


def test_health_detailed_resend_check_unconfigured(unauthenticated_client, monkeypatch):
    """Kill-switch on but secret missing → 'unconfigured' (operator
    forgot a step).  Different from 'disabled' so the diagnosis is
    obvious from the status page."""
    from app.core.config import settings as app_settings
    monkeypatch.setattr(app_settings, "EMAIL_ENABLED", True)
    monkeypatch.setattr(app_settings, "RESEND_API_KEY", "")

    resp = unauthenticated_client.get("/api/health/detailed")
    data = resp.json()

    assert data["checks"]["resend"]["status"] == "unconfigured"


def test_health_detailed_resend_check_ok_when_configured(unauthenticated_client, monkeypatch):
    """Kill-switch on AND secret set → 'ok'."""
    from app.core.config import settings as app_settings
    monkeypatch.setattr(app_settings, "EMAIL_ENABLED", True)
    monkeypatch.setattr(app_settings, "RESEND_API_KEY", "re_test_dummy")
    monkeypatch.setattr(app_settings, "EMAIL_FROM_ADDRESS", "n@s.test")

    resp = unauthenticated_client.get("/api/health/detailed")
    data = resp.json()

    assert data["checks"]["resend"]["status"] == "ok"


def test_health_detailed_resend_queue_depth_counts_pending(unauthenticated_client, db):
    """Queue depth is the count of EmailOutbox rows in 'pending' status —
    drives status-page graphs and informs alerting on a backlog that
    isn't draining."""
    from app.models.models import EmailOutbox

    # Mix of statuses: only 'pending' should count.
    for status, n in [("pending", 3), ("sent", 5), ("failed", 1)]:
        for i in range(n):
            db.add(EmailOutbox(
                org_id="org_x",
                recipient_email=f"u{i}@x.test",
                subject="x",
                body_text="t",
                body_html="<p>t</p>",
                kind="camera_offline",
                status=status,
            ))
    db.commit()

    resp = unauthenticated_client.get("/api/health/detailed")
    data = resp.json()

    assert data["checks"]["resend"]["queue_depth"] == 3


def test_health_detailed_disk_check_returns_usage(unauthenticated_client):
    """External uptime monitors (UptimeRobot, BetterStack) poll this
    endpoint and parse `checks.disk.percent_used` to alert before the
    Fly volume fills.  Pin the shape — accidentally renaming a field
    or dropping a metric would silently break those alerts."""
    resp = unauthenticated_client.get("/api/health/detailed")
    data = resp.json()

    disk = data["checks"]["disk"]
    # In tests we don't have /data, so the fallback to '.' kicks in.
    # `error` is acceptable on platforms where shutil.disk_usage('.')
    # somehow fails; the rest of the keys must be present in the
    # success path.
    assert disk["status"] in ("ok", "warn", "critical", "error")
    assert "path" in disk
    if disk["status"] != "error":
        assert isinstance(disk["bytes_used"], int)
        assert isinstance(disk["bytes_free"], int)
        assert isinstance(disk["bytes_total"], int)
        assert isinstance(disk["percent_used"], (int, float))
        assert 0 <= disk["percent_used"] <= 100


def test_health_detailed_database_check_includes_latency(unauthenticated_client):
    """The DB latency number is the bit on-call cares about — confirm it's
    actually populated when the ping succeeds (in tests we use in-memory
    SQLite, so this should always be a small positive number)."""
    resp = unauthenticated_client.get("/api/health/detailed")
    data = resp.json()

    db = data["checks"]["database"]
    assert db["status"] == "ok"
    assert "latency_ms" in db
    assert db["latency_ms"] >= 0
    # Wide upper bound — in CI this might be a millisecond or two,
    # locally it's microseconds. Just sanity-check it's not e.g. 60_000ms.
    assert db["latency_ms"] < 5000


def test_health_detailed_does_not_leak_org_or_camera_ids(
    unauthenticated_client, db,
):
    """Privacy regression: the endpoint is unauthenticated so it must
    NOT include identifiers (org_id, camera_id, user_id, email) in any
    field. Counts are fine; identifiers are not."""
    # Seed something so the cache + subscriber maps could leak names if
    # we built them wrong.
    import asyncio as _asyncio

    from app.api.hls import _playlist_cache, _segment_cache
    from app.api.notifications import notification_broadcaster

    _playlist_cache["org_secret_camera_123"] = ("playlist body", 0.0)
    _segment_cache["org_secret_camera_123"] = {}
    notification_broadcaster._subscribers["org_secret_456"] = [
        (_asyncio.Queue(), False),
    ]
    try:
        resp = unauthenticated_client.get("/api/health/detailed")
        body = resp.text  # raw text — searches into all values
        assert "org_secret_camera_123" not in body
        assert "org_secret_456" not in body

        # Counts must still reflect the seeded data, otherwise we'd be
        # passing this assertion by accidentally returning empty.
        data = resp.json()
        assert data["checks"]["hls_cache"]["playlists_cached"] >= 1
        assert data["checks"]["sse"]["subscriber_orgs"] >= 1
    finally:
        # Cleanup so the seeded entries don't leak into the next test.
        _playlist_cache.pop("org_secret_camera_123", None)
        _segment_cache.pop("org_secret_camera_123", None)
        notification_broadcaster._subscribers.pop("org_secret_456", None)


def test_health_detailed_status_unhealthy_when_db_down(
    unauthenticated_client, monkeypatch,
):
    """If the DB ping raises, overall status flips to "unhealthy" and the
    error class surfaces (but not the exception message — that could
    contain connection strings).

    We patch ``SessionLocal`` in the health_probes module specifically
    (the probe imports it directly at use-time).  The Resend queue-
    depth read in ``/api/health/detailed`` uses ``main.SessionLocal``
    and gets its own fake here too so the probe's failure surfaces
    without an unrelated AttributeError on the queue-depth path."""
    from app import main
    from app.core import health_probes

    class _FakeSession:
        def execute(self, *_a, **_kw):
            raise RuntimeError("boom — would-be-leaked DSN here")

        def query(self, *_a, **_kw):
            # The detailed endpoint also reads the Resend queue depth
            # via main.SessionLocal — give it a stub that fails the
            # count gracefully (logged + queue_depth=-1) so the test
            # still reaches the rollup logic we're trying to verify.
            raise RuntimeError("queue probe also down")

        def close(self):
            pass

    monkeypatch.setattr(health_probes, "SessionLocal", lambda: _FakeSession())
    monkeypatch.setattr(main, "SessionLocal", lambda: _FakeSession())

    resp = unauthenticated_client.get("/api/health/detailed")
    assert resp.status_code == 200  # endpoint itself still works
    data = resp.json()

    assert data["status"] == "unhealthy"
    # New schema: probes use "critical" (drives the rollup) instead of
    # the old "error".  Same semantic, more consistent with the
    # readiness endpoint that returns 503 on critical-tier failures.
    assert data["checks"]["database"]["status"] == "critical"
    assert data["checks"]["database"]["error_class"] == "RuntimeError"
    # Crucially, the exception message must NOT have leaked.
    assert "would-be-leaked" not in resp.text
    assert "DSN" not in resp.text


# ── /api/health/ready ────────────────────────────────────────────────


import pytest

# Snapshot the real probe_clerk before any autouse fixture monkeypatches
# it.  Used by test_probe_clerk_unconfigured_*.  Module-level capture
# = runs at import time, runs once, can't be clobbered by per-test
# fixtures (which only set the patched value during their own test).
from app.core.health_probes import probe_clerk as _REAL_PROBE_CLERK


@pytest.fixture(autouse=True)
def _reset_readiness_cache():
    """Each test starts with a fresh readiness cache so probe results
    don't bleed across tests.  Without this, a test that injects a
    failing DB would poison the cache for every subsequent test."""
    from app import main
    main._reset_health_ready_cache_for_tests()
    yield
    main._reset_health_ready_cache_for_tests()


@pytest.fixture(autouse=True)
def _stub_clerk_probe(monkeypatch):
    """Conftest sets ``CLERK_SECRET_KEY=sk_test_fake`` so the Clerk SDK
    is initialised, but the fake key won't authenticate against Clerk's
    real API.  Stub the probe to return ``ok`` by default so tests
    don't false-503 on Clerk reachability.  Tests that specifically
    want to exercise the unconfigured / down paths override this
    fixture's effect by re-monkeypatching probe_clerk themselves."""
    from app.core import health_probes

    async def _ok_probe(*_a, **_kw):
        return health_probes.ProbeResult(
            status="ok", data={"latency_ms": 1.0, "stubbed": True},
        )

    monkeypatch.setattr(health_probes, "probe_clerk", _ok_probe)
    yield


@pytest.fixture(autouse=True)
def _stub_disk_probe(monkeypatch):
    """``probe_disk`` reads the real filesystem.  On a developer
    machine with a nearly-full disk (or in CI containers with
    weird mount layouts), the probe legitimately reports
    "critical" — which would flip every readiness test to 503
    for reasons unrelated to what the test is actually checking.

    Stub to ``ok`` by default.  Tests that specifically want to
    exercise the disk-critical path override this with their own
    monkeypatch."""
    from app.core import health_probes

    def _ok_disk():
        return health_probes.ProbeResult(
            status="ok",
            data={
                "path": "/test",
                "bytes_used": 100,
                "bytes_free": 900,
                "bytes_total": 1000,
                "percent_used": 10.0,
                "stubbed": True,
            },
        )

    monkeypatch.setattr(health_probes, "probe_disk", _ok_disk)
    yield


@pytest.fixture(autouse=True)
def _stub_email_worker_tick(monkeypatch):
    """The email worker doesn't tick during tests (the background loop
    isn't started by the TestClient), so once uptime exceeds the
    startup grace window the probe correctly reports "never ticked,
    critical".  In test mode EMAIL_ENABLED defaults to False so the
    probe short-circuits to ``disabled`` anyway, but tests that flip
    EMAIL_ENABLED on (e.g. test_health_ready_returns_503_when_db_down
    via cascading fixtures) would otherwise see the worker probe
    flip to critical and obscure what they're actually testing.

    Pretend the worker ticked recently.  Tests that specifically want
    to exercise the wedge path override this with their own monkeypatch."""
    import time as _time

    from app.core import email_worker
    monkeypatch.setattr(
        email_worker, "_last_tick_monotonic", _time.monotonic(),
    )
    yield


def test_health_ready_returns_200_when_all_probes_pass(unauthenticated_client):
    """Happy path: in-memory SQLite is up, Clerk SDK either succeeds
    or returns ``unconfigured`` (test env doesn't have a real key);
    disk + email_worker pass.  Endpoint returns 200 with ``ready: true``.

    External uptime monitors (BetterStack, UptimeRobot, etc.) only
    look at the HTTP status code; the body is for humans + diagnostics.
    """
    resp = unauthenticated_client.get("/api/health/ready")
    assert resp.status_code == 200, f"non-200 body: {resp.text}"
    body = resp.json()
    assert body["ready"] is True
    assert body["version"] == "2.1.2"
    assert isinstance(body["uptime_seconds"], (int, float))
    # Same probe set as /detailed's dependency-probe section.
    assert set(body["checks"].keys()) == {
        "database", "clerk", "disk", "email_worker",
    }


def test_health_ready_returns_503_when_db_down(
    unauthenticated_client, monkeypatch,
):
    """The single most-important readiness behavior: a wedged DB
    must flip the response to HTTP 503.  Without this, the whole
    point of the readiness endpoint (paging external monitors)
    falls flat — they'd see 200 and never alert."""
    from app.core import health_probes

    class _FakeSession:
        def execute(self, *_a, **_kw):
            raise RuntimeError("sql connection refused")
        def close(self):
            pass

    monkeypatch.setattr(health_probes, "SessionLocal", lambda: _FakeSession())

    resp = unauthenticated_client.get("/api/health/ready")
    assert resp.status_code == 503
    body = resp.json()
    assert body["ready"] is False
    assert body["checks"]["database"]["status"] == "critical"
    assert body["checks"]["database"]["error_class"] == "RuntimeError"


def test_health_ready_caches_results(unauthenticated_client, monkeypatch):
    """30s cache: a swarm of external pollers shouldn't multiply the
    real probe load.  Pin by counting probe invocations across
    back-to-back requests.

    Without caching, every /ready request would hit Clerk's API +
    do a SELECT 1 — a status-page checking every 10s plus 3 uptime
    monitors checking every 30s = ~24 round-trips per minute on each
    probe.  With caching: ~2."""
    from app.core import health_probes

    call_count = {"db": 0}
    real_probe_database = health_probes.probe_database

    def _counting_probe():
        call_count["db"] += 1
        return real_probe_database()

    monkeypatch.setattr(health_probes, "probe_database", _counting_probe)

    # First request runs probes.
    r1 = unauthenticated_client.get("/api/health/ready")
    assert r1.status_code == 200
    after_first = call_count["db"]
    assert after_first == 1

    # Three more back-to-back requests: cached, no probe runs.
    for _ in range(3):
        r = unauthenticated_client.get("/api/health/ready")
        assert r.status_code == 200
    assert call_count["db"] == after_first, (
        f"expected cached responses, but probe ran "
        f"{call_count['db'] - after_first} extra time(s)"
    )


def test_health_ready_nocache_bypasses(unauthenticated_client, monkeypatch):
    """``?nocache=1`` is a diagnostic escape hatch — operators
    troubleshooting a probe should be able to force a fresh run
    without waiting up to 30s for the cache to expire."""
    from app.core import health_probes

    call_count = {"db": 0}
    real_probe_database = health_probes.probe_database

    def _counting_probe():
        call_count["db"] += 1
        return real_probe_database()

    monkeypatch.setattr(health_probes, "probe_database", _counting_probe)

    # Each ?nocache request runs a fresh probe.
    for _ in range(3):
        r = unauthenticated_client.get("/api/health/ready?nocache=1")
        assert r.status_code == 200
    assert call_count["db"] == 3


def test_health_ready_resend_disabled_does_not_fail_readiness(
    unauthenticated_client, monkeypatch,
):
    """An org running with EMAIL_ENABLED=false must NOT have its
    readiness endpoint return 503 — email is opt-in and a deliberate
    "off" state is not a failure mode.  The email_worker probe
    surfaces ``status="disabled"`` which the rollup ignores."""
    from app.core.config import settings as app_settings
    monkeypatch.setattr(app_settings, "EMAIL_ENABLED", False)

    resp = unauthenticated_client.get("/api/health/ready")
    assert resp.status_code == 200
    body = resp.json()
    assert body["ready"] is True
    assert body["checks"]["email_worker"]["status"] == "disabled"


async def test_probe_clerk_unconfigured_returns_unconfigured_not_critical(monkeypatch):
    """Unit-level: probe_clerk's branch when CLERK_SECRET_KEY is unset.
    Returns ``status="unconfigured"`` (not critical) so a dev/test
    environment without Clerk wired up doesn't false-page.

    Tested at the probe-function level rather than through the
    endpoint because the endpoint-level autouse fixture stubs
    probe_clerk to ``ok`` for every other test.  Use the captured
    ``_REAL_PROBE_CLERK`` reference (snapshotted at module import,
    before the autouse fixture can monkeypatch it) so we exercise
    the actual function rather than the test stub."""
    from app.core.config import settings as app_settings

    monkeypatch.setattr(app_settings, "CLERK_SECRET_KEY", "")
    result = await _REAL_PROBE_CLERK()

    assert result.status == "unconfigured"
    assert not result.is_critical


# ── Worker-tick wiring ──────────────────────────────────────────────


def test_email_worker_seconds_since_last_tick_starts_none():
    """Before the worker runs, the timestamp is unset.  Health probe
    treats this as "ok" within the startup grace window so a fresh
    process doesn't immediately page on a worker that's about to
    tick for the first time."""
    from app.core import email_worker

    email_worker._reset_tick_for_tests()
    assert email_worker.seconds_since_last_tick() is None


def test_email_worker_run_one_tick_stamps_timestamp(db):
    """``run_one_tick`` must update the timestamp at the end of every
    successful tick — that's how the health probe knows the worker
    is alive.  A regression that moved the stamp to the loop body
    (instead of inside run_one_tick) would mean tests that drive
    the worker by calling run_one_tick directly silently lose
    coverage of the wedge-detection path."""
    from app.core import email_worker

    email_worker._reset_tick_for_tests()
    assert email_worker.seconds_since_last_tick() is None

    # Empty outbox; tick should still complete + stamp.
    email_worker.run_one_tick(db)

    age = email_worker.seconds_since_last_tick()
    assert age is not None
    assert 0 <= age < 1  # just ticked


def test_email_worker_probe_critical_when_stale(unauthenticated_client, monkeypatch):
    """If the worker stops ticking, /api/health/ready must flip to
    503 (when email is enabled).  This is the regression catch for
    the original "wedged worker, queue depth 0, nobody knows" gap."""
    from app.core import email_worker, health_probes
    from app.core.config import settings as app_settings

    monkeypatch.setattr(app_settings, "EMAIL_ENABLED", True)

    # Simulate a worker that ticked once a long time ago.  Use the
    # private attr so we don't have to actually wait STALE_AFTER_SECONDS.
    import time as _time
    monkeypatch.setattr(
        email_worker, "_last_tick_monotonic",
        _time.monotonic() - (health_probes.EMAIL_WORKER_STALE_AFTER_SECONDS + 5),
    )

    resp = unauthenticated_client.get("/api/health/ready")
    assert resp.status_code == 503
    body = resp.json()
    assert body["checks"]["email_worker"]["status"] == "critical"
    assert body["checks"]["email_worker"]["error_class"] == "WorkerStale"
