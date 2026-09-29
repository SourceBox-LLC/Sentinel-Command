-- Rows for the background-loop probes, on top of seed_cameras.sql.
--
-- Separate from the main fixture for one reason: the main fixture cannot
-- exercise either loop at all, and changing it to would break the other
-- harnesses.
--
--   * the OFFLINE SWEEP filters `status = 'online' AND last_seen IS NOT
--     NULL`. Every camera in seed_cameras.sql is 'streaming', 'offline'
--     or NULL, and the five nodes that ARE 'online' all have a NULL
--     last_seen — so the sweep is a no-op there. That is why Python's
--     sweep has been running every thirty seconds against the shared
--     database throughout this project without disturbing anything,
--     and also why nothing has ever tested the flip.
--   * the LOG CLEANUP deletes past a per-tier cutoff of 30, 90 or 365
--     days. The main fixture's log rows are days old at most.
--
-- Three orgs, so the per-tier retention branches all run in one pass:
--
--   loops-free      no org_plan setting  -> free_org  -> 30 days
--   loops-pro       org_plan = pro       ->            90 days
--   loops-plus      org_plan = pro_plus  ->           365 days
--
-- Ages are written relative to now() rather than as literals: a literal
-- drifts across a retention boundary as the branch ages, and the row
-- that was meant to survive starts being deleted. That trap has already
-- cost this suite two debugging sessions on other fixtures.

DELETE FROM notifications       WHERE org_id LIKE 'loops-%';
DELETE FROM stream_access_logs  WHERE org_id LIKE 'loops-%';
DELETE FROM mcp_activity_logs   WHERE org_id LIKE 'loops-%';
DELETE FROM audit_log           WHERE org_id LIKE 'loops-%';
DELETE FROM motion_events       WHERE org_id LIKE 'loops-%';
DELETE FROM email_log           WHERE org_id LIKE 'loops-%';
DELETE FROM email_outbox        WHERE org_id LIKE 'loops-%';
DELETE FROM cameras             WHERE org_id LIKE 'loops-%';
DELETE FROM camera_nodes        WHERE org_id LIKE 'loops-%';
DELETE FROM settings            WHERE org_id LIKE 'loops-%';
DELETE FROM processed_webhooks  WHERE svix_msg_id LIKE 'loops-%';

INSERT INTO settings (org_id, key, value, updated_at) VALUES
  ('loops-pro',  'org_plan', 'pro',      now()::timestamp),
  ('loops-plus', 'org_plan', 'pro_plus', now()::timestamp);

-- ---- the offline sweep ----------------------------------------------
--
-- Nodes. `sweep-stale` is past the 90s threshold and must flip;
-- `sweep-fresh` is inside it and must not; `sweep-null` is 'online'
-- with no last_seen at all, which is the row that must be LEFT ALONE
-- rather than announced as having gone offline — it has never been
-- heard from, so there is no transition to report. `sweep-already` is
-- stale but already offline, so it must not be announced twice.
INSERT INTO camera_nodes (node_id, org_id, api_key_hash, name, status, last_seen) VALUES
  ('sweep-stale',   'loops-free', 'x', 'Stale Node',   'online',
   now()::timestamp - interval '300 seconds'),
  ('sweep-fresh',   'loops-free', 'x', 'Fresh Node',   'online',
   now()::timestamp - interval '10 seconds'),
  ('sweep-null',    'loops-free', 'x', 'Never Seen',   'online', NULL),
  ('sweep-already', 'loops-free', 'x', 'Already Down', 'offline',
   now()::timestamp - interval '300 seconds'),
  -- An empty name, so the `name or node_id` fallback runs. The
  -- notification title carries whichever it picks.
  ('sweep-noname',  'loops-free', 'x', '',             'online',
   now()::timestamp - interval '300 seconds');

-- Cameras, the same four states plus one on a node so the
-- notification's `node_id` is populated through the FK, and one with no
-- node at all so the NULL branch runs.
INSERT INTO cameras (camera_id, org_id, node_id, name, status, last_seen,
                     node_type, capabilities, continuous_24_7, scheduled_recording,
                     created_at, updated_at)
VALUES
  ('sweep-cam-stale', 'loops-free',
   (SELECT id FROM camera_nodes WHERE node_id = 'sweep-stale'),
   'Stale Cam', 'online', now()::timestamp - interval '300 seconds',
   'rtsp', 'streaming', false, false, now()::timestamp, now()::timestamp),
  ('sweep-cam-fresh', 'loops-free',
   (SELECT id FROM camera_nodes WHERE node_id = 'sweep-fresh'),
   'Fresh Cam', 'online', now()::timestamp - interval '10 seconds',
   'rtsp', 'streaming', false, false, now()::timestamp, now()::timestamp),
  ('sweep-cam-null', 'loops-free',
   (SELECT id FROM camera_nodes WHERE node_id = 'sweep-stale'),
   'Never Seen Cam', 'online', NULL,
   'rtsp', 'streaming', false, false, now()::timestamp, now()::timestamp),
  -- No node: the transition notification's node_id must be null, not
  -- an error and not a stale id from a previous row.
  ('sweep-cam-nonode', 'loops-free', NULL,
   'Orphan Cam', 'online', now()::timestamp - interval '300 seconds',
   'rtsp', 'streaming', false, false, now()::timestamp, now()::timestamp),
  -- Another org's, to prove the sweep is not org-scoped (it is not, and
  -- must not be — a node is stale regardless of who owns it) while each
  -- notification still lands in the right org.
  ('sweep-cam-other', 'loops-pro',
   (SELECT id FROM camera_nodes WHERE node_id = 'sweep-stale'),
   'Other Org Cam', 'online', now()::timestamp - interval '300 seconds',
   'rtsp', 'streaming', false, false, now()::timestamp, now()::timestamp);

-- ---- log retention --------------------------------------------------
--
-- Per org, one row on each side of that org's cutoff and one just
-- inside it. `generate_series` keeps the ages explicit and relative.
--
-- The ages are chosen so a port that applied the WRONG TIER to an org
-- is visible rather than merely differently-numbered: 45 days survives
-- under pro and pro_plus and dies under free, 200 days survives only
-- under pro_plus.
INSERT INTO stream_access_logs (org_id, user_id, camera_id, node_id, ip_address, accessed_at)
SELECT o.org_id, 'u1', 'c1', 'n1', '10.0.0.1', now()::timestamp - make_interval(days => d)
  FROM (VALUES ('loops-free'), ('loops-pro'), ('loops-plus')) AS o(org_id),
       (VALUES (1), (45), (200), (400)) AS ages(d);

INSERT INTO mcp_activity_logs (org_id, tool_name, key_name, status, timestamp)
SELECT o.org_id, 'list_cameras', 'k', 'completed',
       now()::timestamp - make_interval(days => d)
  FROM (VALUES ('loops-free'), ('loops-pro'), ('loops-plus')) AS o(org_id),
       (VALUES (1), (45), (200), (400)) AS ages(d);

INSERT INTO audit_log (org_id, event, user_id, timestamp)
SELECT o.org_id, 'probe', 'u1', now()::timestamp - make_interval(days => d)
  FROM (VALUES ('loops-free'), ('loops-pro'), ('loops-plus')) AS o(org_id),
       (VALUES (1), (45), (200), (400)) AS ages(d);

INSERT INTO motion_events (org_id, camera_id, node_id, score, timestamp)
SELECT o.org_id, 'c1', 'n1', 50, now()::timestamp - make_interval(days => d)
  FROM (VALUES ('loops-free'), ('loops-pro'), ('loops-plus')) AS o(org_id),
       (VALUES (1), (45), (200), (400)) AS ages(d);

-- Notifications use created_at, not timestamp. A port that reached for
-- the wrong column here would delete nothing and report zero, which
-- reads exactly like a clean sweep.
INSERT INTO notifications (org_id, kind, audience, title, body, severity, created_at)
SELECT o.org_id, 'motion', 'all', 'old', 'body', 'info',
       now()::timestamp - make_interval(days => d)
  FROM (VALUES ('loops-free'), ('loops-pro'), ('loops-plus')) AS o(org_id),
       (VALUES (1), (45), (200), (400)) AS ages(d);

INSERT INTO email_log (org_id, recipient_email, kind, status, timestamp)
SELECT o.org_id, 'a@example.com', 'motion', 'sent',
       now()::timestamp - make_interval(days => d)
  FROM (VALUES ('loops-free'), ('loops-pro'), ('loops-plus')) AS o(org_id),
       (VALUES (1), (45), (200), (400)) AS ages(d);

-- ---- the outbox, which is NOT tiered --------------------------------
--
-- Fixed 7-day window and terminal states only. `pending` and `sending`
-- must survive at ANY age: deleting an in-flight retry loses the email
-- with nothing to show it ever existed. The 400-day pending row is the
-- whole point of this block.
INSERT INTO email_outbox (org_id, recipient_email, subject, body_text, body_html, kind,
                          status, attempts, created_at)
SELECT o.org_id, 'a@example.com', 's', 'b', '<p>b</p>', 'motion', st.status, 0,
       now()::timestamp - make_interval(days => d)
  FROM (VALUES ('loops-free'), ('loops-plus')) AS o(org_id),
       (VALUES ('sent'), ('failed'), ('suppressed'), ('pending'), ('sending')) AS st(status),
       (VALUES (1), (30), (400)) AS ages(d);

-- ---- webhook dedup markers, global and untiered ----------------------
--
-- 30 days, and `processed_at` — the column is not called created_at,
-- and a port that guessed wrong 500'd every message-id case in the
-- webhook differential before this was caught there.
INSERT INTO processed_webhooks (svix_msg_id, event_type, processed_at)
SELECT 'loops-' || d, 'probe.event', now()::timestamp - make_interval(days => d)
  FROM (VALUES (1), (29), (31), (400)) AS ages(d);

-- ---- the sentinel reaper --------------------------------------------
--
-- Three sweeps, in order, and a row for every branch of each:
--
--   running    > 20 min  -> error, "Stranded"
--   pending    >  2 min  -> re-fire the wakeup (a COUNT, not a write)
--   pending    >  6 hr   -> error, "Abandoned"
--
-- The 3-hour pending row is the one that separates the second sweep
-- from the third: it is counted for the re-fire and must NOT be
-- abandoned. A port that used one cutoff for both would still report a
-- plausible count.
DELETE FROM sentinel_runs WHERE org_id LIKE 'loops-%';
INSERT INTO sentinel_runs (id, org_id, triggered_at, trigger_type, outcome,
                           started_at, completed_at, tool_call_count)
VALUES
  -- running, well past the 20-minute strand threshold
  ('loops-run-stranded', 'loops-free', now()::timestamp - interval '40 minutes',
   'motion', 'running', now()::timestamp - interval '40 minutes', NULL, 0),
  -- running, INSIDE it: a run that is merely slow is not a run that is lost
  ('loops-run-working',  'loops-free', now()::timestamp - interval '5 minutes',
   'motion', 'running', now()::timestamp - interval '5 minutes', NULL, 0),
  -- running with a NULL started_at. `/start` sets both together, so this
  -- is a row that should not exist — and it must be left alone rather
  -- than reaped on the strength of a NULL comparison.
  ('loops-run-nostart',  'loops-free', now()::timestamp - interval '40 minutes',
   'motion', 'running', NULL, NULL, 0),
  -- pending, old enough to re-fire the wakeup and NOT old enough to abandon
  ('loops-run-pending',  'loops-free', now()::timestamp - interval '3 hours',
   'motion', 'pending', NULL, NULL, 0),
  -- pending, fresh: neither counted nor abandoned
  ('loops-run-fresh',    'loops-free', now()::timestamp - interval '30 seconds',
   'motion', 'pending', NULL, NULL, 0),
  -- pending past six hours: abandoned
  ('loops-run-lost',     'loops-plus', now()::timestamp - interval '9 hours',
   'motion', 'pending', NULL, NULL, 0),
  -- already terminal, and must stay exactly as it is
  ('loops-run-done',     'loops-plus', now()::timestamp - interval '9 hours',
   'motion', 'incident', now()::timestamp - interval '9 hours',
   now()::timestamp - interval '8 hours', 3);
