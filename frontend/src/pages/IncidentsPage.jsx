import { useEffect, useMemo, useState } from "react"
import { useNavigate, useParams } from "react-router-dom"
import { useAuth, useOrganization } from "@clerk/clerk-react"

import { getIncidents, getIncidentCounts } from "../services/api"
import IncidentReportModal from "../components/IncidentReportModal.jsx"
import NewIncidentModal from "../components/NewIncidentModal.jsx"

const SEVERITY_ORDER = { critical: 0, high: 1, medium: 2, low: 3 }

const SEVERITY_LABELS = {
  critical: "CRIT",
  high: "HIGH",
  medium: "MED",
  low: "LOW",
}

const STATUS_LABELS = {
  open: "Open",
  acknowledged: "Acknowledged",
  resolved: "Resolved",
  dismissed: "Dismissed",
}

const POLL_INTERVAL_MS = 10_000

function isAiAuthored(createdBy) {
  return typeof createdBy === "string" && createdBy.startsWith("mcp:")
}

function isToday(iso) {
  if (!iso) return false
  const d = new Date(iso)
  const now = new Date()
  return (
    d.getFullYear() === now.getFullYear() &&
    d.getMonth() === now.getMonth() &&
    d.getDate() === now.getDate()
  )
}

function relativeTime(iso) {
  if (!iso) return ""
  const ts = new Date(iso).getTime()
  const diffSec = Math.max(0, Math.floor((Date.now() - ts) / 1000))
  if (diffSec < 60) return "just now"
  if (diffSec < 3600) return `${Math.floor(diffSec / 60)}m ago`
  if (diffSec < 86400) return `${Math.floor(diffSec / 3600)}h ago`
  if (diffSec < 86400 * 30) return `${Math.floor(diffSec / 86400)}d ago`
  return new Date(iso).toLocaleDateString()
}

/**
 * /incidents — admin-only across all tiers (no plan gate).  Lists every
 * incident in the org regardless of author.  AI-filed (`mcp:<key_name>`)
 * and human-filed (`user:<clerk_id>`) rows render side-by-side, with a
 * source filter and a small badge per row to distinguish them.
 *
 * Routes /incidents AND /incidents/:incidentId both point here; if
 * :incidentId is present we open the report modal on mount.  Lets
 * notification deep-links and the SentinelPage RunDetailDrawer link
 * resolve to a real page.
 */
export default function IncidentsPage() {
  const { getToken } = useAuth()
  const { organization } = useOrganization()
  const navigate = useNavigate()
  const { incidentId: urlIncidentId } = useParams()

  const [incidents, setIncidents] = useState([])
  const [incidentCounts, setIncidentCounts] = useState({
    open: 0,
    open_critical: 0,
    open_high: 0,
    total: 0,
  })
  const [filter, setFilter] = useState("open") // "open" | "all"
  const [sourceFilter, setSourceFilter] = useState("any") // "any" | "ai" | "human"
  const [openIncidentId, setOpenIncidentId] = useState(null)
  const [showCreate, setShowCreate] = useState(false)
  const [loading, setLoading] = useState(true)

  // Open the deep-linked incident in the modal on mount / route change.
  useEffect(() => {
    if (urlIncidentId) {
      const parsed = parseInt(urlIncidentId, 10)
      if (!Number.isNaN(parsed)) setOpenIncidentId(parsed)
    }
  }, [urlIncidentId])

  // Initial load + 10 s polling.  Same cadence the MCP page used.
  useEffect(() => {
    if (!organization) return

    let cancelled = false

    const load = async () => {
      try {
        const token = await getToken()
        const params = { limit: 100 }
        if (filter === "open") params.status = "open"
        const [listData, countsData] = await Promise.all([
          getIncidents(() => Promise.resolve(token), params),
          getIncidentCounts(() => Promise.resolve(token)),
        ])
        if (cancelled) return
        const sorted = [...(listData.incidents || [])].sort((a, b) => {
          const sa = SEVERITY_ORDER[a.severity] ?? 9
          const sb = SEVERITY_ORDER[b.severity] ?? 9
          if (sa !== sb) return sa - sb
          return new Date(b.created_at).getTime() - new Date(a.created_at).getTime()
        })
        setIncidents(sorted)
        setIncidentCounts(countsData)
      } catch (err) {
        console.error("Failed to load incidents:", err)
      } finally {
        if (!cancelled) setLoading(false)
      }
    }

    load()
    const interval = setInterval(load, POLL_INTERVAL_MS)
    return () => {
      cancelled = true
      clearInterval(interval)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [organization, filter])

  // Derived stats — computed off the loaded list so we get an AI/human
  // breakdown and "today" count without a second backend call.
  const derived = useMemo(() => {
    let aiCount = 0
    let humanCount = 0
    let todayCount = 0
    for (const r of incidents) {
      if (isAiAuthored(r.created_by)) aiCount += 1
      else humanCount += 1
      if (isToday(r.created_at)) todayCount += 1
    }
    return { aiCount, humanCount, todayCount }
  }, [incidents])

  const visibleIncidents = useMemo(() => {
    if (sourceFilter === "any") return incidents
    if (sourceFilter === "ai")
      return incidents.filter((r) => isAiAuthored(r.created_by))
    return incidents.filter((r) => !isAiAuthored(r.created_by))
  }, [incidents, sourceFilter])

  const handleCreated = (created) => {
    setShowCreate(false)
    if (created && typeof created.id === "number") {
      setIncidents((prev) => {
        if (prev.some((r) => r.id === created.id)) return prev
        return [created, ...prev]
      })
      setIncidentCounts((prev) => ({
        ...prev,
        open: (prev.open || 0) + 1,
        total: (prev.total || 0) + 1,
      }))
      setOpenIncidentId(created.id)
    }
  }

  const handleModalClose = () => {
    setOpenIncidentId(null)
    if (urlIncidentId) navigate("/incidents", { replace: true })
  }

  const handleModalUpdated = async () => {
    try {
      const token = await getToken()
      const params = { limit: 100 }
      if (filter === "open") params.status = "open"
      const [listData, countsData] = await Promise.all([
        getIncidents(() => Promise.resolve(token), params),
        getIncidentCounts(() => Promise.resolve(token)),
      ])
      const sorted = [...(listData.incidents || [])].sort((a, b) => {
        const sa = SEVERITY_ORDER[a.severity] ?? 9
        const sb = SEVERITY_ORDER[b.severity] ?? 9
        if (sa !== sb) return sa - sb
        return new Date(b.created_at).getTime() - new Date(a.created_at).getTime()
      })
      setIncidents(sorted)
      setIncidentCounts(countsData)
    } catch (err) {
      console.error("Failed to refresh after update:", err)
    }
  }

  const handleModalDeleted = (deletedId) => {
    // Optimistic update: drop the row from the list and decrement counts
    // before any network round-trip.  The backend has already confirmed
    // the delete (the modal awaits the API response before calling us);
    // refreshing from server would just be a redundant round-trip, and
    // the next 10-s poll will reconcile if anything diverged.
    const removed = incidents.find((r) => r.id === deletedId)
    setIncidents((prev) => prev.filter((r) => r.id !== deletedId))
    if (removed) {
      setIncidentCounts((prev) => ({
        ...prev,
        total: Math.max(0, (prev.total || 0) - 1),
        open: removed.status === "open" ? Math.max(0, (prev.open || 0) - 1) : (prev.open || 0),
        open_critical:
          removed.status === "open" && removed.severity === "critical"
            ? Math.max(0, (prev.open_critical || 0) - 1)
            : (prev.open_critical || 0),
        open_high:
          removed.status === "open" && removed.severity === "high"
            ? Math.max(0, (prev.open_high || 0) - 1)
            : (prev.open_high || 0),
      }))
    }
    // Close the modal + clear the URL deep-link if any.
    handleModalClose()
  }

  if (!organization) {
    return (
      <div className="incidents-container">
        <h1 className="page-title">Incidents</h1>
        <p className="text-muted">Please select an organization.</p>
      </div>
    )
  }

  const criticalAndHigh =
    (incidentCounts.open_critical || 0) + (incidentCounts.open_high || 0)
  const empty = !loading && visibleIncidents.length === 0

  return (
    <div className="incidents-container">
      {/* Ambient glows behind the hero — purely decorative, matches the
          locked-MCP-page design language so the surface feels related. */}
      <div className="incidents-glow incidents-glow-1" />
      <div className="incidents-glow incidents-glow-2" />

      {/* Hero */}
      <section className="incidents-hero">
        <div className="incidents-hero-left">
          <div className="incidents-hero-icon">
            <svg width="28" height="28" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
              <path d="M12 2L3 7v6c0 5 3.5 9.5 9 11 5.5-1.5 9-6 9-11V7l-9-5z"/>
              <path d="M9 12l2 2 4-4"/>
            </svg>
          </div>
          <div>
            <div className="incidents-hero-eyebrow">Security Operations</div>
            <h1 className="incidents-hero-title">Incident Reports</h1>
            <p className="incidents-hero-subtitle">
              Filed by AI agents and operators. Click a row to view the full report,
              walk through evidence, and update its status.
            </p>
          </div>
        </div>
        <div className="incidents-hero-right">
          <div className="incidents-live-badge" title="Auto-refreshing every 10 s">
            <span className="incidents-live-dot" />
            <span>LIVE</span>
          </div>
          <button
            className="incidents-new-btn"
            onClick={() => setShowCreate(true)}
            title="File a new incident manually"
          >
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round">
              <line x1="12" y1="5" x2="12" y2="19" />
              <line x1="5" y1="12" x2="19" y2="12" />
            </svg>
            New Incident
          </button>
        </div>
      </section>

      {/* Stat strip */}
      <section className="incidents-stats">
        <div
          className={`incidents-stat ${incidentCounts.open > 0 ? "incidents-stat-warn" : "incidents-stat-ok"}`}
        >
          <div className="incidents-stat-value">{incidentCounts.open}</div>
          <div className="incidents-stat-label">Open</div>
          <div className="incidents-stat-spark" />
        </div>
        <div
          className={`incidents-stat ${criticalAndHigh > 0 ? "incidents-stat-danger" : "incidents-stat-ok"}`}
        >
          <div className="incidents-stat-value">{criticalAndHigh}</div>
          <div className="incidents-stat-label">High &amp; Critical</div>
          <div className="incidents-stat-spark" />
        </div>
        <div className="incidents-stat incidents-stat-ai">
          <div className="incidents-stat-value">{derived.aiCount}</div>
          <div className="incidents-stat-label">AI-Authored</div>
          <div className="incidents-stat-spark" />
        </div>
        <div className="incidents-stat incidents-stat-cyan">
          <div className="incidents-stat-value">{derived.todayCount}</div>
          <div className="incidents-stat-label">Today</div>
          <div className="incidents-stat-spark" />
        </div>
      </section>

      {/* Filter strip */}
      <section className="incidents-filter-bar">
        <div className="incidents-filter-group">
          <button
            className={`incidents-pill ${filter === "open" ? "active" : ""}`}
            onClick={() => setFilter("open")}
          >
            Open <span className="incidents-pill-count">{incidentCounts.open}</span>
          </button>
          <button
            className={`incidents-pill ${filter === "all" ? "active" : ""}`}
            onClick={() => setFilter("all")}
          >
            All <span className="incidents-pill-count">{incidentCounts.total}</span>
          </button>
        </div>
        <div className="incidents-filter-divider" />
        <div className="incidents-filter-group">
          <button
            className={`incidents-pill incidents-pill-source ${sourceFilter === "any" ? "active" : ""}`}
            onClick={() => setSourceFilter("any")}
          >
            Anyone
          </button>
          <button
            className={`incidents-pill incidents-pill-source ${sourceFilter === "ai" ? "active source-ai" : ""}`}
            onClick={() => setSourceFilter("ai")}
          >
            <SourceIconAi /> AI
          </button>
          <button
            className={`incidents-pill incidents-pill-source ${sourceFilter === "human" ? "active source-human" : ""}`}
            onClick={() => setSourceFilter("human")}
          >
            <SourceIconHuman /> Human
          </button>
        </div>
      </section>

      {/* List */}
      <section className="incidents-list-wrap">
        {loading ? (
          <div className="incidents-empty">
            <div className="incidents-empty-spinner" />
            <p>Loading incidents…</p>
          </div>
        ) : empty ? (
          <div className="incidents-empty">
            <div className="incidents-empty-icon">
              <svg width="48" height="48" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">
                <path d="M22 11.08V12a10 10 0 11-5.93-9.14" />
                <polyline points="22 4 12 14.01 9 11.01" />
              </svg>
            </div>
            <h3 className="incidents-empty-title">
              {filter === "open"
                ? "Watchlist clear"
                : sourceFilter === "ai"
                  ? "No AI-filed incidents yet"
                  : sourceFilter === "human"
                    ? "No human-filed incidents yet"
                    : "No incidents yet"}
            </h3>
            <p className="incidents-empty-msg">
              {filter === "open"
                ? "No open incidents — every report has been triaged."
                : "File one with + New Incident, or let an AI agent create one."}
            </p>
          </div>
        ) : (
          <div className="incidents-list">
            {visibleIncidents.map((incident, idx) => {
              const ai = isAiAuthored(incident.created_by)
              const sevLabel = SEVERITY_LABELS[incident.severity] || "—"
              return (
                <button
                  key={incident.id}
                  className={`incidents-row incidents-row-${incident.severity} incidents-row-status-${incident.status}`}
                  style={{ animationDelay: `${Math.min(idx, 18) * 25}ms` }}
                  onClick={() => setOpenIncidentId(incident.id)}
                >
                  <span
                    className={`incidents-row-stripe incidents-row-stripe-${incident.severity}`}
                  />
                  <div className="incidents-row-main">
                    <div className="incidents-row-header">
                      <span
                        className={`incidents-sev-tag incidents-sev-tag-${incident.severity}`}
                      >
                        {sevLabel}
                      </span>
                      <span
                        className={`incidents-source-chip ${ai ? "source-ai" : "source-human"}`}
                        title={incident.created_by}
                      >
                        {ai ? <SourceIconAi /> : <SourceIconHuman />}
                        {ai ? "AI" : "Human"}
                      </span>
                      <span className="incidents-row-time">
                        {relativeTime(incident.created_at)}
                        <span className="incidents-row-time-abs">
                          {new Date(incident.created_at).toLocaleString([], {
                            month: "short",
                            day: "numeric",
                            hour: "2-digit",
                            minute: "2-digit",
                          })}
                        </span>
                      </span>
                    </div>
                    <div className="incidents-row-title">{incident.title}</div>
                    <div className="incidents-row-meta">
                      {incident.camera_id && (
                        <span className="incidents-row-camera">
                          <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                            <path d="M23 7l-7 5 7 5V7z" />
                            <rect x="1" y="5" width="15" height="14" rx="2" ry="2" />
                          </svg>
                          {incident.camera_id}
                        </span>
                      )}
                      {incident.evidence_count > 0 && (
                        <span className="incidents-row-evidence">
                          <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                            <path d="M21 15v4a2 2 0 01-2 2H5a2 2 0 01-2-2v-4" />
                            <polyline points="17 8 12 3 7 8" />
                            <line x1="12" y1="3" x2="12" y2="15" />
                          </svg>
                          {incident.evidence_count} evidence
                        </span>
                      )}
                    </div>
                  </div>
                  <div className="incidents-row-right">
                    <span
                      className={`incidents-status incidents-status-${incident.status}`}
                    >
                      {STATUS_LABELS[incident.status] || incident.status}
                    </span>
                    <span className="incidents-row-chev" aria-hidden>
                      ›
                    </span>
                  </div>
                </button>
              )
            })}
          </div>
        )}
      </section>

      {openIncidentId !== null && (
        <IncidentReportModal
          incidentId={openIncidentId}
          onClose={handleModalClose}
          onUpdated={handleModalUpdated}
          onDeleted={handleModalDeleted}
        />
      )}

      {showCreate && (
        <NewIncidentModal
          onClose={() => setShowCreate(false)}
          onCreated={handleCreated}
        />
      )}
    </div>
  )
}

// ── Tiny inline icons ────────────────────────────────────────────
// Kept inline (not imported) so the chip + filter pill render the
// same SVGs without an extra component round-trip.

function SourceIconAi() {
  return (
    <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
      <rect x="3" y="6" width="18" height="14" rx="2" />
      <line x1="8" y1="2" x2="8" y2="6" />
      <line x1="16" y1="2" x2="16" y2="6" />
      <circle cx="9" cy="13" r="1" />
      <circle cx="15" cy="13" r="1" />
      <line x1="9" y1="17" x2="15" y2="17" />
    </svg>
  )
}

function SourceIconHuman() {
  return (
    <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
      <path d="M20 21v-2a4 4 0 00-4-4H8a4 4 0 00-4 4v2" />
      <circle cx="12" cy="7" r="4" />
    </svg>
  )
}
