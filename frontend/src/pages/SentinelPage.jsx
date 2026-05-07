import { useState, useMemo, useEffect } from "react"
import { Link } from "react-router-dom"

// SentinelPage v3 — service control UI rebuilt as a tabbed dashboard.
// Three tabs (Overview / Configure / History) with a compact persistent
// header strip on top. Front-end only — every control is wired to React
// local state and a "PREVIEW" banner at the top makes that explicit.
//
// v3 rationale: v2 was a single long scroll where 7 visually-identical
// cards competed for attention and the live signal (recent runs, armed
// status) sat below all the configuration. The tabbed layout puts each
// concern in one focused screen and promotes the live activity feed
// to the centerpiece on the default Overview tab.

// ── data ──────────────────────────────────────────────────────────────

const NOTIFICATION_TRIGGERS = [
  {
    key: "motion",
    label: "Motion detected",
    description:
      "Wake Sentinel when a camera reports motion. Uses your existing per-camera motion threshold — there's no separate Sentinel threshold to keep in sync.",
    defaultOn: true,
    extras: ["cooldown"],
  },
  {
    key: "incident_opened",
    label: "Incident opened by a human",
    description:
      "When someone files an incident manually, Sentinel auto-collects supporting evidence from the relevant cameras — like a guard helping document a report.",
    defaultOn: true,
  },
]

const MOCK_CAMERAS = [
  { id: "cam_porch", name: "Front Porch", location: "Outdoor", scopeDefault: true },
  { id: "cam_drive", name: "Driveway", location: "Outdoor", scopeDefault: true },
  { id: "cam_backyard", name: "Backyard", location: "Outdoor", scopeDefault: true },
  { id: "cam_side", name: "Side Gate", location: "Outdoor", scopeDefault: true },
  { id: "cam_garage", name: "Garage (interior)", location: "Indoor", scopeDefault: true },
  { id: "cam_living", name: "Living Room", location: "Indoor", scopeDefault: false },
  { id: "cam_kitchen", name: "Kitchen", location: "Indoor", scopeDefault: false },
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

const MOCK_RUNS = [
  { id: "r12", at: "3 min ago", atFull: "2026-05-07 03:42", trigger: "motion", camera: "Front Porch", tools: 4, outcome: "incident", severity: "low", incidentId: 423, summary: "A delivery driver dropped a small package on the porch. The driver did not enter the property. No threat detected, but the package delivery is logged for owner reference." },
  { id: "r11", at: "12 min ago", atFull: "2026-05-07 03:33", trigger: "motion", camera: "Driveway", tools: 6, outcome: "incident", severity: "medium", incidentId: 422, summary: "An unfamiliar dark-colored sedan pulled into the driveway and remained stationary for ~3 minutes. The driver did not exit the vehicle. The vehicle then reversed and left. License plate not legible from camera angle. Recommend owner review the attached snapshot." },
  { id: "r10", at: "47 min ago", atFull: "2026-05-07 02:58", trigger: "scheduled", camera: "All cameras", tools: 9, outcome: "no_action", summary: "Hourly sweep — captured snapshots from all 5 cameras in scope. Nothing of concern. All cameras reporting nominally." },
  { id: "r09", at: "2 hr ago", atFull: "2026-05-07 01:45", trigger: "manual", camera: "Backyard", tools: 3, outcome: "no_action", summary: "Operator-initiated check of Backyard camera. Scene clear; deck chairs and patio table in expected positions. No movement detected in the snapshot." },
  { id: "r08", at: "4 hr ago", atFull: "2026-05-06 23:45", trigger: "motion", camera: "Side Gate", tools: 5, outcome: "incident", severity: "low", incidentId: 421, summary: "A neighborhood cat passed through the side gate area. Confirmed via watch_camera burst (4 frames over 2 seconds). No threat." },
  { id: "r07", at: "6 hr ago", atFull: "2026-05-06 21:45", trigger: "motion", camera: "Driveway", tools: 5, outcome: "no_action", summary: "Brief motion event triggered by car headlights from the street reflecting off the driveway. No vehicles or people on property. False positive — recommend tightening the per-camera motion threshold for Driveway." },
  { id: "r06", at: "Yesterday", atFull: "2026-05-06 14:22", trigger: "motion", camera: "Driveway", tools: 8, outcome: "incident", severity: "high", incidentId: 418, summary: "Two unfamiliar individuals approached the front door from the driveway. They lingered near the porch for ~90 seconds, examining the property. One individual photographed the front door area. They left in an unmarked white van. License partially captured. Owner should review attached evidence and consider notifying local authorities if needed." },
  { id: "r05", at: "Yesterday", atFull: "2026-05-06 09:15", trigger: "motion", camera: "Front Porch", tools: 4, outcome: "incident", severity: "low", incidentId: 417, summary: "Mail carrier delivered mail to the porch mailbox. Routine delivery — recognized uniform and timing matches typical mail schedule." },
  { id: "r04", at: "2 days ago", atFull: "2026-05-05 02:00", trigger: "scheduled", camera: "All cameras", tools: 7, outcome: "no_action", summary: "Scheduled overnight sweep. All cameras reporting nominally. No motion events detected during the sweep window." },
  { id: "r03", at: "3 days ago", atFull: "2026-05-04 16:30", trigger: "incident_opened", camera: "operator filing", tools: 2, outcome: "no_action", summary: "Operator manually filed an incident regarding suspicious activity reported by a neighbor. Sentinel reviewed footage from the relevant time window across Front Porch and Driveway cameras. No supporting visual evidence found." },
  { id: "r02", at: "4 days ago", atFull: "2026-05-03 11:45", trigger: "motion", camera: "Driveway", tools: 6, outcome: "incident", severity: "low", incidentId: 410, summary: "Lawn care service arrived. Routine landscaping — recognized vehicle and worker patterns from prior visits." },
  { id: "r01", at: "5 days ago", atFull: "2026-05-02 22:10", trigger: "motion", camera: "Backyard", tools: 4, outcome: "error", summary: "Run errored — view_camera tool returned a 504 timeout from the Backyard CloudNode. The node had recovered by the time of the next motion event. No action required; incident not filed." },
]

// Mocked tool-call traces for the run-detail drawer. Keeping these
// inline rather than synthesised on demand so the demo content reads
// like a real agent reasoning trace, not a placeholder.
const MOCK_TOOL_TRACE = [
  { tool: "list_cameras", args: {}, result: "5 cameras in scope · all reporting nominally" },
  { tool: "view_camera", args: { camera_id: "cam_porch" }, result: "JPEG snapshot retrieved · 1280×720" },
  { tool: "create_incident", args: { camera_id: "cam_porch", severity: "low", title: "Package delivery on Front Porch" }, result: "Incident #423 created" },
  { tool: "attach_snapshot", args: { incident_id: 423, camera_id: "cam_porch" }, result: "Snapshot attached · 4.2 KB JPEG" },
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

function formatLastCheck(seconds) {
  if (seconds < 60) return `${seconds}s ago`
  const m = Math.floor(seconds / 60)
  const s = seconds % 60
  if (m < 60) return s ? `${m}m ${s}s ago` : `${m}m ago`
  const h = Math.floor(m / 60)
  return `${h}h ${m % 60}m ago`
}

function isHourActive(hour, startStr, endStr) {
  const startH = parseInt(startStr.split(":")[0])
  const endH = parseInt(endStr.split(":")[0])
  if (startH < endH) return hour >= startH && hour < endH
  return hour >= startH || hour < endH
}

// ── main ──────────────────────────────────────────────────────────────

function SentinelPage() {
  // ── tab nav ─────────────────────────────────────────────
  const [tab, setTab] = useState("overview")

  // ── master state ────────────────────────────────────────
  const [enabled, setEnabled] = useState(true)

  // ── triggers ────────────────────────────────────────────
  const [triggers, setTriggers] = useState(() =>
    Object.fromEntries(NOTIFICATION_TRIGGERS.map(t => [t.key, t.defaultOn])),
  )
  const [motionCooldownMin, setMotionCooldownMin] = useState(5)

  // ── schedule ────────────────────────────────────────────
  const [scheduleMode, setScheduleMode] = useState("always")
  const [scheduleStart, setScheduleStart] = useState("22:00")
  const [scheduleEnd, setScheduleEnd] = useState("06:00")
  const [activeDays, setActiveDays] = useState(() =>
    Object.fromEntries(DAY_CHIPS.map(d => [d.key, true])),
  )
  const [showCron, setShowCron] = useState(false)
  const [cron, setCron] = useState("0 */1 * * *")

  // ── camera scope ────────────────────────────────────────
  const [scope, setScope] = useState(() =>
    Object.fromEntries(MOCK_CAMERAS.map(c => [c.id, c.scopeDefault])),
  )

  // ── history filters ─────────────────────────────────────
  const [historyFilter, setHistoryFilter] = useState("all")
  const [historySearch, setHistorySearch] = useState("")

  // ── run detail drawer ───────────────────────────────────
  const [selectedRunId, setSelectedRunId] = useState(null)

  // ── manual run modal ────────────────────────────────────
  const [showRunModal, setShowRunModal] = useState(false)
  const [manualPrompt, setManualPrompt] = useState("")
  const [running, setRunning] = useState(false)

  // ── live last-check tick (mocked, gives the page a pulse) ─
  const [lastCheckSec, setLastCheckSec] = useState(12)
  useEffect(() => {
    const tickId = setInterval(() => setLastCheckSec(s => s + 1), 1000)
    // every ~45s the agent does a heartbeat check; reset
    const heartbeatId = setInterval(() => setLastCheckSec(0), 45000)
    return () => {
      clearInterval(tickId)
      clearInterval(heartbeatId)
    }
  }, [])

  // ── derived ─────────────────────────────────────────────
  const enabledScopeCount = useMemo(
    () => Object.values(scope).filter(Boolean).length,
    [scope],
  )
  const enabledTriggerCount = useMemo(
    () => Object.values(triggers).filter(Boolean).length,
    [triggers],
  )
  const selectedRun = useMemo(
    () => MOCK_RUNS.find(r => r.id === selectedRunId) || null,
    [selectedRunId],
  )

  // ── handlers ────────────────────────────────────────────
  function toggleTrigger(key) {
    setTriggers(prev => ({ ...prev, [key]: !prev[key] }))
  }
  function toggleScope(id) {
    setScope(prev => ({ ...prev, [id]: !prev[id] }))
  }
  function toggleDay(key) {
    setActiveDays(prev => ({ ...prev, [key]: !prev[key] }))
  }
  function runNow() {
    setRunning(true)
    setTimeout(() => {
      setRunning(false)
      setShowRunModal(false)
      setManualPrompt("")
    }, 1800)
  }

  return (
    <div className="sentinel-page-v3">
      {/* ── preview banner ─────────────────────────────── */}
      <div className="sentinel-preview-banner">
        <span className="sentinel-preview-pill">PREVIEW</span>
        <span className="sentinel-preview-text">
          The Sentinel control UI ships before the agent itself. Controls are
          wired to local state only — your changes won't persist until the
          backend lands. <Link to="/docs#sentinel" className="sentinel-preview-link">What's coming →</Link>
        </span>
      </div>

      {/* ── compact persistent header ──────────────────── */}
      <CompactHeader
        enabled={enabled}
        onToggle={() => setEnabled(v => !v)}
        triggerCount={enabledTriggerCount}
        scopeCount={enabledScopeCount}
        totalCameras={MOCK_CAMERAS.length}
        lastRun={MOCK_RUNS[0]}
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
            enabled={enabled}
            scopeCount={enabledScopeCount}
            lastCheckSec={lastCheckSec}
            onSelectRun={setSelectedRunId}
          />
        )}
        {tab === "configure" && (
          <ConfigureTab
            enabled={enabled}
            triggers={triggers}
            toggleTrigger={toggleTrigger}
            motionCooldownMin={motionCooldownMin}
            setMotionCooldownMin={setMotionCooldownMin}
            scheduleMode={scheduleMode}
            setScheduleMode={setScheduleMode}
            scheduleStart={scheduleStart}
            setScheduleStart={setScheduleStart}
            scheduleEnd={scheduleEnd}
            setScheduleEnd={setScheduleEnd}
            activeDays={activeDays}
            toggleDay={toggleDay}
            showCron={showCron}
            setShowCron={setShowCron}
            cron={cron}
            setCron={setCron}
            scope={scope}
            toggleScope={toggleScope}
          />
        )}
        {tab === "history" && (
          <HistoryTab
            filter={historyFilter}
            setFilter={setHistoryFilter}
            search={historySearch}
            setSearch={setHistorySearch}
            onSelectRun={setSelectedRunId}
          />
        )}
      </div>

      {/* ── run detail drawer ──────────────────────────── */}
      {selectedRun && (
        <RunDetailDrawer run={selectedRun} onClose={() => setSelectedRunId(null)} />
      )}

      {/* ── manual run modal ───────────────────────────── */}
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

function CompactHeader({ enabled, onToggle, triggerCount, scopeCount, totalCameras, lastRun, onRunNow }) {
  const lastRunLine = lastRun.outcome === "incident"
    ? `${lastRun.at} — motion on ${lastRun.camera} → opened incident #${lastRun.incidentId} (${lastRun.severity})`
    : lastRun.outcome === "error"
      ? `${lastRun.at} — ${lastRun.trigger} on ${lastRun.camera} → run errored`
      : `${lastRun.at} — ${lastRun.trigger} on ${lastRun.camera} → no action`

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
        <button type="button" className="sentinel-run-now-btn" onClick={onRunNow}>
          <span aria-hidden="true">▶</span> Run now
        </button>
        <ToggleSwitch
          active={enabled}
          onClick={onToggle}
          ariaLabel={enabled ? "Pause Sentinel" : "Resume Sentinel"}
        />
      </div>
    </header>
  )
}

// ── overview tab ─────────────────────────────────────────────────────

function OverviewTab({ enabled, scopeCount, lastCheckSec, onSelectRun }) {
  const recentRuns = MOCK_RUNS.slice(0, 8)
  const todayRuns = MOCK_RUNS.filter(r => r.at.includes("min") || r.at.includes("hr")).length
  const monthRuns = 142
  const monthCap = 300
  const monthIncidents = 38
  const usagePct = (monthRuns / monthCap) * 100

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
                  ? <>Listening for triggers. Last health-check <strong>{formatLastCheck(lastCheckSec)}</strong>.</>
                  : "No triggers will fire until you re-enable Sentinel from the header."}
              </p>
            </div>
          </div>

          {/* activity timeline */}
          <div className="sentinel-panel">
            <div className="sentinel-panel-header">
              <h3>Recent activity</h3>
              <span className="sentinel-panel-meta">{recentRuns.length} most recent runs</span>
            </div>
            <ol className="sentinel-timeline">
              {recentRuns.map((r, i) => (
                <li
                  key={r.id}
                  className="sentinel-timeline-row"
                  onClick={() => onSelectRun(r.id)}
                >
                  <div className="sentinel-timeline-when">{r.at}</div>
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
                      <span className={`sentinel-trigger-pill sentinel-trigger-pill-${r.trigger}`}>
                        {r.trigger.replace(/_/g, " ")}
                      </span>
                      <span className="sentinel-timeline-camera">{r.camera}</span>
                      <span className="sentinel-timeline-tools">{r.tools} tool{r.tools === 1 ? "" : "s"}</span>
                    </div>
                    <div className="sentinel-timeline-row2">
                      {r.outcome === "incident" && (
                        <span className={`sentinel-outcome-chip sentinel-outcome-chip-incident sentinel-severity-${r.severity}`}>
                          incident · {r.severity}
                        </span>
                      )}
                      {r.outcome === "no_action" && (
                        <span className="sentinel-outcome-chip sentinel-outcome-chip-noop">
                          no action
                        </span>
                      )}
                      {r.outcome === "error" && (
                        <span className="sentinel-outcome-chip sentinel-outcome-chip-error">
                          errored
                        </span>
                      )}
                      <span className="sentinel-timeline-summary">{r.summary.slice(0, 90)}{r.summary.length > 90 ? "…" : ""}</span>
                    </div>
                  </div>
                </li>
              ))}
            </ol>
          </div>
        </div>

        {/* side column: stats + allowance */}
        <aside className="sentinel-overview-side">
          <div className="sentinel-stat-card">
            <div className="sentinel-stat-card-value">{todayRuns}</div>
            <div className="sentinel-stat-card-label">runs today</div>
          </div>
          <div className="sentinel-stat-card">
            <div className="sentinel-stat-card-value">{monthRuns}</div>
            <div className="sentinel-stat-card-label">runs this month</div>
          </div>
          <div className="sentinel-stat-card sentinel-stat-card-incidents">
            <div className="sentinel-stat-card-value">{monthIncidents}</div>
            <div className="sentinel-stat-card-label">incidents filed</div>
          </div>

          <div className="sentinel-allowance-widget">
            <div className="sentinel-allowance-widget-header">
              <span className="sentinel-allowance-widget-title">Monthly allowance</span>
              <span className="sentinel-allowance-widget-pill">PRO PLUS</span>
            </div>
            <div className="sentinel-allowance-widget-meter">
              <div
                className="sentinel-allowance-widget-meter-fill"
                style={{ width: `${usagePct}%` }}
              />
            </div>
            <div className="sentinel-allowance-widget-text">
              <strong>{monthRuns}</strong> of {monthCap} runs · {Math.round(100 - usagePct)}% remaining
            </div>
            <p className="sentinel-allowance-widget-help">
              Included with your plan — no per-run charge. Resets on the 1st.
            </p>
          </div>
        </aside>
      </div>
    </div>
  )
}

function outcomeDotClass(run) {
  if (run.outcome === "incident") return `sentinel-timeline-dot-incident sentinel-severity-${run.severity}`
  if (run.outcome === "error") return "sentinel-timeline-dot-error"
  return "sentinel-timeline-dot-noop"
}

// ── configure tab ────────────────────────────────────────────────────

function ConfigureTab(props) {
  const {
    enabled, triggers, toggleTrigger,
    motionCooldownMin, setMotionCooldownMin,
    scheduleMode, setScheduleMode,
    scheduleStart, setScheduleStart, scheduleEnd, setScheduleEnd,
    activeDays, toggleDay,
    showCron, setShowCron, cron, setCron,
    scope, toggleScope,
  } = props

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
                  active={triggers[t.key]}
                  onClick={() => toggleTrigger(t.key)}
                  disabled={!enabled}
                />
              </label>
              {t.key === "motion" && triggers.motion && enabled && (
                <div className="sentinel-trigger-extras">
                  <div className="sentinel-trigger-extra">
                    <label htmlFor="motion-cooldown">Per-camera cooldown</label>
                    <div className="sentinel-trigger-cooldown">
                      <input
                        id="motion-cooldown"
                        type="number"
                        min="1"
                        max="60"
                        value={motionCooldownMin}
                        onChange={e => setMotionCooldownMin(parseInt(e.target.value) || 1)}
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
                onChange={() => setScheduleMode(opt.v)}
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
                  onChange={e => setScheduleStart(e.target.value)}
                />
              </div>
              <div className="sentinel-schedule-time">
                <label>Until</label>
                <input
                  type="time"
                  value={scheduleEnd}
                  onChange={e => setScheduleEnd(e.target.value)}
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
                    className={`sentinel-day-chip ${activeDays[d.key] ? "active" : ""}`}
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
                <label htmlFor="sentinel-cron-input">Cron expression</label>
                <input
                  id="sentinel-cron-input"
                  type="text"
                  value={cron}
                  onChange={e => setCron(e.target.value)}
                  placeholder="0 */1 * * *"
                />
                <p className="sentinel-help-text">
                  Five-field cron. Used for scheduled "wake-up" sweeps that run regardless
                  of triggers — e.g. every hour, take a snapshot of every camera and
                  report any change. The window above still gates the cron — Sentinel
                  won't fire even on a cron tick if the current time is outside the active
                  window.
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
        <div className="sentinel-scope-grid">
          {MOCK_CAMERAS.map(c => (
            <label key={c.id} className="sentinel-scope-row">
              <div className="sentinel-scope-row-info">
                <span className="sentinel-scope-row-name">{c.name}</span>
                <span className="sentinel-scope-row-loc">{c.location}</span>
              </div>
              <ToggleSwitch
                active={scope[c.id]}
                onClick={() => toggleScope(c.id)}
                disabled={!props.enabled}
              />
            </label>
          ))}
        </div>
      </section>
    </div>
  )
}

function WeeklyScheduleGrid({ mode, start, end, activeDays }) {
  const isCellActive = (dayKey, hour) => {
    if (mode === "always") return true
    if (mode === "off") return false
    if (!activeDays[dayKey]) return false
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
          <span></span>
          {[0, 6, 12, 18].map(h => (
            <span key={h} className="sentinel-week-grid-hour-label">
              {String(h).padStart(2, "0")}
            </span>
          ))}
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

function HistoryTab({ filter, setFilter, search, setSearch, onSelectRun }) {
  const filtered = useMemo(() => {
    let r = MOCK_RUNS
    if (filter === "today") r = r.filter(x => x.at.includes("min") || x.at.includes("hr"))
    if (filter === "7d") r = r.filter(x => !x.at.includes("days") || parseInt(x.at) <= 7)
    if (search.trim()) {
      const q = search.toLowerCase()
      r = r.filter(x => x.camera.toLowerCase().includes(q) || x.trigger.toLowerCase().includes(q) || x.summary.toLowerCase().includes(q))
    }
    return r
  }, [filter, search])

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
                <td className="sentinel-history-when">{r.at}</td>
                <td>
                  <span className={`sentinel-trigger-pill sentinel-trigger-pill-${r.trigger}`}>
                    {r.trigger.replace(/_/g, " ")}
                  </span>
                </td>
                <td className="sentinel-history-camera">{r.camera}</td>
                <td className="sentinel-history-tools">{r.tools}</td>
                <td>
                  {r.outcome === "incident" && (
                    <span className={`sentinel-outcome-chip sentinel-outcome-chip-incident sentinel-severity-${r.severity}`}>
                      incident · {r.severity}
                    </span>
                  )}
                  {r.outcome === "no_action" && (
                    <span className="sentinel-outcome-chip sentinel-outcome-chip-noop">no action</span>
                  )}
                  {r.outcome === "error" && (
                    <span className="sentinel-outcome-chip sentinel-outcome-chip-error">errored</span>
                  )}
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
    </div>
  )
}

// ── run detail drawer ────────────────────────────────────────────────

function RunDetailDrawer({ run, onClose }) {
  return (
    <div className="sentinel-drawer-backdrop" onClick={onClose}>
      <aside className="sentinel-drawer" onClick={e => e.stopPropagation()}>
        <div className="sentinel-drawer-header">
          <div>
            <div className="sentinel-drawer-eyebrow">Run · {run.id}</div>
            <h2 className="sentinel-drawer-title">{run.camera}</h2>
            <div className="sentinel-drawer-meta">
              {run.atFull} · trigger:{" "}
              <span className={`sentinel-trigger-pill sentinel-trigger-pill-${run.trigger}`}>
                {run.trigger.replace(/_/g, " ")}
              </span>
            </div>
          </div>
          <button
            type="button"
            className="sentinel-drawer-close"
            onClick={onClose}
            aria-label="Close drawer"
          >
            ×
          </button>
        </div>

        <div className="sentinel-drawer-body">
          <section className="sentinel-drawer-section">
            <h3>Outcome</h3>
            <div className="sentinel-drawer-outcome">
              {run.outcome === "incident" && (
                <>
                  <span className={`sentinel-outcome-chip sentinel-outcome-chip-incident sentinel-severity-${run.severity}`}>
                    incident · {run.severity}
                  </span>
                  <Link to={`/incidents/${run.incidentId}`} className="sentinel-drawer-link">
                    View incident #{run.incidentId} →
                  </Link>
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
            </div>
          </section>

          <section className="sentinel-drawer-section">
            <h3>Agent reasoning</h3>
            <p className="sentinel-drawer-reasoning">{run.summary}</p>
          </section>

          <section className="sentinel-drawer-section">
            <h3>Tools called <span className="sentinel-drawer-section-meta">{MOCK_TOOL_TRACE.length} of {run.tools}</span></h3>
            <ol className="sentinel-tool-trace">
              {MOCK_TOOL_TRACE.slice(0, run.tools).map((t, i) => (
                <li key={i} className="sentinel-tool-trace-row">
                  <span className="sentinel-tool-trace-num">{i + 1}</span>
                  <div className="sentinel-tool-trace-content">
                    <code className="sentinel-tool-trace-name">{t.tool}</code>
                    <div className="sentinel-tool-trace-args">
                      {Object.keys(t.args).length === 0
                        ? <span className="sentinel-tool-trace-args-empty">no arguments</span>
                        : Object.entries(t.args).map(([k, v]) => (
                          <span key={k} className="sentinel-tool-trace-arg">
                            <span className="sentinel-tool-trace-arg-key">{k}:</span>{" "}
                            <span className="sentinel-tool-trace-arg-val">{String(v)}</span>
                          </span>
                        ))}
                    </div>
                    <div className="sentinel-tool-trace-result">{t.result}</div>
                  </div>
                </li>
              ))}
            </ol>
          </section>

          <section className="sentinel-drawer-actions">
            <button type="button" className="sentinel-drawer-btn-ghost">Re-run</button>
            <button type="button" className="sentinel-drawer-btn-ghost">Mark as false positive</button>
          </section>
        </div>
      </aside>
    </div>
  )
}

// ── manual run modal ─────────────────────────────────────────────────

function ManualRunModal({ prompt, setPrompt, running, onRun, onClose }) {
  return (
    <div className="sentinel-modal-backdrop" onClick={onClose}>
      <div className="sentinel-modal" onClick={e => e.stopPropagation()}>
        <h3>Run Sentinel now</h3>
        <p className="sentinel-modal-desc">
          Trigger a one-off agent run with a custom prompt. Same tools, same
          camera scope, same incident-creation rules — just kicked off
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
            {running ? "Running…" : "Run"}
          </button>
        </div>
      </div>
    </div>
  )
}

export default SentinelPage
