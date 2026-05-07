import { useState, useMemo } from "react"
import { Link } from "react-router-dom"

// SentinelPage v2 — service control UI (front-end only, mock state).
// Replaces the v1 marketing/coming-soon page with a real
// configuration + run-history surface so an operator can see
// what shipping Sentinel will look like before the backend lands.
// All controls are wired to local React state — there's no API
// call here yet. The "Preview UI" banner at the top makes that
// clear so a curious user doesn't think they actually configured
// something.

// Sentinel is intentionally narrow — it's a security role (a "night
// guard"), not an admin or IT role. Triggers only fire for events a
// human security guard would actually act on. Infrastructure events
// (camera offline, node offline, disk low) and admin events (member
// changes, MCP key audits) deliberately go to your normal notification
// channels instead — they're not the agent's job.
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

// Mock run history — 12 realistic runs spanning ~1 week.
// Outcomes mix incident-filed / no-action / one error so the
// table reads like a real ops surface.
const MOCK_RUNS = [
  { id: "r12", at: "3 min ago", trigger: "motion", camera: "Front Porch", tools: 4, outcome: "incident", severity: "low", incidentId: 423 },
  { id: "r11", at: "12 min ago", trigger: "motion", camera: "Driveway", tools: 6, outcome: "incident", severity: "medium", incidentId: 422 },
  { id: "r10", at: "47 min ago", trigger: "scheduled", camera: "All cameras", tools: 9, outcome: "no_action" },
  { id: "r09", at: "2 hr ago", trigger: "manual", camera: "Backyard", tools: 3, outcome: "no_action" },
  { id: "r08", at: "4 hr ago", trigger: "motion", camera: "Side Gate", tools: 5, outcome: "incident", severity: "low", incidentId: 421 },
  { id: "r07", at: "6 hr ago", trigger: "node_offline", camera: "Garage CloudNode", tools: 2, outcome: "no_action" },
  { id: "r06", at: "Yesterday", trigger: "motion", camera: "Driveway", tools: 8, outcome: "incident", severity: "high", incidentId: 418 },
  { id: "r05", at: "Yesterday", trigger: "motion", camera: "Front Porch", tools: 4, outcome: "incident", severity: "low", incidentId: 417 },
  { id: "r04", at: "2 days ago", trigger: "cloudnode_disk_low", camera: "Garage CloudNode", tools: 1, outcome: "no_action" },
  { id: "r03", at: "3 days ago", trigger: "incident_opened", camera: "manual filing", tools: 2, outcome: "no_action" },
  { id: "r02", at: "4 days ago", trigger: "motion", camera: "Driveway", tools: 6, outcome: "incident", severity: "low", incidentId: 410 },
  { id: "r01", at: "5 days ago", trigger: "motion", camera: "Backyard", tools: 4, outcome: "error" },
]

function StatusDot({ kind }) {
  return <span className={`sentinel-status-dot sentinel-status-dot-${kind}`} aria-hidden="true" />
}

function ToggleSwitch({ active, onClick, disabled }) {
  return (
    <button
      type="button"
      className={`toggle-switch ${active ? "active" : ""}`}
      onClick={onClick}
      disabled={disabled}
      aria-pressed={active}
    >
      <span className="toggle-knob" />
    </button>
  )
}

function SentinelPage() {
  // ── master state ─────────────────────────────────────────
  const [enabled, setEnabled] = useState(true)

  // ── triggers ─────────────────────────────────────────────
  const [triggers, setTriggers] = useState(() =>
    Object.fromEntries(NOTIFICATION_TRIGGERS.map(t => [t.key, t.defaultOn])),
  )
  const [motionCooldownMin, setMotionCooldownMin] = useState(5)

  // ── schedule ─────────────────────────────────────────────
  const [scheduleMode, setScheduleMode] = useState("always") // always | scheduled | off
  const [scheduleStart, setScheduleStart] = useState("22:00")
  const [scheduleEnd, setScheduleEnd] = useState("06:00")
  const [activeDays, setActiveDays] = useState(() =>
    Object.fromEntries(DAY_CHIPS.map(d => [d.key, true])),
  )
  const [showCron, setShowCron] = useState(false)
  const [cron, setCron] = useState("0 */1 * * *")

  // ── camera scope ─────────────────────────────────────────
  const [scope, setScope] = useState(() =>
    Object.fromEntries(MOCK_CAMERAS.map(c => [c.id, c.scopeDefault])),
  )

  // ── run history ──────────────────────────────────────────
  const [historyFilter, setHistoryFilter] = useState("all") // all | today | 7d | 30d

  // ── manual run modal ─────────────────────────────────────
  const [showRunModal, setShowRunModal] = useState(false)
  const [manualPrompt, setManualPrompt] = useState("")
  const [running, setRunning] = useState(false)

  const enabledScopeCount = useMemo(
    () => Object.values(scope).filter(Boolean).length,
    [scope],
  )
  const enabledTriggerCount = useMemo(
    () => Object.values(triggers).filter(Boolean).length,
    [triggers],
  )

  // Filtered runs (mock — dates are strings so we just take all rows;
  // a real impl would parse and compare against the filter window).
  const filteredRuns = useMemo(() => {
    if (historyFilter === "today") return MOCK_RUNS.filter(r => r.at.includes("min") || r.at.includes("hr"))
    if (historyFilter === "7d") return MOCK_RUNS.filter(r => !r.at.includes("days") || parseInt(r.at) <= 7)
    if (historyFilter === "30d") return MOCK_RUNS
    return MOCK_RUNS
  }, [historyFilter])

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

  // ── "last run" summary line — derived from MOCK_RUNS[0] ────
  const lastRun = MOCK_RUNS[0]
  const lastRunSummary = lastRun.outcome === "incident"
    ? `${lastRun.at} — ${lastRun.trigger} on ${lastRun.camera} → opened incident #${lastRun.incidentId} (${lastRun.severity})`
    : lastRun.outcome === "error"
      ? `${lastRun.at} — ${lastRun.trigger} on ${lastRun.camera} → run errored`
      : `${lastRun.at} — ${lastRun.trigger} on ${lastRun.camera} → no action`

  return (
    <div className="sentinel-control-page">
      {/* ── preview banner ─────────────────────────────────── */}
      <div className="sentinel-preview-banner">
        <span className="sentinel-preview-pill">PREVIEW</span>
        <span className="sentinel-preview-text">
          The Sentinel control UI ships before the agent itself. Controls below
          are wired to local state only — your changes won't persist until the
          backend lands. <Link to="/docs#sentinel" className="sentinel-preview-link">What's coming →</Link>
        </span>
      </div>

      {/* ── status header strip ────────────────────────────── */}
      <header className="sentinel-status-header">
        <div className="sentinel-status-header-left">
          <div className="sentinel-status-pill-row">
            <StatusDot kind={enabled ? "active" : "paused"} />
            <span className="sentinel-status-pill-label">
              {enabled ? "Active" : "Paused"}
            </span>
            <span className="sentinel-status-config-summary">
              · {enabledTriggerCount} trigger{enabledTriggerCount === 1 ? "" : "s"} on
              · {enabledScopeCount} of {MOCK_CAMERAS.length} cameras in scope
            </span>
          </div>
          <h1 className="sentinel-status-title">Sentinel</h1>
          <p className="sentinel-status-subtitle">
            Last run: <span className="sentinel-status-subtitle-strong">{lastRunSummary}</span>
          </p>
          <div className="sentinel-status-stats">
            <div className="sentinel-status-stat">
              <span className="sentinel-status-stat-value">7</span>
              <span className="sentinel-status-stat-label">runs today</span>
            </div>
            <div className="sentinel-status-stat">
              <span className="sentinel-status-stat-value">142</span>
              <span className="sentinel-status-stat-label">runs this month</span>
            </div>
            <div className="sentinel-status-stat">
              <span className="sentinel-status-stat-value">38</span>
              <span className="sentinel-status-stat-label">incidents filed</span>
            </div>
          </div>
        </div>
        <div className="sentinel-status-header-right">
          <button
            type="button"
            className="sentinel-run-now-btn"
            onClick={() => setShowRunModal(true)}
          >
            Run now
          </button>
          <label className="sentinel-master-toggle">
            <span className="sentinel-master-toggle-label">
              {enabled ? "Sentinel is on" : "Sentinel is paused"}
            </span>
            <ToggleSwitch active={enabled} onClick={() => setEnabled(v => !v)} />
          </label>
        </div>
      </header>

      {/* ── triggers ───────────────────────────────────────── */}
      <section className="settings-section sentinel-section">
        <div className="sentinel-section-header">
          <h2>Triggers</h2>
          <p className="section-description">
            Sentinel responds only to security-relevant events — it's a guard
            role, not an admin role. Infrastructure issues (camera offline,
            disk almost full) and admin events (member changes, MCP key audits)
            still flow through your normal notification channels; they're just
            not the agent's job.
          </p>
        </div>
        <div className="settings-toggles">
          {NOTIFICATION_TRIGGERS.map(t => (
            <div key={t.key} className="sentinel-trigger-row">
              <label className="toggle-row sentinel-trigger-row-main">
                <div className="toggle-info">
                  <span className="toggle-label">{t.label}</span>
                  <span className="toggle-desc">{t.description}</span>
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

      {/* ── schedule ───────────────────────────────────────── */}
      <section className="settings-section sentinel-section">
        <div className="sentinel-section-header">
          <h2>Schedule</h2>
          <p className="section-description">
            When should Sentinel be available to respond to triggers? "Always
            on" means the agent runs whenever a configured trigger fires.
            "Scheduled" limits Sentinel to a window — useful if you only want
            agentic monitoring overnight.
          </p>
        </div>

        <div className="sentinel-schedule-radio-row">
          {[
            { v: "always", label: "Always on", desc: "Sentinel runs on every configured trigger, 24/7." },
            { v: "scheduled", label: "Scheduled", desc: "Sentinel only runs during the window below." },
            { v: "off", label: "Off", desc: "Sentinel will not run, regardless of triggers." },
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
                  of triggers — e.g. "every hour, take a snapshot of every camera and
                  report any change." The window above still gates the cron — Sentinel
                  won't fire even on a cron tick if the current time is outside the active
                  window.
                </p>
              </div>
            )}
          </div>
        )}
      </section>

      {/* ── camera scope ───────────────────────────────────── */}
      <section className="settings-section sentinel-section">
        <div className="sentinel-section-header">
          <h2>Camera scope</h2>
          <p className="section-description">
            Pick which cameras Sentinel is allowed to investigate. Triggers from
            cameras outside this scope are ignored entirely — Sentinel never
            sees the frames. Useful for keeping privacy-sensitive cameras (a
            bedroom, a child's room) off the agent's desk.
          </p>
        </div>
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
                disabled={!enabled}
              />
            </label>
          ))}
        </div>
      </section>

      {/* ── run history ────────────────────────────────────── */}
      <section className="settings-section sentinel-section">
        <div className="sentinel-section-header sentinel-section-header-row">
          <div>
            <h2>Run history</h2>
            <p className="section-description">
              Every Sentinel run, what triggered it, what tools it called, and
              what came of it. Click a filed-incident link to jump to the
              report.
            </p>
          </div>
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
                className={`sentinel-history-filter-chip ${historyFilter === f.v ? "active" : ""}`}
                onClick={() => setHistoryFilter(f.v)}
              >
                {f.label}
              </button>
            ))}
          </div>
        </div>
        <div className="sentinel-runs-table-wrap">
          <table className="sentinel-runs-table">
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
              {filteredRuns.map(r => (
                <tr key={r.id}>
                  <td className="sentinel-runs-when">{r.at}</td>
                  <td>
                    <span className={`sentinel-runs-trigger sentinel-runs-trigger-${r.trigger}`}>
                      {r.trigger.replace(/_/g, " ")}
                    </span>
                  </td>
                  <td className="sentinel-runs-camera">{r.camera}</td>
                  <td className="sentinel-runs-tools">{r.tools}</td>
                  <td>
                    {r.outcome === "incident" && (
                      <span className={`sentinel-runs-outcome sentinel-runs-outcome-incident sentinel-severity-${r.severity}`}>
                        incident · {r.severity}
                      </span>
                    )}
                    {r.outcome === "no_action" && (
                      <span className="sentinel-runs-outcome sentinel-runs-outcome-noop">
                        no action
                      </span>
                    )}
                    {r.outcome === "error" && (
                      <span className="sentinel-runs-outcome sentinel-runs-outcome-error">
                        errored
                      </span>
                    )}
                  </td>
                  <td className="sentinel-runs-link-cell">
                    {r.outcome === "incident" && (
                      <Link to={`/incidents/${r.incidentId}`} className="sentinel-runs-link">
                        view #{r.incidentId} →
                      </Link>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </section>

      {/* ── usage footer ───────────────────────────────────── */}
      <section className="settings-section sentinel-section sentinel-usage-section">
        <div className="sentinel-section-header">
          <h2>Usage this month</h2>
          <p className="section-description">
            Sentinel runs cost LLM tokens. Track consumption here; if a noisy
            camera is driving the bill, tune the trigger cooldown or move it
            out of scope.
          </p>
        </div>
        <div className="sentinel-usage-grid">
          <div className="sentinel-usage-stat">
            <span className="sentinel-usage-value">142</span>
            <span className="sentinel-usage-label">runs</span>
          </div>
          <div className="sentinel-usage-stat">
            <span className="sentinel-usage-value">1.2M</span>
            <span className="sentinel-usage-label">tokens consumed</span>
          </div>
          <div className="sentinel-usage-stat">
            <span className="sentinel-usage-value">$8.40</span>
            <span className="sentinel-usage-label">est. cost</span>
          </div>
          <div className="sentinel-usage-stat">
            <span className="sentinel-usage-value">$25</span>
            <span className="sentinel-usage-label">monthly budget</span>
          </div>
        </div>
        <div className="sentinel-usage-meter">
          <div className="sentinel-usage-meter-fill" style={{ width: "33.6%" }} />
        </div>
        <p className="sentinel-usage-meter-text">
          $8.40 of $25 used (33.6%) — on track for ~$25 this month.
        </p>
      </section>

      {/* ── manual run modal ───────────────────────────────── */}
      {showRunModal && (
        <div className="sentinel-modal-backdrop" onClick={() => !running && setShowRunModal(false)}>
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
              value={manualPrompt}
              onChange={e => setManualPrompt(e.target.value)}
              rows={4}
              disabled={running}
            />
            <div className="sentinel-modal-actions">
              <button
                type="button"
                className="sentinel-modal-btn-ghost"
                onClick={() => setShowRunModal(false)}
                disabled={running}
              >
                Cancel
              </button>
              <button
                type="button"
                className="sentinel-modal-btn-primary"
                onClick={runNow}
                disabled={running || !manualPrompt.trim()}
              >
                {running ? "Running…" : "Run"}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  )
}

export default SentinelPage
