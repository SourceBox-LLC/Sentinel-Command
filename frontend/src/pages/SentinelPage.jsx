import { useState, useMemo, useEffect, useCallback } from "react"
import { Link } from "react-router-dom"
import { useAuth } from "@clerk/clerk-react"
import {
  getSentinelConfig,
  updateSentinelConfig,
  getSentinelRuns,
  getSentinelRun,
  getCameras,
  dispatchSentinelManualRun,
} from "../services/api"
import { useToasts } from "../hooks/useToasts.jsx"

// SentinelPage v3 (slice 1) — service control UI wired to the real
// backend.  Three tabs (Overview / Configure / History), compact
// persistent header on top.  Configuration persists per-org via
// /api/sentinel/config.  Run history reads /api/sentinel/runs;
// initially empty for every org because the agent itself isn't yet
// wired up — slice 3 will start producing rows.
//
// State load order on mount: getSentinelConfig (always fires —
// returns plan_gated flag for orgs without Sentinel access), getCameras (real
// cameras for the scope panel), getSentinelRuns (small page).
// All three settle independently so the UI streams in instead of
// blocking on the slowest.

// ── data ──────────────────────────────────────────────────────────────

const NOTIFICATION_TRIGGERS = [
  {
    key: "motion_enabled",
    configField: "motion_enabled",
    label: "Motion detected",
    description:
      "Wake Sentinel when a camera reports motion. Uses your existing per-camera motion threshold — there's no separate Sentinel threshold to keep in sync.",
    extras: ["cooldown"],
  },
  {
    key: "incident_opened_enabled",
    configField: "incident_opened_enabled",
    label: "Incident opened by a human",
    description:
      "When someone files an incident manually, Sentinel auto-collects supporting evidence from the relevant cameras — like a guard helping document a report.",
  },
]

const DAY_CHIPS = [
  { key: "mon", label: "Mon" },
  { key: "tue", label: "Tue" },
  { key: "wed", label: "Wed" },
  { key: "thu", label: "Thu" },
  { key: "fri", label: "Fri" },
  { key: "sat", label: "Sat" },
  { key: "sun", label: "Sun" },
]

// ── helpers ───────────────────────────────────────────────────────────

function StatusDot({ kind, pulse }) {
  return (
    <span
      className={`sentinel-dot sentinel-dot-${kind} ${pulse ? "sentinel-dot-pulse" : ""}`}
      aria-hidden="true"
    />
  )
}

function ToggleSwitch({ active, onClick, disabled, ariaLabel }) {
  return (
    <button
      type="button"
      className={`toggle-switch ${active ? "active" : ""}`}
      onClick={onClick}
      disabled={disabled}
      aria-pressed={active}
      aria-label={ariaLabel}
    >
      <span className="toggle-knob" />
    </button>
  )
}

function isHourActive(hour, startStr, endStr) {
  const startH = parseInt(startStr.split(":")[0])
  const endH = parseInt(endStr.split(":")[0])
  if (startH < endH) return hour >= startH && hour < endH
  return hour >= startH || hour < endH
}

// Cameras absent from camera_scope default to true (in scope) — see
// SentinelConfig docstring on the backend for the rationale.
function isCameraInScope(scope, cameraId) {
  if (!scope || typeof scope !== "object") return true
  return scope[cameraId] !== false
}

function formatRunTimestamp(iso) {
  if (!iso) return ""
  try {
    const d = new Date(iso)
    const now = new Date()
    const diffMs = now - d
    const diffMin = Math.floor(diffMs / 60000)
    const diffHr = Math.floor(diffMs / 3600000)
    const diffDay = Math.floor(diffMs / 86400000)
    if (diffMin < 1) return "just now"
    if (diffMin < 60) return `${diffMin} min ago`
    if (diffHr < 24) return `${diffHr} hr ago`
    if (diffDay === 1) return "Yesterday"
    if (diffDay < 7) return `${diffDay} days ago`
    return d.toLocaleDateString()
  } catch {
    return iso
  }
}

function outcomeDotClass(run) {
  if (run.outcome === "incident") return `sentinel-timeline-dot-incident sentinel-severity-${run.severity || "low"}`
  if (run.outcome === "error") return "sentinel-timeline-dot-error"
  if (run.outcome === "pending") return "sentinel-timeline-dot-pending"
  if (run.outcome === "running") return "sentinel-timeline-dot-running"
  return "sentinel-timeline-dot-noop"
}

function isPendingOrRunning(run) {
  return run.outcome === "pending" || run.outcome === "running"
}

// Returns the display label and chip class for the run's outcome.
function outcomeChip(run) {
  if (run.outcome === "incident") {
    return {
      label: `incident · ${run.severity || "low"}`,
      cls: `sentinel-outcome-chip sentinel-outcome-chip-incident sentinel-severity-${run.severity || "low"}`,
    }
  }
  if (run.outcome === "no_action") return { label: "no action", cls: "sentinel-outcome-chip sentinel-outcome-chip-noop" }
  if (run.outcome === "error") return { label: "errored", cls: "sentinel-outcome-chip sentinel-outcome-chip-error" }
  if (run.outcome === "pending") return { label: "pending", cls: "sentinel-outcome-chip sentinel-outcome-chip-pending" }
  if (run.outcome === "running") return { label: "running", cls: "sentinel-outcome-chip sentinel-outcome-chip-running" }
  return { label: run.outcome || "—", cls: "sentinel-outcome-chip sentinel-outcome-chip-noop" }
}

// ── main ──────────────────────────────────────────────────────────────

function SentinelPage() {
  const { getToken } = useAuth()
  const { showToast } = useToasts()

  // ── tab nav ─────────────────────────────────────────────
  const [tab, setTab] = useState("overview")

  // ── server-loaded state ─────────────────────────────────
  const [config, setConfig] = useState(null)
  const [planGated, setPlanGated] = useState(false)
  const [planCurrent, setPlanCurrent] = useState("")
  const [cameras, setCameras] = useState([])
  const [runs, setRuns] = useState([])
  const [runStats, setRunStats] = useState({ runs_today: 0, runs_total: 0, incidents_filed: 0 })
  const [loadingConfig, setLoadingConfig] = useState(true)
  const [loadingRuns, setLoadingRuns] = useState(true)

  // ── history filters ─────────────────────────────────────
  const [historyFilter, setHistoryFilter] = useState("all")
  const [historySearch, setHistorySearch] = useState("")

  // ── advanced cron toggle ────────────────────────────────
  const [showCron, setShowCron] = useState(false)

  // ── manual run modal ───────────────────────────────────
  const [showRunModal, setShowRunModal] = useState(false)
  const [manualPrompt, setManualPrompt] = useState("")
  const [running, setRunning] = useState(false)

  // ── run detail drawer ───────────────────────────────────
  const [selectedRunId, setSelectedRunId] = useState(null)
  const [selectedRun, setSelectedRun] = useState(null)
  const [loadingRun, setLoadingRun] = useState(false)

  // ── load config + cameras + runs on mount ───────────────
  useEffect(() => {
    let cancelled = false

    getSentinelConfig(getToken)
      .then(res => {
        if (cancelled) return
        setConfig(res.config)
        setPlanGated(!!res.plan_gated)
        setPlanCurrent(res.plan_current || "")
      })
      .catch(err => {
        if (cancelled) return
        showToast(err.message || "Couldn't load Sentinel config", "error")
      })
      .finally(() => {
        if (!cancelled) setLoadingConfig(false)
      })

    getCameras(getToken)
      .then(res => {
        if (cancelled) return
        // /api/cameras returns { cameras: [...] }
        setCameras(Array.isArray(res?.cameras) ? res.cameras : [])
      })
      .catch(() => {
        // Non-fatal — scope panel renders an empty state
      })

    getSentinelRuns(getToken, { limit: 50 })
      .then(res => {
        if (cancelled) return
        setRuns(Array.isArray(res?.runs) ? res.runs : [])
        setRunStats(res?.stats || { runs_today: 0, runs_total: 0, incidents_filed: 0 })
      })
      .catch(err => {
        if (cancelled) return
        showToast(err.message || "Couldn't load run history", "error")
      })
      .finally(() => {
        if (!cancelled) setLoadingRuns(false)
      })

    return () => {
      cancelled = true
    }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  // ── load run detail when a row is clicked ───────────────
  useEffect(() => {
    if (!selectedRunId) {
      setSelectedRun(null)
      return
    }
    let cancelled = false
    setLoadingRun(true)
    getSentinelRun(getToken, selectedRunId)
      .then(res => { if (!cancelled) setSelectedRun(res) })
      .catch(err => {
        if (cancelled) return
        showToast(err.message || "Couldn't load run detail", "error")
        setSelectedRunId(null)
      })
      .finally(() => { if (!cancelled) setLoadingRun(false) })
    return () => { cancelled = true }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedRunId])

  // ── optimistic update + rollback ────────────────────────
  const patchConfig = useCallback((patch) => {
    if (planGated) return  // toggles disabled in this state anyway
    const previous = config
    setConfig(prev => ({ ...prev, ...patch }))
    updateSentinelConfig(getToken, patch)
      .then(res => {
        // Server may have normalised values (e.g. active_days filtering)
        // — accept its version of truth as the new state.
        if (res?.config) setConfig(res.config)
      })
      .catch(err => {
        setConfig(previous)
        showToast(err.message || "Couldn't save", "error")
      })
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [config, planGated])

  // ── derived ─────────────────────────────────────────────
  const enabledTriggerCount = useMemo(() => {
    if (!config) return 0
    let n = 0
    for (const t of NOTIFICATION_TRIGGERS) {
      if (config[t.configField]) n++
    }
    return n
  }, [config])

  const enabledScopeCount = useMemo(() => {
    if (!config || !cameras.length) return 0
    return cameras.filter(c => isCameraInScope(config.camera_scope, c.camera_id)).length
  }, [config, cameras])

  const lastRun = runs[0] || null
  const masterEnabled = config?.enabled ?? false
  const interactive = !planGated && !loadingConfig

  // ── handlers ────────────────────────────────────────────
  function toggleTrigger(field) {
    if (!config) return
    patchConfig({ [field]: !config[field] })
  }

  function toggleScope(cameraId) {
    if (!config) return
    const next = { ...(config.camera_scope || {}) }
    next[cameraId] = !isCameraInScope(config.camera_scope, cameraId)
    patchConfig({ camera_scope: next })
  }

  function toggleDay(dayKey) {
    if (!config) return
    const current = Array.isArray(config.active_days) ? config.active_days : []
    const next = current.includes(dayKey)
      ? current.filter(d => d !== dayKey)
      : [...current, dayKey]
    patchConfig({ active_days: next })
  }

  // Manual run dispatch — creates a pending sentinel_runs row.  The
  // agent (slice 3) picks it up and updates the row to a terminal
  // outcome.  Until then the row stays pending; UI shows the pending
  // state and the operator can poke the row via the drawer.
  function runNow() {
    if (running) return
    setRunning(true)
    dispatchSentinelManualRun(getToken, { prompt: manualPrompt })
      .then(newRun => {
        // Prepend the new run so it shows up at the top of the timeline
        // immediately.  The agent will update it later.
        setRuns(prev => [newRun, ...prev])
        setRunStats(prev => ({
          ...prev,
          runs_today: (prev.runs_today || 0) + 1,
          runs_total: (prev.runs_total || 0) + 1,
          pending: (prev.pending || 0) + 1,
          // Optimistically tick the cap counter down; fall back to the
          // monthly_cap the API last reported (plan-aware: 100 for Pro,
          // 500 for Pro Plus) instead of a hardcoded 300.  Worst case
          // a missing cap value just means we don't optimistically
          // decrement — the next poll will reconcile.
          remaining_this_month: Math.max(
            0,
            (prev.remaining_this_month ?? prev.monthly_cap ?? 0) - 1,
          ),
        }))
        showToast("Sentinel queued — agent will pick it up", "success")
        setShowRunModal(false)
        setManualPrompt("")
      })
      .catch(err => {
        if (err?.code === "monthly_cap_reached") {
          showToast("Monthly cap reached — try again next month", "error")
        } else {
          showToast(err.message || "Couldn't dispatch run", "error")
        }
      })
      .finally(() => setRunning(false))
  }

  // ── render ──────────────────────────────────────────────
  if (loadingConfig) {
    return (
      <div className="sentinel-page-v3">
        <div className="sentinel-loading-state">Loading Sentinel…</div>
      </div>
    )
  }

  return (
    <div className="sentinel-page-v3">
      {/* ── plan-gate banner (free / past-due-too-long orgs) ───── */}
      {planGated && (
        <div className="sentinel-plan-banner">
          <span className="sentinel-plan-banner-pill">PRO</span>
          <span className="sentinel-plan-banner-text">
            Sentinel is a paid feature. Your current plan: <strong>{planCurrent}</strong>.
            Upgrade to Pro for 100 runs/month or Pro Plus for 500 runs/month.
            You can preview the configuration UI; changes won't save until you upgrade.
          </span>
          <Link to="/pricing" className="sentinel-plan-banner-cta">
            See plans →
          </Link>
        </div>
      )}

      {/* ── compact persistent header ──────────────────── */}
      <CompactHeader
        enabled={masterEnabled}
        onToggle={() => patchConfig({ enabled: !masterEnabled })}
        triggerCount={enabledTriggerCount}
        scopeCount={enabledScopeCount}
        totalCameras={cameras.length}
        lastRun={lastRun}
        interactive={interactive}
        onRunNow={() => setShowRunModal(true)}
      />

      {/* ── tabs ───────────────────────────────────────── */}
      <nav className="sentinel-tabs" role="tablist">
        {[
          { v: "overview", label: "Overview" },
          { v: "configure", label: "Configure" },
          { v: "history", label: "History" },
        ].map(t => (
          <button
            key={t.v}
            type="button"
            role="tab"
            aria-selected={tab === t.v}
            className={`sentinel-tab ${tab === t.v ? "active" : ""}`}
            onClick={() => setTab(t.v)}
          >
            {t.label}
          </button>
        ))}
      </nav>

      {/* ── tab panel ──────────────────────────────────── */}
      <div className="sentinel-tab-panel">
        {tab === "overview" && (
          <OverviewTab
            enabled={masterEnabled}
            scopeCount={enabledScopeCount}
            runs={runs}
            runStats={runStats}
            loadingRuns={loadingRuns}
            onSelectRun={setSelectedRunId}
          />
        )}
        {tab === "configure" && (
          <ConfigureTab
            config={config}
            interactive={interactive}
            cameras={cameras}
            patchConfig={patchConfig}
            toggleTrigger={toggleTrigger}
            toggleScope={toggleScope}
            toggleDay={toggleDay}
            showCron={showCron}
            setShowCron={setShowCron}
          />
        )}
        {tab === "history" && (
          <HistoryTab
            runs={runs}
            loadingRuns={loadingRuns}
            filter={historyFilter}
            setFilter={setHistoryFilter}
            search={historySearch}
            setSearch={setHistorySearch}
            onSelectRun={setSelectedRunId}
          />
        )}
      </div>

      {/* ── run detail drawer ──────────────────────────── */}
      {selectedRunId && (
        <RunDetailDrawer
          run={selectedRun}
          loading={loadingRun}
          onClose={() => setSelectedRunId(null)}
        />
      )}

      {/* ── manual run modal ──────────────────────────── */}
      {showRunModal && (
        <ManualRunModal
          prompt={manualPrompt}
          setPrompt={setManualPrompt}
          running={running}
          onRun={runNow}
          onClose={() => !running && setShowRunModal(false)}
        />
      )}
    </div>
  )
}

// ── components ────────────────────────────────────────────────────────

function CompactHeader({ enabled, onToggle, triggerCount, scopeCount, totalCameras, lastRun, interactive, onRunNow }) {
  let lastRunLine
  if (!lastRun) {
    lastRunLine = "No runs yet — Sentinel hasn't responded to any triggers."
  } else if (lastRun.outcome === "incident") {
    lastRunLine = `${formatRunTimestamp(lastRun.triggered_at)} — ${lastRun.trigger_type.replace(/_/g, " ")} on ${lastRun.camera_id || "all cameras"} → opened incident #${lastRun.incident_id} (${lastRun.severity})`
  } else if (lastRun.outcome === "error") {
    lastRunLine = `${formatRunTimestamp(lastRun.triggered_at)} — ${lastRun.trigger_type.replace(/_/g, " ")} on ${lastRun.camera_id || "all cameras"} → run errored`
  } else {
    lastRunLine = `${formatRunTimestamp(lastRun.triggered_at)} — ${lastRun.trigger_type.replace(/_/g, " ")} on ${lastRun.camera_id || "all cameras"} → no action`
  }

  return (
    <header className="sentinel-compact-header">
      <div className="sentinel-compact-left">
        <div className="sentinel-compact-status">
          <StatusDot kind={enabled ? "active" : "paused"} pulse={enabled} />
          <span className="sentinel-compact-status-label">
            {enabled ? "ARMED" : "PAUSED"}
          </span>
        </div>
        <div className="sentinel-compact-divider" aria-hidden="true" />
        <div className="sentinel-compact-summary">
          <div className="sentinel-compact-summary-line">
            <span className="sentinel-compact-summary-strong">Sentinel</span>
            <span className="sentinel-compact-summary-meta">
              {triggerCount} trigger{triggerCount === 1 ? "" : "s"} · {scopeCount} of {totalCameras} cameras in scope
            </span>
          </div>
          <div className="sentinel-compact-summary-sub">
            Last: {lastRunLine}
          </div>
        </div>
      </div>
      <div className="sentinel-compact-right">
        <button
          type="button"
          className="sentinel-run-now-btn"
          disabled={!interactive}
          onClick={onRunNow}
          title={interactive
            ? "Queue a one-off agent run with a custom prompt"
            : "Upgrade to Pro or Pro Plus to use Sentinel"
          }
        >
          <span aria-hidden="true">▶</span> Run now
        </button>
        <ToggleSwitch
          active={enabled}
          onClick={onToggle}
          disabled={!interactive}
          ariaLabel={enabled ? "Pause Sentinel" : "Resume Sentinel"}
        />
      </div>
    </header>
  )
}

// ── overview tab ─────────────────────────────────────────────────────

function OverviewTab({ enabled, scopeCount, runs, runStats, loadingRuns, onSelectRun }) {
  const recentRuns = runs.slice(0, 8)
  // Plan-aware monthly cap from the API (100 for Pro, 500 for Pro
  // Plus, 0 for ineligible plans).  `runs_this_month` correctly
  // counts runs in the current calendar month — was previously
  // wired to `runs_total` (lifetime), which made the meter only
  // ever fill up and never reset on month rollover.
  const monthCap = runStats.monthly_cap ?? 0
  const monthRuns = runStats.runs_this_month ?? 0
  const usagePct = monthCap > 0 ? Math.min(100, (monthRuns / monthCap) * 100) : 0
  // Tier pill on the allowance widget.  Derive from the cap itself
  // rather than passing planCurrent down as a prop — avoids prop
  // drilling and stays in sync with whatever cap the backend
  // actually reports.  100 → Pro, 500 → Pro Plus.  When the cap is
  // 0 the page is plan-gated and OverviewTab isn't rendered, so
  // this fallback is a defensive default the user shouldn't see.
  const planTierLabel = monthCap >= 500 ? "PRO PLUS" : "PRO"

  return (
    <div className="sentinel-overview">
      <div className="sentinel-overview-grid">
        {/* main column: armed hero + activity timeline */}
        <div className="sentinel-overview-main">
          {/* armed hero */}
          <div className={`sentinel-armed-hero ${enabled ? "armed" : "paused"}`}>
            <div className="sentinel-armed-hero-bg" aria-hidden="true" />
            <div className="sentinel-armed-hero-content">
              <div className="sentinel-armed-pill-row">
                <StatusDot kind={enabled ? "active" : "paused"} pulse={enabled} />
                <span className="sentinel-armed-pill">
                  {enabled ? "ARMED" : "PAUSED"}
                </span>
              </div>
              <h2 className="sentinel-armed-headline">
                {enabled
                  ? `Watching ${scopeCount} camera${scopeCount === 1 ? "" : "s"} for motion`
                  : "Sentinel is paused"}
              </h2>
              <p className="sentinel-armed-sub">
                {enabled
                  ? "Listening for triggers. Runs land in the activity feed below as they happen."
                  : "No triggers will fire until you re-enable Sentinel from the header."}
              </p>
            </div>
          </div>

          {/* activity timeline */}
          <div className="sentinel-panel">
            <div className="sentinel-panel-header">
              <h3>Recent activity</h3>
              {recentRuns.length > 0 && (
                <span className="sentinel-panel-meta">{recentRuns.length} most recent runs</span>
              )}
            </div>
            {loadingRuns ? (
              <div className="sentinel-empty-state">Loading…</div>
            ) : recentRuns.length === 0 ? (
              <div className="sentinel-empty-state">
                <p className="sentinel-empty-state-strong">Sentinel hasn't run yet.</p>
                <p className="sentinel-empty-state-sub">
                  Once a configured trigger fires, runs will appear here in chronological order.
                </p>
              </div>
            ) : (
              <ol className="sentinel-timeline">
                {recentRuns.map((r, i) => (
                  <li
                    key={r.id}
                    className="sentinel-timeline-row"
                    onClick={() => onSelectRun(r.id)}
                  >
                    <div className="sentinel-timeline-when">{formatRunTimestamp(r.triggered_at)}</div>
                    <div className="sentinel-timeline-line">
                      <span
                        className={`sentinel-timeline-dot ${outcomeDotClass(r)}`}
                        aria-hidden="true"
                      />
                      {i < recentRuns.length - 1 && (
                        <span className="sentinel-timeline-connector" aria-hidden="true" />
                      )}
                    </div>
                    <div className="sentinel-timeline-content">
                      <div className="sentinel-timeline-row1">
                        <span className={`sentinel-trigger-pill sentinel-trigger-pill-${r.trigger_type}`}>
                          {r.trigger_type.replace(/_/g, " ")}
                        </span>
                        <span className="sentinel-timeline-camera">{r.camera_id || "all cameras"}</span>
                        <span className="sentinel-timeline-tools">{r.tool_call_count} tool{r.tool_call_count === 1 ? "" : "s"}</span>
                      </div>
                      <div className="sentinel-timeline-row2">
                        {(() => {
                          const chip = outcomeChip(r)
                          return <span className={chip.cls}>{chip.label}</span>
                        })()}
                        {r.summary
                          ? (
                            <span className="sentinel-timeline-summary">
                              {r.summary.slice(0, 90)}{r.summary.length > 90 ? "…" : ""}
                            </span>
                          )
                          : isPendingOrRunning(r) && (
                            <span className="sentinel-timeline-summary sentinel-timeline-summary-muted">
                              {r.outcome === "running"
                                ? "Agent is investigating…"
                                : "Waiting for the agent to pick this up"}
                            </span>
                          )}
                      </div>
                    </div>
                  </li>
                ))}
              </ol>
            )}
          </div>
        </div>

        {/* side column: stats + allowance */}
        <aside className="sentinel-overview-side">
          <div className="sentinel-stat-card">
            <div className="sentinel-stat-card-value">{runStats.runs_today || 0}</div>
            <div className="sentinel-stat-card-label">runs today</div>
          </div>
          <div className="sentinel-stat-card">
            <div className="sentinel-stat-card-value">{runStats.runs_total || 0}</div>
            <div className="sentinel-stat-card-label">runs total</div>
          </div>
          <div className="sentinel-stat-card sentinel-stat-card-incidents">
            <div className="sentinel-stat-card-value">{runStats.incidents_filed || 0}</div>
            <div className="sentinel-stat-card-label">incidents filed</div>
          </div>

          <div className="sentinel-allowance-widget">
            <div className="sentinel-allowance-widget-header">
              <span className="sentinel-allowance-widget-title">Monthly allowance</span>
              <span className="sentinel-allowance-widget-pill">{planTierLabel}</span>
            </div>
            <div className="sentinel-allowance-widget-meter">
              <div
                className="sentinel-allowance-widget-meter-fill"
                style={{ width: `${usagePct}%` }}
              />
            </div>
            <div className="sentinel-allowance-widget-text">
              <strong>{monthRuns}</strong> of {monthCap} runs · {Math.max(0, monthCap - monthRuns)} remaining
            </div>
            <p className="sentinel-allowance-widget-help">
              Included with your plan — no per-run charge. {monthCap} runs / month, enforced at dispatch.
            </p>
          </div>
        </aside>
      </div>
    </div>
  )
}

// ── configure tab ────────────────────────────────────────────────────

function ConfigureTab({ config, interactive, cameras, patchConfig, toggleTrigger, toggleScope, toggleDay, showCron, setShowCron }) {
  if (!config) return null

  const motionEnabled = !!config.motion_enabled
  const scheduleMode = config.schedule_mode || "always"
  const scheduleStart = config.schedule_start || "22:00"
  const scheduleEnd = config.schedule_end || "06:00"
  const activeDays = Array.isArray(config.active_days) ? config.active_days : []

  return (
    <div className="sentinel-configure">
      {/* triggers */}
      <section className="sentinel-panel">
        <div className="sentinel-panel-header">
          <h3>Triggers</h3>
        </div>
        <p className="sentinel-panel-desc">
          Sentinel responds only to security-relevant events — it's a guard role,
          not an admin role. Infrastructure issues (camera offline, disk almost
          full) and admin events (member changes, MCP key audits) still flow
          through your normal notification channels; they're just not the agent's
          job.
        </p>
        <div className="sentinel-trigger-list">
          {NOTIFICATION_TRIGGERS.map(t => (
            <div key={t.key} className="sentinel-trigger-item">
              <label className="sentinel-trigger-row">
                <div className="sentinel-trigger-info">
                  <span className="sentinel-trigger-label">{t.label}</span>
                  <span className="sentinel-trigger-desc">{t.description}</span>
                </div>
                <ToggleSwitch
                  active={!!config[t.configField]}
                  onClick={() => toggleTrigger(t.configField)}
                  disabled={!interactive || !config.enabled}
                />
              </label>
              {t.configField === "motion_enabled" && motionEnabled && config.enabled && (
                <div className="sentinel-trigger-extras">
                  <div className="sentinel-trigger-extra">
                    <label htmlFor="motion-cooldown">Per-camera cooldown</label>
                    <div className="sentinel-trigger-cooldown">
                      <input
                        id="motion-cooldown"
                        type="number"
                        min="1"
                        max="60"
                        value={config.motion_cooldown_min ?? 5}
                        disabled={!interactive}
                        onChange={e => {
                          const n = parseInt(e.target.value)
                          if (!isNaN(n) && n >= 1 && n <= 60) {
                            patchConfig({ motion_cooldown_min: n })
                          }
                        }}
                      />
                      <span>minutes</span>
                    </div>
                  </div>
                  <p className="sentinel-trigger-extra-help">
                    A noisy outdoor camera can fire many motion events per minute. Cooldown
                    keeps Sentinel from running over and over for the same camera within
                    the window — separate from your email-digest cooldown, which governs
                    notification volume rather than agent runs.
                  </p>
                </div>
              )}
            </div>
          ))}
        </div>
      </section>

      {/* schedule */}
      <section className="sentinel-panel">
        <div className="sentinel-panel-header">
          <h3>Schedule</h3>
        </div>
        <p className="sentinel-panel-desc">
          When should Sentinel be available to respond to triggers? "Always on"
          means the agent runs whenever a configured trigger fires. "Scheduled"
          limits Sentinel to a window — useful if you only want agentic monitoring
          overnight.
        </p>
        <div className="sentinel-schedule-radio-row">
          {[
            { v: "always", label: "Always on", desc: "24/7 response to every trigger." },
            { v: "scheduled", label: "Scheduled", desc: "Only during the window below." },
            { v: "off", label: "Off", desc: "Never run, regardless of triggers." },
          ].map(opt => (
            <label
              key={opt.v}
              className={`sentinel-schedule-radio ${scheduleMode === opt.v ? "active" : ""}`}
            >
              <input
                type="radio"
                name="schedule-mode"
                value={opt.v}
                checked={scheduleMode === opt.v}
                disabled={!interactive}
                onChange={() => patchConfig({ schedule_mode: opt.v })}
              />
              <div className="sentinel-schedule-radio-text">
                <span className="sentinel-schedule-radio-label">{opt.label}</span>
                <span className="sentinel-schedule-radio-desc">{opt.desc}</span>
              </div>
            </label>
          ))}
        </div>

        {scheduleMode === "scheduled" && (
          <div className="sentinel-schedule-window">
            <div className="sentinel-schedule-window-row">
              <div className="sentinel-schedule-time">
                <label>Active from</label>
                <input
                  type="time"
                  value={scheduleStart}
                  disabled={!interactive}
                  onChange={e => patchConfig({ schedule_start: e.target.value })}
                />
              </div>
              <div className="sentinel-schedule-time">
                <label>Until</label>
                <input
                  type="time"
                  value={scheduleEnd}
                  disabled={!interactive}
                  onChange={e => patchConfig({ schedule_end: e.target.value })}
                />
              </div>
              <div className="sentinel-schedule-tz">Times in your org's timezone.</div>
            </div>
            <div className="sentinel-schedule-days">
              <span className="sentinel-schedule-days-label">Active days</span>
              <div className="sentinel-schedule-days-chips">
                {DAY_CHIPS.map(d => (
                  <button
                    key={d.key}
                    type="button"
                    className={`sentinel-day-chip ${activeDays.includes(d.key) ? "active" : ""}`}
                    disabled={!interactive}
                    onClick={() => toggleDay(d.key)}
                  >
                    {d.label}
                  </button>
                ))}
              </div>
            </div>
          </div>
        )}

        {scheduleMode !== "off" && (
          <WeeklyScheduleGrid
            mode={scheduleMode}
            start={scheduleStart}
            end={scheduleEnd}
            activeDays={activeDays}
          />
        )}

        {scheduleMode === "scheduled" && (
          <>
            <div className="sentinel-schedule-cron-toggle">
              <button
                type="button"
                className="sentinel-link-button"
                onClick={() => setShowCron(v => !v)}
              >
                {showCron ? "Hide" : "Show"} cron expression (advanced)
              </button>
            </div>
            {showCron && (
              <div className="sentinel-schedule-cron">
                <p className="sentinel-help-text">
                  Cron-driven wake-up sweeps land in a follow-up release. The
                  per-window schedule above governs trigger response in the
                  meantime.
                </p>
              </div>
            )}
          </>
        )}
      </section>

      {/* camera scope */}
      <section className="sentinel-panel">
        <div className="sentinel-panel-header">
          <h3>Camera scope</h3>
        </div>
        <p className="sentinel-panel-desc">
          Pick which cameras Sentinel is allowed to investigate. Triggers from
          cameras outside this scope are ignored entirely — Sentinel never sees
          the frames. Useful for keeping privacy-sensitive cameras (a bedroom,
          a child's room) off the agent's desk.
        </p>
        {cameras.length === 0 ? (
          <div className="sentinel-empty-state">
            <p className="sentinel-empty-state-strong">No cameras yet.</p>
            <p className="sentinel-empty-state-sub">
              Connect a CloudNode and your cameras will appear here.{" "}
              <Link to="/settings">Settings → Add Node</Link> to get started.
            </p>
          </div>
        ) : (
          <div className="sentinel-scope-grid">
            {cameras.map(c => (
              <label key={c.camera_id} className="sentinel-scope-row">
                <div className="sentinel-scope-row-info">
                  <span className="sentinel-scope-row-name">{c.name || c.camera_id}</span>
                  <span className="sentinel-scope-row-loc">{c.status || "unknown"}</span>
                </div>
                <ToggleSwitch
                  active={isCameraInScope(config.camera_scope, c.camera_id)}
                  onClick={() => toggleScope(c.camera_id)}
                  disabled={!interactive || !config.enabled}
                />
              </label>
            ))}
          </div>
        )}
      </section>
    </div>
  )
}

function WeeklyScheduleGrid({ mode, start, end, activeDays }) {
  const isCellActive = (dayKey, hour) => {
    if (mode === "always") return true
    if (mode === "off") return false
    if (!activeDays.includes(dayKey)) return false
    return isHourActive(hour, start, end)
  }
  const totalActiveHours = useMemo(() => {
    let n = 0
    for (const d of DAY_CHIPS) {
      for (let h = 0; h < 24; h++) {
        if (isCellActive(d.key, h)) n++
      }
    }
    return n
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mode, start, end, activeDays])

  return (
    <div className="sentinel-week-grid">
      <div className="sentinel-week-grid-header">
        <span className="sentinel-week-grid-title">Weekly schedule preview</span>
        <span className="sentinel-week-grid-meta">{totalActiveHours} of 168 hours active</span>
      </div>
      <div className="sentinel-week-grid-table">
        <div className="sentinel-week-grid-hours">
          <span aria-hidden="true" />
          <div className="sentinel-week-grid-hour-row">
            {[0, 6, 12, 18].map(h => (
              <span key={h} className="sentinel-week-grid-hour-label">
                {String(h).padStart(2, "0")}
              </span>
            ))}
          </div>
        </div>
        {DAY_CHIPS.map(d => (
          <div key={d.key} className="sentinel-week-grid-row">
            <span className="sentinel-week-grid-day-label">{d.label}</span>
            <div className="sentinel-week-grid-cells">
              {Array.from({ length: 24 }).map((_, h) => (
                <span
                  key={h}
                  className={`sentinel-week-grid-cell ${isCellActive(d.key, h) ? "active" : ""}`}
                  title={`${d.label} ${String(h).padStart(2, "0")}:00`}
                />
              ))}
            </div>
          </div>
        ))}
      </div>
    </div>
  )
}

// ── history tab ─────────────────────────────────────────────────────

function HistoryTab({ runs, loadingRuns, filter, setFilter, search, setSearch, onSelectRun }) {
  const filtered = useMemo(() => {
    let r = runs
    const now = Date.now()
    if (filter === "today") {
      const start = new Date()
      start.setHours(0, 0, 0, 0)
      r = r.filter(x => new Date(x.triggered_at) >= start)
    } else if (filter === "7d") {
      const cutoff = now - 7 * 86400000
      r = r.filter(x => new Date(x.triggered_at).getTime() >= cutoff)
    } else if (filter === "30d") {
      const cutoff = now - 30 * 86400000
      r = r.filter(x => new Date(x.triggered_at).getTime() >= cutoff)
    }
    if (search.trim()) {
      const q = search.toLowerCase()
      r = r.filter(x =>
        (x.camera_id || "").toLowerCase().includes(q) ||
        (x.trigger_type || "").toLowerCase().includes(q) ||
        (x.summary || "").toLowerCase().includes(q),
      )
    }
    return r
  }, [runs, filter, search])

  return (
    <div className="sentinel-history">
      <div className="sentinel-history-toolbar">
        <input
          type="text"
          className="sentinel-history-search"
          placeholder="Search by camera, trigger, or summary…"
          value={search}
          onChange={e => setSearch(e.target.value)}
        />
        <div className="sentinel-history-filters">
          {[
            { v: "today", label: "Today" },
            { v: "7d", label: "7 days" },
            { v: "30d", label: "30 days" },
            { v: "all", label: "All" },
          ].map(f => (
            <button
              key={f.v}
              type="button"
              className={`sentinel-history-filter-chip ${filter === f.v ? "active" : ""}`}
              onClick={() => setFilter(f.v)}
            >
              {f.label}
            </button>
          ))}
        </div>
      </div>

      {loadingRuns ? (
        <div className="sentinel-empty-state">Loading…</div>
      ) : runs.length === 0 ? (
        <div className="sentinel-empty-state">
          <p className="sentinel-empty-state-strong">Sentinel hasn't run yet.</p>
          <p className="sentinel-empty-state-sub">
            Once a configured trigger fires — motion on a camera, an incident
            opened, or a manual "Run now" — the agent investigates and the run
            appears here.
          </p>
        </div>
      ) : (
        <div className="sentinel-history-table-wrap">
          <table className="sentinel-history-table">
            <thead>
              <tr>
                <th>When</th>
                <th>Trigger</th>
                <th>Camera</th>
                <th>Tools</th>
                <th>Outcome</th>
                <th></th>
              </tr>
            </thead>
            <tbody>
              {filtered.map(r => (
                <tr key={r.id} onClick={() => onSelectRun(r.id)}>
                  <td className="sentinel-history-when">{formatRunTimestamp(r.triggered_at)}</td>
                  <td>
                    <span className={`sentinel-trigger-pill sentinel-trigger-pill-${r.trigger_type}`}>
                      {r.trigger_type.replace(/_/g, " ")}
                    </span>
                  </td>
                  <td className="sentinel-history-camera">{r.camera_id || "all cameras"}</td>
                  <td className="sentinel-history-tools">{r.tool_call_count}</td>
                  <td>
                    {(() => {
                      const chip = outcomeChip(r)
                      return <span className={chip.cls}>{chip.label}</span>
                    })()}
                  </td>
                  <td className="sentinel-history-link-cell">
                    <span className="sentinel-history-link">view →</span>
                  </td>
                </tr>
              ))}
              {filtered.length === 0 && (
                <tr>
                  <td colSpan="6" className="sentinel-history-empty">
                    No runs match your filters.
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      )}
    </div>
  )
}

// ── run detail drawer ────────────────────────────────────────────────

function RunDetailDrawer({ run, loading, onClose }) {
  return (
    <div className="sentinel-drawer-backdrop" onClick={onClose}>
      <aside className="sentinel-drawer" onClick={e => e.stopPropagation()}>
        {loading || !run ? (
          <div className="sentinel-drawer-header">
            <div>
              <div className="sentinel-drawer-eyebrow">Loading…</div>
              <h2 className="sentinel-drawer-title">&nbsp;</h2>
            </div>
            <button
              type="button"
              className="sentinel-drawer-close"
              onClick={onClose}
              aria-label="Close drawer"
            >×</button>
          </div>
        ) : (
          <>
            <div className="sentinel-drawer-header">
              <div>
                <div className="sentinel-drawer-eyebrow">Run · {run.id.slice(0, 8)}</div>
                <h2 className="sentinel-drawer-title">{run.camera_id || "All cameras"}</h2>
                <div className="sentinel-drawer-meta">
                  {new Date(run.triggered_at).toLocaleString()} · trigger:{" "}
                  <span className={`sentinel-trigger-pill sentinel-trigger-pill-${run.trigger_type}`}>
                    {run.trigger_type.replace(/_/g, " ")}
                  </span>
                </div>
              </div>
              <button
                type="button"
                className="sentinel-drawer-close"
                onClick={onClose}
                aria-label="Close drawer"
              >×</button>
            </div>

            <div className="sentinel-drawer-body">
              <section className="sentinel-drawer-section">
                <h3>Outcome</h3>
                <div className="sentinel-drawer-outcome">
                  {run.outcome === "incident" && (
                    <>
                      <span className={`sentinel-outcome-chip sentinel-outcome-chip-incident sentinel-severity-${run.severity || "low"}`}>
                        incident · {run.severity || "low"}
                      </span>
                      {run.incident_id && (
                        <Link to={`/incidents/${run.incident_id}`} className="sentinel-drawer-link">
                          View incident #{run.incident_id} →
                        </Link>
                      )}
                    </>
                  )}
                  {run.outcome === "no_action" && (
                    <span className="sentinel-outcome-chip sentinel-outcome-chip-noop">
                      no action — agent investigated and decided not to file an incident
                    </span>
                  )}
                  {run.outcome === "error" && (
                    <span className="sentinel-outcome-chip sentinel-outcome-chip-error">
                      errored — see reasoning for details
                    </span>
                  )}
                  {run.outcome === "pending" && (
                    <span className="sentinel-outcome-chip sentinel-outcome-chip-pending">
                      pending — waiting for the agent to pick this up
                    </span>
                  )}
                  {run.outcome === "running" && (
                    <span className="sentinel-outcome-chip sentinel-outcome-chip-running">
                      running — agent is investigating
                    </span>
                  )}
                </div>
              </section>

              {run.manual_prompt && (
                <section className="sentinel-drawer-section">
                  <h3>Operator prompt</h3>
                  <p className="sentinel-drawer-reasoning">{run.manual_prompt}</p>
                </section>
              )}

              {run.summary && (
                <section className="sentinel-drawer-section">
                  <h3>Agent reasoning</h3>
                  <p className="sentinel-drawer-reasoning">{run.summary}</p>
                </section>
              )}

              {Array.isArray(run.tool_trace) && run.tool_trace.length > 0 && (
                <section className="sentinel-drawer-section">
                  <h3>Tools called <span className="sentinel-drawer-section-meta">{run.tool_trace.length} of {run.tool_call_count}</span></h3>
                  <ol className="sentinel-tool-trace">
                    {run.tool_trace.map((t, i) => (
                      <li key={i} className="sentinel-tool-trace-row">
                        <span className="sentinel-tool-trace-num">{i + 1}</span>
                        <div className="sentinel-tool-trace-content">
                          <code className="sentinel-tool-trace-name">{t.tool}</code>
                          <div className="sentinel-tool-trace-args">
                            {t.args && Object.keys(t.args).length > 0
                              ? Object.entries(t.args).map(([k, v]) => (
                                  <span key={k} className="sentinel-tool-trace-arg">
                                    <span className="sentinel-tool-trace-arg-key">{k}:</span>{" "}
                                    <span className="sentinel-tool-trace-arg-val">{String(v)}</span>
                                  </span>
                                ))
                              : <span className="sentinel-tool-trace-args-empty">no arguments</span>}
                          </div>
                          {t.result && (
                            <div className="sentinel-tool-trace-result">{t.result}</div>
                          )}
                        </div>
                      </li>
                    ))}
                  </ol>
                </section>
              )}
            </div>
          </>
        )}
      </aside>
    </div>
  )
}

// ── manual run modal ─────────────────────────────────────────────────

function ManualRunModal({ prompt, setPrompt, running, onRun, onClose }) {
  return (
    <div className="sentinel-modal-backdrop" onClick={onClose}>
      <div className="sentinel-modal" onClick={e => e.stopPropagation()}>
        <h3>Queue a Sentinel run</h3>
        <p className="sentinel-modal-desc">
          Creates a pending run with your prompt. The agent will pick it up,
          investigate, and update the run with what it found. Same tools,
          same camera scope, same incident-creation rules — just kicked off
          manually instead of by a notification.
        </p>
        <textarea
          className="sentinel-modal-input"
          placeholder="e.g. Check every camera for anything unusual; pay extra attention to the driveway."
          value={prompt}
          onChange={e => setPrompt(e.target.value)}
          rows={4}
          disabled={running}
        />
        <div className="sentinel-modal-actions">
          <button
            type="button"
            className="sentinel-modal-btn-ghost"
            onClick={onClose}
            disabled={running}
          >
            Cancel
          </button>
          <button
            type="button"
            className="sentinel-modal-btn-primary"
            onClick={onRun}
            disabled={running || !prompt.trim()}
          >
            {running ? "Queuing…" : "Queue run"}
          </button>
        </div>
      </div>
    </div>
  )
}

export default SentinelPage
