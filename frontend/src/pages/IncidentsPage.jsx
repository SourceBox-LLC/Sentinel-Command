import { useEffect, useState } from "react"
import { useNavigate, useParams } from "react-router-dom"
import { useAuth, useOrganization } from "@clerk/clerk-react"

import { getIncidents, getIncidentCounts } from "../services/api"
import IncidentReportModal from "../components/IncidentReportModal.jsx"
import NewIncidentModal from "../components/NewIncidentModal.jsx"

const SEVERITY_ORDER = { critical: 0, high: 1, medium: 2, low: 3 }

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

/**
 * /incidents — admin-only across all tiers (no plan gate).  Lists every
 * incident in the org regardless of author.  AI-filed (`mcp:<key_name>`)
 * and human-filed (`user:<clerk_id>`) rows render side-by-side, with a
 * small badge per row to distinguish them at a glance.
 *
 * Deep-links: routes /incidents AND /incidents/:incidentId both point
 * here; if :incidentId is present we open the report modal on mount.
 * Notification deep-links and the SentinelPage RunDetailDrawer link
 * land on this route.
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

  // Initial load + 10 s polling.  Same cadence the MCP page used to
  // poll incidents on; preserved here.
  useEffect(() => {
    if (!organization) return

    let cancelled = false

    const load = async () => {
      try {
        const token = await getToken()
        const params = { limit: 50 }
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

  const handleCreated = (created) => {
    setShowCreate(false)
    // Optimistically prepend so the row appears even before the next poll;
    // the next poll will reconcile with the canonical server order.
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
      // Pop the report modal immediately so the operator can add a
      // long-form report or evidence right away.
      setOpenIncidentId(created.id)
    }
  }

  const handleModalClose = () => {
    setOpenIncidentId(null)
    // If we landed here via /incidents/:id, navigate back to /incidents
    // when the modal closes so the URL doesn't keep referencing a row
    // the user has dismissed.
    if (urlIncidentId) navigate("/incidents", { replace: true })
  }

  const handleModalUpdated = async () => {
    // Refresh list + counts after a status change inside the modal.
    try {
      const token = await getToken()
      const params = { limit: 50 }
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

  if (!organization) {
    return (
      <div className="incidents-container">
        <h1 className="page-title">Incidents</h1>
        <p className="text-muted">Please select an organization.</p>
      </div>
    )
  }

  const empty = !loading && incidents.length === 0

  return (
    <div className="incidents-container">
      <header className="incidents-header">
        <div className="incidents-header-left">
          <h1>Incidents</h1>
          <p className="incidents-subtitle">
            Reports filed by AI agents and operators. Click a row to view the full report.
          </p>
        </div>
        <button className="btn btn-primary" onClick={() => setShowCreate(true)}>
          + New Incident
        </button>
      </header>

      <div className="incidents-filters">
        <button
          className={`mcp-incidents-filter ${filter === "open" ? "active" : ""}`}
          onClick={() => setFilter("open")}
        >
          Open ({incidentCounts.open})
        </button>
        <button
          className={`mcp-incidents-filter ${filter === "all" ? "active" : ""}`}
          onClick={() => setFilter("all")}
        >
          All ({incidentCounts.total})
        </button>
      </div>

      <div className="mcp-incidents-list incidents-list">
        {loading ? (
          <div className="mcp-incidents-empty">
            <p>Loading incidents…</p>
          </div>
        ) : empty ? (
          <div className="mcp-incidents-empty">
            <div className="mcp-incidents-empty-icon">
              <svg
                width="40"
                height="40"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="1.5"
                opacity="0.3"
              >
                <path d="M22 11.08V12a10 10 0 11-5.93-9.14" />
                <polyline points="22 4 12 14.01 9 11.01" />
              </svg>
            </div>
            <p>{filter === "open" ? "No open incidents" : "No incidents yet"}</p>
            <span>
              {filter === "open"
                ? "Your watchlist is clear — nice."
                : "File one with + New Incident, or let an AI agent create one."}
            </span>
          </div>
        ) : (
          incidents.map((incident) => {
            const ai = isAiAuthored(incident.created_by)
            return (
              <button
                key={incident.id}
                className={`mcp-incident-row mcp-incident-${incident.severity} mcp-incident-status-${incident.status}`}
                onClick={() => setOpenIncidentId(incident.id)}
              >
                <span
                  className={`mcp-incident-sev-dot mcp-incident-sev-${incident.severity}`}
                />
                <span className="mcp-incident-time">
                  {new Date(incident.created_at).toLocaleString([], {
                    month: "short",
                    day: "numeric",
                    hour: "2-digit",
                    minute: "2-digit",
                  })}
                </span>
                <span
                  className={`incidents-source-badge incidents-source-${ai ? "ai" : "human"}`}
                  title={incident.created_by}
                >
                  {ai ? "AI" : "Human"}
                </span>
                <span className="mcp-incident-camera">
                  {incident.camera_id || "—"}
                </span>
                <span className="mcp-incident-title">{incident.title}</span>
                <span
                  className={`mcp-incident-status mcp-incident-status-badge-${incident.status}`}
                >
                  {STATUS_LABELS[incident.status] || incident.status}
                </span>
                {incident.evidence_count > 0 && (
                  <span className="mcp-incident-evidence-count">
                    {incident.evidence_count} evidence
                  </span>
                )}
              </button>
            )
          })
        )}
      </div>

      {openIncidentId !== null && (
        <IncidentReportModal
          incidentId={openIncidentId}
          onClose={handleModalClose}
          onUpdated={handleModalUpdated}
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
