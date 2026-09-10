# Disaster recovery — Sentinel Command Center

> **Audience:** the operator restoring service after the database is
> lost, corrupted, or the machine/volume is gone.
> **Goal:** get back to a known-good database with the least data loss
> and the least chance of compounding the damage.

This is the runbook `ON_CALL.md` deliberately doesn't cover: not "the
app is slow" but **"the data is gone."** Everything customer-facing —
accounts, cameras, nodes, incidents, MCP keys, audit logs, and the
`Setting(org_plan)` row that links an org to its paid plan — lives in
the **`sentinel_command` database on the managed `sentinel-postgres`
Postgres cluster**. Recovery is **restore from a backup**, so the backup
must exist and the restore must have been rehearsed.

> 🔀 **Migrated to Postgres (2026-09-07).** Until this date the hosted
> database was a single SQLite file on the `sentinel_data` Fly volume,
> and this runbook was written around that. What changed:
>
> - The database is `sentinel_command` on the **`sentinel-postgres`**
>   cluster — one cluster hosting one database per service
>   (`sentinel_command`, `sentinel_license`, `sentinel_sync`).
>   `DATABASE_URL` is now a **Fly secret**, not a `fly.toml` env value,
>   because it carries a password.
> - The `sentinel_data` volume still exists and still holds HLS segment
>   working files and `/data/backups`. It no longer holds the database,
>   so **losing the volume is no longer losing the data.**
>
>   ⚠️ **Failure domain, stated precisely.** Moving off SQLite made the
>   database survive losing an app machine, but it also made all three
>   services depend on one Postgres app. Before, Command Center and
>   License Service each ran SQLite on their own volume and failed
>   alone; now a `sentinel-postgres` outage takes down all three at
>   once. That is a deliberate trade — one cluster is a third of the
>   cost and a third of the operational surface — but it is a real
>   regression in blast radius and should not be forgotten.
>
>   **Single node, no replica — decided 2026-09-07.** The cluster runs
>   one machine (`shared-cpu-1x:512MB`, one `pg_data` volume) and a
>   standby was considered and declined. The reasoning: a replica buys
>   *uptime*, not durability, and durability is already covered by
>   snapshots plus the portable dumps, with the restore path rehearsed
>   and passing. At current scale the cost and operational surface of a
>   second node is not worth the uptime it would buy.
>
>   What that means when it bites: a node failure is **downtime for all
>   three services** until Fly restarts the machine, or — in a genuine
>   host/volume loss — until someone restores from a snapshot. There is
>   no automatic failover and nothing to promote. Budget for a recovery
>   measured in minutes, not seconds.
>
>   `fly machine clone -a sentinel-postgres` is how you'd add a standby
>   if the trade ever stops making sense.
> - `backup_db.sh` / `restore_db.sh` are `pg_dump` / `pg_restore` now.
>   The managed cluster's own snapshots became the *primary* backup.
> - The pre-migration SQLite file is still at `/data/sentinel.db` (and
>   `/data/backups/sentinel-20260907T023056Z.db.gz`) as the rollback
>   point. Delete it once you're confident, not before.
>
> **Superseded history (2026-07-06):** a `DATABASE_URL` secret pinned to
> the old OpenSentry-era filename silently overrode `fly.toml` and made
> the daily backup fail on a missing `/data/sentinel.db` for ~1 day. The
> lesson outlived the SQLite setup and is why the note above spells out
> that `DATABASE_URL` is a secret: **the secret always wins over
> `fly.toml`, so check `fly secrets list` before believing the config
> file.**
>
> **Decided 2026-09-07: backups live only on Fly.** The operator
> accepted this explicitly. `BACKUP_ENCRYPTION_KEY` and
> `BACKUP_S3_BUCKET` are intentionally unset, so there is no
> off-platform copy and that is not a gap to be closed. The reasoning
> is that cluster snapshots plus the portable dumps cover the failure
> modes that actually happen (bad migration, accidental delete, cluster
> loss), and the residual — losing the Fly account itself — is accepted.
> Both code paths remain in place, so reversing the decision is a single
> repo secret and no code change.
>
> ⚠️ **The cluster is new, so snapshot history is short.**
> `sentinel-postgres` was created 2026-09-07; its snapshot history
> starts then and reaches full 5-day depth around 2026-09-12. Until
> then the only recovery points are the `pg_dump` files in
> `/data/backups` and the dumps saved off-platform to
> `~/sentinel-db-rollback/` on the operator's machine.

> 🔒 **Resolved (2026-09-07): cross-service database access — and it
> stays resolved only if you re-apply this after any new attach.**
>
> `fly postgres attach` creates every role as a Postgres **SUPERUSER**.
> On a shared cluster that meant any single leaked `DATABASE_URL` was
> full read/write on all three databases — including `sentinel_sync`,
> which holds self-hosted customers' mirrored data. Verified at the
> time, not assumed.
>
> The fix is role hardening rather than separate clusters. Per database:
>
> ```sql
> ALTER DATABASE sentinel_command OWNER TO sentinel_command;
> REVOKE CONNECT ON DATABASE sentinel_command FROM PUBLIC;
> ALTER ROLE sentinel_command NOSUPERUSER;
> ```
>
> Ownership must move **before** dropping superuser: the database is
> created owned by `postgres` and the `public` schema by
> `pg_database_owner`, so a bare `NOSUPERUSER` strips the app's ability
> to create its own tables and breaks `ensure_schema` on the next boot.
> This was rehearsed on a scratch cluster first — the full 816-test
> suite passes against a hardened, role-owned, non-superuser database,
> which is what proves the app still has the rights it needs.
>
> Verified afterwards on the real cluster: **all six** cross-database
> connection attempts refused, all three own-database connections work,
> `rolsuper=false` on every service role, and each database owned by its
> own role. `fly postgres db list` shows it plainly — each database now
> lists only its own role.
>
> ⚠️ **`fly postgres attach` will recreate a superuser role.** Any
> service added later, or any re-attach, reopens this. Re-run the three
> statements above for the new database and re-verify.

---

## The one thing to do before launch

**Run a real restore drill once, end to end, before onboarding the
first paying customer.** A backup you have never restored is a guess,
not a backup. The drill is in the last section — do it now, not during
an incident.

---

## Backups: how they're produced

There are **two independent layers**, and it matters which one you reach
for:

1. **Managed cluster snapshots — the primary.** Fly takes these on the
   `sentinel-postgres` cluster automatically. They are the fastest path
   back and the one that covers "the cluster is gone". Check them with
   `fly volumes list -a sentinel-postgres` and
   `fly volumes snapshots list <volume-id>`.
2. **`backend/scripts/backup_db.sh` — the portable secondary.** Runs
   `pg_dump --format=custom` (compressed; restorable *selectively* with
   `pg_restore`, not just all-or-nothing), verifies the result by
   reading its table of contents back with `pg_restore --list`,
   optionally uploads off-platform, and prunes old local copies. This is
   the layer that restores onto **any** Postgres anywhere — a different
   provider, a laptop — which is exactly what a Fly account loss needs.

| Env | Default | Purpose |
|---|---|---|
| `DATABASE_URL` | _(required)_ | source DB — read from the machine's own env, so there is no second copy of the credential to drift |
| `BACKUP_DIR` | `/data/backups` | local destination |
| `BACKUP_RETENTION_DAYS` | `14` | local prune window |
| `BACKUP_S3_BUCKET` | _(unset)_ | off-platform target, e.g. `s3://bucket/cc` (needs `aws` CLI + creds) |

> **Every copy of the database is inside Fly — by choice.** Verified
> 2026-09-07 and accepted by the operator the same day, so read this as
> the deliberate position rather than an outstanding risk:
>
> | Copy | Where it lives | Status |
> |---|---|---|
> | Cluster snapshots | Fly | ✅ primary — daily, **5-day retention** |
> | Portable `pg_dump` | Fly volume (`/data/backups`) | ✅ secondary, 14-day prune |
> | Encrypted GH artifact | GitHub | ⬜ off by choice (`BACKUP_ENCRYPTION_KEY` unset) |
> | S3 | elsewhere | ⬜ off by choice (`BACKUP_S3_BUCKET` unset) |
>
> **What this buys and what it costs.** Recovery from a bad migration,
> an accidental delete, or cluster loss is covered. The recovery window
> is about **5 days** — anything older than the snapshot retention is
> gone. A Fly account suspension or billing lapse would take every copy
> simultaneously; that is the accepted risk. Keep the Fly account's
> billing current and its login secured, because that account is now
> the single thing standing between you and total data loss.

**Two gotchas the scripts handle for you, worth knowing before you run
`pg_dump` by hand:**

- `DATABASE_URL` is `postgresql+psycopg://…`. That `+psycopg` suffix is
  a SQLAlchemy driver selector; **libpq does not understand it** and
  fails with an unhelpful "invalid URI". The scripts strip it.
- `pg_dump` **refuses** to dump a server whose major version is newer
  than its own ("aborting because of server version mismatch"). The
  cluster is 18.x and Debian bookworm ships client 15, so the Dockerfile
  pins `postgresql-client-18` from PGDG. If you dump from your laptop,
  check `pg_dump --version` first.

### Schedule it — WIRED: `.github/workflows/backup.yml`

A scheduled GitHub Action runs daily (09:17 UTC, plus manual
`workflow_dispatch`):

1. Executes `bash /app/scripts/backup_db.sh` on the Fly machine (note:
   the Dockerfile copies `backend/` to `/app/`, so scripts live at
   `/app/scripts/`, **not** `/app/backend/scripts/`). It runs *on the
   machine* so `DATABASE_URL` never has to be copied into GitHub
   secrets. Dumps land in `/data/backups` with 14-day pruning.
2. **Off-platform copy** — if the `BACKUP_ENCRYPTION_KEY` repo secret is
   set, the workflow pulls the newest dump, encrypts it with
   AES-256-CBC/PBKDF2, and stores it as a GitHub Actions artifact
   (30-day retention). The repo is public, so ONLY ciphertext is ever
   uploaded; without the key the workflow warns and stays local-only.
   Decrypt with:

   ```bash
   openssl enc -d -aes-256-cbc -pbkdf2 -iter 200000 \
     -in backup.dump.enc -out backup.dump -pass pass:<key>
   ```

   This path is **off by decision** (2026-09-07) — see the note near the
   top. If it is ever turned back on: `openssl rand -hex 32` → repo
   secret `BACKUP_ENCRYPTION_KEY` → **store the same key in your
   password manager**, because an encrypted backup with a lost key is
   no backup.
3. Prints the cluster's snapshot list for visibility.

`BACKUP_S3_BUCKET` on the Fly app remains the alternative/additional
off-platform target supported by the script itself.

**Sentinel-License-Service has its own identical job** in that repo
(`.github/workflows/backup.yml`, 09:47 UTC). Its database is on the same
cluster; the two are separate databases with separate dumps. If you
change one workflow, change the other.

---

## Restore procedure

`backend/scripts/restore_db.sh` makes this executable. It **verifies the
dump is readable before touching anything**, dumps the current database
to a `pre-restore-<stamp>.dump` rollback point, then restores with
`pg_restore --clean --if-exists`.

> ⚠️ **This is destructive in a way the SQLite version was not.** That
> script moved a file aside; a live Postgres has no such move, so the
> rollback point has to be *created* — which is why step 2 exists and why
> the script prompts for a typed confirmation unless you pass `--yes`.

**Stop writes first.** Restoring under a live writer gives you a mix of
old and new rows.

```bash
# 1. Stop the app so nothing writes during the restore.
fly status -a sentinel-command                 # note the machine id
fly machine stop <machine-id> -a sentinel-command

# 2. Get a dump onto the machine (if restoring from S3 or an artifact).
fly ssh console -a sentinel-command
#   inside the machine:
#   aws s3 cp s3://bucket/cc/sentinel-<stamp>.dump /data/backups/

# 3. Restore (verifies the dump, writes a rollback point, prompts).
bash /app/scripts/restore_db.sh /data/backups/sentinel-<stamp>.dump

# 4. Start the app and verify BEFORE deleting the pre-restore dump.
exit
fly machine start <machine-id> -a sentinel-command
curl -fsS https://sentinel-command.com/api/health/ready
```

Then sanity-check in the dashboard: an org loads, cameras list, a known
incident is present, and a paid org still shows its plan. Only after
that, remove `/data/backups/pre-restore-<stamp>.dump`.

**Restoring into a fresh database instead** (preferred when you want the
old one kept for comparison): create a database on the cluster, point
`DATABASE_URL` at it, restore, then flip the app's secret over. The
dumps are taken `--no-owner --no-privileges` precisely so they restore
somewhere the original roles don't exist.

### If the cluster is gone

1. Provision a new Postgres cluster and create the `sentinel_command`
   database and role on it.
2. Restore the newest dump into it (`restore_db.sh`, or `pg_restore`
   directly from a laptop with a client ≥ the cluster's major version).
3. Point the app at it: `fly secrets set DATABASE_URL=…` — remember the
   `postgresql+psycopg://` scheme, since `fly postgres attach` emits a
   bare `postgres://` that SQLAlchemy routes to the uninstalled psycopg2.
4. If the app machine/volume is *also* gone, recreate them
   (`fly volumes create sentinel_data …`, then `fly deploy`) — the
   volume now only holds HLS segment working files and `/data/backups`,
   so it starts empty without data loss.

⚠️ **Losing the cluster loses all three services, not just this one.**
Everything above restores `sentinel_command`. `sentinel_license` and
`sentinel_sync` live on the same cluster and go down with it, and this
runbook used to say nothing about them.

| Database | Portable dump | Where it lives | Restore |
|---|---|---|---|
| `sentinel_command` | ✅ daily, `/data/backups` | Command Center's volume | `restore_db.sh` above |
| `sentinel_license` | ✅ daily, `/data/backups` | License Service's volume | that repo's `restore_db.sh` |
| `sentinel_sync` | ❌ none — snapshots only | — | snapshot restore (below) |

**Why Sync has no dump job, deliberately.** The other two write their
dump to a Fly volume. `sentinel-sync` has none, and giving it one would
pin it to a single machine, because Fly volumes are single-attach.

The original reasoning here was that Sync ran **two machines**, so a
volume would cost real redundancy. That is no longer the fact pattern —
it was scaled to one machine on 2026-09-09 and now scales to zero
between its 30-minute pushes, so there is no redundancy left to trade
away. The conclusion survives the reason changing, on stronger grounds:

`sentinel_sync` holds a **mirror**, not a source of truth. Every row in
it was pushed from a self-hosted operator's local SQLite, which remains
authoritative, and `push_pending_changes` only advances its cursors on
confirmed success. Losing this database entirely costs one sync cycle;
the operators re-push. A dump job would be a second copy of data the
cluster snapshot already holds, of data that is itself already a copy.

Don't "fix" this in a later pass without re-reading this paragraph.

**Restoring `sentinel_sync` therefore means a snapshot restore**, which
is cluster-level and brings back all three databases at once:

```bash
fly volumes list -a sentinel-postgres
fly volumes snapshots list <volume-id> -a sentinel-postgres
# Create a new volume from the snapshot, attach it to a fresh
# Postgres app, then repoint each service's DATABASE_URL secret.
```

Sync is also the least painful of the three to lose: it is a **mirror**.
The self-hosted customer's own database is the source of truth, so a
lost `sentinel_sync` costs them their cloud copy, not their data, and
refills as their installs push again.

### If only the machine/volume is gone

The database is unaffected. Recreate the volume and deploy; the app
reconnects to Postgres and comes back with all data intact. You lose
only in-flight HLS segments and the local copies of dumps.
4. Start, verify, and **rotate the Clerk webhook endpoint** if the host
   changed so billing events resume syncing.

---

## Self-hosted installs: restoring from the cloud mirror

Everything above is about **this** hosted deployment — Postgres on a
managed cluster, backed up by snapshots plus `backup_db.sh`. A
self-hosted operator has none of that: they run **SQLite** (the
`DATABASE_URL` default), on their own hardware, with no Fly volume, no
S3 bucket, and no backup cron. Their recovery story is the **cloud
data-sync tier**, and it's a different procedure.

> Note the asymmetry this creates: the `pg_dump`-based scripts above are
> hosted-only and do not apply to a self-hosted install. Since 2026-09
> the Docker image no longer carries the `sqlite3` CLI either — nothing
> in the hosted deployment reads a SQLite file any more. The Python
> `sqlite3` module is stdlib and untouched, so a self-hosted run of this
> same codebase works exactly as before.

**Who this applies to:** `AUTH_PROVIDER=local` installs whose licence
has the data-sync entitlement (`sync_enabled`). Without that
entitlement nothing is mirrored, and there is nothing to restore from —
their backup story is whatever they arranged themselves.

**What is and isn't mirrored.** Cameras, nodes, incidents, motion
events, notifications, and Sentinel AI config/runs, as raw column
values. Deliberately **not** mirrored:

- **Node API keys** (`camera_nodes.api_key_hash`). A restored node
  cannot authenticate and **must re-register**, which mints a fresh key
  anyway. The restore writes a deliberately non-hex placeholder so the
  old credential can't appear to have survived.
- **Incident evidence blobs** (snapshots/clips). The metadata rows
  restore — you'll know evidence existed, what kind, and when — but the
  bytes stayed local. If those matter, back them up separately.
- **Recordings.** They never left the camera node.

### Procedure

```bash
cd backend

# 1. What's actually up there? Also the quickest way to confirm sync
#    was working — do this BEFORE you need it, not during.
uv run python scripts/restore_from_cloud.py --list

# 2. See what would be written, without touching the database.
uv run python scripts/restore_from_cloud.py --dry-run

# 3. Restore. Creates the schema itself, so this works on a machine
#    that has never started the app.
uv run python scripts/restore_from_cloud.py
```

Then start the app and re-register each camera node to issue fresh API
keys.

### Things worth knowing before you run it

- **It won't overwrite existing rows** unless you pass `--overwrite`.
  Running it against a database that still has data is safe: rows whose
  primary key already exists are skipped and counted, not replaced.
- **A partial restore exits `2`** and prints each row it couldn't
  write. A bad row is stepped over rather than aborting the run, so one
  malformed record can't cost you the other 9,999 — but check the exit
  code, because a partial restore that reads as success is how you find
  out months later that data you believed was recovered never came back.
- **RPO is one sync interval** (30 minutes, `SENTINEL_SYNC_INTERVAL_SECONDS`)
  — worse than the hosted daily backup in staleness terms, better in
  granularity. Anything written in the final half hour before the disk
  died is gone.
- Same rule as the rest of this runbook: **rehearse it.** Restore into a
  scratch `DATABASE_URL` on a machine you don't care about and confirm
  the row counts match `--list`.

### Acceptable data loss (RPO) / time to recover (RTO)

- **RPO:** near-zero from cluster snapshots; up to one backup interval
  (24h on the daily schedule) if you have to fall back to a `pg_dump`.
  Shrink the latter by running the workflow more often.
- **RTO:** minutes — dominated by the restore itself plus the ~30–60s
  app restart. Communicate the downtime (see `ON_CALL.md` Scenario E).

---

## Rehearsal drill (do this before launch, then quarterly)

The point of the drill is to restore **somewhere other than the
origin** — that's the scenario the portable dump exists for, and it's
the half that a snapshot restore can't prove.

1. `bash /app/scripts/backup_db.sh` on the live machine. Confirm a
   `.dump` lands in `BACKUP_DIR` (and in S3 if configured).
2. Start a throwaway Postgres of the same major version:
   `docker run -d --name pgdrill -e POSTGRES_PASSWORD=drill -e POSTGRES_DB=drill -p 15499:5432 postgres:18-alpine`
3. Restore into it — note this is the real script, not a hand-typed
   `pg_restore`, so the drill tests what production actually runs:

   ```bash
   DATABASE_URL=postgresql://postgres:drill@127.0.0.1:15499/drill \
     bash backend/scripts/restore_db.sh <dump> --yes
   ```

4. Verify content, not just table count: row counts on a core table, and
   that Boolean columns still carry `default false` (the dialect trap
   that `test_dialect_portability.py` guards):

   ```bash
   psql postgresql://postgres:drill@127.0.0.1:15499/drill -c \
     "select count(*) from organizations"
   ```

5. `docker rm -f pgdrill`. Write the date + result in the log below so
   "last rehearsed" is always visible.

### Rehearsal log

- **2026-09-07 (later) — Post-consolidation drill on `sentinel-postgres`.
  PASS, both services, no findings.** The earlier drill that day covered
  a topology that no longer exists, so it was re-run against the current
  single cluster. Followed the documented path end to end rather than
  shortcutting it: ran `backup_db.sh` on the production machine, pulled
  the dump off with `fly ssh sftp get` (the same step the workflow uses,
  so that step is now exercised too), and restored with the real
  `restore_db.sh` into a throwaway `postgres:18-alpine` container —
  deliberately not the origin cluster.
  Command Center restored to **20 tables, 83 indexes, 3 FK constraints,
  19 sequences**, the migrated `user_notification_state` row intact, and
  all three `cameras` Boolean columns still carrying `default false`.
  License Service restored to 2 tables with both licences and 3
  check-ins, `sync_enabled` still a real boolean.
  **Extra check beyond the runbook's steps:** ran the application's own
  `sync_schema` / `sync_indexes` against the restored database — it
  reported **no columns and no indexes to add**, which is a stronger
  signal than row counts that the restored schema is complete and
  current, not merely populated.

- **2026-09-07 — First post-Postgres drill, both services. PASS (with a
  finding).** Ran the real `backup_db.sh` against production
  (`sentinel_command`: 72 KB dump, 202 catalog entries) and restored it
  with the real `restore_db.sh` into an **independent** `postgres:18-alpine`
  container — deliberately not the origin cluster, since portability is
  the whole reason the dump exists alongside snapshots. Verified in the
  restored copy: **20 tables, 83 indexes, 3 FK constraints, 19
  sequences**, the migrated `user_notification_state` row intact, and all
  three `cameras` Boolean columns carrying `default false`. Repeated
  end-to-end for License-Service (`licenses-*.dump`, 21 catalog entries →
  2 tables, both licences and 3 check-ins intact). Then ran both scripts
  **on the production machines** to confirm `pg_dump` is present in the
  deployed image and the `+psycopg` URL is stripped correctly — both
  succeeded. **Finding:** `restore_db.sh` initially passed the connection
  URL positionally *and* via `--dbname`; `pg_restore` takes the dump file
  as its one positional argument, so it aborted with "too many
  command-line arguments" before restoring anything. Caught only because
  the drill ran the script rather than a hand-typed command. Fixed and
  re-drilled. **Still open:** `BACKUP_ENCRYPTION_KEY` remains unset, so
  there is still no off-platform copy.
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
