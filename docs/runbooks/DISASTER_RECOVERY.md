# Disaster recovery — Sentinel Command Center

> **Audience:** the operator restoring service after the database is
> lost, corrupted, or the machine/volume is gone.
> **Goal:** get back to a known-good database with the least data loss
> and the least chance of compounding the damage.

This is the runbook `ON_CALL.md` deliberately doesn't cover: not "the
app is slow" but **"the data is gone."** Everything customer-facing —
accounts, cameras, nodes, incidents, MCP keys, audit logs, and the
`Setting(org_plan)` row that links an org to its paid plan — lives in
one SQLite file on one Fly volume (`sentinel_data`) on one machine.
There is no live replica. Recovery is **restore from a backup**, so the
backup must exist and the restore must have been rehearsed.

> ✅ **Filename reconciled (2026-07-06) — this runbook's `sentinel.db`
> paths are now correct.** A discrepancy was found and fixed the same
> day: the live DB was `/data/opensentry.db` while all tooling expected
> `sentinel.db`, because a `DATABASE_URL` **secret** (which overrides the
> `fly.toml` env) was still pinned to the old OpenSentry-era filename. The
> daily `backup_db.sh` job had been *failing* on the missing
> `/data/sentinel.db` since the volume was recreated on ~07-05. Fix
> applied: the secret now points to `sqlite:////data/sentinel.db`; the app
> was restarted onto a fresh `sentinel.db` (the old file held zero rows —
> pre-launch, no data lost); the leftover `opensentry.db` and the orphaned
> `opensentry_data` volume were removed; and the backup job now succeeds
> (verified — `sentinel-…​.db.gz` produced in `/data/backups`). Still
> open: set the `BACKUP_ENCRYPTION_KEY` repo secret so backups also get an
> off-platform encrypted copy (today they're Fly-volume-local + Fly
> snapshots only).

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
| `DB_PATH` | `/data/sentinel.db` | source DB |
| `BACKUP_DIR` | `/data/backups` | local destination |
| `BACKUP_RETENTION_DAYS` | `14` | local prune window |
| `BACKUP_S3_BUCKET` | _(unset)_ | off-platform target, e.g. `s3://bucket/cc` (needs `aws` CLI + creds) |

> ⚠️ **Local-only backups live on the same volume as the primary.** A
> volume loss takes them with it. Set `BACKUP_S3_BUCKET` (or run
> Litestream) so at least one copy is off-platform. Local copies still
> protect against the far more common case: a bad migration or an
> accidental delete, not a volume loss.

### Schedule it — WIRED: `.github/workflows/backup.yml`

A scheduled GitHub Action runs daily (09:17 UTC, plus manual
`workflow_dispatch`):

1. Executes `bash /app/scripts/backup_db.sh` on the Fly machine (note:
   the Dockerfile copies `backend/` to `/app/`, so scripts live at
   `/app/scripts/`, **not** `/app/backend/scripts/`). Local copies land
   in `/data/backups` with 14-day pruning.
2. **Off-platform copy** — if the `BACKUP_ENCRYPTION_KEY` repo secret is
   set, the workflow pulls the newest backup, encrypts it with
   AES-256-CBC/PBKDF2, and stores it as a GitHub Actions artifact
   (30-day retention). The repo is public, so ONLY ciphertext is ever
   uploaded; without the key the workflow warns and stays local-only.
   Decrypt with:

   ```bash
   openssl enc -d -aes-256-cbc -pbkdf2 -iter 200000 \
     -in backup.db.gz.enc -out backup.db.gz -pass pass:<key>
   ```

   Generate + set the key once: `openssl rand -hex 32` → repo secret
   `BACKUP_ENCRYPTION_KEY` → **store the same key in your password
   manager** (an encrypted backup with a lost key is no backup).
3. Prints the Fly volume snapshot list for visibility.

`BACKUP_S3_BUCKET` on the Fly app remains the alternative/additional
off-platform target supported by the script itself.

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
fly status -a sentinel-command                 # note the machine id
fly machine stop <machine-id> -a sentinel-command

# 2. Get a backup onto the machine (if restoring from S3).
fly ssh console -a sentinel-command
#   inside the machine:
#   aws s3 cp s3://bucket/cc/sentinel-<stamp>.db.gz /data/backups/

# 3. Restore (verifies integrity, keeps a rollback copy).
bash /app/scripts/restore_db.sh /data/backups/sentinel-<stamp>.db.gz

# 4. Start the app and verify BEFORE deleting the .pre-restore copy.
exit
fly machine start <machine-id> -a sentinel-command
curl -fsS https://sentinel-command.com/api/health/ready
```

Then sanity-check in the dashboard: an org loads, cameras list, a known
incident is present, and a paid org still shows its plan. Only after
that, remove `/data/sentinel.db.pre-restore-<stamp>`.

### If the whole machine/volume is gone

1. Recreate the app/volume (`fly volumes create sentinel_data ...`) and
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

- **2026-07-06 — Fly volume-snapshot restore drill. PASS (with a
  finding).** Restored the newest snapshot (`vs_ggX2o2kX99xofojJJAJ35`,
  ~4h old, 5-day retention) of `sentinel_data` into a throwaway volume
  (`sentinel_restore_test`), mounted it on a disposable alpine machine,
  and verified: `PRAGMA integrity_check` → **ok**; all **20 tables**
  present. Row counts were 0 across the board — expected pre-launch (no
  orgs onboarded yet). Throwaway machine + volume destroyed after;
  production volume/machine never touched (no downtime). **Finding:** the
  DB file is `/data/opensentry.db`, not `sentinel.db`, and there was no
  `/data/backups/` directory. **Resolved same day** (see the reconciled
  callout at the top): the `DATABASE_URL` secret was repointed to
  `sentinel.db`, the app restarted onto a fresh `sentinel.db`, the
  leftover `opensentry.db` + orphaned `opensentry_data` volume were
  removed, and a `workflow_dispatch` of the backup job then **succeeded**
  (`sentinel-20260706T064510Z.db.gz`). `/api/health/detailed` reported
  `database: ok` after the switch. **Re-run this drill once there is real
  customer data to restore (non-zero rows).**
