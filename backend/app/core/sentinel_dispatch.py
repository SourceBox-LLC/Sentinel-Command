"""
Sentinel dispatch — decides whether a notification should create a
pending agent run, and inserts the row when yes.

The agent service itself doesn't yet exist (slice 3), but the
dispatcher does — pending rows queue up in `sentinel_runs` and the
agent will pick them up when it ships. This module is the single
gate that enforces:

  - is Sentinel enabled for this org?
  - is the trigger this notification kind belongs to enabled?
  - is the camera in scope?
  - does the schedule allow runs right now?
  - is the org under the monthly cap?

If all five answer yes → INSERT a pending sentinel_runs row and
return it. Otherwise no-op.

This is also the helper the manual "Run now" endpoint calls to bypass
some checks (the operator's manual click skips the schedule + scope
gates intentionally — the operator overrode them on purpose by clicking).

Cap enforcement: 300 runs per calendar month per org. The cap value
is hard-coded for now — slice 5 will surface it as a per-plan setting
if/when we offer multiple Pro Plus tiers.
"""

from __future__ import annotations

import hashlib
import hmac
import json
import logging
import threading
import time
import uuid
from datetime import UTC, datetime, timedelta
from typing import Optional

import httpx
from sqlalchemy.orm import Session

from app.core.config import settings
from app.core.plans import effective_plan_for_caps
from app.models.models import SentinelConfig, SentinelRun, Setting

logger = logging.getLogger(__name__)


# Per-plan monthly run cap.  Sentinel is now available on both paid
# tiers; the cap is the differentiator.  100/mo on Pro is roughly
# 3 runs/day — comfortable for casual home use.  500/mo on Pro Plus
# is roughly 16 runs/day — commercial-shaped usage.  Anyone past 500
# is enterprise; we'll talk to them.
#
# Drives both the gate-side check (`_can_dispatch_for_kind`,
# `dispatch_manual_run`) and the in-transaction recount in
# `_commit_run_with_cap_check`.  Resolved per request via
# `effective_plan_for_caps` so a downgrade flips the cap immediately
# (no stale per-month cap that follows the old plan).
MONTHLY_RUN_CAP_BY_PLAN: dict[str, int] = {
    "pro": 100,
    "pro_plus": 500,
}

# Set of plans that get to use Sentinel at all.  Used as the
# tier-gate replacement for the previous Pro-Plus-only constant —
# `_can_dispatch_for_kind` and the MCP agent-key resolver both
# check membership here.
SENTINEL_PLANS = frozenset(MONTHLY_RUN_CAP_BY_PLAN.keys())


def cap_for_plan(plan: str | None) -> int:
    """Return the org's monthly Sentinel run cap given its plan slug.

    Defaults to 0 for plans that don't include Sentinel (free,
    free_org from the past-due grace path) — fail-closed so any
    code path that reaches `cap_remaining` for a non-Sentinel org
    sees zero remaining and the dispatch hard-rejects.  The plan
    gate above the cap check should already reject these orgs;
    this is the second layer of defence.
    """
    return MONTHLY_RUN_CAP_BY_PLAN.get(plan or "", 0)

# Stranded-run reaper threshold.  The agent's wall-clock budget is
# 540 s (9 min) and the strand-cleanup wrapper marks the in-flight
# run as `error` on TimeoutError.  But if the agent process crashes
# (OOM, panic, container kill) before the wrapper fires, the run
# sits in `running` forever — list_pending only returns `pending`
# rows and start() doesn't re-claim `running` ones.  The reaper
# closes that gap by marking anything in `running` for >> the
# wall-clock budget as errored, so the UI shows it as terminal
# instead of perpetually-running.  20 min gives the wrapper a
# generous 11 min buffer past its budget.
STRANDED_RUN_AGE_MINUTES = 20


# Notification kinds that map to a Sentinel trigger.  Keys are
# notification kind strings; values are the SentinelConfig boolean
# field that has to be on for the trigger to fire.
_KIND_TO_TRIGGER_FIELD: dict[str, str] = {
    "motion": "motion_enabled",
    # incident_created is the kind emitted when a human files an
    # incident.  See _NOTIFICATION_KIND_TO_SETTING in notifications.py
    # — the kind string predates the more readable "incident_opened"
    # we use in the UI; we map to the same trigger here.
    "incident_created": "incident_opened_enabled",
}

# Map back from trigger field → run trigger_type label that lands in
# sentinel_runs.trigger_type. Keep in sync with the frontend trigger
# pill colour mapping (.sentinel-trigger-pill-*).
_FIELD_TO_TRIGGER_TYPE = {
    "motion_enabled": "motion",
    "incident_opened_enabled": "incident_opened",
}


def _start_of_month_utc() -> datetime:
    now = datetime.now(tz=UTC)
    return now.replace(day=1, hour=0, minute=0, second=0, microsecond=0, tzinfo=None)


def runs_used_this_month(db: Session, org_id: str) -> int:
    """Count of sentinel runs for this org in the current calendar
    month.  Includes pending + running + terminal — every dispatch
    counts against the cap regardless of outcome."""
    return (
        db.query(SentinelRun)
        .filter(
            SentinelRun.org_id == org_id,
            SentinelRun.triggered_at >= _start_of_month_utc(),
        )
        .count()
    )


def cap_remaining(db: Session, org_id: str) -> int:
    """Remaining Sentinel runs in the current calendar month, given
    the org's effective plan.  Returns 0 for orgs that don't have
    Sentinel access at all (the cap_for_plan default)."""
    cap = cap_for_plan(effective_plan_for_caps(db, org_id))
    return max(0, cap - runs_used_this_month(db, org_id))


def _is_camera_in_scope(scope_dict: dict | None, camera_id: str | None) -> bool:
    """Cameras absent from camera_scope default to in-scope (True).
    This matches the SentinelConfig docstring and the frontend's
    isCameraInScope helper — new cameras don't silently disappear
    from the agent's purview when added.
    """
    if not camera_id:
        # Triggers without a camera (e.g. scheduled all-camera sweeps)
        # are always considered in scope — the agent decides which
        # cameras to investigate.
        return True
    if not scope_dict:
        return True
    return scope_dict.get(camera_id) is not False


def _parse_hhmm_to_minutes(value: str, default: int) -> int:
    """Parse 'HH:MM' (or 'HH') into minutes-since-midnight.

    The UI accepts HH:MM but the previous version stripped minutes via
    int(value.split(':')[0]) — '22:30 → 23:00' silently evaluated as
    '22:00 → 23:00'.  Now both H and HH:MM are honoured, and clamped
    to [0, 24*60].
    """
    try:
        parts = (value or "").split(":")
        h = int(parts[0])
        m = int(parts[1]) if len(parts) > 1 else 0
        return max(0, min(24 * 60, h * 60 + m))
    except (AttributeError, ValueError, IndexError):
        return default


def _schedule_allows_now(cfg: SentinelConfig, db: Session) -> bool:
    """Is right-now within the configured schedule window?

    "always" mode: yes.
    "off" mode: no.
    "scheduled" mode: yes iff today is an active day AND the current
    minute-of-day is within the start..end window.  Times are
    interpreted in the org's timezone (Setting key 'timezone',
    defaults to UTC).
    """
    mode = cfg.schedule_mode or "always"
    if mode == "always":
        return True
    if mode == "off":
        return False

    # scheduled mode — check window + day-of-week
    from zoneinfo import ZoneInfo, ZoneInfoNotFoundError
    tz_name = Setting.get(db, cfg.org_id, "timezone", "UTC") or "UTC"
    try:
        tz = ZoneInfo(tz_name)
    except (ZoneInfoNotFoundError, ValueError):
        tz = ZoneInfo("UTC")

    now_local = datetime.now(tz=tz)
    day_keys = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]
    today_key = day_keys[now_local.weekday()]

    active_days = cfg.get_active_days()
    if today_key not in active_days:
        return False

    start_m = _parse_hhmm_to_minutes(cfg.schedule_start or "00:00", 0)
    end_m = _parse_hhmm_to_minutes(cfg.schedule_end or "24:00", 24 * 60)
    cur_m = now_local.hour * 60 + now_local.minute

    if start_m < end_m:
        return start_m <= cur_m < end_m
    # Wrap-around (e.g. 22:30 → 06:15)
    return cur_m >= start_m or cur_m < end_m


def _motion_cooldown_allows(
    cfg: SentinelConfig,
    kind: str,
    camera_id: Optional[str],
    db: Session,
) -> bool:
    """Per-camera cooldown gate for motion triggers.

    The UI promises 'wait N minutes between motion-triggered runs on
    the same camera'.  Without this gate a busy camera (waving tree,
    blinking light) burns the monthly cap in hours.

    Only applies to motion (incident_opened is a one-shot human
    action; manual / scheduled aren't motion-driven).  Skips when
    the cooldown is 0 or when the trigger isn't camera-scoped.

    Implementation: query the most recent SentinelRun for this
    (org, camera, motion) tuple — if it fired within the cooldown
    window, refuse.  The 'fired' includes pending/running/terminal —
    every dispatch counts, regardless of how it eventually resolved,
    so a stuck or errored run still suppresses the next trigger.
    """
    if kind != "motion" or not camera_id:
        return True

    cooldown_min = int(cfg.motion_cooldown_min or 0)
    if cooldown_min <= 0:
        return True

    cutoff = datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(
        minutes=cooldown_min
    )
    recent = (
        db.query(SentinelRun.id)
        .filter(
            SentinelRun.org_id == cfg.org_id,
            SentinelRun.camera_id == camera_id,
            SentinelRun.trigger_type == "motion",
            SentinelRun.triggered_at >= cutoff,
        )
        .first()
    )
    return recent is None


def _can_dispatch_for_kind(
    cfg: SentinelConfig,
    kind: str,
    camera_id: Optional[str],
    db: Session,
) -> tuple[bool, str]:
    """Run the full dispatch gate.  Returns (ok, reason)."""
    if not cfg.enabled:
        return False, "sentinel_disabled"

    # Plan gate first — Sentinel is paid-only (Pro or Pro Plus).
    # A downgrade to free leaves SentinelConfig.enabled=True but the
    # org is no longer entitled to run the agent; without this check
    # motion events would keep enqueueing pending rows that the agent
    # auth path would later reject (and burn the per-plan cap).
    plan = effective_plan_for_caps(db, cfg.org_id)
    if plan not in SENTINEL_PLANS:
        return False, "plan_not_eligible"

    field = _KIND_TO_TRIGGER_FIELD.get(kind)
    if field is None:
        return False, "kind_not_a_sentinel_trigger"

    if not getattr(cfg, field, False):
        return False, f"trigger_{field}_off"

    if not _is_camera_in_scope(cfg.get_camera_scope(), camera_id):
        return False, "camera_out_of_scope"

    if not _schedule_allows_now(cfg, db):
        return False, "outside_schedule_window"

    # Motion cooldown — per-camera throttle so a busy camera doesn't
    # eat the monthly cap.  Checked AFTER the cheap gates (enabled,
    # plan, trigger flag, scope) so we only do the SentinelRun query
    # when everything else passed.
    if not _motion_cooldown_allows(cfg, kind, camera_id, db):
        return False, "motion_cooldown_active"

    if cap_remaining(db, cfg.org_id) <= 0:
        return False, "monthly_cap_reached"

    return True, "ok"


def reap_stranded_runs(db: Session) -> dict:
    """Mark long-stranded `running` rows as errored.

    Background-task entry point.  Finds SentinelRun rows where:
      - `outcome == 'running'`, AND
      - `started_at < now - STRANDED_RUN_AGE_MINUTES`

    ...and stamps them as `outcome='error'` with a clear summary.
    Belt-and-suspenders for cases the agent's own strand-cleanup
    wrapper can't handle:

      - Agent process crashed mid-run (OOM, panic, container kill)
        before `process_with_timeout`'s except-TimeoutError block
        fired
      - Agent machine was force-stopped (Fly preemption, network
        partition long enough that the cleanup POST itself failed)
      - The run was claimed via /start, then the wakeup HTTP
        connection dropped before the agent finished

    Without this loop those runs sit at `outcome='running'` forever
    — list_pending only returns `pending`, start() doesn't re-claim
    `running`, and the UI shows a perpetually-spinning indicator.

    Forward-compatible with the /complete error → real upgrade path
    (sentinel.py:post_run_complete): if the agent later actually
    completes the run after the reaper stamped it errored, the real
    outcome (`incident` / `no_action`) replaces the reaper's `error`.

    Returns ``{"reaped": <count>}`` for the caller's log line.
    Cross-org by design — the reaper is a system-level sweep.
    """
    cutoff = datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(
        minutes=STRANDED_RUN_AGE_MINUTES
    )
    stranded_ids = [
        row.id
        for row in db.query(SentinelRun.id)
        .filter(
            SentinelRun.outcome == "running",
            SentinelRun.started_at != None,  # noqa: E711 — SQLAlchemy IS NOT NULL
            SentinelRun.started_at < cutoff,
        )
        .all()
    ]

    reaped = 0
    if stranded_ids:
        now = datetime.now(tz=UTC).replace(tzinfo=None)
        # Conditional bulk UPDATE, re-checking outcome=='running' in the
        # WRITE statement itself.  The previous load-then-stamp pattern
        # had a window between the SELECT and the commit where a
        # concurrent /complete could land — its real outcome was then
        # silently overwritten with 'error' (leaving an inconsistent row:
        # outcome=error with severity/incident_id from the completion,
        # and no repair path since the agent's POST already succeeded).
        reaped = (
            db.query(SentinelRun)
            .filter(
                SentinelRun.id.in_(stranded_ids),
                SentinelRun.outcome == "running",
            )
            .update(
                {
                    SentinelRun.outcome: "error",
                    SentinelRun.summary: (
                        f"Stranded — agent never completed within "
                        f"{STRANDED_RUN_AGE_MINUTES} min.  Reaped automatically."
                    ),
                    SentinelRun.completed_at: now,
                },
                synchronize_session=False,
            )
        )
        db.commit()
        logger.warning("sentinel: reaper marked %d stranded run(s) as error", reaped)

    # ── Stale-PENDING recovery ──────────────────────────────────────
    # A lost wakeup (the single fire-and-forget POST timing out against
    # a cold-starting agent) used to strand rows at `pending` FOREVER on
    # a quiet system: the reaper only handled `running`, /runs/pending
    # only helps an agent that's already awake, and "the next wakeup"
    # never comes when this org's motion was the only trigger.  Re-fire
    # the wakeup whenever pending work has sat unclaimed for a couple of
    # minutes — one webhook wakes the agent, which then drains EVERY
    # pending run across all orgs.
    pending_cutoff = datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(minutes=2)
    stale_pending = (
        db.query(SentinelRun)
        .filter(
            SentinelRun.outcome == "pending",
            SentinelRun.triggered_at < pending_cutoff,
        )
        .count()
    )
    if stale_pending:
        logger.warning(
            "sentinel: %d pending run(s) unclaimed for >2 min — re-firing wakeup",
            stale_pending,
        )
        _fire_wakeup_webhook()

    # Terminal backstop: pending rows older than 6 hours mean the agent
    # has been unreachable across ~70+ re-fired wakeups — surface the
    # failure instead of holding a cap slot + UI spinner forever.  (The
    # documented error → incident/no_action upgrade path still applies
    # if the agent ever completes one of these later.)
    abandoned_cutoff = datetime.now(tz=UTC).replace(tzinfo=None) - timedelta(hours=6)
    abandoned = (
        db.query(SentinelRun)
        .filter(
            SentinelRun.outcome == "pending",
            SentinelRun.triggered_at < abandoned_cutoff,
        )
        .update(
            {
                SentinelRun.outcome: "error",
                SentinelRun.summary: (
                    "Abandoned — agent never claimed this run within 6 hours "
                    "(wakeup webhook unreachable?).  Marked errored automatically."
                ),
                SentinelRun.completed_at: datetime.now(tz=UTC).replace(tzinfo=None),
            },
            synchronize_session=False,
        )
    )
    if abandoned:
        db.commit()
        logger.warning("sentinel: marked %d abandoned pending run(s) as error", abandoned)

    return {
        "reaped": reaped,
        "ids": stranded_ids if reaped else [],
        "rewoken_pending": stale_pending,
        "abandoned": abandoned,
    }


def _commit_run_with_cap_check(
    db: Session,
    run: SentinelRun,
    org_id: str,
    cap: int,
) -> bool:
    """Flush the run row, then recount within the same transaction;
    roll back if we've gone over the cap.

    `cap` is passed in (rather than re-resolved here) so the caller
    can pin the plan-derived cap once at the top of its flow and not
    pay another resolver round-trip in the hot path.

    The naive `cap_remaining(...) <= 0` gate before INSERT is a
    classic read-then-write race: two concurrent dispatchers both
    pass the gate at cap-1, both INSERT, and the org overshoots by
    one.  Re-counting AFTER `flush()` (so the new row is visible to
    the COUNT inside this txn) catches the second writer.

    On SQLite — the current prod DB — concurrent writes are already
    serialised by SQLite's writer lock, so this pattern is sufficient
    on its own.  On Postgres (future) the per-org `SentinelConfig`
    row would also need a `with_for_update()` lock at the top of the
    caller to serialise reads against another transaction's
    in-progress write.

    Returns True if the run was committed, False if rolled back due
    to the cap race (caller treats False as "lost the race").
    """
    db.add(run)
    db.flush()
    used = runs_used_this_month(db, org_id)
    if used > cap:
        db.rollback()
        logger.info(
            "sentinel: dispatch lost cap race org=%s cap=%d used_after_flush=%d",
            org_id, cap, used,
        )
        return False
    db.commit()
    db.refresh(run)
    return True


def maybe_dispatch_for_notification(
    db: Session,
    org_id: str,
    kind: str,
    camera_id: Optional[str] = None,
) -> Optional[SentinelRun]:
    """Called from create_notification() — best-effort dispatch.

    Returns the new SentinelRun row if the gate allowed dispatch,
    None otherwise. Never raises (a failed dispatch must NEVER block
    the underlying notification from being delivered to the inbox or
    email channels).
    """
    try:
        cfg = db.query(SentinelConfig).filter_by(org_id=org_id).first()
        if cfg is None:
            return None  # No config = Sentinel never configured = no dispatch

        ok, reason = _can_dispatch_for_kind(cfg, kind, camera_id, db)
        if not ok:
            logger.debug(
                "sentinel: dispatch skipped org=%s kind=%s camera=%s reason=%s",
                org_id, kind, camera_id, reason,
            )
            return None

        trigger_type = _FIELD_TO_TRIGGER_TYPE.get(_KIND_TO_TRIGGER_FIELD[kind], kind)
        run = SentinelRun(
            id=uuid.uuid4().hex,
            org_id=org_id,
            triggered_at=datetime.now(tz=UTC).replace(tzinfo=None),
            trigger_type=trigger_type,
            camera_id=camera_id,
            tool_call_count=0,
            outcome="pending",
        )
        # `_can_dispatch_for_kind` already verified the org's plan is
        # in SENTINEL_PLANS, so cap_for_plan returns a non-zero value
        # here.  Resolved fresh for the recount in case the plan has
        # changed between the gate and the commit (rare, but the
        # function above is sync-database-call latency-bounded).
        cap = cap_for_plan(effective_plan_for_caps(db, org_id))
        if not _commit_run_with_cap_check(db, run, org_id, cap):
            return None  # cap race — silently drop, same UX as gate fail
        logger.info(
            "sentinel: dispatched pending run id=%s org=%s trigger=%s camera=%s",
            run.id, org_id, trigger_type, camera_id,
        )
        # Wake the agent so it picks up this run.
        _fire_wakeup_webhook()
        return run
    except Exception:  # noqa: BLE001
        # Dispatch failure must NOT cascade into the notification path.
        # A run that should have queued is regrettable; an unhandled
        # exception that 500s on a motion event is far worse.
        logger.exception("sentinel: dispatch failed silently org=%s kind=%s", org_id, kind)
        try:
            db.rollback()
        except Exception:  # noqa: BLE001
            pass
        return None


def dispatch_manual_run(
    db: Session,
    org_id: str,
    prompt: str,
    camera_id: Optional[str] = None,
) -> SentinelRun:
    """Operator-initiated run from the "Run now" button.

    Skips the schedule + scope checks (the operator overrode those
    by clicking), but still enforces the per-plan cap.  Sentinel
    doesn't have to be enabled — the operator can run a one-off
    check on a paused agent — but the org DOES need to be on a paid
    plan; the route handler does that check upstream and we re-check
    here as defence in depth.

    Raises ValueError("monthly_cap_reached") on cap exhaustion so
    the API endpoint can surface a 429 with the existing detail
    shape.  Raises ValueError("plan_not_eligible") if the org isn't
    on a Sentinel-eligible plan.
    """
    # Ensure config exists so the manual-run path works for orgs that
    # have never opened the Sentinel page (an unusual case but
    # possible).
    cfg = db.query(SentinelConfig).filter_by(org_id=org_id).first()
    if cfg is None:
        cfg = SentinelConfig(org_id=org_id)
        db.add(cfg)
        db.commit()
        db.refresh(cfg)

    # Resolve the plan once so the cap check, the recount cap, and
    # any potential downstream usage all see the same value within
    # this transaction.
    plan = effective_plan_for_caps(db, org_id)
    if plan not in SENTINEL_PLANS:
        raise ValueError("plan_not_eligible")
    cap = cap_for_plan(plan)

    if max(0, cap - runs_used_this_month(db, org_id)) <= 0:
        raise ValueError("monthly_cap_reached")

    run = SentinelRun(
        id=uuid.uuid4().hex,
        org_id=org_id,
        triggered_at=datetime.now(tz=UTC).replace(tzinfo=None),
        trigger_type="manual",
        camera_id=camera_id,
        tool_call_count=0,
        outcome="pending",
        manual_prompt=(prompt or "")[:2000],  # bound prompt size
    )
    if not _commit_run_with_cap_check(db, run, org_id, cap):
        # Lost the cap race against a concurrent dispatcher — surface
        # the same error the gate-side check raises so the route
        # handler returns 429 with the existing detail shape.
        raise ValueError("monthly_cap_reached")
    logger.info(
        "sentinel: manual run id=%s org=%s camera=%s prompt_len=%d",
        run.id, org_id, camera_id, len(prompt or ""),
    )
    # Wake the agent so it picks up this run.
    _fire_wakeup_webhook()
    return run


# ── Wakeup webhook firing ────────────────────────────────────────────
# After a pending run is created we POST a fire-and-forget request to
# SENTINEL_AGENT_WEBHOOK_URL.  The body carries only a timestamp — the
# agent re-fetches pending runs via the API.  We HMAC-sign the body
# with SENTINEL_AGENT_KEY so a leaked URL alone can't trigger the
# agent.
#
# Why a timestamp (vs the old static `{}`): the signature of a fixed
# body under a fixed key is a CONSTANT — one captured request (proxy
# log, agent access log) could be replayed forever to force agent
# cold-starts at will.  Signing `{"ts": <unix>}` makes each request's
# signature unique and lets the agent reject stale timestamps (skew
# window ~5 min).  The signature is still plain HMAC(secret, raw_body),
# so agents that haven't shipped the skew check yet keep verifying.
#
# Fire-and-forget runs in a background thread so the request handler
# that triggered the dispatch (motion ingestion, manual run, etc.) is
# not blocked on the agent's network round-trip.  A 5-second timeout
# keeps the thread short-lived; if Fly's auto-stop is mid-spin-up and
# the webhook times out, the run will get picked up by the NEXT
# wakeup that fires (the agent always drains, not just the run that
# triggered the wakeup).


def _wakeup_payload() -> bytes:
    return json.dumps({"ts": int(time.time())}).encode("utf-8")


def _compute_signature(body: bytes, secret: str) -> str:
    return "sha256=" + hmac.new(
        secret.encode("utf-8"), body, hashlib.sha256,
    ).hexdigest()


def _fire_wakeup_webhook_blocking() -> None:
    """Run inside a thread.  Hits the agent webhook with a short
    timeout; logs and returns (any failure is non-fatal here).
    """
    url = settings.SENTINEL_AGENT_WEBHOOK_URL
    secret = settings.SENTINEL_AGENT_KEY
    if not url:
        return  # no agent configured — pending run sits until polled
    if not secret:
        logger.warning(
            "sentinel wakeup: SENTINEL_AGENT_WEBHOOK_URL set but "
            "SENTINEL_AGENT_KEY is empty — skipping webhook"
        )
        return

    try:
        payload = _wakeup_payload()
        signature = _compute_signature(payload, secret)
        with httpx.Client(timeout=5.0) as client:
            resp = client.post(
                url,
                content=payload,
                headers={
                    "Content-Type": "application/json",
                    "X-Sentinel-Signature": signature,
                },
            )
            if resp.status_code >= 400:
                logger.warning(
                    "sentinel wakeup: %s returned %d — pending run will be "
                    "picked up by the next wakeup",
                    url, resp.status_code,
                )
            else:
                logger.debug("sentinel wakeup: pinged %s status=%d", url, resp.status_code)
    except Exception as exc:  # noqa: BLE001
        # Common case: the agent's machine is auto-stopped and Fly is
        # cold-starting it; the request returns before the boot
        # finishes.  That's fine — the next dispatch will fire another
        # wakeup, and the agent will drain everything when it's up.
        logger.info(
            "sentinel wakeup: %s unreachable (%s) — pending run will be "
            "picked up by the next wakeup or a manual drain",
            url, type(exc).__name__,
        )


def _fire_wakeup_webhook() -> None:
    """Fire-and-forget the wakeup webhook in a background thread.

    Runs in a daemon thread so it doesn't block app shutdown if the
    agent endpoint hangs.  We don't reuse asyncio.create_task because
    the dispatch may be called from synchronous code paths (e.g.
    _claim_motion_cooldown_or_silence is sync), and re-entering the
    event loop from there is fragile.  A thread is the simplest
    fire-and-forget primitive that works in both contexts.
    """
    threading.Thread(
        target=_fire_wakeup_webhook_blocking,
        name="sentinel-wakeup-fire",
        daemon=True,
    ).start()
