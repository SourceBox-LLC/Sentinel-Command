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

-- `node_version` is set here rather than by a later UPDATE: an UPDATE
-- rewrites the row, which moves it in the heap, and the GDPR export
-- walks nodes with no ORDER BY — so the two passes of a case started
-- returning them in different orders. The versions themselves give
-- `check_node_version` all five of its branches: absent, below the
-- floor (0.1.0), exactly the floor, current (0.1.77), and unparseable.
INSERT INTO camera_nodes (node_id, org_id, api_key_hash, name, status, local_ip, http_port, node_version) VALUES
  -- sha256("test-node-key"). A real hash, so streaming_diff.py can open an
  -- authenticated WebSocket; the other two keep a placeholder.
  ('node-aaaa1111', 'self-host',
   'f3702f9692e7bce4e7dc0b10fe460daf0bdc6c2c741d4c2fabcdb6df44dbb4c9',
   'Front Yard Pi', 'online', NULL, NULL, NULL),
  -- a LAN address on an OFFLINE node: the integration list must build
  -- no local_url for it, which is invisible unless some node has both
  ('node-bbbb2222', 'self-host', 'x', 'Garage Pi',     'offline', '192.168.1.22', 8080, '0.0.9'),
  ('node-cccc3333', 'other-org', 'x', 'Someone Else',  'online', NULL, NULL, NULL);

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
  -- Inside the 90s window, with room to stay there for the length of a
  -- run. It was 85 seconds, which left five: a run slower than that
  -- crossed the threshold mid-flight and the two tiers, answering
  -- milliseconds apart, landed on opposite sides of it. That is a
  -- fixture racing the clock, not a port difference, and it reported
  -- eleven of them in one read run. The exclusive `> 90` comparison
  -- itself is pinned by `the_grace_boundary_is_exclusive` in
  -- src/models.rs, where the clock is an argument rather than the wall.
  ('cam-boundary',  'self-host', (SELECT id FROM camera_nodes WHERE node_id='node-aaaa1111'), 'Boundary', 'rtsp', 'streaming',
     NULL, now()::timestamp - interval '30 seconds', 'streaming', NULL, false, false, false, NULL, NULL),
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

-- ---- node-key auth fixtures (nodes 4-6, cameras after the set above) --
--
-- CameraNode routes hash `api_key.encode()`: Starlette has decoded the
-- header as latin-1, and .encode() then produces UTF-8. The agent-key
-- path re-encodes latin-1 instead. So for the key "node-key-\u00ff",
-- sent on the wire as the single byte 0xFF:
--   node-dddd4444 stores sha256 of its UTF-8 form  -> Python authenticates it
--   node-eeee5555 stores sha256 of the raw bytes   -> a port hashing raw
--                                                     bytes would
-- node-dddd4444 also already has a codec, so reporting one must not
-- update the node (and must not move its updated_at). node-ffff6666 has
-- an EMPTY codec, which `not node.video_codec` treats as unset.
INSERT INTO camera_nodes (node_id, org_id, api_key_hash, name, status, video_codec, audio_codec, local_ip, http_port, node_version) VALUES
  ('node-dddd4444', 'self-host',
   '513ceeab86d874d7de558cef2a9f5b8d10659f3c162d0410d12ccb6c65dc1372',
   'High Byte Key', 'online', 'avc1.640028', 'mp4a.40.2', '192.168.1.40', 8081, '0.1.0'),
  ('node-eeee5555', 'self-host',
   '48cec6821e84f2d39d3eab0b705ea046826b1f74862d81f71f4d7e2327ffb7ef',
   -- http_port 0: `node.http_port or 8080` makes that 8080
   'Raw Byte Decoy', 'online', NULL, NULL, '10.0.0.5', 0, '0.1.77'),
  ('node-ffff6666', 'self-host',
   '5f04ce6f1784775a88a8b9f0a5dbfa7af121a27a5a4addce92bac3e1ac0e2aea',
   -- an empty local_ip is falsy: no LAN URL at all
   'Empty Codec Node', 'online', '', NULL, '', NULL, 'garbage');



INSERT INTO cameras (camera_id, org_id, node_id, name, node_type, capabilities, status,
                     disabled_by_plan, continuous_24_7, scheduled_recording)
VALUES
  ('cam-dddd', 'self-host', (SELECT id FROM camera_nodes WHERE node_id='node-dddd4444'),
   'High Byte Cam', 'rtsp', 'streaming', 'streaming', false, false, false),
  ('cam-eeee', 'self-host', (SELECT id FROM camera_nodes WHERE node_id='node-eeee5555'),
   'Decoy Cam', 'rtsp', 'streaming', 'streaming', false, false, false),
  ('cam-ffff', 'self-host', (SELECT id FROM camera_nodes WHERE node_id='node-ffff6666'),
   'Empty Codec Cam', 'rtsp', 'streaming', 'streaming', false, false, false);

-- ---- settings -------------------------------------------------------
DELETE FROM settings WHERE org_id IN ('self-host', 'other-org');
INSERT INTO settings (org_id, key, value) VALUES
  -- Self-hosted licence state, matching what a check-in against
  -- fake_license.py writes: valid, reachable, sync off. The two
  -- timestamps are relative to now so the write differential
  -- normalises them to <recent> — the same token the loop's own write
  -- would produce, which is what makes its fifteen-minute tick a
  -- no-op here rather than a flake. install_id is seeded so
  -- _get_or_create_install_id returns it instead of minting one.
  ('self-host', 'sentinel_install_id', 'aaaaaaaabbbbbbbbccccccccdddddddd'),
  ('self-host', 'sentinel_license_valid', 'true'),
  ('self-host', 'sentinel_license_last_check_reachable', 'true'),
  -- Fixed, not now(): the two reseeds happen seconds apart and these
  -- are Text values, so a relative one drifts and every case diffs.
  -- The gate ignores them while the last check was reachable — the
  -- grace window is only consulted when it was not.
  ('self-host', 'sentinel_license_last_check_at', '2026-09-01T00:00:00.000000+00:00'),
  ('self-host', 'sentinel_license_last_ok_at', '2026-09-01T00:00:00.000000+00:00'),
  ('self-host', 'sentinel_data_sync_enabled', 'false'),
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
  -- 4
  (4, 'observation', 'Theirs', 'cam-theirs', NULL, NULL, timestamp '2026-09-03 10:01:00'),

  -- Evidence blob + synthetic-playlist fixtures (ids 5-15).
  --
  -- The playlist route reads a duration back out of the stored MIME
  -- with a bare float(), which is looser than it looks, and the blob
  -- route strips MIME parameters and then re-adds a charset for text/*
  -- only. Both behaviours are data-dependent, so each branch needs a
  -- row rather than a unit test alone. Timestamps are distinct so the
  -- evidence list inside GET /api/incidents/1 has no tied sort key.
  -- 5: the ordinary clip
  (1, 'clip', NULL, 'cam-live', '\x47400011'::bytea, 'video/mp2t;duration=12.5',
   timestamp '2026-09-01 08:04:00'),
  -- 6: a clip with no duration parameter -> the 60s fallback
  (1, 'clip', NULL, 'cam-live', '\x47400011'::bytea, 'video/mp2t',
   timestamp '2026-09-01 08:05:00'),
  -- 7: unparseable duration -> float() raises, Python skips the
  --    parameter and keeps the fallback
  (1, 'clip', NULL, 'cam-live', '\x47400011'::bytea, 'video/mp2t;duration=oops',
   timestamp '2026-09-01 08:06:00'),
  -- 8: negative duration -> int() truncates toward zero, then max(1, ..)
  (1, 'clip', NULL, 'cam-live', '\x47400011'::bytea, 'video/mp2t;duration=-3.7',
   timestamp '2026-09-01 08:07:00'),
  -- 9: zero-length blob -> `not evidence.data` is true, so 404 from both
  (1, 'clip', NULL, 'cam-live', '\x'::bytea, 'video/mp2t;duration=5',
   timestamp '2026-09-01 08:08:00'),
  -- 10: data with no MIME at all -> application/octet-stream
  (1, 'snapshot', NULL, 'cam-live', '\x89504e47'::bytea, NULL,
   timestamp '2026-09-01 08:09:00'),
  -- 11: a text/* MIME -> Starlette appends "; charset=utf-8"
  (1, 'snapshot', NULL, 'cam-live', '\x68690a'::bytea, 'text/plain',
   timestamp '2026-09-01 08:10:00'),
  -- 12: a MIME that is nothing but a parameter -> split()[0] is empty
  --     and the `or` falls through to octet-stream
  (1, 'snapshot', NULL, 'cam-live', '\x68690a'::bytea, ';weird',
   timestamp '2026-09-01 08:11:00'),
  -- 13: two duration parameters -> the loop does not break, last wins
  (1, 'clip', NULL, 'cam-live', '\x47400011'::bytea, 'video/mp2t;duration=5;duration=9',
   timestamp '2026-09-01 08:12:00'),
  -- 14: a clip row whose blob was never attached -> 404 from both,
  --     even though has_data reads true off the non-null MIME
  (1, 'clip', NULL, 'cam-live', NULL, 'video/mp2t;duration=5',
   timestamp '2026-09-01 08:13:00'),
  -- 15: whitespace and a digit-group underscore, both of which
  --     Python's float() accepts and Rust's parser does not
  (1, 'clip', NULL, 'cam-live', '\x47400011'::bytea, 'video/mp2t; duration= 1_0.5',
   timestamp '2026-09-01 08:14:00');

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
   NULL, false, 'all', NULL, 'integration'),
  -- 11: a non-ASCII name, which the revoke audit row writes into its
  --     `details` JSON. json.dumps defaults to ensure_ascii=True and
  --     escapes every character outside printable ASCII to \uXXXX,
  --     astral ones as a surrogate pair; serde_json emits UTF-8 and
  --     agrees on neither. Nothing in this fixture had a non-ASCII
  --     string before, so the port stored "Café" where Python
  --     stored "Caf\u00e9" and every harness called it identical.
  ('self-host', 'hash_11', 'Café 🎥 — Terrasse', timestamp '2026-09-10 10:00:00',
   NULL, false, 'all', NULL, 'integration'),
  -- 12-14: integration keys with REAL hashes, for /api/integration/*.
  --   12  osi_live_integration_key    authenticates
  --   13  osi_revoked_integration     revoked, must not
  --   14  osc_mcp_kind_key            an MCP key: right hash, wrong kind,
  --                                   must not — kind is the boundary
  ('self-host', '805ff9bd0d9fe483225995f3fcaa97b7b232084ed5f21e5452d5257fd009404c', 'Home Assistant Live',
   timestamp '2026-09-11 10:00:00', NULL, false, 'all', NULL, 'integration'),
  ('self-host', '5e136420d4db74b8fec00578900fbb7a3faf3043ca3526785b7041a336dd1b16', 'HA Revoked',
   timestamp '2026-09-11 11:00:00', NULL, true, 'all', NULL, 'integration'),
  ('self-host', '9e6a477e4b9c3ccd58cd8027d87d55c34fdb59c06d53f9bacceeb4aef5646cba', 'MCP Not Integration',
   timestamp '2026-09-11 12:00:00', NULL, false, 'all', NULL, 'mcp');

-- ---- notifications --------------------------------------------------
DELETE FROM user_notification_state;
DELETE FROM notifications;
ALTER SEQUENCE IF EXISTS notifications_id_seq RESTART WITH 1;
ALTER SEQUENCE IF EXISTS user_notification_state_id_seq RESTART WITH 1;

INSERT INTO notifications
  (org_id, kind, audience, title, body, severity, link, camera_id, node_id, meta_json, created_at)
SELECT
  CASE WHEN i % 9 = 0 THEN 'other-org' ELSE 'self-host' END,
  (ARRAY['motion','camera_offline','node_offline','incident_created'])[1 + (i % 4)],
  -- a third are admin-only, so the audience filter has something to hide
  CASE WHEN i % 3 = 0 THEN 'admin' ELSE 'all' END,
  'Notification ' || i,
  'Body text for ' || i,
  (ARRAY['info','warning','error','critical'])[1 + (i % 4)],
  CASE WHEN i % 5 = 0 THEN NULL ELSE '/cameras/' || i END,
  CASE WHEN i % 4 = 0 THEN 'cam-live' ELSE NULL END,
  CASE WHEN i % 6 = 0 THEN 'node-aaaa1111' ELSE NULL END,
  -- valid JSON, absent, and malformed: meta parses to null on failure
  CASE WHEN i % 7 = 0 THEN 'not json at all'
       WHEN i % 5 = 0 THEN NULL
       ELSE '{"i": ' || i || ', "z": "last"}' END,
  -- distinct timestamps: ORDER BY created_at DESC has no tiebreaker
  now()::timestamp - (i || ' minutes')::interval
-- 130 rows: the unread badge caps display at >99, so a smaller fixture
-- leaves that branch untested (a mutation to the threshold scored
-- 296/296 before this).
FROM generate_series(1, 130) AS i;

-- a row older than the default 168h window
INSERT INTO notifications (org_id, kind, audience, title, body, severity, created_at)
VALUES ('self-host', 'motion', 'all', 'Ancient', 'outside the window', 'info',
        now()::timestamp - interval '40 days');

-- The admin's read-state is seeded OLD so every notification counts as
-- unread and the >99 display cap is actually exercised; a state row
-- created on first access is stamped with now(), which makes unread 0
-- and leaves that branch dead. The member is deliberately left
-- unseeded, so the create-on-first-access path is covered too.
INSERT INTO user_notification_state (clerk_user_id, org_id, last_viewed_at, cleared_at)
VALUES
  -- Admin: everything unread, so `capped` is true (>99).
  ('local-admin', 'self-host', timestamp '2026-01-01 00:00:00', NULL),
  -- Member: partway back, landing the count BETWEEN 51 and 99. Without
  -- a caller in that band, `count > 99` and `count > 50` agree on every
  -- request and a wrong threshold is invisible — which it was.
  ('local-member', 'self-host', now()::timestamp - interval '100 minutes', NULL);

-- ---- sentinel agent keys + runs -------------------------------------
--
-- The agent data plane authenticates on X-Sentinel-Agent-Key: either
-- the shared SENTINEL_AGENT_KEY (unset here, and deliberately so — an
-- unset shared key must not disable the scoped path) or a row below,
-- matched by SHA-256 of the presented bytes. The raw keys are
--   osa_...0001  live, self-host
--   osa_...0002  revoked
--   osa_...0003  another tenant's
DELETE FROM sentinel_agent_keys;
ALTER SEQUENCE IF EXISTS sentinel_agent_keys_id_seq RESTART WITH 1;
INSERT INTO sentinel_agent_keys
  (org_id, key_hash, key_last4, name, created_at, created_by, last_used_at, revoked)
VALUES
  ('self-host', '09ac8c6b2b2c93693461f987c7eeee30a332606638b2b95de2479f0322d62d08',
   '0001', 'Live Agent', timestamp '2026-09-01 10:00:00', 'user:local-admin',
   timestamp '2026-09-10 09:00:00', false),
  ('self-host', 'dd6d4c83fc5f5501a8f3b53847163680df4eb527d69dddd8934a108335fa5c16',
   '0002', 'Revoked Agent', timestamp '2026-09-02 10:00:00', 'user:local-admin',
   NULL, true),
  ('other-org', 'e4e30ad9b553c9f0456588a064dbe606c7041b0cb1e408a8e7f301adeadb69a3',
   '0003', 'Theirs', timestamp '2026-09-03 10:00:00', 'user:them', NULL, false);

-- Another org's config, so sentinel_config is never empty in the
-- write differential's snapshot — while self-host's own row is still
-- absent, and created lazily by the case under test.
DELETE FROM sentinel_config;
ALTER SEQUENCE IF EXISTS sentinel_config_id_seq RESTART WITH 1;
INSERT INTO sentinel_config (org_id, enabled, motion_enabled, incident_opened_enabled,
    motion_cooldown_min, schedule_mode, schedule_start, schedule_end, active_days,
    camera_scope, created_at, updated_at)
VALUES ('other-org', false, true, true, 9, 'scheduled', '21:00', '07:00',
        '["mon"]', '{"cam-theirs": true}', timestamp '2026-09-01 00:00:00',
        timestamp '2026-09-01 00:00:00');

DELETE FROM sentinel_runs;
INSERT INTO sentinel_runs
  (id, org_id, triggered_at, trigger_type, camera_id, tool_call_count, outcome,
   severity, incident_id, started_at, completed_at, manual_prompt, summary,
   tool_trace, updated_at)
VALUES
  -- pending, oldest first: /runs/pending is FIFO and a scoped key must
  -- see only its own org's queue
  ('run0000000000000000000000000001', 'self-host', timestamp '2026-09-01 07:00:00',
   'motion', 'cam-live', 0, 'pending', NULL, NULL, NULL, NULL, NULL, '', NULL,
   timestamp '2026-09-01 07:00:00'),
  ('run0000000000000000000000000002', 'self-host', timestamp '2026-09-01 08:00:00',
   'scheduled', NULL, 0, 'pending', NULL, NULL, NULL, NULL, NULL, '', NULL,
   timestamp '2026-09-01 08:00:00'),
  -- another tenant's pending run: present in the shared queue, absent
  -- from a scoped one
  ('run0000000000000000000000000003', 'other-org', timestamp '2026-09-01 06:00:00',
   'motion', 'cam-theirs', 0, 'pending', NULL, NULL, NULL, NULL, NULL, '', NULL,
   timestamp '2026-09-01 06:00:00'),
  -- already running: /start must answer claimed=false
  ('run0000000000000000000000000004', 'self-host', timestamp '2026-09-02 09:00:00',
   'manual', 'cam-live', 3, 'running', NULL, NULL, timestamp '2026-09-02 09:00:05',
   NULL, 'have a look at the front door', '', NULL, timestamp '2026-09-02 09:00:05'),
  -- terminal: error, which /complete may upgrade to a real outcome
  ('run0000000000000000000000000005', 'self-host', timestamp '2026-09-03 09:00:00',
   'motion', 'cam-live', 2, 'error', NULL, NULL, timestamp '2026-09-03 09:00:01',
   timestamp '2026-09-03 09:04:31', NULL, 'timed out', NULL,
   timestamp '2026-09-03 09:04:31'),
  -- terminal: incident, which /complete must NOT downgrade to error
  ('run0000000000000000000000000006', 'self-host', timestamp '2026-09-04 09:00:00',
   'incident_opened', 'cam-live', 5, 'incident', 'high', 1,
   timestamp '2026-09-04 09:00:01', timestamp '2026-09-04 09:02:00', NULL,
   'filed one', '[{"tool": "get_camera", "args": {"camera_id": "cam-live"}, "result": "ok"}]',
   timestamp '2026-09-04 09:02:00'),
  -- terminal: no_action, with a tool_trace that is not valid JSON —
  -- get_tool_trace() swallows the error and returns []
  ('run0000000000000000000000000007', 'self-host', timestamp '2026-09-05 09:00:00',
   'scheduled', NULL, 1, 'no_action', NULL, NULL, timestamp '2026-09-05 09:00:01',
   timestamp '2026-09-05 09:00:30', NULL, 'nothing to report', 'not json at all',
   timestamp '2026-09-05 09:00:30'),
  -- a trace that parses but is not a list: also []
  ('run0000000000000000000000000008', 'self-host', timestamp '2026-09-06 09:00:00',
   'scheduled', NULL, 0, 'no_action', NULL, NULL, NULL, timestamp '2026-09-06 09:00:10',
   NULL, '', '{"not": "a list"}', timestamp '2026-09-06 09:00:10'),
  -- another tenant's terminal run, for the cross-org 404s
  ('run0000000000000000000000000009', 'other-org', timestamp '2026-09-07 09:00:00',
   'motion', 'cam-theirs', 0, 'pending', NULL, NULL, NULL, NULL, NULL, '', NULL,
   timestamp '2026-09-07 09:00:00');

-- ---- email: outbox, suppression list, processed webhook ids ---------
--
-- For POST /api/webhooks/resend. A bounce or complaint suppresses the
-- recipients and marks the originating outbox row — but only a row still
-- 'sent', so em_pending must stay as it is. One address is already
-- suppressed (a duplicate insert is swallowed, not an error), and one
-- message id has already been processed (a retried delivery answers
-- "duplicate" and changes nothing).
DELETE FROM email_outbox;
DELETE FROM email_suppression;
DELETE FROM processed_webhooks;
ALTER SEQUENCE IF EXISTS email_outbox_id_seq RESTART WITH 1;
ALTER SEQUENCE IF EXISTS email_suppression_id_seq RESTART WITH 1;
ALTER SEQUENCE IF EXISTS processed_webhooks_id_seq RESTART WITH 1;
INSERT INTO email_outbox (org_id, recipient_email, subject, body_text, body_html, kind,
                          status, attempts, sent_at, resend_message_id, created_at)
VALUES
  ('self-host', 'bounce@example.com', 'Motion', 't', '<p>t</p>', 'motion',
   'sent', 1, timestamp '2026-09-01 10:00:00', 'em_sent', timestamp '2026-09-01 10:00:00'),
  ('self-host', 'queued@example.com', 'Motion', 't', '<p>t</p>', 'motion',
   'pending', 0, NULL, 'em_pending', timestamp '2026-09-01 11:00:00'),
  ('other-org', 'theirs@example.com', 'Motion', 't', '<p>t</p>', 'motion',
   'sent', 1, timestamp '2026-09-01 12:00:00', 'em_theirs', timestamp '2026-09-01 12:00:00');
INSERT INTO email_suppression (address, reason, source, created_at)
VALUES ('already@example.com', 'bounce', 'resend_webhook', timestamp '2026-08-01 00:00:00');
INSERT INTO processed_webhooks (svix_msg_id, event_type, processed_at)
VALUES ('msg_already_seen', 'email.bounced', timestamp '2026-08-01 00:00:00');
