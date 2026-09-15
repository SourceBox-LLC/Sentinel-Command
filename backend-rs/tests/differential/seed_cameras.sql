-- Fixture data for the slice-2 HTTP differential.
--
-- Timestamps are relative to now() on purpose: `effective_status` turns
-- a camera offline after 90 seconds without a heartbeat, so a fixture
-- seeded once and reused an hour later exercises only the offline path.
-- An earlier run of this harness did exactly that and reported 31/31
-- identical while never testing a live camera. Re-run this file
-- immediately before diffing.

DELETE FROM cameras;
DELETE FROM camera_groups;
DELETE FROM camera_nodes;

INSERT INTO camera_nodes (node_id, org_id, api_key_hash, name, status) VALUES
  ('node-aaaa1111', 'self-host', 'x', 'Front Yard Pi', 'online'),
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
