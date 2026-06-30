#!/usr/bin/env bash
#
# Restore the Command Center database from a backup produced by
# backup_db.sh. Makes the DISASTER_RECOVERY runbook executable instead
# of improvised-at-3am.
#
# WHAT IT DOES
#   1. Decompresses the chosen backup (if .gz) to a temp file.
#   2. Runs PRAGMA integrity_check on it and aborts if not "ok".
#   3. Moves the CURRENT db aside to <db>.pre-restore-<stamp> (never
#      destroys the existing file — you can roll the restore back).
#   4. Puts the restored copy in place and removes any stale -wal/-shm.
#
# IMPORTANT: stop the app first so nothing is writing during the swap.
#   On Fly:  fly machine stop <id>   (or scale to 0), restore, then start.
#
# USAGE
#   bash backend/scripts/restore_db.sh /data/backups/opensentry-<stamp>.db.gz
#   DB_PATH=./opensentry.db bash backend/scripts/restore_db.sh ./backup.db.gz
#
# ENV
#   DB_PATH   default /data/opensentry.db

set -euo pipefail

DB_PATH="${DB_PATH:-/data/opensentry.db}"
SRC="${1:-}"

log() { printf '[restore_db] %s\n' "$*"; }
die() { printf '[restore_db] ERROR: %s\n' "$*" >&2; exit 1; }

[ -n "$SRC" ] || die "usage: restore_db.sh <backup-file(.db|.db.gz)>"
[ -f "$SRC" ] || die "backup file not found: $SRC"
command -v sqlite3 >/dev/null 2>&1 || die "sqlite3 not found on PATH"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
TMP="$(mktemp "${TMPDIR:-/tmp}/cc-restore-XXXXXX.db")"
trap 'rm -f "$TMP"' EXIT

case "$SRC" in
  *.gz) log "decompressing $SRC..."; gzip -dc "$SRC" > "$TMP" ;;
  *)    log "copying $SRC..."; cp "$SRC" "$TMP" ;;
esac

log "verifying integrity of the backup before swapping..."
RESULT="$(sqlite3 "$TMP" 'PRAGMA integrity_check;')"
[ "$RESULT" = "ok" ] || die "integrity_check failed on the backup ($RESULT) — NOT restoring"

if [ -f "$DB_PATH" ]; then
  ASIDE="${DB_PATH}.pre-restore-${STAMP}"
  log "moving current DB aside to $ASIDE (rollback point)..."
  mv "$DB_PATH" "$ASIDE"
fi

# Clear any stale WAL/SHM from the old DB so SQLite doesn't try to
# replay them onto the freshly-restored file.
rm -f "${DB_PATH}-wal" "${DB_PATH}-shm"

log "installing restored DB at $DB_PATH..."
cp "$TMP" "$DB_PATH"

log "post-restore integrity check..."
RESULT="$(sqlite3 "$DB_PATH" 'PRAGMA integrity_check;')"
[ "$RESULT" = "ok" ] || die "post-restore integrity_check failed ($RESULT) — investigate before starting the app"

log "done. Start the app and verify before deleting the .pre-restore copy."
