import { useEffect, useMemo, useState } from "react"
import { useAuth, useOrganization } from "@clerk/clerk-react"
import {
  getOrgAuditLogs,
  downloadOrgAuditLogsCsv,
} from "../services/api"
import { useToasts } from "../hooks/useToasts.jsx"

/*
 * Organization Audit Log panel.
 *
 * Surfaces the rows written by ``write_audit()`` across the backend
 * — member changes, MCP key generation, settings changes, danger-zone
 * actions, GDPR exports, etc.  Until this shipped, the
 * ``/api/audit-logs`` endpoint had no UI consumer and admins had to
 * curl it (or wait for the CSV export) to see who-did-what to their
 * org.
 *
 * Event-type dropdown:
 *   Hardcoded list grouped by domain.  Mirrors every ``event=`` string
 *   passed to write_audit() in the backend.  Grep the backend api/
 *   directory for ``event=`` to find them all.  When a new audit
 *   event is added, append it here in the matching group — the
 *   backend doesn't care, but the dropdown is the only way an
 *   operator can filter by it.
 *
 * Pagination + CSV export:
 *   Same shape as the sibling Stream Access Logs and MCP Tool Activity
 *   sections; reused intentionally so the visual + behavioural
 *   patterns are consistent across the three audit surfaces.
 */

// Grouped for the <optgroup> presentation.  Order within groups is
// alphabetic; group order roughly follows "how often an admin
// browses for this category."
const EVENT_GROUPS = [
  {
    label: "Member lifecycle",
    events: [
      ["member_promotion_requested", "Member requested promotion"],
    ],
  },
  {
    label: "MCP key audit",
    events: [
      ["mcp_key_created", "MCP key created"],
      ["mcp_key_revoked", "MCP key revoked"],
    ],
  },
  {
    label: "Camera + recording",
    events: [
      ["camera_recording_policy_updated", "Recording policy updated"],
      ["recording_toggled", "Manual recording toggle"],
    ],
  },
  {
    label: "CameraNode lifecycle",
    events: [
      ["node_created", "Node created"],
      ["node_decommissioned", "Node decommissioned"],
      ["node_deleted", "Node deleted"],
      ["node_key_rotated", "Node API key rotated"],
    ],
  },
  {
    label: "Settings",
    events: [
      ["motion_ingestion_toggled", "Motion ingestion toggled"],
      ["notification_settings_updated", "Notification settings updated"],
      ["timezone_updated", "Timezone updated"],
    ],
  },
  {
    label: "Danger zone + compliance",
    events: [
      ["full_reset", "Full org reset"],
      ["gdpr_export", "GDPR data export"],
      ["logs_wiped", "Logs wiped"],
    ],
  },
]

// Pretty labels for known events (used in the table column).  Falls
// back to the raw event string for anything not in the map.
const EVENT_LABELS = Object.fromEntries(
  EVENT_GROUPS.flatMap((g) => g.events),
)

function formatEvent(event) {
  return EVENT_LABELS[event] || event
}

export default function OrgAuditLogPanel() {
  const { getToken } = useAuth()
  const { organization } = useOrganization()
  const orgId = organization?.id || null
  const { showToast } = useToasts()

  const [logs, setLogs] = useState([])
  const [total, setTotal] = useState(0)
  const [loading, setLoading] = useState(true)
  const [exporting, setExporting] = useState(false)
  const [filters, setFilters] = useState({
    event: "",
    username: "",
    limit: 50,
    offset: 0,
  })

  // Memoise the page math so we don't recompute on every render.
  const { pageCount, currentPage } = useMemo(() => {
    const pc = Math.max(1, Math.ceil(total / filters.limit))
    const cur = Math.floor(filters.offset / filters.limit) + 1
    return { pageCount: pc, currentPage: cur }
  }, [total, filters.limit, filters.offset])

  // Reload on filter / page changes.  Same pattern as the existing
  // Stream Access + MCP sections.
  //
  // ``getToken`` and ``showToast`` are intentionally omitted from
  // the deps array — they're hook returns whose object identity
  // changes on every render, which would re-fire this effect on
  // every render and burn one redundant API call per mount.  The
  // closure captures the latest references at effect-fire time
  // (which is what we want — old token would be a stale auth bug).
  useEffect(() => {
    let cancelled = false
    const load = async () => {
      setLoading(true)
      try {
        const token = await getToken()
        const data = await getOrgAuditLogs(() => Promise.resolve(token), filters)
        if (cancelled) return
        setLogs(data.logs || [])
        setTotal(data.total || 0)
      } catch (err) {
        if (cancelled) return
        console.error("Failed to load org audit log:", err)
        showToast("Failed to load audit log.", "error")
      } finally {
        if (!cancelled) setLoading(false)
      }
    }
    load()
    return () => { cancelled = true }
    // orgId: a cross-tab org switch syncs into this tab WITHOUT
    // navigation — without the dep this compliance surface kept
    // rendering the previous org's audit rows under the new org's
    // chrome until a filter was touched.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [filters, orgId])

  const handleFilterChange = (key, value) => {
    // Reset offset whenever a filter changes — paging into a row
    // index that no longer exists is a worse UX than going back to
    // page 1 of the new result set.
    setFilters((prev) => ({ ...prev, [key]: value, offset: 0 }))
  }

  const handlePageChange = (newOffset) => {
    setFilters((prev) => ({ ...prev, offset: Math.max(0, newOffset) }))
  }

  const handleExportCsv = async () => {
    setExporting(true)
    try {
      const token = await getToken()
      // CSV honours the active filters (event + username); per-page
      // limit/offset is intentionally NOT forwarded — backend caps
      // CSV at 50k rows so an export is always a meaningful audit
      // window, not just the current screen page.
      const params = {}
      if (filters.event) params.event = filters.event
      if (filters.username) params.username = filters.username
      await downloadOrgAuditLogsCsv(() => Promise.resolve(token), params)
      showToast("Audit log CSV downloaded.", "success")
    } catch (err) {
      console.error("Audit log CSV export failed:", err)
      showToast(`Export failed: ${err.message || "unknown error"}`, "error")
    } finally {
      setExporting(false)
    }
  }

  return (
    <div className="audit-section">
      <div className="audit-section-header">
        <div>
          <h2>Organization Audit Log</h2>
          <p className="section-description">
            Member changes, MCP key activity, settings changes,
            danger-zone actions, GDPR exports — the chronological
            history of who did what to your organization.  Retained
            per your plan&rsquo;s log-retention window.
          </p>
        </div>
        <button
          type="button"
          className="btn btn-secondary"
          onClick={handleExportCsv}
          disabled={exporting}
          title="Download the current view (with filters applied) as a CSV file"
        >
          {exporting ? "Exporting…" : "Export CSV"}
        </button>
      </div>

      <div className="audit-filters">
        <div className="filter-group">
          <label htmlFor="audit-event-filter">Event type</label>
          <select
            id="audit-event-filter"
            value={filters.event}
            onChange={(e) => handleFilterChange("event", e.target.value)}
          >
            <option value="">All events</option>
            {EVENT_GROUPS.map((group) => (
              <optgroup key={group.label} label={group.label}>
                {group.events.map(([value, label]) => (
                  <option key={value} value={value}>{label}</option>
                ))}
              </optgroup>
            ))}
          </select>
        </div>

        <div className="filter-group">
          <label htmlFor="audit-username-filter">Username</label>
          <input
            id="audit-username-filter"
            type="text"
            placeholder="Substring match"
            value={filters.username}
            onChange={(e) => handleFilterChange("username", e.target.value)}
          />
        </div>

        <div className="filter-group">
          <label htmlFor="audit-per-page">Per Page</label>
          <select
            id="audit-per-page"
            value={filters.limit}
            onChange={(e) => handleFilterChange("limit", parseInt(e.target.value))}
          >
            <option value="25">25</option>
            <option value="50">50</option>
            <option value="100">100</option>
          </select>
        </div>
      </div>

      {loading ? (
        <div className="loading-spinner" />
      ) : logs.length === 0 ? (
        <div className="audit-empty">
          <div className="audit-empty-icon">📋</div>
          <p>No audit log entries match your filters.</p>
        </div>
      ) : (
        <>
          <div className="audit-table-wrapper">
            <table className="audit-table">
              <thead>
                <tr>
                  <th>Timestamp</th>
                  <th>Event</th>
                  <th>User</th>
                  <th>IP</th>
                  <th>Details</th>
                </tr>
              </thead>
              <tbody>
                {logs.map((log) => (
                  <tr key={log.id}>
                    <td className="timestamp">
                      {new Date(log.timestamp).toLocaleString()}
                    </td>
                    <td><code>{formatEvent(log.event)}</code></td>
                    <td className="user-id">
                      {log.username || (
                        <span className="audit-na">&mdash;</span>
                      )}
                    </td>
                    <td className="ip-address">
                      {log.ip || <span className="audit-na">&mdash;</span>}
                    </td>
                    <td className="audit-details">
                      {log.details ? (
                        <span title={log.details}>{log.details}</span>
                      ) : (
                        <span className="audit-na">&mdash;</span>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          {pageCount > 1 && (
            <div className="audit-pagination">
              <button
                onClick={() => handlePageChange(filters.offset - filters.limit)}
                disabled={filters.offset === 0}
              >
                Previous
              </button>
              <span className="audit-page-indicator">
                Page {currentPage} of {pageCount}
              </span>
              <button
                onClick={() => handlePageChange(filters.offset + filters.limit)}
                disabled={filters.offset + filters.limit >= total}
              >
                Next
              </button>
            </div>
          )}
        </>
      )}
    </div>
  )
}
