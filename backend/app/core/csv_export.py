"""
CSV export helper for audit-log endpoints.

Why this exists in its own module:
  - Three audit endpoints (audit-logs, stream-logs, mcp/activity/logs)
    need identical CSV mechanics — shared columns logic, shared
    streaming pattern, shared response headers.  Without this module
    each endpoint would copy-paste 30 lines and they'd drift.
  - Streaming matters: org with 50k audit rows shouldn't materialise
    a 5 MB string in memory before the response goes out.  We yield
    one CSV row per query result — constant memory regardless of
    row count.

Public API:
  - ``stream_csv_response(filename, header, row_iter)`` returns a
    ``StreamingResponse`` ready to hand back from a FastAPI handler.
  - ``filename_for(prefix, org_id)`` produces a stable filename of
    the form ``audit-log-org_xxx-20260505.csv`` so a customer with
    multiple exports across days can keep them straight.

The CSV is RFC 4180 compliant: CRLF line endings, double-quoted
fields, embedded quotes escaped as ``""``.  Excel + Numbers + Sheets
all open the result without prompting for delimiter choice.
"""

from __future__ import annotations

import csv
import io
import re
from collections.abc import Iterable, Iterator
from datetime import UTC, datetime
from typing import Any

from fastapi.responses import StreamingResponse

# ── Public API ──────────────────────────────────────────────────────


def filename_for(prefix: str, org_id: str | None = None) -> str:
    """Build a stable, sortable export filename.

    Format: ``{prefix}-{org_id}-{YYYYMMDD}.csv``

    The org_id is included so a multi-org auditor (rare today, common
    once we have a partner channel) can keep exports from different
    customers straight without having to re-tag the files manually.
    The date is pure YYYYMMDD so files sort lexicographically by
    download date in any file browser.
    """
    today = datetime.now(tz=UTC).strftime("%Y%m%d")
    org_part = _safe_filename_segment(org_id) if org_id else "unknown"
    return f"{_safe_filename_segment(prefix)}-{org_part}-{today}.csv"


def stream_csv_response(
    *,
    filename: str,
    header: list[str],
    rows: Iterable[list[Any]],
) -> StreamingResponse:
    """Wrap an iterable of CSV rows in a ``StreamingResponse``.

    Parameters
    ----------
    filename
        Suggested download filename — set as the ``Content-Disposition``
        ``filename=`` attribute.  Browsers honour this when the user
        clicks a download link.  Must be a safe filename (no quotes,
        slashes, control chars) — pass through ``filename_for()`` rather
        than building one ad-hoc.
    header
        First-row column names.  Goes through the same csv.writer escape
        path as data rows so embedded commas / quotes wouldn't break
        downstream parsers.
    rows
        Iterable of row-lists.  Each list must have the same length as
        ``header``.  Generator-friendly — pass a yield-based producer to
        keep peak memory constant regardless of row count.

    Returns
    -------
    StreamingResponse with media_type=text/csv and a
    Content-Disposition header that triggers the browser save dialog.

    Notes
    -----
    Errors during iteration are logged by the generator's caller; this
    helper deliberately doesn't swallow exceptions — if the DB query
    fails mid-stream, the partial CSV download stops and the browser
    surfaces a "download failed" rather than us silently truncating.
    """

    def _generate() -> Iterator[bytes]:
        # Reuse a single StringIO buffer + csv.writer so we don't
        # re-allocate per row.  Truncate-and-rewind after each yield
        # to keep the buffer's underlying bytes scoped to one row.
        buf = io.StringIO()
        writer = csv.writer(buf, lineterminator="\r\n")

        writer.writerow(header)
        yield buf.getvalue().encode("utf-8")
        buf.seek(0)
        buf.truncate()

        for row in rows:
            writer.writerow([_defang_formula(cell) for cell in row])
            yield buf.getvalue().encode("utf-8")
            buf.seek(0)
            buf.truncate()

    safe_name = _safe_filename_segment(filename) or "export.csv"
    if not safe_name.endswith(".csv"):
        safe_name = f"{safe_name}.csv"

    return StreamingResponse(
        _generate(),
        media_type="text/csv; charset=utf-8",
        headers={
            "Content-Disposition": f'attachment; filename="{safe_name}"',
            # Don't let an intermediary cache an audit log — content is
            # specific to one user's org + has compliance implications
            # (someone else's browser / proxy seeing audit trails would
            # be a real privacy regression).
            "Cache-Control": "no-store",
        },
    )


# ── Internals ───────────────────────────────────────────────────────

# Allow letters, digits, dot, hyphen, underscore.  Anything else gets
# replaced with a hyphen so a user-supplied org_id (typically of the
# form ``org_28X...``) and a hardcoded prefix can't combine into a
# filename with a quote, slash, or control character that would break
# the Content-Disposition header.
_SAFE_RE = re.compile(r"[^A-Za-z0-9._-]")


def _safe_filename_segment(text: str) -> str:
    return _SAFE_RE.sub("-", text or "")


# Characters that make Excel / Sheets / LibreOffice treat a cell as a
# live formula (incl. DDE `=cmd|...` payloads).  Tab and CR cover the
# legacy-Excel variants that re-trigger formula parsing after a strip.
_FORMULA_PREFIXES = ("=", "+", "-", "@", "\t", "\r")


def _defang_formula(cell: Any) -> Any:
    """Neutralize CSV formula injection (OWASP) for string cells.

    Several exported columns carry caller-controlled text — MCP key
    names and node names inside audit ``details`` JSON, MCP
    ``args_summary``, Clerk-supplied emails — so a value like
    ``=HYPERLINK(...)`` or ``=cmd|' /C ...'!A0`` would execute when an
    admin opens the export in a spreadsheet.  Prefixing a single quote
    makes the spreadsheet render it as inert text (the standard
    mitigation); non-strings (ints, datetimes, None) pass through
    untouched so numeric columns keep sorting numerically.
    """
    if isinstance(cell, str) and cell.startswith(_FORMULA_PREFIXES):
        return f"'{cell}"
    return cell
