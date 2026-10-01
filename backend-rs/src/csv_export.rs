//! Streaming CSV exports for the three audit surfaces.
//!
//! Ported from `backend/app/core/csv_export.py`, which existed for the
//! same reason this module does: `/api/audit-logs`,
//! `/api/audit/stream-logs` and `/api/mcp/activity/logs` all offer
//! `?format=csv`, and without one helper each of the three would grow
//! its own copy of the quoting, the filename and the download headers
//! and they would drift.
//!
//! **This was the last thing the proxy still forwarded, and it was a
//! live regression for one commit.** All three `?format=csv` branches
//! called `proxy::forward` on the grounds that "streaming is a different
//! shape of work" — true while there was a Python to forward to, and a
//! 502 the moment there wasn't. Deleting the web tier turned three
//! documented deferrals into three broken downloads, which is the exact
//! failure mode a strangler invites if you cut before the list is empty.
//!
//! Three things here are not obvious and are all load-bearing:
//!
//! 1. **The quoting is Python's `csv.writer`, not a general CSV writer.**
//!    `QUOTE_MINIMAL` with `lineterminator="\r\n"`, which quotes a field
//!    containing a comma, a double quote, a CR or an LF and leaves every
//!    other field bare — including the empty string. A writer that
//!    quoted everything would produce a file that opens identically in
//!    every spreadsheet and still differs byte for byte from what
//!    Python sent, and these exports are archived for compliance.
//! 2. **Formula defanging applies to strings only.** `_defang_formula`
//!    guards `isinstance(cell, str)`, so `duration_ms` — an `int` in the
//!    Python row builder — is not defanged even when negative. Convert
//!    every cell to a string first and a `-5` duration would come out
//!    `'-5`, which is a real difference in an exported number column.
//!    Hence [`Cell::Raw`] for cells that were never strings.
//! 3. **The stream is a stream.** The point of the Python generator was
//!    constant memory over a 50,000-row window, so rows travel from the
//!    query to the socket through a bounded channel rather than being
//!    collected. A `Vec<Row>` here would work fine in a test and hold
//!    ~15 MB per concurrent export on a 1 GB machine whose segment cache
//!    is already spoken for.

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use sqlx::postgres::PgRow;
use sqlx::PgPool;

use crate::error::ApiError;

/// One CSV field, carrying whether Python would have defanged it.
///
/// The distinction is `isinstance(cell, str)` in `_defang_formula` and
/// nothing else: a `str` cell gets a leading apostrophe when it starts
/// with a character a spreadsheet reads as the start of a formula, and a
/// non-`str` cell is written as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    /// A `str` in the Python row builder — defanged.
    Text(String),
    /// A non-`str` (an `int`, today only `duration_ms`) — not defanged.
    Raw(String),
}

impl Cell {
    /// `value or ""` — the shape every nullable text column uses.
    pub fn text(value: Option<String>) -> Self {
        Cell::Text(value.unwrap_or_default())
    }

    /// `value if value is not None else ""`.
    ///
    /// The `""` branch yields a `str` and so is [`Cell::Text`], which
    /// makes no difference for the empty string but keeps the mapping to
    /// the Python honest.
    pub fn int(value: Option<i32>) -> Self {
        match value {
            Some(value) => Cell::Raw(value.to_string()),
            None => Cell::Text(String::new()),
        }
    }

    /// The field as Python's `csv.writer` receives it, after defanging.
    fn rendered(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Cell::Raw(value) => std::borrow::Cow::Borrowed(value),
            Cell::Text(value) => {
                if value.starts_with(FORMULA_PREFIXES) {
                    std::borrow::Cow::Owned(format!("'{value}"))
                } else {
                    std::borrow::Cow::Borrowed(value)
                }
            }
        }
    }
}

/// The characters that make a spreadsheet treat a cell as a live
/// formula, including the legacy-Excel DDE variants that re-trigger
/// parsing after a leading tab or CR is stripped.
///
/// `-` is on the list, which is why `Cell::Raw` has to exist: a negative
/// number written as text would be quoted into inertness.
const FORMULA_PREFIXES: [char; 6] = ['=', '+', '-', '@', '\t', '\r'];

/// Rows are handed to the response one at a time, with the query's
/// errors in band so a failure mid-export truncates the download
/// instead of being swallowed.
type RowResult = Result<Vec<Cell>, sqlx::Error>;
type RowStream = std::pin::Pin<Box<dyn futures_util::Stream<Item = RowResult> + Send>>;

/// How much CSV to accumulate before handing a chunk to the socket.
///
/// The Python yielded one chunk per row, and chunk boundaries are not
/// observable to a client — the reassembled body is identical either
/// way. Batching trades a fixed 8 KiB for one write per ~40 rows rather
/// than one per row, which matters at 50,000 rows.
const CHUNK_TARGET_BYTES: usize = 8 * 1024;

/// How many rows may sit between the query and the socket.
///
/// Bounded, so a client reading slowly (or not at all) applies
/// back-pressure to the query rather than letting rows pile up in
/// memory — which is the whole reason the Python streamed.
const ROW_BUFFER: usize = 500;

/// Run `sql` and map each row into CSV cells, as a stream.
///
/// The query runs in its own task owning a pool handle, because the
/// response body must be `'static` and `Executor` borrows the pool. The
/// task stops as soon as the receiver is dropped, so a cancelled
/// download does not keep streaming rows into nothing.
///
/// Every bind is a `String` — all three call sites bind an org id and
/// text filters — which keeps this monomorphic rather than generic over
/// a heterogeneous bind list for no present gain.
pub fn stream_rows(
    pool: PgPool,
    sql: String,
    binds: Vec<String>,
    map: fn(&PgRow) -> RowResult,
) -> RowStream {
    let (tx, rx) = tokio::sync::mpsc::channel::<RowResult>(ROW_BUFFER);
    tokio::spawn(async move {
        let mut query = sqlx::query(&sql);
        for bind in &binds {
            query = query.bind(bind);
        }
        let mut rows = query.fetch(&pool);
        while let Some(row) = rows.next().await {
            let item = row.and_then(|row| map(&row));
            let failed = item.is_err();
            // A send error means the client hung up; stop reading rather
            // than draining the rest of the window into a dead channel.
            if tx.send(item).await.is_err() {
                break;
            }
            if failed {
                break;
            }
        }
    });
    Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }))
}

/// `{prefix}-{org_id}-{YYYYMMDD}.csv`, with both segments sanitised.
///
/// The date is UTC and bare, so a browser's download folder sorts a
/// month of exports chronologically. A missing org id becomes
/// `unknown` — unreachable on an authenticated route, and reproduced
/// rather than asserted because a filename is not the place to fail.
pub fn filename_for(prefix: &str, org_id: Option<&str>) -> String {
    filename_for_at(prefix, org_id, chrono::Utc::now())
}

/// [`filename_for`] with the clock injected, for the tests.
pub fn filename_for_at(
    prefix: &str,
    org_id: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let date = now.format("%Y%m%d");
    let org = match org_id {
        Some(org) => safe_segment(org),
        None => "unknown".to_string(),
    };
    format!("{}-{org}-{date}.csv", safe_segment(prefix))
}

/// `_SAFE_RE.sub("-", text)`, where `_SAFE_RE` is `[^A-Za-z0-9._-]`.
///
/// One replacement per *code point*, not per byte, because the Python
/// substitutes over a `str`. This is what keeps a quote or a slash out
/// of the `Content-Disposition` header: an org id is Clerk's string, not
/// ours, and `HeaderValue::from_str` would reject a control character
/// here and turn an export into a 500.
///
/// Shared with the GDPR export, which in Python called `filename_for`
/// for exactly this reason.
pub fn safe_segment(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Append one CSV record, `csv.writer(lineterminator="\r\n")`-style.
///
/// Quoting is `QUOTE_MINIMAL`: a field is quoted when it contains the
/// delimiter, the quote character, or either half of the line
/// terminator, and an embedded quote is doubled. Everything else,
/// including the empty string, is written bare.
fn write_row<'a>(out: &mut String, cells: impl IntoIterator<Item = &'a Cell>) {
    let cells: Vec<&Cell> = cells.into_iter().collect();
    let single = cells.len() == 1;
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let field = cell.rendered();
        // `_csv.c` quotes an empty field when it is the only field in
        // the record, because `\r\n` on its own would read back as an
        // empty record rather than a record holding one empty field. No
        // export here is single-column, but a helper that gets this
        // wrong is a trap for the one that eventually is.
        let needs_quotes = (single && field.is_empty()) || field.contains([',', '"', '\r', '\n']);
        if needs_quotes {
            out.push('"');
            for ch in field.chars() {
                if ch == '"' {
                    out.push('"');
                }
                out.push(ch);
            }
            out.push('"');
        } else {
            out.push_str(&field);
        }
    }
    out.push_str("\r\n");
}

/// Serialise `header` plus `rows` as a streaming CSV download.
///
/// The header goes through the same quoting path as the data — so a
/// column name containing a comma could not break a parser — but is
/// **not** defanged, matching `writer.writerow(header)` on a list of
/// plain `str`.
pub fn stream_csv_response(
    filename: &str,
    header: &[&str],
    rows: RowStream,
) -> Result<Response, ApiError> {
    // `Content-Disposition` carries the name, so a name that cannot be a
    // header value has to fail before the body starts rather than
    // half-way through a download.
    let name = {
        let safe = safe_segment(filename);
        let safe = if safe.is_empty() {
            "export.csv".to_string()
        } else {
            safe
        };
        if safe.ends_with(".csv") {
            safe
        } else {
            format!("{safe}.csv")
        }
    };
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{name}\""))
        .map_err(|_| ApiError::internal("filename is not a header value"))?;

    let mut first = String::new();
    let header_cells: Vec<Cell> = header.iter().map(|h| Cell::Raw((*h).to_string())).collect();
    write_row(&mut first, header_cells.iter());

    struct Chunker {
        pending: Option<Bytes>,
        rows: RowStream,
        done: bool,
    }
    let state = Chunker {
        pending: Some(Bytes::from(first)),
        rows,
        done: false,
    };

    let body = futures_util::stream::unfold(state, |mut state| async move {
        if let Some(chunk) = state.pending.take() {
            return Some((Ok(chunk), state));
        }
        if state.done {
            return None;
        }
        let mut buf = String::new();
        while buf.len() < CHUNK_TARGET_BYTES {
            match state.rows.next().await {
                Some(Ok(cells)) => write_row(&mut buf, cells.iter()),
                Some(Err(err)) => {
                    // Deliberately not swallowed, as the Python's
                    // docstring insisted: the download stops and the
                    // browser reports a failure, rather than handing an
                    // auditor a file that is silently short.
                    tracing::error!(error = %err, "csv export failed mid-stream");
                    state.done = true;
                    return Some((Err(std::io::Error::other(err)), state));
                }
                None => {
                    state.done = true;
                    break;
                }
            }
        }
        if buf.is_empty() {
            None
        } else {
            Some((Ok(Bytes::from(buf)), state))
        }
    });

    // Starlette writes the explicit headers first and appends the
    // derived content-type; a streaming response carries no
    // content-length.
    let mut out = HeaderMap::new();
    out.insert(header::CONTENT_DISPOSITION, disposition);
    // An audit export is one org's compliance data. A proxy or a shared
    // browser cache holding it would be a real privacy regression.
    out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    Ok((out, Body::from_stream(body)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(cells: Vec<Cell>) -> String {
        let mut out = String::new();
        write_row(&mut out, cells.iter());
        out
    }

    /// The baseline: minimal quoting and CRLF, which is what makes the
    /// file byte-identical to what Python sent.
    #[test]
    fn plain_fields_are_not_quoted() {
        assert_eq!(
            row(vec![
                Cell::Text("a".into()),
                Cell::Text(String::new()),
                Cell::Raw("7".into())
            ]),
            "a,,7\r\n"
        );
    }

    /// The three characters that force quotes, and the doubling rule.
    #[test]
    fn only_the_minimal_set_forces_quotes() {
        assert_eq!(row(vec![Cell::Text("a,b".into())]), "\"a,b\"\r\n");
        assert_eq!(
            row(vec![Cell::Text("say \"hi\"".into())]),
            "\"say \"\"hi\"\"\"\r\n"
        );
        assert_eq!(
            row(vec![Cell::Text("two\nlines".into())]),
            "\"two\nlines\"\r\n"
        );
        assert_eq!(row(vec![Cell::Text("cr\rhere".into())]), "\"cr\rhere\"\r\n");
        // Not on the list: a semicolon or a tab mid-field stays bare.
        assert_eq!(row(vec![Cell::Text("a;b\tc".into())]), "a;b\tc\r\n");
    }

    /// `_csv.c`'s one special case. Nothing here exports a single column
    /// today, which is exactly why it would go unnoticed.
    #[test]
    fn a_lone_empty_field_is_quoted() {
        assert_eq!(row(vec![Cell::Text(String::new())]), "\"\"\r\n");
        // Two empty fields are not — `,\r\n` already reads back as two.
        assert_eq!(
            row(vec![Cell::Text(String::new()), Cell::Text(String::new())]),
            ",\r\n"
        );
    }

    /// The injection mitigation, on the cells that carry caller text.
    #[test]
    fn formula_leaders_are_defanged_in_strings() {
        for leader in ["=", "+", "-", "@", "\t", "\r"] {
            let cell = Cell::Text(format!("{leader}cmd|' /C calc'!A0"));
            assert!(
                cell.rendered().starts_with('\''),
                "{leader:?} was not defanged"
            );
        }
        assert_eq!(Cell::Text("plain".into()).rendered(), "plain");
    }

    /// The reason `Cell::Raw` exists. A negative integer column is not a
    /// formula, and Python never defanged it because it was not a `str`.
    #[test]
    fn integers_are_never_defanged() {
        assert_eq!(Cell::int(Some(-5)).rendered(), "-5");
        assert_eq!(Cell::int(Some(120)).rendered(), "120");
        assert_eq!(Cell::int(None).rendered(), "");
        // And the same value arriving as text still is.
        assert_eq!(Cell::Text("-5".into()).rendered(), "'-5");
    }

    /// A defanged field that also needs quotes gets both, in that order.
    #[test]
    fn defanging_happens_before_quoting() {
        assert_eq!(
            row(vec![Cell::Text("=SUM(A1,A2)".into())]),
            "\"'=SUM(A1,A2)\"\r\n"
        );
    }

    /// The oracle test: a corpus whose expected bytes were produced by
    /// the **real** `csv.writer`, with `_defang_formula` copied verbatim
    /// from the module this one replaces.
    ///
    /// Every other test here asserts my reading of `QUOTE_MINIMAL`. This
    /// one asserts Python's behaviour, and it moved two of those
    /// readings from "believed" to "checked": a tab mid-field or leading
    /// a field is *not* quoted (only the two line-terminator halves
    /// are), and an empty field is bare unless it is the whole record.
    /// Regenerate with `scratchpad/csv_oracle.py` if the corpus grows.
    #[test]
    fn the_bytes_match_pythons_csv_writer() {
        let corpus: Vec<Vec<Cell>> = vec![
            vec![
                Cell::Text("a".into()),
                Cell::Text("b".into()),
                Cell::Text("c".into()),
            ],
            vec![Cell::text(None), Cell::text(None), Cell::text(None)],
            vec![
                Cell::Text("a,b".into()),
                Cell::Text("say \"hi\"".into()),
                Cell::Text("two\nlines".into()),
            ],
            vec![
                Cell::Text("cr\rhere".into()),
                Cell::Text("semi;colon".into()),
                Cell::Text("tab\there".into()),
            ],
            vec![
                Cell::Text("=SUM(A1,A2)".into()),
                Cell::Text("+1".into()),
                Cell::Text("-5".into()),
            ],
            vec![
                Cell::Text("@now".into()),
                Cell::Text("\tlead".into()),
                Cell::Text("\rlead".into()),
            ],
            // The int row: 42 and -5 arrive as ints in the Python row
            // builder and are not defanged, unlike the "-5" above.
            vec![
                Cell::Text("2026-05-05T00:00:00".into()),
                Cell::int(Some(42)),
                Cell::int(Some(-5)),
            ],
            vec![
                Cell::Text(String::new()),
                Cell::int(Some(0)),
                Cell::int(None),
            ],
            vec![
                Cell::Text("=\"a,b\"".into()),
                Cell::Text("\"".into()),
                Cell::Text("\"\"".into()),
            ],
            vec![
                Cell::Text("héllo — ünicode".into()),
                Cell::Text("ok".into()),
                Cell::Text(String::new()),
            ],
            vec![Cell::Text("solo".into())],
            vec![Cell::Text(String::new())],
            vec![
                Cell::Text("a\\b".into()),
                Cell::Text("end\r\n".into()),
                Cell::Text("\n".into()),
            ],
        ];

        let mut out = String::new();
        write_row(
            &mut out,
            [
                Cell::Raw("c1".into()),
                Cell::Raw("c2".into()),
                Cell::Raw("c3".into()),
            ]
            .iter(),
        );
        for row in &corpus {
            write_row(&mut out, row.iter());
        }

        // Verbatim from `csv.writer(lineterminator="\r\n")`.
        let expected = concat!(
            "c1,c2,c3\r\n",
            "a,b,c\r\n",
            ",,\r\n",
            "\"a,b\",\"say \"\"hi\"\"\",\"two\nlines\"\r\n",
            "\"cr\rhere\",semi;colon,tab\there\r\n",
            "\"'=SUM(A1,A2)\",'+1,'-5\r\n",
            "'@now,'\tlead,\"'\rlead\"\r\n",
            "2026-05-05T00:00:00,42,-5\r\n",
            ",0,\r\n",
            "\"'=\"\"a,b\"\"\",\"\"\"\",\"\"\"\"\"\"\r\n",
            "héllo — ünicode,ok,\r\n",
            "solo\r\n",
            "\"\"\r\n",
            "a\\b,\"end\r\n\",\"\n\"\r\n",
        );
        assert_eq!(out, expected);
    }

    #[test]
    fn filenames_are_dated_and_sanitised() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-05T23:59:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            filename_for_at("audit-log", Some("org_28Xabc"), now),
            "audit-log-org_28Xabc-20260505.csv"
        );
        // A hostile org id cannot escape the quoted header value.
        assert_eq!(
            filename_for_at("audit-log", Some("a\"b/../c\n"), now),
            "audit-log-a-b-..-c--20260505.csv"
        );
        assert_eq!(
            filename_for_at("audit-log", None, now),
            "audit-log-unknown-20260505.csv"
        );
    }

    /// The header row is quoted like any other row but never defanged.
    #[tokio::test]
    async fn the_response_leads_with_the_header() {
        let rows: RowStream = Box::pin(futures_util::stream::iter(vec![
            Ok(vec![
                Cell::Text("2026-05-05T00:00:00".into()),
                Cell::int(Some(3)),
            ]),
            Ok(vec![Cell::Text("=evil".into()), Cell::int(None)]),
        ]));
        let response =
            stream_csv_response("audit-log-org_1-20260505.csv", &["timestamp", "n"], rows).unwrap();

        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/csv; charset=utf-8"
        );
        assert_eq!(
            response.headers().get(header::CONTENT_DISPOSITION).unwrap(),
            "attachment; filename=\"audit-log-org_1-20260505.csv\""
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        // A streamed body has no content-length, which is what lets the
        // export start before the query has finished.
        assert!(response.headers().get(header::CONTENT_LENGTH).is_none());

        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8(body.to_vec()).unwrap(),
            "timestamp,n\r\n2026-05-05T00:00:00,3\r\n'=evil,\r\n"
        );
    }

    /// A name without the extension gets one; an unusable one is
    /// replaced rather than allowed to fail the response.
    #[tokio::test]
    async fn the_download_name_is_always_usable() {
        for (given, expected) in [
            ("export", "export.csv"),
            ("a\"b.csv", "a-b.csv"),
            ("", "export.csv"),
        ] {
            let rows: RowStream = Box::pin(futures_util::stream::iter(vec![]));
            let response = stream_csv_response(given, &["a"], rows).unwrap();
            assert_eq!(
                response.headers().get(header::CONTENT_DISPOSITION).unwrap(),
                &format!("attachment; filename=\"{expected}\"")[..],
                "for {given:?}"
            );
        }
    }

    /// An empty window is a file with a header and no rows, not an empty
    /// file: an auditor opening it should see the columns and conclude
    /// "nothing matched", not "the export broke".
    #[tokio::test]
    async fn an_empty_export_still_has_its_header() {
        let rows: RowStream = Box::pin(futures_util::stream::iter(vec![]));
        let response = stream_csv_response("x.csv", &["a", "b"], rows).unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(String::from_utf8(body.to_vec()).unwrap(), "a,b\r\n");
    }

    /// Rows before a mid-stream failure are delivered; the body then
    /// ends in an error rather than a clean EOF, so the client sees a
    /// failed download instead of a short file it would trust.
    #[tokio::test]
    async fn a_failure_mid_stream_truncates_rather_than_swallows() {
        let rows: RowStream = Box::pin(futures_util::stream::iter(vec![
            Ok(vec![Cell::Text("first".into())]),
            Err(sqlx::Error::RowNotFound),
            Ok(vec![Cell::Text("never".into())]),
        ]));
        let response = stream_csv_response("x.csv", &["a"], rows).unwrap();
        let err = axum::body::to_bytes(response.into_body(), 1 << 20).await;
        assert!(
            err.is_err(),
            "the body should have failed, not ended cleanly"
        );
    }

    /// Batching is invisible in the bytes, which is the property that
    /// lets the chunk size be a tuning decision rather than a contract.
    #[tokio::test]
    async fn chunking_does_not_change_the_bytes() {
        let items: Vec<RowResult> = (0..2_000)
            .map(|i| Ok(vec![Cell::Text(format!("row-{i}")), Cell::int(Some(i))]))
            .collect();
        let rows: RowStream = Box::pin(futures_util::stream::iter(items));
        let response = stream_csv_response("x.csv", &["name", "n"], rows).unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        let lines: Vec<&str> = text.split_terminator("\r\n").collect();
        assert_eq!(lines.len(), 2_001);
        assert_eq!(lines[0], "name,n");
        assert_eq!(lines[1], "row-0,0");
        assert_eq!(lines[2_000], "row-1999,1999");
    }
}
