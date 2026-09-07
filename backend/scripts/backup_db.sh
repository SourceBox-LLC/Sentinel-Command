#!/usr/bin/env bash
#
# Consistent Postgres backup for the Command Center database.
#
# WHY THIS EXISTS
#   The managed cluster takes its own snapshots, and those are the
#   PRIMARY backup. This script exists for the thing snapshots can't do:
#   produce a portable dump that can be restored somewhere else — a
#   different provider, a local machine, a fresh cluster — so a Fly
#   account/region loss can't take the backups down with the primary.
#   It is also the only copy you can inspect before restoring.
#
#   (Before 2026-09, this backed up a SQLite file with the online
#   `.backup` API. The database moved to Postgres; the shape of the job
#   is deliberately unchanged so the workflow and runbook around it
#   still apply.)
#
# WHAT IT DOES
#   1. Dumps with pg_dump in custom format (-Fc): compressed, and
#      restorable selectively with pg_restore (single table, schema
#      only, reordered) rather than all-or-nothing.
#   2. Verifies the dump by reading its table of contents back with
#      `pg_restore --list` — the moral equivalent of the old
#      PRAGMA integrity_check, and it catches a truncated file.
#   3. If BACKUP_S3_BUCKET is set and the `aws` CLI is present, uploads
#      it to object storage (off-platform durability).
#   4. Prunes local backups older than BACKUP_RETENTION_DAYS.
#
# USAGE
#   On the Fly machine:   bash backend/scripts/backup_db.sh
#   Locally:              DATABASE_URL=postgresql://... bash backend/scripts/backup_db.sh
#
# ENV
#   DATABASE_URL           required. Read from the app's own environment
#                          on the machine, so there is no second copy of
#                          the credential to drift.
#   BACKUP_DIR             default /data/backups
#   BACKUP_RETENTION_DAYS  default 14  (local copies)
#   BACKUP_S3_BUCKET       optional, e.g. s3://my-bucket/cc-backups
#                          (requires the aws CLI + credentials in env)
#
# Run it from a scheduled GitHub Action (see .github/workflows/backup.yml
# and docs/runbooks/DISASTER_RECOVERY.md).

set -euo pipefail

BACKUP_DIR="${BACKUP_DIR:-/data/backups}"
RETENTION_DAYS="${BACKUP_RETENTION_DAYS:-14}"

log() { printf '[backup_db] %s\n' "$*"; }
die() { printf '[backup_db] ERROR: %s\n' "$*" >&2; exit 1; }

command -v pg_dump >/dev/null 2>&1 || die "pg_dump not found on PATH"
[ -n "${DATABASE_URL:-}" ] || die "DATABASE_URL is not set"

# SQLAlchemy's driver suffix is meaningless to libpq: it parses
# `postgresql+psycopg://` as scheme "postgresql+psycopg" and fails with
# an unhelpful "invalid URI". The app needs the suffix, pg_dump must not
# see it — so strip it here rather than keeping two spellings of the URL.
PG_URL="${DATABASE_URL/postgresql+psycopg:\/\//postgresql://}"
PG_URL="${PG_URL/postgres+psycopg:\/\//postgresql://}"

case "$PG_URL" in
  postgresql://*|postgres://*) ;;
  *) die "DATABASE_URL is not a Postgres URL (got: ${PG_URL%%://*}://...)" ;;
esac

# pg_dump refuses outright when the server is a NEWER major than the
# client ("aborting because of server version mismatch") — it cannot know
# about catalog changes that postdate it. The Dockerfile pins
# postgresql-client-18 from PGDG for exactly this reason; Debian
# bookworm's default client is 15 and would fail against our 18.x server.
# Verified directly, not assumed.
log "pg_dump $(pg_dump --version | awk '{print $3}') -> server $(psql "$PG_URL" -tAc 'show server_version' 2>/dev/null || echo '?')"

mkdir -p "$BACKUP_DIR"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
FINAL="$BACKUP_DIR/sentinel-${STAMP}.dump"

log "dumping (custom format, compressed)..."
# --no-owner / --no-privileges: the role names are Fly-attachment
# specific. Keeping them would make the dump refuse to restore anywhere
# the same roles don't exist — exactly the portability this job is for.
pg_dump "$PG_URL" \
  --format=custom \
  --compress=9 \
  --no-owner \
  --no-privileges \
  --file="$FINAL"

log "verifying the dump is readable..."
TOC_LINES="$(pg_restore --list "$FINAL" | grep -vc '^;' || true)"
[ "${TOC_LINES:-0}" -gt 0 ] || { rm -f "$FINAL"; die "dump has an empty table of contents — treating as corrupt"; }

SIZE="$(du -h "$FINAL" | cut -f1)"
log "wrote $FINAL ($SIZE, $TOC_LINES catalog entries)"

if [ -n "${BACKUP_S3_BUCKET:-}" ]; then
  if command -v aws >/dev/null 2>&1; then
    log "uploading to ${BACKUP_S3_BUCKET}/ ..."
    aws s3 cp "$FINAL" "${BACKUP_S3_BUCKET%/}/$(basename "$FINAL")"
    log "off-platform upload complete"
  else
    log "WARNING: BACKUP_S3_BUCKET set but 'aws' CLI not found — skipping off-platform upload"
  fi
else
  log "BACKUP_S3_BUCKET not set — local copy only. Managed cluster snapshots"
  log "are the primary backup; this file is the portable secondary."
fi

log "pruning local backups older than ${RETENTION_DAYS} days..."
find "$BACKUP_DIR" -name 'sentinel-*.dump' -type f -mtime "+${RETENTION_DAYS}" -print -delete || true
# Sweep the pre-migration SQLite-era artifacts too, on the same clock.
find "$BACKUP_DIR" -name 'sentinel-*.db.gz' -type f -mtime "+${RETENTION_DAYS}" -print -delete || true

log "done."
