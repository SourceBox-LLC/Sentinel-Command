# Disaster recovery — Sentinel Command Center

> **Audience:** the operator restoring service after the database is
> lost, corrupted, or the machine/volume is gone.
> **Goal:** get back to a known-good database with the least data loss
> and the least chance of compounding the damage.

This is the runbook `ON_CALL.md` deliberately doesn't cover: not "the
app is slow" but **"the data is gone."** Everything customer-facing —
accounts, cameras, nodes, incidents, MCP keys, audit logs, and the
`Setting(org_plan)` row that links an org to its paid plan — lives in
one SQLite file (`/data/opensentry.db`) on one Fly volume
(`opensentry_data`) on one machine. There is no live replica. Recovery
is **restore from a backup**, so the backup must exist and the restore
must have been rehearsed.

---

## The one thing to do before launch

**Run a real restore drill once, end to end, before onboarding the
first paying customer.** A backup you have never restored is a guess,
not a backup. The drill is in the last section — do it now, not during
an incident.

---

## Backups: how they're produced

`backend/scripts/backup_db.sh` makes a **transactionally consistent**
copy using SQLite's online backup API (not a raw file copy — a raw copy
of a live WAL-mode DB can be torn and un-openable). It checkpoints the
WAL, runs `.backup`, verifies the copy with `PRAGMA integrity_check`,
gzips it, optionally uploads off-platform, and prunes old local copies.

| Env | Default | Purpose |
|---|---|---|
| `DB_PATH` | `/data/opensentry.db` | source DB |
| `BACKUP_DIR` | `/data/backups` | local destination |
| `BACKUP_RETENTION_DAYS` | `14` | local prune window |
| `BACKUP_S3_BUCKET` | _(unset)_ | off-platform target, e.g. `s3://bucket/cc` (needs `aws` CLI + creds) |

> ⚠️ **Local-only backups live on the same volume as the primary.** A
> volume loss takes them with it. Set `BACKUP_S3_BUCKET` (or run
> Litestream) so at least one copy is off-platform. Local copies still
> protect against the far more common case: a bad migration or an
> accidental delete, not a volume loss.

### Schedule it

Two good options:

1. **Fly cron / scheduled machine** running `bash backend/scripts/backup_db.sh`
   daily, with `BACKUP_S3_BUCKET` + AWS creds set as Fly secrets.
2. **A scheduled GitHub Action** that `flyctl ssh console -C` into the
   machine to run the script, then confirms the S3 object exists. (Keep
   `FLY_API_TOKEN` + AWS creds as repo secrets.)

Either way: **also verify Fly's own volume snapshots exist** —
`fly volumes snapshots list <volume-id>` — as a second line of defense,
but treat them as *raw* snapshots (possibly torn) and prefer the
`.backup`-produced copies for restores.

---

## Restore procedure

`backend/scripts/restore_db.sh` makes this executable. It decompresses,
**integrity-checks the backup before touching anything**, moves the
current DB aside to a `.pre-restore-<stamp>` rollback point (never
deletes it), clears stale `-wal`/`-shm`, installs the restored copy, and
re-checks integrity.

**Stop writes first.** Restoring under a live writer corrupts the swap.

```bash
# 1. Stop the app so nothing writes during the swap.
fly status -a opensentry-command                 # note the machine id
fly machine stop <machine-id> -a opensentry-command

# 2. Get a backup onto the machine (if restoring from S3).
fly ssh console -a opensentry-command
#   inside the machine:
#   aws s3 cp s3://bucket/cc/opensentry-<stamp>.db.gz /data/backups/

# 3. Restore (verifies integrity, keeps a rollback copy).
bash /app/backend/scripts/restore_db.sh /data/backups/opensentry-<stamp>.db.gz

# 4. Start the app and verify BEFORE deleting the .pre-restore copy.
exit
fly machine start <machine-id> -a opensentry-command
curl -fsS https://opensentry-command.fly.dev/api/health/ready
```

Then sanity-check in the dashboard: an org loads, cameras list, a known
incident is present, and a paid org still shows its plan. Only after
that, remove `/data/opensentry.db.pre-restore-<stamp>`.

### If the whole machine/volume is gone

1. Recreate the app/volume (`fly volumes create opensentry_data ...`) and
   deploy the image (push to `master`, or `fly deploy`).
2. The app starts on an **empty** DB and recreates the schema on boot.
3. Stop it, pull the latest off-platform backup onto the new volume, and
   run the restore procedure above.
4. Start, verify, and **rotate the Clerk webhook endpoint** if the host
   changed so billing events resume syncing.

### Acceptable data loss (RPO) / time to recover (RTO)

- **RPO:** up to one backup interval (e.g. 24h on a daily schedule).
  Shrink it by running the script more often, or move to Litestream for
  near-continuous replication.
- **RTO:** minutes — dominated by getting the backup onto the volume and
  the ~30–60s app restart. Single-machine means the restore itself is
  downtime; communicate it (see `ON_CALL.md` Scenario E).

---

## Rehearsal drill (do this before launch, then quarterly)

1. `bash backend/scripts/backup_db.sh` on the live machine. Confirm a
   `.db.gz` lands in `BACKUP_DIR` (and in S3 if configured).
2. Copy it to a scratch path and restore into a throwaway DB:
   `DB_PATH=/data/restore-test.db bash backend/scripts/restore_db.sh <backup>`.
3. Open it: `sqlite3 /data/restore-test.db 'PRAGMA integrity_check; SELECT count(*) FROM organizations;'`
   (or any core table) and confirm row counts look sane.
4. Delete the throwaway DB. Write the date + result in this file's log
   below so "last rehearsed" is always visible.

### Rehearsal log

- _(none yet — run the drill before onboarding the first paying customer)_
