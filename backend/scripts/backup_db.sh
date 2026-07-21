#!/usr/bin/env bash
#
# Consistent SQLite backup for the Command Center database.
#
# WHY THIS EXISTS
#   The whole product is one SQLite file on one Fly volume on one VM.
#   A raw `fly volumes snapshot` taken while the app is writing in WAL
#   mode can capture a torn, un-openable database — you only find out
#   mid-disaster. This script produces a *transactionally consistent*
#   copy using SQLite's online backup API, verifies it, and (optionally)
#   ships it off-platform so a Fly account/region/volume loss can't take
#   the backups down with the primary.
#
# WHAT IT DOES
#   1. Checkpoints the WAL into the main DB file.
#   2. Uses `.backup` (online backup API — safe while the app runs) to
#      make a consistent snapshot.
#   3. Runs PRAGMA integrity_check on the COPY and aborts if it's not "ok".
#   4. gzips it.
#   5. If BACKUP_S3_BUCKET is set and the `aws` CLI is present, uploads
#      it to object storage (off-platform durability).
#   6. Prunes local backups older than BACKUP_RETENTION_DAYS.
#
# USAGE
#   On the Fly machine:   bash backend/scripts/backup_db.sh
#   Locally:              DB_PATH=./sentinel.db bash backend/scripts/backup_db.sh
#
# ENV
#   DB_PATH                default /data/sentinel.db
#   BACKUP_DIR             default /data/backups
#   BACKUP_RETENTION_DAYS  default 14  (local copies)
#   BACKUP_S3_BUCKET       optional, e.g. s3://my-bucket/cc-backups
#                          (requires the aws CLI + credentials in env)
#
# Run it from cron / a scheduled GitHub Action (see
# docs/runbooks/DISASTER_RECOVERY.md). Consider Litestream for
# continuous replication once usage warrants it — this script is the
# minimum viable, rehearsable safety net.

set -euo pipefail

DB_PATH="${DB_PATH:-/data/sentinel.db}"
BACKUP_DIR="${BACKUP_DIR:-/data/backups}"
RETENTION_DAYS="${BACKUP_RETENTION_DAYS:-14}"

log() { printf '[backup_db] %s\n' "$*"; }
die() { printf '[backup_db] ERROR: %s\n' "$*" >&2; exit 1; }

command -v sqlite3 >/dev/null 2>&1 || die "sqlite3 not found on PATH"

# Resolve the live DB file. The rename (opensentry → sentinel, PR #53)
# changed DATABASE_URL and this script's default to /data/sentinel.db, but
# the data file on the existing Fly volume was NOT renamed — it still lives
# at /data/opensentry.db. Hard-failing here means the daily backup safety
# net goes dark while production runs against the orphaned old file. So:
# if the expected path is missing, look for the pre-rename fallback in the
# same directory and back THAT up, with a loud warning. This keeps real
# data protected through the rename dust and surfaces the drift instead of
# masking it. The proper fix is to rename the file on the volume
# (ops runbook), but the backup must not wait for it.
if [ ! -f "$DB_PATH" ]; then
  DB_DIR="$(dirname "$DB_PATH")"
  FALLBACK="$DB_DIR/opensentry.db"       # pre-rename name (hard-coded; the only legacy alias we carry)
  if [ -f "$FALLBACK" ]; then
    # Keep WAL/SHM siblings consistent if present.
    [ -f "$FALLBACK-wal" ] && cp "$FALLBACK-wal" "$DB_PATH-wal"
    [ -f "$FALLBACK-shm" ] && cp "$FALLBACK-shm" "$DB_PATH-shm"
    printf '[backup_db] WARNING: %s not found — backing up the pre-rename %s instead.\n' "$DB_PATH" "$FALLBACK" >&2
    printf '[backup_db] WARNING: this means DATABASE_URL points at %s but the\n' "$DB_PATH" >&2
    printf '[backup_db]          real data still lives at %s. The app is running against\n' "$FALLBACK" >&2
    printf '[backup_db]          the WRONG file (or no file). See docs/runbooks for the\n' >&2
    printf '[backup_db]          rename-volume-file ops fix. Backup proceeds on the real data.\n' >&2
    DB_PATH="$FALLBACK"
  else
    die "database not found at $DB_PATH (and no pre-rename fallback at $FALLBACK)"
  fi
fi

mkdir -p "$BACKUP_DIR"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
WORK="$BACKUP_DIR/sentinel-${STAMP}.db"
FINAL="${WORK}.gz"

log "checkpointing WAL into the main DB file..."
# TRUNCATE so the -wal file is folded in and reset; harmless if already small.
sqlite3 "$DB_PATH" 'PRAGMA wal_checkpoint(TRUNCATE);' >/dev/null

log "creating consistent backup via the online backup API..."
# .backup is safe to run against a live DB — it copies a consistent
# snapshot even while the app keeps writing.
sqlite3 "$DB_PATH" ".backup '$WORK'"

log "verifying integrity of the backup copy..."
RESULT="$(sqlite3 "$WORK" 'PRAGMA integrity_check;')"
if [ "$RESULT" != "ok" ]; then
  rm -f "$WORK"
  die "integrity_check failed on the backup: $RESULT"
fi

log "compressing..."
gzip -f "$WORK"
SIZE="$(du -h "$FINAL" | cut -f1)"
log "wrote $FINAL ($SIZE)"

if [ -n "${BACKUP_S3_BUCKET:-}" ]; then
  if command -v aws >/dev/null 2>&1; then
    log "uploading to ${BACKUP_S3_BUCKET}/ ..."
    aws s3 cp "$FINAL" "${BACKUP_S3_BUCKET%/}/$(basename "$FINAL")"
    log "off-platform upload complete"
  else
    log "WARNING: BACKUP_S3_BUCKET set but 'aws' CLI not found — skipping off-platform upload"
  fi
else
  log "BACKUP_S3_BUCKET not set — local backup only (NOT off-platform; set it for real durability)"
fi

log "pruning local backups older than ${RETENTION_DAYS} days..."
find "$BACKUP_DIR" -name 'sentinel-*.db.gz' -type f -mtime "+${RETENTION_DAYS}" -print -delete || true

log "done."
