-- Rows designed to break a CSV writer, for csv_diff.py.
--
-- The three `?format=csv` exports were the last routes to leave the
-- proxy, and they were ported AFTER the Python was deleted — so unlike
-- every other slice they had no running reference at the time. This
-- fixture is what makes one possible again against a worktree of the
-- pre-cut commit: see csv_run.sh.
--
-- Every value here is chosen because it changes the bytes:
--
--   * a comma, a double quote, an LF and a CR each force quoting, and
--     `csv.writer` quotes on exactly those four and nothing else — a tab
--     or a semicolon must stay bare;
--   * the six formula leaders (= + - @ TAB CR) are what `_defang_formula`
--     prefixes with an apostrophe, and they are reachable from caller
--     text: MCP key names, node names inside audit `details`, Clerk
--     emails;
--   * a NEGATIVE duration_ms, which is the one cell that must NOT be
--     defanged — it is an int in the Python row builder, so `-5` stays
--     `-5` where the string "-5" becomes `'-5`. Nothing but a column of
--     the right type can catch that;
--   * NULLs in every nullable column, which render as bare empty fields;
--   * non-ASCII, so the UTF-8 body length is not the character count.
--
-- Timestamps are distinct to the microsecond ON PURPOSE. All three
-- routes order by a timestamp with no tiebreak, so two rows sharing one
-- make the page non-deterministic and any difference a flake —
-- http_run.sh fails outright on a tie for that reason.

ALTER SEQUENCE IF EXISTS audit_log_id_seq RESTART WITH 1;
ALTER SEQUENCE IF EXISTS stream_access_logs_id_seq RESTART WITH 1;
ALTER SEQUENCE IF EXISTS mcp_activity_logs_id_seq RESTART WITH 1;

DELETE FROM audit_log;
DELETE FROM stream_access_logs;
DELETE FROM mcp_activity_logs;

-- ── audit_log ──────────────────────────────────────────────────────
-- Columns exported: timestamp, event, username, user_id, ip_address,
-- details.
INSERT INTO audit_log (org_id, event, user_id, username, ip_address, details, timestamp) VALUES
  ('self-host', 'login',  'u-1', 'admin',        '10.0.0.1',
   '{"note": "plain"}',                      '2026-01-02 03:04:05.000001'),
  -- A comma and a quote inside the details JSON, which is the widest
  -- caller-influenced column on this route.
  ('self-host', 'login',  'u-2', 'a,b',          '10.0.0.2',
   '{"name": "he said \"hi\", loudly"}',     '2026-01-02 03:04:05.000002'),
  -- Embedded newline and carriage return.
  ('self-host', 'wipe',   'u-3', E'two\nlines',  '10.0.0.3',
   E'cr\rinside',                            '2026-01-02 03:04:05.000003'),
  -- Every formula leader, one per row, in the username.
  ('self-host', 'export', 'u-4', '=SUM(A1,A2)',  NULL,
   '=cmd|'' /C calc''!A0',                   '2026-01-02 03:04:05.000004'),
  ('self-host', 'export', 'u-5', '+1',           NULL,
   '@SUM(1)',                                '2026-01-02 03:04:05.000005'),
  ('self-host', 'export', 'u-6', '-5',           NULL,
   E'\tleading tab',                         '2026-01-02 03:04:05.000006'),
  -- NULLs in every nullable column.
  ('self-host', 'rotate', NULL,  NULL,           NULL,
   NULL,                                     '2026-01-02 03:04:05.000007'),
  -- Non-ASCII, and a LIKE wildcard in the username so the escaping is
  -- exercised by `?username=%` and `?username=_`.
  ('self-host', 'login',  'u-8', 'héllo_wörld%', '::1',
   '{"unicode": "— ünicode —"}',             '2026-01-02 03:04:05.000008'),
  -- Another org, to prove the export is tenant-scoped. If this leaks
  -- into either body the diff says so, and if it leaks into BOTH the
  -- row count assertion in csv_diff.py does.
  ('other-org', 'login',  'x-1', 'intruder',     '10.9.9.9',
   '{"note": "must not appear"}',            '2026-01-02 03:04:05.000009');

-- ── stream_access_logs ─────────────────────────────────────────────
-- Exported: accessed_at, camera_id, node_id, user_email, user_id,
-- ip_address.
INSERT INTO stream_access_logs (org_id, user_id, user_email, camera_id, node_id, ip_address, accessed_at) VALUES
  ('self-host', 'u-1', 'admin@example.com', 'cam-1', 'node-1', '10.0.0.1',
   '2026-01-03 03:04:05.000001'),
  ('self-host', 'u-2', 'a,b@example.com',   'cam-1', 'node-1', '10.0.0.2',
   '2026-01-03 03:04:05.000002'),
  ('self-host', 'u-3', '=evil@example.com', 'cam-2', 'node-1', NULL,
   '2026-01-03 03:04:05.000003'),
  -- A NULL email, which the JSON path renders as "" and the CSV as a
  -- bare empty field.
  ('self-host', 'u-4', NULL,                'cam-2', 'node-2', '::1',
   '2026-01-03 03:04:05.000004'),
  ('other-org', 'x-1', 'intruder@example.com', 'cam-9', 'node-9', '10.9.9.9',
   '2026-01-03 03:04:05.000005');

-- ── mcp_activity_logs ──────────────────────────────────────────────
-- Exported: timestamp, tool_name, key_name, status, duration_ms,
-- args_summary, error. `duration_ms` is the only non-text cell any of
-- the three exports has.
INSERT INTO mcp_activity_logs (org_id, tool_name, key_name, status, duration_ms, args_summary, error, timestamp) VALUES
  ('self-host', 'list_cameras', 'ci_robot', 'success', 12,
   '{}', NULL,                              '2026-01-04 03:04:05.000001'),
  ('self-host', 'view_camera',  'ci_robot', 'error',   0,
   'camera_id=cam-1', E'boom,\n"quoted"',   '2026-01-04 03:04:05.000002'),
  -- The negative duration. `-5` is a formula leader as text and is not
  -- one as a number; this row is the whole reason `Cell::Raw` exists.
  ('self-host', 'watch_camera', '=formula', 'error',   -5,
   '=HYPERLINK("http://x")', '-1 is not a formula',
                                            '2026-01-04 03:04:05.000003'),
  -- NULL duration, which renders as an empty field rather than 0.
  ('self-host', 'get_camera',   'prod_main', 'success', NULL,
   NULL, NULL,                              '2026-01-04 03:04:05.000004'),
  ('other-org', 'list_cameras', 'intruder',  'success', 7,
   '{}', NULL,                              '2026-01-04 03:04:05.000005');
