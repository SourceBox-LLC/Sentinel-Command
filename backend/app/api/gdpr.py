"""
GDPR endpoints — Article 20 (data portability) export.

Article 17 (right to erasure) is served by the existing
``POST /api/settings/danger/full-reset`` endpoint, which now
routes through the shared ``app.core.gdpr.delete_org_data``
helper so both customer-initiated and operator-initiated
deletes produce identical end-states.

The export endpoint here is admin-only — only an org admin can
request a full data dump, since the export contains every member's
notification + audit history for the org.  Member-tier users who
want their personal data should ask their admin (the audit log
shows who exported what + when via the ``write_audit`` row).

Format: streaming ZIP of one JSON file per org-scoped table, plus
a ``manifest.json`` with metadata.  JSON over CSV because:

  - Per Article 20, the format must be "structured, commonly used,
    machine-readable" — JSON satisfies all three explicitly named
    in the GDPR Working Party 29 guidance.
  - Schema-agnostic — a re-importer can map fields by name without
    inferring types from CSV strings.
  - Round-trips cleanly with our internal ``to_dict()`` shapes,
    which are the same shapes the API returns elsewhere.

What's excluded from the export:

  - Recordings.  These live on the customer's CameraNode, not on
    Command Center — same reason they're absent from
    ``organization.deleted`` cleanup.
  - IncidentEvidence binary blobs (snapshots / clip captures).
    Metadata IS exported; the bytes are fetchable via the
    existing ``/api/incidents/{id}/evidence/{eid}`` URL during
    the customer's portability window.  This keeps the ZIP
    under sane size bounds (a single org with a few clip
    captures could otherwise produce a 100+ MB download).
"""

from __future__ import annotations

import io
import json
import logging
import zipfile
from collections.abc import Iterator
from datetime import UTC, datetime

from fastapi import APIRouter, Depends, Request
from fastapi.responses import StreamingResponse
from sqlalchemy.orm import Session

from app.core.audit import audit_label, write_audit
from app.core.auth import AuthUser, require_admin
from app.core.csv_export import filename_for
from app.core.database import get_db
from app.core.gdpr import export_org_data
from app.core.limiter import limiter

logger = logging.getLogger(__name__)

router = APIRouter(prefix="/api/gdpr", tags=["gdpr"])


# ── Endpoints ──────────────────────────────────────────────────────


@router.post("/export")
@limiter.limit("3/hour")
async def export_organization_data(
    request: Request,
    user: AuthUser = Depends(require_admin),
    db: Session = Depends(get_db),
):
    """GDPR Article 20 — full organization data export as a ZIP.

    Admin-only.  Rate-limited to 3 requests/hour per org so a
    runaway script can't repeatedly dump multi-megabyte exports
    (each ZIP can be sizeable for a year-old org).  3/hour matches
    the cap on the sibling ``danger/full-reset`` endpoint — both
    are operator-rare, high-impact actions.

    Audited.  Every export call writes an ``audit_log`` row with
    the requesting admin + timestamp so the org has a record of
    "when did someone download our entire data history".  Useful
    if a member ever asks "did anyone export my data?" — the
    audit row shows who clicked + when.
    """
    timestamp = datetime.now(tz=UTC)
    filename = filename_for("gdpr-export", user.org_id).replace(".csv", ".zip")

    # Audit BEFORE streaming starts.  If we audited after, a
    # browser disconnect mid-stream would give us no record that
    # the export was attempted.  At-least-once-audited >
    # at-most-once-audited for compliance signals.
    write_audit(
        db,
        org_id=user.org_id,
        event="gdpr_export",
        user_id=user.user_id,
        username=audit_label(user),
        details={"filename": filename},
        request=request,
    )
    db.commit()

    return StreamingResponse(
        _build_zip_stream(db, user.org_id, timestamp),
        media_type="application/zip",
        headers={
            "Content-Disposition": f'attachment; filename="{filename}"',
            # Same no-store as the audit CSV exports — exports contain
            # tenant-private data and must not be cached by any
            # intermediary (browser, CDN, corporate proxy).
            "Cache-Control": "no-store",
        },
    )


# ── ZIP streaming ──────────────────────────────────────────────────


def _build_zip_stream(
    db: Session, org_id: str, exported_at: datetime,
) -> Iterator[bytes]:
    """Yield bytes of a ZIP archive containing one JSON file per
    org-scoped table.

    The archive is assembled fully in memory and yielded once after
    ``ZipFile`` closes.  It MUST NOT be drained mid-archive: ZipFile
    records each member's offset via the buffer's absolute position
    (``start_dir``), so a seek(0)+truncate() between members makes it
    seek back past the drained region, zero-fill the gap, and bake
    offsets into the central directory that don't match the
    concatenated stream.  The resulting ZIP lists fine (the central
    directory at the tail is intact) and its LAST member reads fine —
    but every earlier member fails with "Bad magic number for file
    header".  That exact corruption shipped once; the regression test
    now reads back every member, not just the manifest.

    Memory profile: each table is one JSON file; with our retention
    windows (motion events tiered at 30/90/365 days) a year-old org
    tops out around 10-50 MB — bounded by retention, so whole-archive
    buffering is fine.  If an org ever outgrows RAM, swap in a
    sequential-write streaming zipper (e.g. zipstream-ng) rather than
    reintroducing a drain.
    """
    buf = io.BytesIO()

    # ZIP_DEFLATED keeps the download size sane — JSON compresses
    # ~10x.  ZipFile in 'w' mode + close()'ing it writes the
    # central directory, which is what makes the file readable.
    with zipfile.ZipFile(buf, mode="w", compression=zipfile.ZIP_DEFLATED) as zf:

        manifest = {
            "org_id": org_id,
            "exported_at": exported_at.isoformat(),
            "format_version": 1,
            "spec": "GDPR Article 20 — data portability export",
            "tables": [],
            "excluded": {
                "recordings": (
                    "Local to your CameraNode device. Not stored on "
                    "Command Center. Use the CameraNode TUI to export."
                ),
                "incident_evidence_blobs": (
                    "Metadata exported here as 'incident_evidence.json'. "
                    "Binary bytes available per-evidence via "
                    "GET /api/incidents/{id}/evidence/{eid} during "
                    "your portability window."
                ),
            },
        }

        # Write tables one at a time into the in-memory archive.
        # No draining between members — see the docstring for why
        # that corrupts the offsets.
        table_count = 0
        row_count = 0
        for table_name, rows in export_org_data(db, org_id):
            payload = json.dumps(rows, indent=2, default=str)
            zf.writestr(f"{table_name}.json", payload)
            manifest["tables"].append({
                "name": table_name,
                "rows": len(rows),
                "filename": f"{table_name}.json",
            })
            table_count += 1
            row_count += len(rows)

        # Manifest goes LAST so it can include the row counts.
        # Some re-importers (and human auditors) read the manifest
        # first to plan the import; that's a UX nicety vs a
        # correctness requirement.
        zf.writestr("manifest.json", json.dumps(manifest, indent=2))

        logger.info(
            "[GDPR] export org=%s tables=%d rows=%d",
            org_id, table_count, row_count,
        )

    # zipfile finalises the central directory on context-manager
    # exit.  Yield the complete, internally-consistent archive in
    # one chunk (StreamingResponse still streams it to the socket).
    yield buf.getvalue()
