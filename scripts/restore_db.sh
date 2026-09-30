#!/usr/bin/env bash
#
# Restore the Command Center database from a dump produced by
# backup_db.sh. Makes the DISASTER_RECOVERY runbook executable instead
# of improvised-at-3am.
#
# WHAT IT DOES
#   1. Verifies the dump is readable (pg_restore --list) BEFORE touching
#      anything — a corrupt dump must not cost you the current database.
#   2. Takes a fresh dump of the CURRENT database to
#      <BACKUP_DIR>/pre-restore-<stamp>.dump. This is the rollback point.
#      The SQLite version of this script could just move the old file
#      aside; a live Postgres has no such move, so the rollback has to be
#      created explicitly. Skipped only with --no-rollback-dump.
#   3. Restores with --clean --if-exists, which drops and recreates each
#      object. This is DESTRUCTIVE and requires typing the confirmation
#      phrase (or passing --yes for unattended use).
#
# IMPORTANT: stop the app first so nothing is writing during the restore.
#   On Fly:  fly machine stop <id>, restore, then start.
#
# USAGE
#   bash backend/scripts/restore_db.sh /data/backups/sentinel-<stamp>.dump
#   DATABASE_URL=postgresql://... bash backend/scripts/restore_db.sh ./backup.dump --yes
#
# ENV
#   DATABASE_URL   required — the database to restore INTO.
#   BACKUP_DIR     default /data/backups (where the rollback dump lands)

set -euo pipefail

BACKUP_DIR="${BACKUP_DIR:-/data/backups}"
SRC=""
ASSUME_YES=false
ROLLBACK_DUMP=true

for arg in "$@"; do
  case "$arg" in
    --yes|-y)          ASSUME_YES=true ;;
    --no-rollback-dump) ROLLBACK_DUMP=false ;;
    -*)                 echo "unknown flag: $arg" >&2; exit 2 ;;
    *)                  SRC="$arg" ;;
  esac
done

log() { printf '[restore_db] %s\n' "$*"; }
die() { printf '[restore_db] ERROR: %s\n' "$*" >&2; exit 1; }

[ -n "$SRC" ] || die "usage: restore_db.sh <backup-file.dump> [--yes] [--no-rollback-dump]"
[ -f "$SRC" ] || die "backup file not found: $SRC"
command -v pg_restore >/dev/null 2>&1 || die "pg_restore not found on PATH"
[ -n "${DATABASE_URL:-}" ] || die "DATABASE_URL is not set"

# See backup_db.sh — libpq does not understand SQLAlchemy's driver suffix.
PG_URL="${DATABASE_URL/postgresql+psycopg:\/\//postgresql://}"
PG_URL="${PG_URL/postgres+psycopg:\/\//postgresql://}"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"

log "verifying the dump is readable before touching the database..."
TOC_LINES="$(pg_restore --list "$SRC" | grep -vc '^;' || true)"
[ "${TOC_LINES:-0}" -gt 0 ] || die "dump has an empty table of contents — NOT restoring"
log "dump looks sane ($TOC_LINES catalog entries)"

TARGET_DESC="$(printf '%s' "$PG_URL" | sed -E 's#//[^:]+:[^@]+@#//***@#')"

if [ "$ASSUME_YES" != true ]; then
  echo
  echo "  About to DROP AND REPLACE every object in:"
  echo "      $TARGET_DESC"
  echo "  from: $SRC"
  echo
  printf '  Type "restore" to proceed: '
  read -r reply
  [ "$reply" = "restore" ] || die "aborted at confirmation"
fi

if [ "$ROLLBACK_DUMP" = true ]; then
  mkdir -p "$BACKUP_DIR"
  ROLLBACK="$BACKUP_DIR/pre-restore-${STAMP}.dump"
  log "dumping the CURRENT database to $ROLLBACK (rollback point)..."
  # Not fatal if the current database is already unusable — that's often
  # exactly why someone is restoring. Warn loudly and continue.
  if pg_dump "$PG_URL" --format=custom --compress=9 --no-owner \
       --no-privileges --file="$ROLLBACK"; then
    log "rollback point written ($(du -h "$ROLLBACK" | cut -f1))"
  else
    log "WARNING: could not dump the current database — proceeding with NO rollback point"
    rm -f "$ROLLBACK"
  fi
fi

log "restoring..."
# --clean --if-exists: drop each object before recreating it, tolerating
# ones that aren't there (a restore into an empty database is normal).
# --exit-on-error is deliberately NOT set: --clean emits benign "does not
# exist" noise, and a mid-restore abort would leave a half-populated
# database. The verification below is what decides success.
# The dump file is pg_restore's ONE positional argument — the connection
# string goes in --dbname. Passing the URL positionally as well makes the
# dump look like a second positional and fails with "too many
# command-line arguments" before anything is restored.
pg_restore \
  --clean --if-exists \
  --no-owner --no-privileges \
  --dbname "$PG_URL" \
  "$SRC" || log "pg_restore reported errors — verifying what actually landed"

log "post-restore verification..."
TABLES="$(psql "$PG_URL" -tAc \
  "select count(*) from information_schema.tables where table_schema='public'")"
log "tables present after restore: $TABLES"
[ "${TABLES:-0}" -gt 0 ] || die "no tables present after restore — investigate before starting the app"

log "done. Start the app and verify before deleting the pre-restore dump."
