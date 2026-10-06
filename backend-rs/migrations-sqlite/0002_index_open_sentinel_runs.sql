-- The agent's queue, indexed by what it asks for.
--
-- `GET /api/sentinel/runs/pending` (every wakeup, and every 30 s from a
-- polling agent) and the reaper both look for runs that are still open,
-- oldest first. Open runs are a handful; terminal runs accumulate at up
-- to the plan cap per org per month and are never pruned. Without this,
-- each poll walked the triggered_at index past every finished run to
-- find the few that were not. Partial, so it stays the size of the queue.
CREATE INDEX IF NOT EXISTS ix_sentinel_runs_open
    ON sentinel_runs (triggered_at)
 WHERE outcome IN ('pending', 'running');
