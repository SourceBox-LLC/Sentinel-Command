-- Fixture data for the slice-2 HTTP differential.
--
-- Timestamps are relative to now() on purpose: `effective_status` turns
-- a camera offline after 90 seconds without a heartbeat, so a fixture
-- seeded once and reused an hour later exercises only the offline path.
-- An earlier run of this harness did exactly that and reported 31/31
-- identical while never testing a live camera. Re-run this file
-- immediately before diffing.

-- Every sequence is RESTARTed so row ids are deterministic across
-- reseeds. The write differential compares ids directly, and a sequence
-- that keeps climbing makes every case look like a side-effect diff —
-- which is exactly what it did before this block existed.
ALTER SEQUENCE IF EXISTS cameras_id_seq RESTART WITH 1;
ALTER SEQUENCE IF EXISTS camera_nodes_id_seq RESTART WITH 1;
ALTER SEQUENCE IF EXISTS camera_groups_id_seq RESTART WITH 1;
ALTER SEQUENCE IF EXISTS settings_id_seq RESTART WITH 1;
ALTER SEQUENCE IF EXISTS audit_log_id_seq RESTART WITH 1;
ALTER SEQUENCE IF EXISTS stream_access_logs_id_seq RESTART WITH 1;
ALTER SEQUENCE IF EXISTS motion_events_id_seq RESTART WITH 1;
ALTER SEQUENCE IF EXISTS mcp_activity_logs_id_seq RESTART WITH 1;
ALTER SEQUENCE IF EXISTS incidents_id_seq RESTART WITH 1;
ALTER SEQUENCE IF EXISTS incident_evidence_id_seq RESTART WITH 1;

DELETE FROM cameras;
DELETE FROM camera_groups;
DELETE FROM camera_nodes;

INSERT INTO camera_nodes (node_id, org_id, api_key_hash, name, status) VALUES
  -- sha256("test-node-key"). A real hash, so streaming_diff.py can open an
  -- authenticated WebSocket; the other two keep a placeholder.
  ('node-aaaa1111', 'self-host',
   'f3702f9692e7bce4e7dc0b10fe460daf0bdc6c2c741d4c2fabcdb6df44dbb4c9',
   'Front Yard Pi', 'online'),
  ('node-bbbb2222', 'self-host', 'x', 'Garage Pi',     'offline'),
  ('node-cccc3333', 'other-org', 'x', 'Someone Else',  'online');

INSERT INTO camera_groups (org_id, name, color, icon) VALUES
  ('self-host', 'Outdoor', '#22c55e', '🌳'),
  ('self-host', 'Indoor',  '#3b82f6', '🏠'),
  ('self-host', 'Empty Group', NULL, NULL),
  ('other-org', 'Their Group', '#ff0000', '🔒');

INSERT INTO cameras
  (camera_id, org_id, node_id, name, node_type, capabilities, group_id, last_seen, status, last_error,
   disabled_by_plan, continuous_24_7, scheduled_recording, scheduled_start, scheduled_end)
SELECT * FROM (VALUES
  -- live, fully populated, in a group, on a node
  ('cam-live',      'self-host', (SELECT id FROM camera_nodes WHERE node_id='node-aaaa1111'), 'Driveway', 'rtsp', 'streaming,motion',
     (SELECT id FROM camera_groups WHERE name='Outdoor'), now()::timestamp - interval '5 seconds', 'streaming', NULL, false, true, false, NULL, NULL),
  -- stale heartbeat -> effective_status must flip to offline
  ('cam-stale',     'self-host', (SELECT id FROM camera_nodes WHERE node_id='node-aaaa1111'), 'Side Gate', 'rtsp', 'streaming',
     (SELECT id FROM camera_groups WHERE name='Outdoor'), now()::timestamp - interval '200 seconds', 'streaming', NULL, false, false, false, NULL, NULL),
  -- just inside the 90s window (comparison is `> 90`, so this stays live)
  ('cam-boundary',  'self-host', (SELECT id FROM camera_nodes WHERE node_id='node-aaaa1111'), 'Boundary', 'rtsp', 'streaming',
     NULL, now()::timestamp - interval '85 seconds', 'streaming', NULL, false, false, false, NULL, NULL),
  -- broken and recent: last_error must be surfaced
  ('cam-failed',    'self-host', (SELECT id FROM camera_nodes WHERE node_id='node-bbbb2222'), 'Garage', 'usb', 'streaming',
     (SELECT id FROM camera_groups WHERE name='Indoor'), now()::timestamp - interval '3 seconds', 'failed', 'ffmpeg exited 1', false, false, true, '22:00', '06:00'),
  -- broken but stale: goes offline, so the stale reason must be suppressed
  ('cam-failedold', 'self-host', (SELECT id FROM camera_nodes WHERE node_id='node-bbbb2222'), 'Attic', 'usb', 'streaming',
     NULL, now()::timestamp - interval '900 seconds', 'failed', 'ffmpeg exited 1', false, false, false, NULL, NULL),
  ('cam-restart',   'self-host', (SELECT id FROM camera_nodes WHERE node_id='node-aaaa1111'), 'Restarting', 'rtsp', 'streaming',
     NULL, now()::timestamp - interval '1 seconds', 'restarting', 'pipeline stalled', false, false, false, NULL, NULL),
  ('cam-error',     'self-host', (SELECT id FROM camera_nodes WHERE node_id='node-aaaa1111'), 'Errored', 'rtsp', 'streaming',
     NULL, now()::timestamp - interval '1 seconds', 'error', 'bad codec', false, false, false, NULL, NULL),
  -- explicitly offline despite a fresh heartbeat
  ('cam-offline',   'self-host', (SELECT id FROM camera_nodes WHERE node_id='node-bbbb2222'), 'Shed', 'rtsp', 'streaming',
     NULL, now()::timestamp - interval '2 seconds', 'offline', 'was unplugged', false, false, false, NULL, NULL),
  ('cam-neverseen', 'self-host', (SELECT id FROM camera_nodes WHERE node_id='node-aaaa1111'), 'New Cam', 'rtsp', 'streaming',
     NULL, NULL, 'streaming', NULL, false, false, false, NULL, NULL),
  -- orphan: no node row at all
  ('cam-orphan',    'self-host', NULL, 'Orphan', 'unknown', 'streaming',
     (SELECT id FROM camera_groups WHERE name='Indoor'), now()::timestamp - interval '4 seconds', 'streaming', NULL, false, false, false, NULL, NULL),
  -- NULL / empty / whitespace capabilities, NULL status
  ('cam-nocaps',    'self-host', NULL, 'No Caps', NULL, NULL,
     NULL, now()::timestamp - interval '4 seconds', 'streaming', NULL, false, false, false, NULL, NULL),
  ('cam-emptycaps', 'self-host', NULL, 'Empty Caps', 'rtsp', '',
     NULL, now()::timestamp - interval '4 seconds', 'streaming', NULL, false, false, false, NULL, NULL),
  ('cam-spacecaps', 'self-host', NULL, 'Spaced Caps', 'rtsp', 'streaming, motion , audio',
     NULL, now()::timestamp - interval '4 seconds', 'streaming', NULL, false, false, false, NULL, NULL),
  ('cam-nullstatus','self-host', NULL, 'Null Status', 'rtsp', 'streaming',
     NULL, now()::timestamp - interval '4 seconds', NULL, 'something', false, false, false, NULL, NULL),
  ('cam-capped',    'self-host', NULL, 'Capped', 'rtsp', 'streaming',
     NULL, now()::timestamp - interval '4 seconds', 'streaming', NULL, true, false, false, NULL, NULL),
  -- isoformat fraction edges: whole second, .100000 (chrono's %.f would
  -- truncate this to .100), and a full six digits
  ('cam-ts-whole',  'self-host', NULL, 'TS Whole', 'rtsp', 'streaming',
     NULL, date_trunc('second', now()::timestamp) - interval '10 seconds', 'streaming', NULL, false, false, false, NULL, NULL),
  ('cam-ts-tenth',  'self-host', NULL, 'TS Tenth', 'rtsp', 'streaming',
     NULL, date_trunc('second', now()::timestamp) - interval '10 seconds' + interval '100000 microseconds', 'streaming', NULL, false, false, false, NULL, NULL),
  ('cam-ts-micro',  'self-host', NULL, 'TS Micro', 'rtsp', 'streaming',
     NULL, date_trunc('second', now()::timestamp) - interval '10 seconds' + interval '123456 microseconds', 'streaming', NULL, false, false, false, NULL, NULL),
  -- another tenant: must never appear in a self-host response
  ('cam-theirs',    'other-org', (SELECT id FROM camera_nodes WHERE node_id='node-cccc3333'), 'Their Cam', 'rtsp', 'streaming',
     (SELECT id FROM camera_groups WHERE name='Their Group'), now()::timestamp, 'streaming', NULL, false, false, false, NULL, NULL)
) AS v;

-- ---- settings -------------------------------------------------------
DELETE FROM settings WHERE org_id IN ('self-host', 'other-org');
INSERT INTO settings (org_id, key, value) VALUES
  ('self-host', 'motion_notifications', 'true'),
  ('self-host', 'camera_transition_notifications', 'false'),
  -- deliberately mixed case: the notification toggles compare with a
  -- bare == "true", so this reads as OFF, while motion-ingestion
  -- lowercases first and would read the same value as ON.
  ('self-host', 'node_transition_notifications', 'TRUE'),
  ('self-host', 'timezone', 'America/Los_Angeles'),
  ('self-host', 'motion_ingestion_enabled', 'TRUE'),
  ('other-org', 'timezone', 'Europe/London');

-- ---- audit log ------------------------------------------------------
DELETE FROM audit_log;
INSERT INTO audit_log (org_id, timestamp, event, ip_address, username, user_id, details)
SELECT
  CASE WHEN i % 7 = 0 THEN 'other-org' ELSE 'self-host' END,
  timestamp '2026-09-01 00:00:00' + (i || ' minutes')::interval
    + CASE WHEN i % 3 = 0 THEN interval '123456 microseconds' ELSE interval '0' END,
  CASE WHEN i % 4 = 0 THEN 'camera_created' ELSE 'node_registered' END,
  '10.0.0.' || (i % 255),
  CASE WHEN i % 5 = 0 THEN 'clerk_user_alpha' ELSE 'beta%user' END,
  'user_' || i,
  '{"i": ' || i || '}'
FROM generate_series(1, 240) AS i;

-- rows that exercise the NULL branches both stacks handle differently
INSERT INTO audit_log (org_id, timestamp, event, ip_address, username, user_id, details) VALUES
  ('self-host', timestamp '2026-09-02 12:00:00', 'null_fields', NULL, NULL, NULL, NULL);

-- ---- stream access / motion / mcp activity --------------------------
-- Row counts per group are strictly DISTINCT, by construction. Several
-- of these routes order by COUNT(*) DESC with no tiebreaker, so equal
-- counts leave the row order up to Postgres and make the differential
-- flaky rather than informative. Camera k gets k*10 rows, tool k gets
-- k*7, and so on.
DELETE FROM stream_access_logs;
INSERT INTO stream_access_logs (user_id, user_email, org_id, camera_id, node_id, ip_address, accessed_at)
SELECT
  -- One user per camera, so per-user counts are distinct too (10..50).
  -- user_3's email is NULL throughout, which exercises the `or ""`
  -- mapping without splitting any user across two groups — a split
  -- would create a COUNT tie and make the LIMIT 10 ordering arbitrary.
  'user_' || k,
  CASE WHEN k = 3 THEN NULL ELSE 'user' || k || '@example.com' END,
  'self-host',
  'cam-' || k,
  'node-aaaa1111',
  '10.1.0.' || (i % 255),
  -- Strictly distinct: k%5 picks a distinct day per camera and i a
  -- distinct second within it. Ties here would make ORDER BY
  -- accessed_at DESC LIMIT 100 return a different page of 150 rows on
  -- each run — which it did, and it read as a port bug.
  now()::timestamp - ((k % 5) || ' days')::interval - (i || ' seconds')::interval
FROM generate_series(1, 5) AS k, generate_series(1, k * 10) AS i;

-- a second tenant's rows, which must never appear in a self-host response
INSERT INTO stream_access_logs (user_id, user_email, org_id, camera_id, node_id, ip_address, accessed_at)
SELECT 'other_user', 'them@example.com', 'other-org', 'cam-theirs', 'node-cccc3333',
       '10.9.9.' || i, now()::timestamp - (i || ' seconds')::interval
FROM generate_series(1, 17) AS i;

DELETE FROM motion_events;
INSERT INTO motion_events (org_id, camera_id, node_id, score, segment_seq, timestamp)
SELECT
  'self-host',
  'cam-' || k,
  'node-aaaa1111',
  (i * 7 + k) % 101,
  CASE WHEN i % 6 = 0 THEN NULL ELSE i END,
  -- inside the default 24h window for the first two cameras, outside it
  -- for the rest, so ?hours= actually changes the answer
  -- k*9 hours puts cameras 1-2 inside the default 24h window and 3-4
  -- outside it, so ?hours= changes the answer; i seconds keeps every
  -- row's sort key distinct.
  now()::timestamp - ((k * 9) || ' hours')::interval - (i || ' seconds')::interval
FROM generate_series(1, 4) AS k, generate_series(1, k * 8) AS i;

INSERT INTO motion_events (org_id, camera_id, node_id, score, segment_seq, timestamp)
SELECT 'other-org', 'cam-theirs', 'node-cccc3333', 50, i,
       now()::timestamp - (i || ' hours')::interval
FROM generate_series(1, 11) AS i;

DELETE FROM mcp_activity_logs;
INSERT INTO mcp_activity_logs (org_id, tool_name, key_name, status, duration_ms, args_summary, error, timestamp)
SELECT
  'self-host',
  'tool_' || k,
  -- key_alpha_one carries a literal underscore and key%beta a literal
  -- percent: this route escapes both, unlike /api/audit/stream-logs
  CASE WHEN k = 1 THEN 'key_alpha_one' ELSE 'key%beta' END,
  CASE WHEN i % 8 = 0 THEN 'error' ELSE 'ok' END,
  CASE WHEN i % 7 = 0 THEN NULL ELSE i * 3 END,
  '{"n": ' || i || '}',
  CASE WHEN i % 8 = 0 THEN 'boom ' || i ELSE NULL END,
  -- i%4 spreads rows across days for the by_day aggregate; i seconds
  -- keeps the ORDER BY timestamp DESC page deterministic.
  now()::timestamp - ((k * 3 + (i % 4)) || ' days')::interval - (i || ' seconds')::interval
FROM generate_series(1, 3) AS k, generate_series(1, k * 7) AS i;

INSERT INTO mcp_activity_logs (org_id, tool_name, key_name, status, duration_ms, args_summary, error, timestamp)
SELECT 'other-org', 'tool_theirs', 'their_key', 'ok', 10, '{}', NULL,
       now()::timestamp - (i || ' days')::interval
FROM generate_series(1, 9) AS i;

-- ---- incidents ------------------------------------------------------
-- Sequences are RESTARTed so ids are deterministic across reseeds; the
-- write differential compares them directly and a drifting sequence
-- would make every case look like a diff.
DELETE FROM incident_evidence;
DELETE FROM incidents;

INSERT INTO incidents
  (org_id, camera_id, title, summary, report, severity, status, created_by,
   created_at, updated_at, resolved_at, resolved_by)
VALUES
  -- 1: open, has evidence, old timestamps so a handler that wrongly
  -- rewrites created_at is caught rather than normalised away
  ('self-host', 'cam-live', 'Front door forced', 'Someone at the door', NULL,
   'high', 'open', 'user:local-admin',
   timestamp '2026-09-01 08:00:00', timestamp '2026-09-01 08:00:00', NULL, NULL),
  -- 2: open, no evidence, NULL report
  ('self-host', NULL, 'Unknown vehicle', 'Idling in the street', NULL,
   'medium', 'open', 'mcp:agent-key',
   timestamp '2026-09-02 09:30:00', timestamp '2026-09-02 09:30:00', NULL, NULL),
  -- 3: already resolved — re-resolving must NOT re-stamp resolved_at
  ('self-host', 'cam-failed', 'Camera offline', 'Garage went dark', 'Full report here',
   'low', 'resolved', 'user:someone-else',
   timestamp '2026-08-20 12:00:00', timestamp '2026-08-21 12:00:00',
   timestamp '2026-08-21 12:00:00', 'user:someone-else'),
  -- 4: another tenant's, must 404 from both stacks
  ('other-org', 'cam-theirs', 'Theirs', 'Not ours', NULL,
   'critical', 'open', 'user:them',
   timestamp '2026-09-03 10:00:00', timestamp '2026-09-03 10:00:00', NULL, NULL);

INSERT INTO incident_evidence (incident_id, kind, text, camera_id, data, data_mime, timestamp)
VALUES
  (1, 'observation', 'Heard knocking', 'cam-live', NULL, NULL, timestamp '2026-09-01 08:01:00'),
  -- has_data is derived from data_mime, never from the deferred blob
  (1, 'snapshot', NULL, 'cam-live', '\x89504e47'::bytea, 'image/png', timestamp '2026-09-01 08:02:00'),
  (1, 'action', 'Notified owner', NULL, NULL, NULL, timestamp '2026-09-01 08:03:00'),
  (4, 'observation', 'Theirs', 'cam-theirs', NULL, NULL, timestamp '2026-09-03 10:01:00');

-- ---- api keys -------------------------------------------------------
DELETE FROM mcp_api_keys;
ALTER SEQUENCE IF EXISTS mcp_api_keys_id_seq RESTART WITH 1;
INSERT INTO mcp_api_keys
  (org_id, key_hash, name, created_at, last_used_at, revoked, scope_mode, scope_tools, kind)
VALUES
  -- 1: mcp, all tools
  ('self-host', 'hash_1', 'Laptop MCP', timestamp '2026-09-01 10:00:00',
   timestamp '2026-09-10 11:00:00', false, 'all', NULL, 'mcp'),
  -- 2: mcp, custom scope
  ('self-host', 'hash_2', 'Scoped MCP', timestamp '2026-09-02 10:00:00',
   NULL, false, 'custom', '["list_cameras", "get_camera"]', 'mcp'),
  -- 3: legacy row — NULL scope_mode and NULL kind both fall back
  ('self-host', 'hash_3', 'Legacy Key', timestamp '2026-09-03 10:00:00',
   NULL, false, NULL, NULL, 'mcp'),
  -- 4: already revoked, so it must not appear in either list
  ('self-host', 'hash_4', 'Revoked Key', timestamp '2026-09-04 10:00:00',
   NULL, true, 'all', NULL, 'mcp'),
  -- 5, 6: integration keys — a separate surface in the same table
  ('self-host', 'hash_5', 'Home Assistant', timestamp '2026-09-05 10:00:00',
   timestamp '2026-09-11 09:00:00', false, 'all', NULL, 'integration'),
  ('self-host', 'hash_6', 'HA Spare', timestamp '2026-09-06 10:00:00',
   NULL, false, 'all', NULL, 'integration'),
  -- 7: malformed scope_tools — parsed as [] rather than raising
  ('self-host', 'hash_7', 'Bad Scope', timestamp '2026-09-07 10:00:00',
   NULL, false, 'custom', 'not json at all', 'mcp'),
  -- 8: non-string elements, which python stringifies
  ('self-host', 'hash_8', 'Odd Scope', timestamp '2026-09-08 10:00:00',
   NULL, false, 'custom', '[1, true, null, "x"]', 'mcp'),
  -- 9: another tenant's, must never appear
  ('other-org', 'hash_9', 'Theirs', timestamp '2026-09-09 10:00:00',
   NULL, false, 'all', NULL, 'mcp'),
  ('other-org', 'hash_10', 'Theirs Integration', timestamp '2026-09-09 11:00:00',
   NULL, false, 'all', NULL, 'integration');
