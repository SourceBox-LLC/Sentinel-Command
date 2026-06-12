import { useState, useEffect, useRef } from "react"
import { Link } from "react-router-dom"
import { useAuth, useOrganization } from "@clerk/clerk-react"
import { getStreamLogs, getStreamStats, getCameras, getMcpLogs, getMcpLogStats, downloadStreamLogsCsv, downloadMcpLogsCsv } from "../services/api"
import { useToasts } from "../hooks/useToasts.jsx"
import { usePlanInfo } from "../hooks/usePlanInfo.jsx"
import OrgAuditLogPanel from "../components/OrgAuditLogPanel.jsx"
import AdminKpiStrip from "../components/AdminKpiStrip.jsx"
import AdminTabs from "../components/AdminTabs.jsx"
import { BarList, DailyActivityChart } from "../components/AdminCharts.jsx"

const API_URL = import.meta.env.VITE_API_URL || ""

function AdminPage() {
  const { getToken } = useAuth()
  const { organization } = useOrganization()
  const { showToast } = useToasts()
  const { planInfo, loading: planLoading } = usePlanInfo()
  const [logs, setLogs] = useState([])
  const [stats, setStats] = useState(null)
  const [cameras, setCameras] = useState([])
  const [loading, setLoading] = useState(true)
  const [statsLoading, setStatsLoading] = useState(true)

  const [filters, setFilters] = useState({
    camera_id: "",
    user_id: "",
    limit: 50,
    offset: 0,
  })
  const [total, setTotal] = useState(0)
  const [days, setDays] = useState(7)

  // MCP activity state
  const [mcpLogs, setMcpLogs] = useState([])
  const [mcpStats, setMcpStats] = useState(null)
  const [mcpLoading, setMcpLoading] = useState(true)
  const [mcpStatsLoading, setMcpStatsLoading] = useState(true)
  const [mcpFilters, setMcpFilters] = useState({
    tool_name: "",
    key_name: "",
    status: "",
    limit: 50,
    offset: 0,
  })
  const [mcpTotal, setMcpTotal] = useState(0)
  const [mcpDays, setMcpDays] = useState(7)
  // Read by the SSE stream callback — a ref so filter changes don't
  // force a stream teardown and the callback never sees stale values.
  const mcpFiltersRef = useRef(mcpFilters)
  useEffect(() => {
    mcpFiltersRef.current = mcpFilters
  }, [mcpFilters])

  // Active tab in the log strip — Stream / Audit / MCP swap into the
  // single panel below the KPI strip rather than all three stacking.
  const [activeTab, setActiveTab] = useState("stream")

  // SSE connection state for the live MCP activity feed.  Drives the
  // small "Live" indicator in the MCP Tool Activity section header.
  const [sseConnected, setSseConnected] = useState(false)

  // Stable boolean for effect deps — depending on the planInfo OBJECT
  // re-fired every loader and tore down the SSE stream on each 60s
  // plan refresh (viewer-hours tick changes the payload while anyone
  // watches video).
  const hasAdminFeature = !!planInfo?.features?.includes("admin")

  // Only load audit data once we know the plan allows it
  useEffect(() => {
    if (hasAdminFeature) {
      loadCameras()
      loadLogs()
      loadStats()
      loadMcpLogs()
      loadMcpStats()
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [organization?.id, hasAdminFeature])

  useEffect(() => {
    if (organization && hasAdminFeature) {
      loadLogs()
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [filters])

  const loadCameras = async () => {
    try {
      const token = await getToken()
      const data = await getCameras(() => Promise.resolve(token))
      // /api/cameras returns a bare array of camera dicts.
      if (Array.isArray(data)) setCameras(data)
    } catch (err) {
      console.error("Failed to load cameras:", err)
    }
  }

  const loadLogs = async () => {
    try {
      setLoading(true)
      const token = await getToken()

      const params = {}
      if (filters.camera_id) params.camera_id = filters.camera_id
      if (filters.user_id) params.user_id = filters.user_id
      params.limit = filters.limit
      params.offset = filters.offset

      const data = await getStreamLogs(() => Promise.resolve(token), params)
      setLogs(data.logs || [])
      setTotal(data.total || 0)
    } catch (err) {
      console.error("Failed to load audit logs:", err)
      showToast("Failed to load audit logs", "error")
    } finally {
      setLoading(false)
    }
  }

  const loadStats = async () => {
    try {
      setStatsLoading(true)
      const token = await getToken()
      const data = await getStreamStats(() => Promise.resolve(token), days)
      setStats(data)
    } catch (err) {
      console.error("Failed to load stats:", err)
      showToast("Failed to load statistics", "error")
    } finally {
      setStatsLoading(false)
    }
  }

  const handleFilterChange = (key, value) => {
    setFilters(prev => ({
      ...prev,
      [key]: value,
      offset: 0,
    }))
  }

  const handlePageChange = (newOffset) => {
    setFilters(prev => ({
      ...prev,
      offset: newOffset,
    }))
  }

  const handleDaysChange = (newDays) => {
    setDays(newDays)
  }

  useEffect(() => {
    if (organization && planInfo?.features?.includes("admin")) {
      loadStats()
    }
  }, [days])

  // MCP activity loaders
  const loadMcpLogs = async () => {
    try {
      setMcpLoading(true)
      const token = await getToken()
      const data = await getMcpLogs(() => Promise.resolve(token), mcpFilters)
      setMcpLogs(data.logs || [])
      setMcpTotal(data.total || 0)
    } catch (err) {
      console.error("Failed to load MCP logs:", err)
    } finally {
      setMcpLoading(false)
    }
  }

  const loadMcpStats = async () => {
    try {
      setMcpStatsLoading(true)
      const token = await getToken()
      const data = await getMcpLogStats(() => Promise.resolve(token), mcpDays)
      setMcpStats(data)
    } catch (err) {
      console.error("Failed to load MCP stats:", err)
    } finally {
      setMcpStatsLoading(false)
    }
  }

  useEffect(() => {
    if (organization && planInfo?.features?.includes("admin")) {
      loadMcpLogs()
    }
  }, [mcpFilters])

  useEffect(() => {
    if (organization && planInfo?.features?.includes("admin")) {
      loadMcpStats()
    }
  }, [mcpDays])

  // Live SSE feed for MCP tool calls — backend at /api/mcp/activity/stream
  // emits {type: "tool_call", ...} events as agents fire tools.  We
  // prepend each one to mcpLogs with an _isNew flag the row renderer
  // picks up to play a short flash animation.  Mirrors the McpPage.jsx
  // pattern (fetch + ReadableStream because EventSource can't carry an
  // Authorization header), with reconnect-on-drop after a 5s backoff.
  useEffect(() => {
    if (!organization || !planInfo?.features?.includes("admin")) return

    let cancelled = false
    let reader = null
    let reconnectTimer = null

    const connect = async () => {
      try {
        const token = await getToken()
        const response = await fetch(`${API_URL}/api/mcp/activity/stream`, {
          headers: { Authorization: `Bearer ${token}` },
        })

        if (!response.ok || cancelled) {
          if (!cancelled) {
            reconnectTimer = setTimeout(connect, 5000)
          }
          return
        }

        setSseConnected(true)
        reader = response.body.getReader()
        const decoder = new TextDecoder()
        let buffer = ""

        while (!cancelled) {
          const { done, value } = await reader.read()
          if (done) break
          if (document.hidden) continue  // drain but don't render while hidden

          buffer += decoder.decode(value, { stream: true })
          const lines = buffer.split("\n")
          buffer = lines.pop() || ""

          for (const line of lines) {
            if (!line.startsWith("data: ")) continue
            try {
              const data = JSON.parse(line.slice(6))
              if (data.type !== "tool_call") continue
              // Backend emits Unix-seconds-float; the row renderer
              // expects what new Date(string) can parse.  Convert.
              const log = {
                ...data,
                timestamp:
                  typeof data.timestamp === "number"
                    ? new Date(data.timestamp * 1000).toISOString()
                    : data.timestamp,
                _isNew: true,
              }
              // Skip live prepends when a filter or pagination offset is
              // active — the event may not match the filtered view, and
              // landing rows mid-page corrupts what the admin is reading.
              const filtered =
                mcpFiltersRef.current.tool_name ||
                mcpFiltersRef.current.status ||
                mcpFiltersRef.current.key_name ||
                mcpFiltersRef.current.offset > 0
              if (!filtered) {
                setMcpLogs((prev) => {
                  if (prev.some((p) => p.id === log.id)) return prev
                  // Bounded: a chatty agent (10 calls/s) must not grow
                  // the array for the lifetime of the tab.
                  return [log, ...prev].slice(0, mcpFiltersRef.current.limit)
                })
                // Best-effort tab-count bump — next loadMcpLogs will
                // reconcile against the persisted count.
                setMcpTotal((t) => t + 1)
              }
            } catch {
              /* ignore parse errors */
            }
          }
        }
        // Graceful server close (deploy/proxy) exits the loop with no
        // exception — without this the LIVE badge stayed lit forever
        // over a dead feed.
        if (!cancelled) {
          setSseConnected(false)
          reconnectTimer = setTimeout(connect, 5000)
        }
      } catch (err) {
        if (!cancelled) {
          console.error("[Admin SSE] Connection error:", err)
          setSseConnected(false)
          reconnectTimer = setTimeout(connect, 5000)
        }
      }
    }

    connect()

    return () => {
      cancelled = true
      setSseConnected(false)
      if (reconnectTimer) clearTimeout(reconnectTimer)
      if (reader) reader.cancel().catch(() => {})
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [organization?.id, hasAdminFeature])

  const handleMcpFilterChange = (key, value) => {
    setMcpFilters(prev => ({ ...prev, [key]: value, offset: 0 }))
  }

  const handleMcpPageChange = (newOffset) => {
    setMcpFilters(prev => ({ ...prev, offset: newOffset }))
  }

  // ── CSV export handlers ────────────────────────────────────────
  // Pass the active filters into the export so what the admin sees
  // on screen matches what they download.  Per-page (`limit`/`offset`)
  // is intentionally NOT forwarded — the CSV branch on the backend
  // ignores those and pulls a 50k-row window so an export is always
  // a meaningful audit slice, not just one screen-page worth.
  const [streamExporting, setStreamExporting] = useState(false)
  const [mcpExporting, setMcpExporting] = useState(false)

  const handleExportStreamCsv = async () => {
    setStreamExporting(true)
    try {
      const token = await getToken()
      const params = {}
      if (filters.camera_id) params.camera_id = filters.camera_id
      if (filters.user_id) params.user_id = filters.user_id
      await downloadStreamLogsCsv(() => Promise.resolve(token), params)
      showToast("Stream access log CSV downloaded.", "success")
    } catch (err) {
      console.error("Stream CSV export failed:", err)
      showToast(`Export failed: ${err.message || "unknown error"}`, "error")
    } finally {
      setStreamExporting(false)
    }
  }

  const handleExportMcpCsv = async () => {
    setMcpExporting(true)
    try {
      const token = await getToken()
      const params = {}
      if (mcpFilters.tool_name) params.tool_name = mcpFilters.tool_name
      if (mcpFilters.key_name) params.key_name = mcpFilters.key_name
      if (mcpFilters.status) params.status = mcpFilters.status
      await downloadMcpLogsCsv(() => Promise.resolve(token), params)
      showToast("MCP activity log CSV downloaded.", "success")
    } catch (err) {
      console.error("MCP CSV export failed:", err)
      showToast(`Export failed: ${err.message || "unknown error"}`, "error")
    } finally {
      setMcpExporting(false)
    }
  }

  if (!organization) {
    return (
      <div className="admin-container">
        <h1 className="page-title">Admin Dashboard</h1>
        <p className="text-muted">Please select an organization to view admin settings.</p>
      </div>
    )
  }

  if (planLoading) {
    return (
      <div className="admin-container">
        <h1 className="page-title">Admin Dashboard</h1>
        <div className="loading-spinner"></div>
      </div>
    )
  }

  if (!planInfo?.features?.includes("admin")) {
    return (
      <div className="admin-container">
        <div className="upgrade-prompt">
          <div className="upgrade-icon">🔒</div>
          <h2>Admin Dashboard</h2>
          <p>
            The Admin Dashboard with stream access logs and usage analytics
            is available on the <strong>Pro</strong> and <strong>Pro Plus</strong> plans.
          </p>
          <div className="upgrade-actions">
            <Link to="/pricing" className="btn btn-primary">
              Upgrade Your Plan
            </Link>
            <Link to="/dashboard" className="btn btn-secondary">
              Back to Dashboard
            </Link>
          </div>
        </div>
      </div>
    )
  }

  const pageCount = Math.ceil(total / filters.limit)
  const currentPage = Math.floor(filters.offset / filters.limit) + 1

  return (
    <div className="admin-container">
      <div className="admin-header">
        <h1>Admin Dashboard</h1>
        <p>View stream access logs and usage statistics for your organization.</p>
      </div>

      <AdminKpiStrip
        stats={stats}
        mcpStats={mcpStats}
        planInfo={planInfo}
        streamDays={days}
        mcpDays={mcpDays}
      />

      <AdminTabs
        activeTab={activeTab}
        onTabChange={setActiveTab}
        streamCount={total}
        mcpCount={mcpStats?.total_calls}
        mcpErrors={mcpStats?.total_errors}
      />

      {activeTab === "stream" && (<>
      <div className="audit-section">
        <div className="audit-section-header">
          <div>
            <h2>Stream Access Logs</h2>
            <p className="section-description">
              View who has accessed your camera streams.
            </p>
          </div>
          {/*
            Export honours the active filters but ignores the
            per-page limit — backend pulls a 50k-row window so an
            audit export is always a meaningful slice, not just
            one screen-page.  See the docstring in the CSV
            branch of /api/audit/stream-logs for details.
          */}
          <button
            type="button"
            className="btn btn-secondary"
            onClick={handleExportStreamCsv}
            disabled={streamExporting}
            title="Download the current view (with filters applied) as a CSV file"
          >
            {streamExporting ? "Exporting…" : "Export CSV"}
          </button>
        </div>

        <div className="audit-filters">
          <div className="filter-group">
            <label>Camera</label>
            <select
              value={filters.camera_id}
              onChange={(e) => handleFilterChange("camera_id", e.target.value)}
            >
              <option value="">All Cameras</option>
              {cameras.map(cam => (
                <option key={cam.camera_id} value={cam.camera_id}>
                  {cam.name || cam.camera_id}
                </option>
              ))}
            </select>
          </div>

          <div className="filter-group">
            <label>User Email</label>
            <input
              type="text"
              placeholder="Filter by email"
              value={filters.user_id}
              onChange={(e) => handleFilterChange("user_id", e.target.value)}
            />
          </div>

          <div className="filter-group">
            <label>Per Page</label>
            <select
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
          <div className="loading-spinner"></div>
        ) : logs.length === 0 ? (
          <div className="audit-empty">
            <div className="audit-empty-icon">📊</div>
            <p>No stream access logs found.</p>
          </div>
        ) : (
          <>
            <div className="audit-table-wrapper">
              <table className="audit-table">
                <thead>
                  <tr>
                    <th>Timestamp</th>
                    <th>Camera</th>
                    <th>User</th>
                    <th>IP Address</th>
                  </tr>
                </thead>
                <tbody>
                  {logs.map(log => (
                    <tr key={log.id}>
                      <td className="timestamp">
                        {new Date(log.accessed_at).toLocaleString()}
                      </td>
                      <td>{log.camera_id}</td>
                      <td className="user-id">{log.user_email || log.user_id.substring(0, 8) + "..."}</td>
                      <td className="ip-address">{log.ip_address || "Unknown"}</td>
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
                <span className="pagination-status">
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

      <div className="audit-section">
        <div className="stats-header">
          <h2>Statistics</h2>
          <select
            value={days}
            onChange={(e) => handleDaysChange(parseInt(e.target.value))}
          >
            <option value="1">Last 24 hours</option>
            <option value="7">Last 7 days</option>
            <option value="14">Last 14 days</option>
            <option value="30">Last 30 days</option>
          </select>
        </div>

        {statsLoading ? (
          <div className="loading-spinner"></div>
        ) : (
          <div className="stats-grid">
            <div className="stat-card">
              <h3>Total Accesses</h3>
              <div className="stat-value">{stats?.total_accesses || 0}</div>
            </div>

            <div className="stat-card">
              <h3>Top Cameras</h3>
              <BarList
                accent="green"
                items={(stats?.by_camera || []).slice(0, 5).map((c) => ({
                  key: c.camera_id,
                  label: c.camera_id,
                  count: c.count,
                }))}
              />
            </div>

            <div className="stat-card">
              <h3>Top Viewers</h3>
              <BarList
                accent="green"
                items={(stats?.by_user || []).slice(0, 5).map((u) => ({
                  key: u.user_id,
                  label: u.user_email || (u.user_id ? u.user_id.substring(0, 12) + "..." : "—"),
                  count: u.count,
                }))}
              />
            </div>

            <div className="stat-card">
              <h3>Daily Activity</h3>
              <DailyActivityChart accent="green" data={stats?.by_day} />
            </div>
          </div>
        )}
      </div>
      </>)}

      {activeTab === "audit" && <OrgAuditLogPanel />}

      {activeTab === "mcp" && (<>
      <div className="audit-section">
        <div className="audit-section-header">
          <div>
            <h2>
              MCP Tool Activity
              <span
                className={`live-dot${sseConnected ? " live-dot-on" : ""}`}
                title={sseConnected ? "Live — new tool calls appear in real time" : "Reconnecting…"}
                aria-label={sseConnected ? "Live feed connected" : "Live feed disconnected"}
              >
                <span className="live-dot-pulse" aria-hidden="true" />
                {sseConnected ? "LIVE" : "OFFLINE"}
              </span>
            </h2>
            <p className="section-description">
              AI tool call history — see what MCP clients have done with your cameras and data.
            </p>
          </div>
          {/* Same filter-aware CSV export as Stream Access Logs above.
              Useful for compliance ("show me every MCP call from
              ci_robot in March") and for diagnosing AI-agent
              regressions in a spreadsheet. */}
          <button
            type="button"
            className="btn btn-secondary"
            onClick={handleExportMcpCsv}
            disabled={mcpExporting}
            title="Download the current view (with filters applied) as a CSV file"
          >
            {mcpExporting ? "Exporting…" : "Export CSV"}
          </button>
        </div>

        <div className="audit-filters">
          <div className="filter-group">
            <label>Tool</label>
            <select
              value={mcpFilters.tool_name}
              onChange={(e) => handleMcpFilterChange("tool_name", e.target.value)}
            >
              <option value="">All Tools</option>
              <optgroup label="Key Management">
                <option value="key_created">key_created (MCP)</option>
                <option value="key_revoked">key_revoked (MCP)</option>
                <option value="node_key_created">node_key_created</option>
                <option value="node_key_rotated">node_key_rotated</option>
                <option value="node_deleted">node_deleted</option>
              </optgroup>
              <optgroup label="Camera Tools">
                <option value="view_camera">view_camera</option>
                <option value="watch_camera">watch_camera</option>
                <option value="list_cameras">list_cameras</option>
                <option value="get_camera">get_camera</option>
                <option value="get_stream_url">get_stream_url</option>
              </optgroup>
              <optgroup label="System Tools">
                <option value="list_nodes">list_nodes</option>
                <option value="get_node">get_node</option>
                <option value="list_camera_groups">list_camera_groups</option>
                <option value="get_stream_logs">get_stream_logs</option>
                <option value="get_stream_stats">get_stream_stats</option>
                <option value="get_system_status">get_system_status</option>
              </optgroup>
              <optgroup label="Recording Tools">
                <option value="get_camera_recording_policy">get_camera_recording_policy</option>
                <option value="set_camera_recording_policy">set_camera_recording_policy</option>
              </optgroup>
              <optgroup label="Incident Tools">
                <option value="list_incidents">list_incidents</option>
                <option value="get_incident">get_incident</option>
                <option value="get_incident_snapshot">get_incident_snapshot</option>
                <option value="get_incident_clip">get_incident_clip</option>
                <option value="create_incident">create_incident</option>
                <option value="add_observation">add_observation</option>
                <option value="attach_snapshot">attach_snapshot</option>
                <option value="attach_clip">attach_clip</option>
                <option value="update_incident">update_incident</option>
                <option value="finalize_incident">finalize_incident</option>
              </optgroup>
            </select>
          </div>

          <div className="filter-group">
            <label>API Key</label>
            <input
              type="text"
              placeholder="Filter by key name"
              value={mcpFilters.key_name}
              onChange={(e) => handleMcpFilterChange("key_name", e.target.value)}
            />
          </div>

          <div className="filter-group">
            <label>Status</label>
            <select
              value={mcpFilters.status}
              onChange={(e) => handleMcpFilterChange("status", e.target.value)}
            >
              <option value="">All</option>
              <option value="completed">Completed</option>
              <option value="error">Error</option>
            </select>
          </div>

          <div className="filter-group">
            <label>Per Page</label>
            <select
              value={mcpFilters.limit}
              onChange={(e) => handleMcpFilterChange("limit", parseInt(e.target.value))}
            >
              <option value="25">25</option>
              <option value="50">50</option>
              <option value="100">100</option>
            </select>
          </div>
        </div>

        {mcpLoading ? (
          <div className="loading-spinner"></div>
        ) : mcpLogs.length === 0 ? (
          <div className="audit-empty">
            <div className="audit-empty-icon">🤖</div>
            <p>No MCP activity logs yet.</p>
          </div>
        ) : (
          <>
            <div className="audit-table-wrapper">
              <table className="audit-table">
                <thead>
                  <tr>
                    <th>Timestamp</th>
                    <th>Tool</th>
                    <th>API Key</th>
                    <th>Status</th>
                    <th>Duration</th>
                    <th>Details</th>
                  </tr>
                </thead>
                <tbody>
                  {mcpLogs.map(log => {
                    const KEY_EVENT_LABELS = {
                      key_created: "MCP Key Created",
                      key_revoked: "MCP Key Revoked",
                      node_key_created: "Node Key Created",
                      node_key_rotated: "Node Key Rotated",
                      node_deleted: "Node Deleted",
                    }
                    const keyLabel = KEY_EVENT_LABELS[log.tool_name]
                    const isKeyEvent = !!keyLabel
                    const isDestructive = log.tool_name === "key_revoked" || log.tool_name === "node_deleted"
                    const baseClass = log.status === "error" ? "row-error" : isKeyEvent ? "row-admin" : ""
                    const rowClass = [baseClass, log._isNew ? "row-new" : ""].filter(Boolean).join(" ")
                    return (
                      <tr key={log.id} className={rowClass}>
                        <td className="timestamp">
                          {new Date(log.timestamp).toLocaleString()}
                        </td>
                        <td>
                          {isKeyEvent ? (
                            <span className={`status-badge status-${isDestructive ? "key-revoked" : "key-created"}`}>
                              {keyLabel}
                            </span>
                          ) : (
                            <code>{log.tool_name}</code>
                          )}
                        </td>
                        <td>{log.key_name}</td>
                        <td>
                          <span className={`status-badge status-${log.status}`}>
                            {log.status}
                          </span>
                        </td>
                        <td>{log.duration_ms != null ? `${log.duration_ms}ms` : "—"}</td>
                        <td className="details-cell">
                          {log.error || log.args_summary || "—"}
                        </td>
                      </tr>
                    )
                  })}
                </tbody>
              </table>
            </div>

            {Math.ceil(mcpTotal / mcpFilters.limit) > 1 && (
              <div className="audit-pagination">
                <button
                  onClick={() => handleMcpPageChange(mcpFilters.offset - mcpFilters.limit)}
                  disabled={mcpFilters.offset === 0}
                >
                  Previous
                </button>
                <span className="page-info">
                  Page {Math.floor(mcpFilters.offset / mcpFilters.limit) + 1} of {Math.ceil(mcpTotal / mcpFilters.limit)}
                </span>
                <button
                  onClick={() => handleMcpPageChange(mcpFilters.offset + mcpFilters.limit)}
                  disabled={mcpFilters.offset + mcpFilters.limit >= mcpTotal}
                >
                  Next
                </button>
              </div>
            )}
          </>
        )}
      </div>

      {/* MCP Statistics */}
      <div className="audit-section">
        <div className="stats-header">
          <h2>MCP Statistics</h2>
          <select
            value={mcpDays}
            onChange={(e) => setMcpDays(parseInt(e.target.value))}
          >
            <option value="1">Last 24 hours</option>
            <option value="7">Last 7 days</option>
            <option value="14">Last 14 days</option>
            <option value="30">Last 30 days</option>
          </select>
        </div>

        {mcpStatsLoading ? (
          <div className="loading-spinner"></div>
        ) : (
          <div className="stats-grid">
            <div className="stat-card">
              <h3>Total MCP Calls</h3>
              <div className="stat-value">{mcpStats?.total_calls || 0}</div>
              {mcpStats?.total_errors > 0 && (
                <div className="stat-sub error">{mcpStats.total_errors} errors</div>
              )}
            </div>

            <div className="stat-card">
              <h3>Top Tools</h3>
              <BarList
                accent="purple"
                monoLabel
                items={(mcpStats?.by_tool || []).slice(0, 5).map((t) => ({
                  key: t.tool_name,
                  label: t.tool_name,
                  count: t.count,
                }))}
              />
            </div>

            <div className="stat-card">
              <h3>By API Key</h3>
              <BarList
                accent="purple"
                items={(mcpStats?.by_key || []).slice(0, 5).map((k) => ({
                  key: k.key_name,
                  label: k.key_name,
                  count: k.count,
                }))}
              />
            </div>

            <div className="stat-card">
              <h3>MCP Daily Activity</h3>
              <DailyActivityChart accent="purple" data={mcpStats?.by_day} />
            </div>
          </div>
        )}
      </div>
      </>)}
    </div>
  )
}

export default AdminPage