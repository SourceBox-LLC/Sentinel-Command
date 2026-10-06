-- Undo loops_fixture.sql, so the shared database is left as the other
-- harnesses expect to find it.
--
-- This is not tidiness. `seed_cameras.sql` restarts `settings_id_seq` at
-- 1 and then deletes only `org_id IN ('self-host', 'other-org')` — so
-- rows this fixture added under `loops-%` and `rec-%` SURVIVE a reseed,
-- and the restarted sequence hands out ids those survivors already hold.
-- The write differential's very first case then dies on
-- `duplicate key value violates unique constraint "settings_pkey"` and
-- reports that the run proves nothing. Which it did, once.
--
-- Run from the runner's EXIT trap, so a failed or interrupted run does
-- not leave the next harness broken.
DELETE FROM notifications      WHERE org_id LIKE 'loops-%' OR kind = 'motion_digest';
DELETE FROM stream_access_logs WHERE org_id LIKE 'loops-%';
DELETE FROM mcp_activity_logs  WHERE org_id LIKE 'loops-%';
DELETE FROM audit_log          WHERE org_id LIKE 'loops-%';
DELETE FROM motion_events      WHERE org_id LIKE 'loops-%';
DELETE FROM email_log          WHERE org_id LIKE 'loops-%';
-- The two single-table orgs the retention mutations need, one of which
-- has an EMPTY org_id and so matches no LIKE pattern.
DELETE FROM audit_log          WHERE org_id = '';
DELETE FROM email_outbox       WHERE org_id LIKE 'loops-%';
DELETE FROM sentinel_runs      WHERE org_id LIKE 'loops-%';
DELETE FROM cameras            WHERE org_id LIKE 'loops-%' OR org_id LIKE 'rec-%';
DELETE FROM camera_nodes       WHERE org_id LIKE 'loops-%';
DELETE FROM settings           WHERE org_id LIKE 'loops-%' OR org_id LIKE 'rec-%';
DELETE FROM processed_webhooks WHERE svix_msg_id LIKE 'loops-%';

-- The licence and sync rows this fixture writes under the REAL local
-- org, which `seed_cameras.sql` does delete — but only as part of its
-- own settings block, and leaving them behind would hand the next
-- harness a self-host install that believes it has a valid licence and
-- the sync entitlement.
DELETE FROM settings WHERE org_id = 'self-host'
   AND (key LIKE 'sentinel_license_%' OR key = 'sentinel_data_sync_enabled'
        OR key LIKE 'sentinel_sync_cursor_%' OR key = 'sentinel_install_id');
