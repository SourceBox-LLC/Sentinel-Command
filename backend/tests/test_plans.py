"""Unit tests for ``app.core.plans``."""

from datetime import UTC, datetime, timedelta, timezone

from app.core.plans import (
    PAYMENT_GRACE_DAYS,
    effective_plan_for_caps,
    enforce_camera_cap,
    wire_plan_slug,
)
from app.models.models import Camera, CameraNode, Setting
from tests.conftest import TestSession


def test_wire_plan_slug_strips_org_suffix():
    """The internal ``free_org`` slug must render as ``free`` on the wire
    so the CameraNode pill badge reads ``[ FREE ]`` rather than
    ``[ FREE_ORG ]``."""
    assert wire_plan_slug("free_org") == "free"


def test_wire_plan_slug_passes_through_clean_slugs():
    """Paid plan slugs have no suffix and must pass through untouched."""
    assert wire_plan_slug("pro") == "pro"
    assert wire_plan_slug("pro_plus") == "pro_plus"


def test_wire_plan_slug_lowercases_and_trims():
    """Defensive: upstream sources have occasionally leaked casing/whitespace
    into stored plan strings. Normalize so the node always sees canonical
    lowercase."""
    assert wire_plan_slug("  PRO  ") == "pro"
    assert wire_plan_slug("Free_Org") == "free"


def test_wire_plan_slug_passes_unknown_tiers_through():
    """A future ``enterprise`` tier must render in the node UI even before
    we ship a node update — the badge falls back to the dimmed default
    rather than being hidden."""
    assert wire_plan_slug("enterprise") == "enterprise"


def test_wire_plan_slug_handles_empty_and_none():
    """Empty inputs collapse to ``free`` so the caller never has to guard
    against ``None`` before putting the value on the wire."""
    assert wire_plan_slug("") == "free"
    assert wire_plan_slug("   ") == "free"
    # The signature is ``str`` but the helper is robust to callers that
    # forget to coalesce a missing Setting value.
    assert wire_plan_slug(None) == "free"  # type: ignore[arg-type]


# ── enforce_camera_cap ─────────────────────────────────────────────────


def _seed_org_and_cameras(db, org_id: str, count: int, plan: str | None = None) -> list[Camera]:
    """Create ``count`` Camera rows for ``org_id`` with ascending created_at.

    Older cameras get earlier timestamps so the ``ORDER BY created_at ASC``
    in `enforce_camera_cap` gives a deterministic keep/drop ordering that
    matches the real plan-downgrade path.
    """
    # A parent node is required — Camera.node_id is a FK.
    node = CameraNode(
        node_id=f"nd_{org_id[-4:]}",
        name=f"Node for {org_id}",
        org_id=org_id,
        api_key_hash="x" * 64,
        status="online",
    )
    db.add(node)
    db.flush()

    base = datetime(2024, 1, 1, tzinfo=UTC).replace(tzinfo=None)
    cameras: list[Camera] = []
    for i in range(count):
        cam = Camera(
            camera_id=f"{org_id}_cam_{i:03d}",
            org_id=org_id,
            node_id=node.id,
            name=f"Camera {i}",
            capabilities="streaming",
            status="online",
            created_at=base + timedelta(minutes=i),
        )
        db.add(cam)
        cameras.append(cam)
    db.flush()

    if plan is not None:
        Setting.set(db, org_id, "org_plan", plan)
        db.flush()

    return cameras


def test_enforce_cap_is_noop_when_under_cap():
    """Free org with 1 camera under the 2-cap: nothing flips."""
    db = TestSession()
    try:
        _seed_org_and_cameras(db, "org_under", count=1, plan="free_org")
        result = enforce_camera_cap(db, "org_under")
        db.commit()
        assert result["changed"] is False
        assert result["disabled"] == []
        assert len(result["enabled"]) == 1

        cam = db.query(Camera).filter_by(org_id="org_under").one()
        assert cam.disabled_by_plan is False
    finally:
        db.close()


def test_enforce_cap_disables_oldest_first_on_free_plan():
    """Free org with 8 cameras over the 5-cap: the 5 OLDEST keep streaming;
    the other 3 flip to disabled. Oldest-first is deterministic and preserves
    long-lived cameras the operator cares about most.

    (Hardware caps are now abuse rails rather than product-tier differentiators,
    but the oldest-first eviction behaviour still needs to hold when a
    legitimate limit *is* exceeded.)"""
    db = TestSession()
    try:
        cams = _seed_org_and_cameras(db, "org_over", count=8, plan="free_org")
        result = enforce_camera_cap(db, "org_over")
        db.commit()

        assert result["changed"] is True
        assert result["max_cameras"] == 5
        assert len(result["enabled"]) == 5
        assert len(result["disabled"]) == 3

        # The 5 kept must be the 5 oldest (by created_at).
        kept = {c.camera_id for c in cams[:5]}
        assert set(result["enabled"]) == kept

        db.expire_all()
        rows = {c.camera_id: c.disabled_by_plan for c in db.query(Camera).filter_by(org_id="org_over").all()}
        for c in cams[:5]:
            assert rows[c.camera_id] is False
        for c in cams[5:]:
            assert rows[c.camera_id] is True
    finally:
        db.close()


def test_enforce_cap_clears_flags_on_upgrade():
    """Simulate a downgrade (cameras disabled) followed by an upgrade that
    raises the cap above the current count: all disabled flags must clear."""
    db = TestSession()
    try:
        # 8 cameras puts us over Free's 5-cap so the initial enforce
        # disables rows, giving the upgrade a real state transition to
        # reverse.
        cams = _seed_org_and_cameras(db, "org_upgrade", count=8, plan="free_org")
        enforce_camera_cap(db, "org_upgrade")
        db.commit()

        disabled_before = [c for c in cams if db.get(Camera, c.id).disabled_by_plan]
        assert len(disabled_before) == 3, "free cap should have disabled 3"

        # Upgrade to Pro (25-camera cap) and re-enforce.
        Setting.set(db, "org_upgrade", "org_plan", "pro")
        db.commit()
        result = enforce_camera_cap(db, "org_upgrade")
        db.commit()

        assert result["changed"] is True
        assert result["disabled"] == []
        assert len(result["enabled"]) == 8

        db.expire_all()
        rows = db.query(Camera).filter_by(org_id="org_upgrade").all()
        assert all(r.disabled_by_plan is False for r in rows)
    finally:
        db.close()


def test_enforce_cap_is_idempotent():
    """Running the helper twice with no plan change must not flip anything
    on the second call. The webhook-and-register safety net fires on every
    heartbeat-triggered registration; it must be cheap when nothing moves."""
    db = TestSession()
    try:
        # 8 on Free forces a real state transition on the first call so the
        # "nothing flips on the second call" invariant has something to prove.
        _seed_org_and_cameras(db, "org_idem", count=8, plan="free_org")
        first = enforce_camera_cap(db, "org_idem")
        db.commit()
        assert first["changed"] is True

        second = enforce_camera_cap(db, "org_idem")
        db.commit()
        assert second["changed"] is False
    finally:
        db.close()


def test_enforce_cap_treats_missing_plan_as_free():
    """Orgs that have never had a Setting row land on the free tier — the
    most restrictive default — not the most permissive."""
    db = TestSession()
    try:
        # 7 cameras on the free 5-cap → 2 must be disabled.
        _seed_org_and_cameras(db, "org_nosetting", count=7, plan=None)
        result = enforce_camera_cap(db, "org_nosetting")
        db.commit()

        assert result["changed"] is True
        assert result["max_cameras"] == 5
        assert len(result["disabled"]) == 2
    finally:
        db.close()


# ── Past-due grace period ──────────────────────────────────────────────


def _set_past_due(db, org_id: str, days_ago: float):
    """Flag an org as past-due with the timestamp set ``days_ago`` days in
    the past (float for sub-day precision)."""
    Setting.set(db, org_id, "payment_past_due", "true")
    past_due_at = (
        datetime.now(tz=UTC) - timedelta(days=days_ago)
    ).isoformat()
    Setting.set(db, org_id, "payment_past_due_at", past_due_at)
    db.flush()


def test_effective_plan_within_grace_keeps_nominal():
    """An org that just went past-due yesterday keeps its paid plan for
    cap purposes — the banner + MCP block are enough deterrent for a
    brief card failure. Tightening caps too aggressively would punish
    people who are already trying to fix their payment info."""
    db = TestSession()
    try:
        Setting.set(db, "org_grace_within", "org_plan", "pro")
        _set_past_due(db, "org_grace_within", days_ago=1)
        db.commit()
        assert effective_plan_for_caps(db, "org_grace_within") == "pro"
    finally:
        db.close()


def test_effective_plan_after_grace_rebases_to_free():
    """After the grace window expires, the effective plan drops to
    ``free_org`` so enforce_camera_cap tightens the caps. Matches the
    cancellation path without needing a separate webhook trigger."""
    db = TestSession()
    try:
        Setting.set(db, "org_grace_over", "org_plan", "pro")
        _set_past_due(db, "org_grace_over", days_ago=PAYMENT_GRACE_DAYS + 1)
        db.commit()
        assert effective_plan_for_caps(db, "org_grace_over") == "free_org"
    finally:
        db.close()


def test_effective_plan_not_past_due_returns_nominal():
    """Happy path — no past-due flag, the nominal plan wins."""
    db = TestSession()
    try:
        Setting.set(db, "org_happy", "org_plan", "pro")
        db.commit()
        assert effective_plan_for_caps(db, "org_happy") == "pro"
    finally:
        db.close()


def test_effective_plan_past_due_without_timestamp_keeps_nominal():
    """Defensive: an incomplete webhook that set the bool but not the
    timestamp must NOT tighten caps — we can't tell how long it's been
    past-due, and silently suspending cameras on a bad webhook payload
    would be worse than letting the leak persist briefly. The banner
    + MCP block still fire on the bool alone."""
    db = TestSession()
    try:
        Setting.set(db, "org_pd_no_ts", "org_plan", "pro")
        Setting.set(db, "org_pd_no_ts", "payment_past_due", "true")
        # Deliberately no payment_past_due_at.
        db.commit()
        assert effective_plan_for_caps(db, "org_pd_no_ts") == "pro"
    finally:
        db.close()


def test_enforce_cap_suspends_cameras_when_grace_expires():
    """End-to-end: a Pro org with 8 cameras that went past-due
    PAYMENT_GRACE_DAYS + some ago must get cameras beyond the free-tier
    cap suspended — same as if they had cancelled."""
    db = TestSession()
    try:
        cams = _seed_org_and_cameras(db, "org_grace_enforce", count=8, plan="pro")
        _set_past_due(db, "org_grace_enforce", days_ago=PAYMENT_GRACE_DAYS + 0.5)
        db.commit()

        result = enforce_camera_cap(db, "org_grace_enforce")
        db.commit()

        assert result["changed"] is True
        # Free tier cap is 5 → 3 of 8 cameras must be suspended.
        assert result["max_cameras"] == 5
        assert len(result["disabled"]) == 3

        # Oldest 5 keep streaming, newest 3 get flagged.
        db.expire_all()
        rows = {
            c.camera_id: c.disabled_by_plan
            for c in db.query(Camera).filter_by(org_id="org_grace_enforce").all()
        }
        for c in cams[:5]:
            assert rows[c.camera_id] is False
        for c in cams[5:]:
            assert rows[c.camera_id] is True
    finally:
        db.close()


# ── Live-lookup throttle cache prune (unbounded-dict leak fix) ──────────

# `resolve_org_plan` throttles live Clerk lookups per org via
# `_last_resolve_at`. Without a sweep that dict grows one entry per org
# that ever hit the live-lookup path, forever — dominated by free-tier
# orgs hammering MCP, exactly the throttle's target population. These
# tests pin that `_prune_resolve_cache` drops inert entries, keeps still-
# throttled ones, and stays time-gated so it isn't an O(orgs) walk per call.


def test_prune_resolve_cache_drops_stale_entries(monkeypatch):
    """An entry older than the throttle window is inert (the throttle check
    already treats it as expired) so the sweep drops it, while an entry
    still inside the window is kept."""
    import app.core.plans as plans

    now = 10_000.0
    monkeypatch.setattr(plans, "_last_resolve_prune_at", 0.0)
    monkeypatch.setattr(
        plans,
        "_last_resolve_at",
        {
            "org_stale": now - plans._RESOLVE_THROTTLE_SECONDS - 1.0,  # aged out
            "org_fresh": now - 5.0,  # still inside the throttle window
        },
    )

    plans._prune_resolve_cache(now)

    assert "org_stale" not in plans._last_resolve_at
    assert "org_fresh" in plans._last_resolve_at


def test_prune_resolve_cache_is_time_gated(monkeypatch):
    """Within `_RESOLVE_PRUNE_INTERVAL` the sweep is a no-op — it must not
    walk the dict on every call. A stale entry planted right after a recent
    prune survives until the gate reopens."""
    import app.core.plans as plans

    now = 10_000.0
    # Pretend we pruned 1s ago → gate closed (interval is 600s).
    monkeypatch.setattr(plans, "_last_resolve_prune_at", now - 1.0)
    monkeypatch.setattr(
        plans,
        "_last_resolve_at",
        {"org_stale": now - plans._RESOLVE_THROTTLE_SECONDS - 1.0},
    )

    plans._prune_resolve_cache(now)

    assert "org_stale" in plans._last_resolve_at  # gate held → not swept yet


def test_prune_resolve_cache_records_sweep_time(monkeypatch):
    """A sweep that actually runs stamps `_last_resolve_prune_at` so the
    next call within the interval short-circuits."""
    import app.core.plans as plans

    now = 10_000.0
    monkeypatch.setattr(plans, "_last_resolve_prune_at", 0.0)
    monkeypatch.setattr(plans, "_last_resolve_at", {})

    plans._prune_resolve_cache(now)
    assert plans._last_resolve_prune_at == now

    # A call 1s later is gated off, so the stamp does not advance.
    plans._prune_resolve_cache(now + 1.0)
    assert plans._last_resolve_prune_at == now
