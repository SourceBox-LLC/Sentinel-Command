"""
Tests for GDPR Article 17 (erasure) + Article 20 (export) compliance.

Most-important invariants pinned here:

  1. **Coverage** — every org-scoped model in app.core.gdpr.ORG_SCOPED_MODELS
     (and ORG_SCOPED_CASCADE_PARENTS) appears in BOTH the export ZIP
     and the delete cascade.  A future migration that adds a new
     org-scoped table without updating gdpr.py would silently leak
     that data on cancellation; the coverage tests catch the gap.
  2. **Org isolation** — neither path leaks rows from a sibling org.
     The highest-impact regression possible (cross-tenant data leak)
     is pinned per-table.
  3. **Cascade integrity** — IncidentEvidence rows (cascading from
     Incident) and Camera rows (cascading from CameraNode) get
     deleted when their parent does, and exported under the parent.
  4. **Idempotency** — the cascade can run twice without error;
     a Clerk webhook redelivery (which fires after the user has
     already manually wiped) must not crash.
  5. **Auth** — export is admin-only; viewers get 403.

The export ZIP shape is also pinned: manifest.json is present + has
the right metadata, every table has its own JSON file.  Re-importers
read the manifest first, so changing its schema would be a breaking
change for any external tooling built around exports.
"""

from __future__ import annotations

import io
import json
import zipfile
from datetime import UTC, datetime, timedelta

import pytest

from app.core.gdpr import (
    ORG_SCOPED_CASCADE_CHILDREN,
    ORG_SCOPED_CASCADE_PARENTS,
    ORG_SCOPED_MODELS,
    delete_org_data,
    export_org_data,
)
from app.models.models import (
    AuditLog,
    Camera,
    CameraGroup,
    CameraNode,
    EmailLog,
    EmailOutbox,
    Incident,
    IncidentEvidence,
    McpActivityLog,
    McpApiKey,
    MotionEvent,
    Notification,
    OrgMonthlyUsage,
    SentinelConfig,
    SentinelRun,
    Setting,
    StreamAccessLog,
    UserNotificationState,
)

TEST_ORG = "org_test123"
OTHER_ORG = "org_OTHER999"


# ── Fixtures ────────────────────────────────────────────────────────


@pytest.fixture
def fully_seeded_org(db):
    """Insert one row in EVERY org-scoped table for both ``TEST_ORG``
    (the caller's) and ``OTHER_ORG`` (a sibling that must NOT be
    touched by export or delete).

    Returns the count of org-scoped tables that got seeded so coverage
    tests can assert against it.  The seed shape is intentionally
    minimal — we're testing inventory completeness, not the per-row
    semantics of each table.
    """
    now = datetime.now(tz=UTC).replace(tzinfo=None)

    def _seed(org_id: str):
        # Bulk-deletable tables.
        db.add(Setting(org_id=org_id, key=f"seed_{org_id}", value="x"))
        db.add(AuditLog(org_id=org_id, event="seed", timestamp=now))
        db.add(StreamAccessLog(
            org_id=org_id, user_id=f"u_{org_id}", camera_id="cam_seed",
            node_id="node_seed", accessed_at=now,
        ))
        db.add(McpActivityLog(
            org_id=org_id, tool_name="seed", key_name="seed",
            status="ok", duration_ms=1, timestamp=now,
        ))
        db.add(McpApiKey(
            org_id=org_id, key_hash=f"hash_{org_id}", name=f"k_{org_id}",
        ))
        db.add(OrgMonthlyUsage(
            org_id=org_id, year_month="2026-05", viewer_seconds=42,
        ))
        db.add(EmailLog(
            org_id=org_id, recipient_email=f"a@{org_id}.test",
            kind="seed", status="sent", timestamp=now,
        ))
        db.add(EmailOutbox(
            org_id=org_id, recipient_email=f"b@{org_id}.test",
            subject="s", body_text="b", body_html="<p>b</p>",
            kind="seed", status="pending",
        ))
        db.add(UserNotificationState(
            clerk_user_id=f"u_{org_id}", org_id=org_id,
            last_viewed_at=now, cleared_at=now,
        ))
        db.add(Notification(
            org_id=org_id, kind="seed", title="t", body="b",
            severity="info", audience="all", created_at=now,
        ))
        db.add(MotionEvent(
            org_id=org_id, camera_id="cam_seed", node_id="node_seed",
            score=50, segment_seq=1, timestamp=now,
        ))
        db.add(CameraGroup(
            org_id=org_id, name=f"g_{org_id}", color="#000", icon="x",
        ))
        # Sentinel — agent config + run history.  Both are org-scoped
        # bulk-deletable tables; seeded so the cross-tenant isolation
        # test can verify SentinelConfig/Run rows for OTHER_ORG survive
        # a TEST_ORG delete.
        db.add(SentinelConfig(org_id=org_id))
        db.add(SentinelRun(
            id=f"run_seed_{org_id}",
            org_id=org_id,
            triggered_at=now,
            trigger_type="motion",
            camera_id="cam_seed",
            tool_call_count=2,
            outcome="no_action",
        ))

        # Cascade parents.  Incident → IncidentEvidence; CameraNode → Camera.
        incident = Incident(
            org_id=org_id, title="i", summary="s",
            severity="medium", status="open",
            created_by=f"u_{org_id}", created_at=now,
        )
        db.add(incident)
        db.flush()
        db.add(IncidentEvidence(
            incident_id=incident.id, kind="observation",
            text="seed evidence",
        ))

        node = CameraNode(
            node_id=f"n_{org_id}", org_id=org_id,
            api_key_hash=f"nh_{org_id}", name=f"node_{org_id}",
        )
        db.add(node)
        db.flush()
        db.add(Camera(
            camera_id=f"c_{org_id}", org_id=org_id,
            node_id=node.id, name=f"cam_{org_id}",
        ))

    _seed(TEST_ORG)
    _seed(OTHER_ORG)
    db.commit()


# ── Coverage: inventory matches what's actually in the schema ──────


def test_org_scoped_inventory_matches_schema(db):
    """Walk every SQLAlchemy model and assert that any table with an
    ``org_id`` column appears in either ORG_SCOPED_MODELS or
    ORG_SCOPED_CASCADE_PARENTS.  This is the regression catch for
    "someone added a new org-scoped table and forgot to update
    gdpr.py" — without this test, that table would silently
    persist on cancellation.

    Tables NOT scoped to org but containing data we deliberately
    retain (ProcessedWebhook, EmailSuppression — see gdpr.py
    docstring) are excluded from the requirement."""
    from app.models.models import Base

    inventoried = {
        m.__tablename__
        for m in (
            ORG_SCOPED_MODELS
            + ORG_SCOPED_CASCADE_PARENTS
            + ORG_SCOPED_CASCADE_CHILDREN
        )
    }

    intentionally_global = {
        "processed_webhooks",      # global Svix dedupe
        "email_suppression",       # global suppression list
    }

    for table in Base.metadata.sorted_tables:
        has_org_id = any(c.name == "org_id" for c in table.columns)
        if not has_org_id:
            continue
        assert table.name in inventoried or table.name in intentionally_global, (
            f"{table.name} has org_id but isn't in ORG_SCOPED_MODELS / "
            f"ORG_SCOPED_CASCADE_PARENTS in app/core/gdpr.py.  "
            f"Add it (or to intentionally_global if it survives "
            f"cancellation by design)."
        )


# ── delete_org_data: erases everything for the right org ──────────


def test_delete_clears_every_org_scoped_table(db, fully_seeded_org):
    """Smoke + coverage: after delete_org_data(TEST_ORG), every model
    in the inventory has zero rows for that org.  Pinned per-model
    so a new table added without inventory wiring fails loudly."""
    delete_org_data(db, TEST_ORG)
    db.commit()

    for Model in (
        ORG_SCOPED_MODELS + ORG_SCOPED_CASCADE_PARENTS + ORG_SCOPED_CASCADE_CHILDREN
    ):
        remaining = db.query(Model).filter_by(org_id=TEST_ORG).count()
        assert remaining == 0, (
            f"{Model.__tablename__} still has {remaining} rows for "
            f"{TEST_ORG} after delete_org_data — table not in cascade"
        )


def test_delete_isolates_other_orgs(db, fully_seeded_org):
    """The most important test in the file.  A regression that
    accidentally widened the filter (e.g. dropped the .filter_by org_id
    while bulk-cleaning) would wipe every customer's data on the
    next cancellation.  Pin per-table because each model applies
    the org filter independently in delete_org_data."""
    delete_org_data(db, TEST_ORG)
    db.commit()

    for Model in (
        ORG_SCOPED_MODELS + ORG_SCOPED_CASCADE_PARENTS + ORG_SCOPED_CASCADE_CHILDREN
    ):
        siblings = db.query(Model).filter_by(org_id=OTHER_ORG).count()
        assert siblings >= 1, (
            f"{Model.__tablename__} lost OTHER_ORG's row(s) when "
            f"deleting TEST_ORG — cross-tenant deletion regression"
        )


def test_delete_cascades_to_camera_and_incident_evidence(db, fully_seeded_org):
    """Cascade-parent integrity: deleting an org's CameraNode rows
    must cascade to its Camera rows; deleting Incident rows must
    cascade to IncidentEvidence rows.  Pinned because we use per-row
    session.delete (rather than bulk Query.delete) specifically to
    keep the SQLAlchemy cascade firing — a refactor that "optimised"
    to bulk-delete would break this."""
    delete_org_data(db, TEST_ORG)
    db.commit()

    # Cameras for TEST_ORG gone.
    assert db.query(Camera).filter_by(org_id=TEST_ORG).count() == 0
    # IncidentEvidence: no direct org_id, so check by walking parents.
    # Both org's incidents are gone for TEST_ORG, so any evidence
    # referencing those incident_ids should also be gone.
    evidence_count = db.query(IncidentEvidence).count()
    # OTHER_ORG's evidence row should still exist (1).
    assert evidence_count == 1, (
        f"expected exactly 1 IncidentEvidence row remaining (OTHER_ORG's), "
        f"got {evidence_count}"
    )


def test_delete_is_idempotent(db, fully_seeded_org):
    """A Clerk webhook ``organization.deleted`` redelivery (Svix retries
    on any 5xx) hits the same handler again after the data is already
    gone.  Second call must be a clean no-op, not an exception."""
    delete_org_data(db, TEST_ORG)
    db.commit()

    # Second call: zero rows to delete, no error.
    second_counts = delete_org_data(db, TEST_ORG)
    db.commit()

    assert all(v == 0 for v in second_counts.values()), (
        f"second delete returned non-zero counts: {second_counts}"
    )


def test_delete_returns_per_table_counts(db, fully_seeded_org):
    """Audit-row payload depends on the per-table counts — pin the
    return shape so a refactor doesn't quietly drop the audit detail.

    Note: cascade-child tables (``cameras`` under ``camera_nodes``)
    can read as 0 in the dict because the SQLAlchemy ORM cascade
    fires BEFORE the mop-up bulk delete runs — the bulk pass then
    finds nothing left.  Cascade-child rows ARE deleted (verified
    by ``test_delete_cascades_to_camera_and_incident_evidence``);
    this test only pins that the dict contains every inventoried
    table as a key, with > 0 counts on the bulk-deleted ones."""
    counts = delete_org_data(db, TEST_ORG)
    db.commit()

    # Every inventory table appears as a key.
    for Model in (
        ORG_SCOPED_MODELS + ORG_SCOPED_CASCADE_PARENTS + ORG_SCOPED_CASCADE_CHILDREN
    ):
        assert Model.__tablename__ in counts, (
            f"{Model.__tablename__} missing from counts dict"
        )

    # Bulk-deleted tables (everything that's not a cascade child)
    # report accurate counts.  We seeded one row per table in each
    # of the two orgs, so each bucket should be 1.
    cascade_child_tables = {m.__tablename__ for m in ORG_SCOPED_CASCADE_CHILDREN}
    for table, count in counts.items():
        if table in cascade_child_tables:
            continue  # cascade swallows the count, see docstring
        assert count >= 1, f"{table}: expected ≥ 1 deleted, got {count}"


# ── export_org_data: yields every org-scoped table ─────────────────


def test_export_yields_every_org_scoped_table(db, fully_seeded_org):
    """The export generator must produce one entry per inventoried
    table.  Coverage regression — a new model added to ORG_SCOPED_MODELS
    without exporter support would silently drop from the GDPR ZIP."""
    yielded = list(export_org_data(db, TEST_ORG))
    table_names = {name for name, _ in yielded}

    expected = {m.__tablename__ for m in ORG_SCOPED_MODELS}
    expected |= {m.__tablename__ for m in ORG_SCOPED_CASCADE_PARENTS}
    expected |= {"incident_evidence", "cameras"}  # cascading children

    missing = expected - table_names
    assert not missing, f"export missing tables: {missing}"


def test_export_isolates_other_orgs(db, fully_seeded_org):
    """Same isolation guarantee as delete: an export of TEST_ORG must
    contain ONLY TEST_ORG's rows.  Catastrophic if regressed —
    customer A would receive customer B's data in their export ZIP."""
    yielded = list(export_org_data(db, TEST_ORG))

    for table_name, rows in yielded:
        for row in rows:
            # Either the row has org_id = TEST_ORG, or it's a cascade-
            # child (cameras, incident_evidence) with no org_id field
            # but referencing a TEST_ORG parent.
            row_org = row.get("org_id")
            if row_org is not None:
                assert row_org == TEST_ORG, (
                    f"{table_name} export contains row from {row_org}, "
                    f"not {TEST_ORG} — cross-tenant export leak"
                )


def test_export_excludes_binary_blobs(db, fully_seeded_org):
    """IncidentEvidence.data (raw bytes) is excluded from the export
    to keep ZIP sizes bounded.  A regression that included blobs
    inline would balloon a year-old org's export from ~10 MB to
    ~hundreds of MB.  See the docstring in app/api/gdpr.py."""
    # Seed a row with actual blob data to make this test meaningful.
    incident = Incident(
        org_id=TEST_ORG, title="binary", summary="b",
        severity="medium", status="open", created_by="u",
        created_at=datetime.now(tz=UTC).replace(tzinfo=None),
    )
    db.add(incident)
    db.flush()
    db.add(IncidentEvidence(
        incident_id=incident.id, kind="snapshot",
        data=b"\x00" * 1024 * 1024,  # 1 MB of zeros
        data_mime="image/jpeg",
    ))
    db.commit()

    yielded = dict(export_org_data(db, TEST_ORG))
    evidence_rows = yielded["incident_evidence"]

    for row in evidence_rows:
        # ``to_dict()`` on IncidentEvidence already excludes ``data`` —
        # confirm the export respects that.
        assert "data" not in row, (
            "IncidentEvidence export contains raw blob bytes — "
            "either to_dict() or _serialize() is leaking the data field"
        )


# ── /api/gdpr/export endpoint integration ─────────────────────────


def test_export_endpoint_returns_zip(admin_client, fully_seeded_org):
    """End-to-end: POST /api/gdpr/export returns a valid ZIP with
    the right Content-Disposition header."""
    resp = admin_client.post("/api/gdpr/export")
    assert resp.status_code == 200
    assert resp.headers["content-type"].startswith("application/zip")
    assert "attachment" in resp.headers["content-disposition"]
    assert "gdpr-export" in resp.headers["content-disposition"]
    assert TEST_ORG in resp.headers["content-disposition"]
    # No-store: exports are tenant-private + sensitive.
    assert resp.headers.get("cache-control") == "no-store"

    # Body parses as a valid ZIP.
    zf = zipfile.ZipFile(io.BytesIO(resp.content))
    names = zf.namelist()
    assert "manifest.json" in names

    # Every inventoried table has its JSON file.
    for Model in (
        ORG_SCOPED_MODELS + ORG_SCOPED_CASCADE_PARENTS + ORG_SCOPED_CASCADE_CHILDREN
    ):
        expected = f"{Model.__tablename__}.json"
        assert expected in names, f"export ZIP missing {expected}"


def test_export_manifest_shape(admin_client, fully_seeded_org):
    """Pin the manifest.json schema so external re-importers built on
    today's shape don't break silently if a refactor changes it."""
    resp = admin_client.post("/api/gdpr/export")
    zf = zipfile.ZipFile(io.BytesIO(resp.content))

    manifest = json.loads(zf.read("manifest.json"))
    assert manifest["org_id"] == TEST_ORG
    assert "exported_at" in manifest
    assert manifest["format_version"] == 1
    assert "tables" in manifest
    # Tables list contains one entry per file in the ZIP.
    table_names_in_manifest = {t["name"] for t in manifest["tables"]}
    for Model in ORG_SCOPED_MODELS:
        assert Model.__tablename__ in table_names_in_manifest


def test_export_writes_audit_row(admin_client, db, fully_seeded_org):
    """Export is a high-impact action — pin that it leaves an audit
    trail.  Useful if a member ever asks "did anyone export my data?"."""
    admin_client.post("/api/gdpr/export")

    # The audit row was committed before streaming the ZIP body, so
    # it's visible immediately.
    audits = db.query(AuditLog).filter_by(
        org_id=TEST_ORG, event="gdpr_export",
    ).all()
    assert len(audits) == 1


def test_export_endpoint_admin_only(viewer_client):
    """Viewers must not be able to dump the entire org's data.
    Member-tier users wanting their own data should ask their admin."""
    resp = viewer_client.post("/api/gdpr/export")
    assert resp.status_code == 403


def test_export_endpoint_rate_limited(admin_client):
    """3/hour cap on exports — same calculus as full-reset.  An empty
    org's ZIP is still a few KB; a runaway script could otherwise
    repeatedly download multi-MB exports."""
    # Seed nothing; even an empty-org export costs work.
    for i in range(3):
        r = admin_client.post("/api/gdpr/export")
        assert r.status_code == 200, f"req #{i + 1}: {r.status_code}"

    overflow = admin_client.post("/api/gdpr/export")
    assert overflow.status_code == 429
    assert overflow.json().get("error") == "rate_limit_exceeded"


# ── full-reset endpoint coverage (gap-fill verification) ──────────


def test_full_reset_clears_every_org_scoped_table(
    admin_client, db, fully_seeded_org, monkeypatch,
):
    """The pre-fix bug: ``danger/full-reset`` only deleted 5 tables
    (Settings, Audit, Stream, MCP activity, CameraNode/Camera),
    leaving motion events / notifications / incidents / email logs /
    monthly usage / MCP keys / camera groups orphaned.  After the
    rewrite to use delete_org_data, all 14+ tables get cleared.

    This is the regression test for the original gap.  If full-reset
    drifts from the shared helper, this test catches it before the
    next customer cancellation does."""
    # full-reset requires an active paid plan + tries to send wipe_data
    # to nodes via the WS manager.  Stub the WS call so it doesn't
    # actually try to talk to a non-existent node.
    from app.api import cameras as cameras_mod

    async def _fake_send(*args, **kwargs):
        return {"status": "success"}

    # Patch require_active_paid_plan to no-op (test admin is on "pro").
    # Stub the WS manager call.
    monkeypatch.setattr(
        cameras_mod, "_require_active_paid_plan", lambda *a, **k: None,
    )
    from app.api import ws as ws_mod
    monkeypatch.setattr(ws_mod.manager, "send_command", _fake_send)

    resp = admin_client.post("/api/settings/danger/full-reset")
    assert resp.status_code == 200, resp.text

    for Model in (
        ORG_SCOPED_MODELS + ORG_SCOPED_CASCADE_PARENTS + ORG_SCOPED_CASCADE_CHILDREN
    ):
        # Special case: full-reset writes its own audit row AFTER the
        # cascade so a "what happened to my data" query has a record
        # of the reset.  That single row is expected; assert it's
        # the only one and it's the right event.
        if Model is AuditLog:
            rows = db.query(AuditLog).filter_by(org_id=TEST_ORG).all()
            assert len(rows) == 1, (
                f"AuditLog should have exactly 1 row (the reset audit) "
                f"after full-reset, got {len(rows)}"
            )
            assert rows[0].event == "full_reset"
            continue

        remaining = db.query(Model).filter_by(org_id=TEST_ORG).count()
        assert remaining == 0, (
            f"full-reset left {remaining} rows in {Model.__tablename__} "
            f"for {TEST_ORG} — cascade gap regressed"
        )

    # OTHER_ORG untouched.
    for Model in (
        ORG_SCOPED_MODELS + ORG_SCOPED_CASCADE_PARENTS + ORG_SCOPED_CASCADE_CHILDREN
    ):
        siblings = db.query(Model).filter_by(org_id=OTHER_ORG).count()
        assert siblings >= 1, (
            f"full-reset wiped OTHER_ORG's {Model.__tablename__} — "
            f"cross-tenant deletion regression"
        )
