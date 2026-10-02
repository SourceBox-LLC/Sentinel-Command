# Sourced, not run: the environment every differential tier starts with.
#
# Split out of tiers.sh when a second launcher appeared (dialect_run.sh,
# which compares the PostgreSQL build against the SQLite build). Two
# launchers each carrying their own copy is how one tier ends up started
# without LOCAL_ADMIN_PASSWORD_HASH and every result gains a constant
# two-case divergence — tiers.sh's own header describes that happening
# once already.
#
# Expects REPO and RS to be set by the caller.

export APP_SECRET_KEY="${APP_SECRET_KEY:-differential-test-secret-not-a-real-key}"
export AUTH_PROVIDER=local
export LOCAL_ORG_ID=self-host
export LOCAL_ADMIN_USERNAME=admin
export LOCAL_ADMIN_EMAIL=admin@example.com
# Argon2 hash of "correct horse battery staple" — which is what
# write_diff.py's login cases actually send. This comment said
# "differential-password" for as long as it existed and was simply wrong:
# nothing caught it because the cases compare the two tiers' ANSWERS, and
# two stacks rejecting the same bad password agree perfectly. Verified by
# posting both strings at a tier configured with this hash: the first is a
# 401, the second returns a token.
#
# Single-quoted on purpose: the
# PHC string starts with `$argon2id`, which a double-quoted assignment
# expands to the empty string — both tiers then report "not configured"
# and agree with each other about nothing.
export LOCAL_ADMIN_PASSWORD_HASH='$argon2id$v=19$m=65536,t=3,p=4$Ro3CVUFhr5w3hNxP8Cfe9A$8L+JCXU1z+/zs9b32O92qCPEvf8AhWmpViV5KwwLRfc'
export REDIS_URL="${REDIS_URL:-redis://127.0.0.1:16379/0}"

# The sweeps are pushed out of the way for BOTH tiers.
#
# They are not a no-op on this fixture: it carries a stranded
# `sentinel_run`, and the reaper's five-minute tick landed inside one
# tier's window and not the other's, which the write differential
# correctly reported as a side-effect difference in `sentinel_runs`
# with no code behind it. The two tiers start seconds apart, so their
# ticks never align.
#
# What the loops DO is compared by loops_run.sh, which calls the bodies
# directly. What the HTTP differentials compare is routes, and a sweep
# firing mid-case is noise in that. Python reads all four from the
# environment; `sentinel_dispatch`'s own comment calls the reaper
# cadence "tunable down for ops or up for quieter environments", and
# this is the quietest environment there is.
export OFFLINE_SWEEP_INTERVAL_SECONDS="${OFFLINE_SWEEP_INTERVAL_SECONDS:-86400}"
export SENTINEL_REAPER_INTERVAL_SECONDS="${SENTINEL_REAPER_INTERVAL_SECONDS:-86400}"
export MOTION_DIGEST_INTERVAL_SECONDS="${MOTION_DIGEST_INTERVAL_SECONDS:-86400}"
export DISK_CHECK_INTERVAL_SECONDS="${DISK_CHECK_INTERVAL_SECONDS:-86400}"

# The scripts directory is the Python's own, resolved the way install.py
# resolves it: `Path(__file__).parent.parent.parent / "scripts"`. Both
# tiers must read the same bytes or /install.sh diffs for a reason that
# has nothing to do with the port.
export SCRIPTS_DIR="$REPO/backend/scripts"

# Python's background loops, pushed out of reach. Every one sleeps before
# its first run, so a ten-year interval means it never fires during a
# session. They write to the same tables the write differential
# snapshots, on their own timer, and only the Python tier runs them:
#
#   offline sweep (30s)   flips `online` nodes with a stale last_seen to
#                         offline and writes transition notifications —
#                         and the write fixture freezes every last_seen
#                         to January;
#   sentinel reaper (5m)  marks pending runs older than 6h, and running
#                         runs older than 20m, as errored — every seeded
#                         run qualifies.
#
# A loop firing between one tier's reseed and its snapshot is a one-case
# diff that is gone on the rerun. The reaper did exactly that: it was
# caught red-handed rewriting run ...0001 to "Abandoned — agent never
# claimed this run within 6 hours" during an unrelated integration case,
# and it is the likeliest author of an earlier sentinel_runs flake that
# never reproduced. The loops themselves are ported, and verified, with
# the background-loop slice — not by racing them here.
# A licence key, so the three states beyond "unlicensed" are reachable
# at all: without it every Sentinel route answers 402 license_required
# and the licensed paths cannot be compared. The reconcile loop's
# interval is a module constant and cannot be pushed out like the
# others, so it is pointed at fake_license.py, which answers exactly
# what the seeded licence state says — a tick mid-run then rewrites the
# same values instead of moving the gate underneath a case.
# The agent's bearer for the MCP tool surface. Set so the SHARED
# multi-tenant path is reachable at all — unset, every attempt to use
# it falls through to the key lookup and the agent allowlist is never
# exercised. Distinct from SENTINEL_AGENT_KEY, which is the run queue's.
export SENTINEL_AGENT_MCP_KEY="${SENTINEL_AGENT_MCP_KEY:-harness-agent-mcp-key}"
export SENTINEL_LICENSE_KEY="${SENTINEL_LICENSE_KEY:-harness-licence-key}"
export SENTINEL_LICENSE_SERVICE_URL="${SENTINEL_LICENSE_SERVICE_URL:-http://127.0.0.1:18090}"

# The HLS caches, shrunk so their eviction paths are reachable from a
# test at all. The real ceilings are 60 segments per camera and 384 MB
# across all of them; filling either honestly would mean pushing
# hundreds of megabytes through both tiers for one case. The policies
# are what the differential is for — which segment goes, and when — and
# those are the same at five as at sixty.
export SEGMENT_CACHE_MAX_PER_CAMERA="${SEGMENT_CACHE_MAX_PER_CAMERA:-5}"
# Three megabytes, not the real 384: small enough that four pushes fill
# it, large enough that a single realistic segment (upload_diff pushes
# 300 KB of real bytes) is not evicted the instant it lands.
export SEGMENT_CACHE_MAX_TOTAL_BYTES="${SEGMENT_CACHE_MAX_TOTAL_BYTES:-3000000}"
# Likewise the sweep cadence: every third playlist push rather than
# every twentieth.
export CLEANUP_INTERVAL="${CLEANUP_INTERVAL:-3}"


# Email on. Off is the production default and was the harness default
# too, which meant `email_enabled_for_kind` refused every kind and not
# one outbox row was ever written — so the templates, the recipient
# lookup and the unsubscribe tokens went entirely uncompared while the
# write differential reported green. The worker interval below is
# already pinned to never, so nothing is sent; the rows are simply
# enqueued, and `email_outbox` is a watched table.
export EMAIL_ENABLED="${EMAIL_ENABLED:-true}"
# Both tiers already default to this. Set explicitly because it is the
# base of every unsubscribe link, and two tiers disagreeing about it
# would look like a template difference.
export FRONTEND_URL="${FRONTEND_URL:-http://localhost:5173}"
# Where the Rust tier loads the shared .j2 templates from. Python finds
# them relative to its own package; there is one copy, and this points
# at it.
# One copy of each template, pointed at by both tiers. It moved to
# backend-rs/templates/emails when the Python tree was deleted — and the
# Rust tier now compiles them in, so this override exists for the Python
# half and for anyone diffing a template edit without a rebuild.
export EMAIL_TEMPLATES_DIR="${EMAIL_TEMPLATES_DIR:-$RS/templates/emails}"

# The same Svix secret for both tiers, so write_diff can sign one
# webhook delivery with the svix library and send it to each.
export RESEND_WEBHOOK_SECRET="${RESEND_WEBHOOK_SECRET:-whsec_aGFybmVzcy13ZWJob29rLXNlY3JldC0xMjM0NTY=}"
# Clerk's webhook secret. Only the Clerk-mode pair mounts that route at
# all — main.py registers the webhooks router under Clerk only — but it
# is exported for both so the two pairs differ by AUTH_PROVIDER and
# nothing else.
export CLERK_WEBHOOK_SECRET="${CLERK_WEBHOOK_SECRET:-whsec_Y2xlcmstaGFybmVzcy1zZWNyZXQtNjU0MzIxMDA=}"

# Placeholder Clerk keys for the Clerk-mode pair: base64 of a made-up
# Frontend API host. Nothing that pair is used for reaches Clerk.
CLERK_PK_PLACEHOLDER=pk_test_aGFybmVzcy5jbGVyay5hY2NvdW50cy5kZXYk

FOREVER=315360000
export OFFLINE_SWEEP_INTERVAL_SECONDS=$FOREVER
# The Rust tier's two HLS loops are pushed out of the way, like the
# other background loops. The viewer-usage flush is the one that
# matters: it writes org_monthly_usage, which the GDPR export reads, so
# a tick landing between the two passes of a write case would report a
# difference that is a timer rather than a port. Python's copy of this
# loop is a literal 60 seconds and cannot be stretched, and this
# comment used to call that exposure "one-sided and narrow" because the
# pending counters are only non-empty just after an HLS run.
#
# The window is narrow; the damage was not. The row Python wrote in it
# was never deleted by anything, so it sat in the database for the rest
# of the session and the next full-reset case counted it -- python 1,
# rust 0, buried in a JSON blob of per-table delete counts, reading
# exactly like a bug in the erasure path. seed_cameras.sql now clears
# `org_monthly_usage` per case, which is what actually bounds it.
# Rust's flush is covered by tests/hls_db.rs, against a real database.
export VIEWER_USAGE_FLUSH_INTERVAL_SECONDS=$FOREVER
# The eviction loop is NOT stretched: it touches only memory, and
# Python's copy of it runs every sixty seconds whatever this does.
# Stretching Rust's made the two tiers disagree about a camera that an
# earlier run had left in the cache — Python had swept it and Rust had
# not, which looked exactly like a port difference on the first push of
# a scenario.
export SEGMENT_CACHE_EVICT_INTERVAL_SECONDS="${SEGMENT_CACHE_EVICT_INTERVAL_SECONDS:-60}"
export SENTINEL_REAPER_INTERVAL_SECONDS=$FOREVER
export MOTION_DIGEST_INTERVAL_SECONDS=$FOREVER
export DISK_CHECK_INTERVAL_SECONDS=$FOREVER
export RELEASE_CACHE_REFRESH_INTERVAL_SECONDS=$FOREVER
# Every 5s it "sends" any pending email_outbox row — which the fixture
# has had only since the Resend webhook cases, so it raced the harness
# only from then on, alternating which tier's snapshot it landed in.
export EMAIL_WORKER_INTERVAL_SECONDS=$FOREVER
#
# Loops with a hardcoded interval, left running because none touches a
# watched table: the viewer-usage flush (60s, writes org_monthly_usage
# only after Python has served HLS segments), segment-cache eviction
# (60s, in memory), log cleanup (24h, sleeps first) and, under Clerk,
# the plan reconcile (hourly, sleeps first). The licence reconcile is
# handled by fake_license.py. Watch one of their tables and this list
# has to be revisited.
export STATIC_DIR="$REPO/backend/static"


