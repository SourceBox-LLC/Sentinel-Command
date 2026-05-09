import { useState, useEffect, useRef } from "react"
import { Link } from "react-router-dom"
import { useAuth, useOrganization } from "@clerk/clerk-react"
import { getNodes, createNode as createNodeApi, rotateNodeKey, deleteNode as deleteNodeApi, wipeStreamLogs, fullReset, getSettings, updateNotificationSettings, updateOrgTimezone, getCameras, getEmailPreferences, updateEmailPreferences, downloadGdprExport } from "../services/api"
import { useToasts } from "../hooks/useToasts.jsx"
import { usePlanInfo } from "../hooks/usePlanInfo.jsx"
import AddNodeModal from "../components/AddNodeModal.jsx"
import KeyRotationModal from "../components/KeyRotationModal.jsx"
import UpgradeModal from "../components/UpgradeModal.jsx"
import NodeStorageBar from "../components/NodeStorageBar.jsx"
import CameraRecordingControls from "../components/CameraRecordingControls.jsx"
import HelpTooltip from "../components/HelpTooltip.jsx"

function formatRelativeTime(dateString) {
  if (!dateString) return ""
  const date = new Date(dateString)
  const now = new Date()
  const diffMs = now - date
  const diffMins = Math.floor(diffMs / 60000)
  const diffHours = Math.floor(diffMs / 3600000)
  const diffDays = Math.floor(diffMs / 86400000)

  if (diffMins < 1) return "Just now"
  if (diffMins < 60) return `${diffMins} min${diffMins === 1 ? "" : "s"} ago`
  if (diffHours < 24) return `${diffHours} hr${diffHours === 1 ? "" : "s"} ago`
  if (diffDays < 7) return `${diffDays} day${diffDays === 1 ? "" : "s"} ago`
  return date.toLocaleDateString()
}

function SettingsPage() {
  const { getToken } = useAuth()
  const { organization, membership } = useOrganization()
  const { showToast } = useToasts()
  const { planInfo, refreshPlanInfo } = usePlanInfo()
  const [nodes, setNodes] = useState([])
  const [nodesLoading, setNodesLoading] = useState(false)
  // Cameras for the recording-policy controls inside each node card.
  // Loaded alongside nodes (separate /api/cameras call) and grouped
  // client-side by `node_id` (the parent CameraNode's string id).
  const [cameras, setCameras] = useState([])
  const [showAddModal, setShowAddModal] = useState(false)
  const [showRotateModal, setShowRotateModal] = useState(false)
  const [selectedNode, setSelectedNode] = useState(null)
  const [deleteConfirm, setDeleteConfirm] = useState(null)
  const [deleting, setDeleting] = useState(false)

  // Notification preferences (org-level — see /api/settings response).
  // Recording configuration moved per-camera in v0.1.43; the per-camera
  // toggles live on each camera in the Camera Nodes section above.
  const [notifications, setNotifications] = useState(null)
  const [notificationsSaving, setNotificationsSaving] = useState(false)
  // Email alert preferences (per-org, per-kind).  Separate from
  // ``notifications`` above — that controls whether events appear
  // in the bell-icon inbox; this controls whether they ALSO email
  // out.  ``emailGloballyEnabled`` mirrors the EMAIL_ENABLED
  // server kill-switch so we can show a banner when the operator
  // has emails turned off platform-wide regardless of per-kind state.
  const [emailPrefs, setEmailPrefs] = useState(null)
  const [emailGloballyEnabled, setEmailGloballyEnabled] = useState(false)
  const [emailPrefsSaving, setEmailPrefsSaving] = useState(false)
  // Per-org timezone for scheduled-recording window interpretation.
  // IANA name; defaults to "UTC" until the operator picks one.  We
  // suggest the browser's tz on first interaction so the operator
  // doesn't have to think about it.
  const [orgTimezone, setOrgTimezone] = useState("UTC")
  const [timezoneSaving, setTimezoneSaving] = useState(false)

  // Upgrade modal
  const [upgradeFeature, setUpgradeFeature] = useState(null)

  // Node status tracking
  const prevNodesRef = useRef(null)

  // Danger Zone
  const [dangerAction, setDangerAction] = useState(null)
  const [dangerConfirmText, setDangerConfirmText] = useState("")
  const [dangerLoading, setDangerLoading] = useState(false)
  const [dangerResult, setDangerResult] = useState(null)

  // GDPR Article 20 export.  Separate state from Danger Zone because
  // export is benign + reversible (you get a ZIP, nothing changes
  // server-side beyond an audit row).
  const [gdprExporting, setGdprExporting] = useState(false)

  const handleExportGdpr = async () => {
    setGdprExporting(true)
    try {
      const token = await getToken()
      await downloadGdprExport(() => Promise.resolve(token))
      showToast("Data export downloaded.", "success")
    } catch (err) {
      console.error("GDPR export failed:", err)
      showToast(`Export failed: ${err.message || "unknown error"}`, "error")
    } finally {
      setGdprExporting(false)
    }
  }

  useEffect(() => {
    if (organization) {
      loadNodes()
      loadSettings()
      // Poll nodes every 30s to detect status changes
      const interval = setInterval(loadNodes, 30000)
      return () => clearInterval(interval)
    }
  }, [organization])

  const loadSettings = async () => {
    try {
      const token = await getToken()
      const tokenFn = () => Promise.resolve(token)
      // Parallel: inbox-level + email-level prefs come from different
      // endpoints (legacy /api/settings vs new /api/notifications/email/preferences)
      // but we want one round-trip-equivalent of latency on the
      // settings page's first paint.
      const [data, emailData] = await Promise.all([
        getSettings(tokenFn),
        getEmailPreferences(tokenFn).catch((err) => {
          // Old backends won't have this endpoint — graceful degrade
          // by leaving the email section unrendered.  Logged so an
          // unexpected 5xx is visible in the console.
          console.warn("Email prefs unavailable:", err?.message || err)
          return null
        }),
      ])
      // Backend defaults to "all on" when the notifications block is
      // missing, but be defensive for older backends that don't send it.
      setNotifications(
        data.notifications || {
          motion_notifications: true,
          camera_transition_notifications: true,
          node_transition_notifications: true,
        },
      )
      setOrgTimezone(data.timezone || "UTC")
      if (emailData) {
        setEmailPrefs(emailData.preferences || null)
        setEmailGloballyEnabled(Boolean(emailData.email_globally_enabled))
      }
    } catch (err) {
      console.error("Failed to load settings:", err)
      showToast("Failed to load settings", "error")
    }
  }

  const saveEmailPrefs = async (updated) => {
    // Optimistic — keep the toggle responsive.  Roll back to server
    // state on failure.
    const previous = emailPrefs
    setEmailPrefs(updated)
    setEmailPrefsSaving(true)
    try {
      const token = await getToken()
      const result = await updateEmailPreferences(
        () => Promise.resolve(token),
        updated,
      )
      // Server is the source of truth — re-sync from response so we
      // catch any field the backend rejected silently.
      if (result?.preferences) setEmailPrefs(result.preferences)
      showToast("Email preferences saved", "success")
    } catch (err) {
      setEmailPrefs(previous)
      showToast(err.message || "Failed to save email preferences", "error")
    } finally {
      setEmailPrefsSaving(false)
    }
  }

  const handleEmailToggle = (key) => {
    if (!emailPrefs) return
    saveEmailPrefs({ ...emailPrefs, [key]: !emailPrefs[key] })
  }

  const saveTimezone = async (tzName) => {
    const previous = orgTimezone
    setOrgTimezone(tzName)
    setTimezoneSaving(true)
    try {
      const token = await getToken()
      await updateOrgTimezone(() => Promise.resolve(token), tzName)
      showToast(`Timezone set to ${tzName}`, "success")
    } catch (err) {
      setOrgTimezone(previous)
      showToast(err.message || "Failed to save timezone", "error")
    } finally {
      setTimezoneSaving(false)
    }
  }

  const saveNotifications = async (updated) => {
    // Optimistic update — keep the toggle responsive even if the save
    // is slow.  Rollback to server state if the request fails.
    const previous = notifications
    setNotifications(updated)
    setNotificationsSaving(true)
    try {
      const token = await getToken()
      await updateNotificationSettings(() => Promise.resolve(token), updated)
      showToast("Notification settings saved", "success")
    } catch (err) {
      setNotifications(previous)
      showToast(err.message || "Failed to save notification settings", "error")
    } finally {
      setNotificationsSaving(false)
    }
  }

  const handleNotificationToggle = (key) => {
    if (!notifications) return
    saveNotifications({ ...notifications, [key]: !notifications[key] })
  }

  const loadNodes = async () => {
    if (!organization) return

    try {
      setNodesLoading(true)
      const token = await getToken()
      // Parallel fetch of nodes and cameras — cameras are needed for
      // the per-camera recording-policy controls inside each node card.
      // Promise.all so polling stays at one round-trip's worth of
      // wall time.
      const [nodesData, camerasData] = await Promise.all([
        getNodes(() => Promise.resolve(token)),
        getCameras(() => Promise.resolve(token)),
      ])

      // Detect nodes that just went offline or came back online
      if (prevNodesRef.current) {
        const prevMap = Object.fromEntries(prevNodesRef.current.map(n => [n.node_id, n]))
        for (const node of nodesData) {
          const prev = prevMap[node.node_id]
          if (prev && prev.status !== "offline" && node.status === "offline") {
            showToast(`Node "${node.name}" went offline`, "warning")
          } else if (prev && prev.status === "offline" && node.status !== "offline") {
            showToast(`Node "${node.name}" is back online`, "success")
          }
        }
      }
      prevNodesRef.current = nodesData

      setNodes(nodesData)
      setCameras(camerasData || [])
    } catch (err) {
      console.error("Failed to load nodes:", err)
      // Only toast on first load error, not poll errors
      if (!prevNodesRef.current) showToast("Failed to load camera nodes", "error")
    } finally {
      setNodesLoading(false)
    }
  }

  const handleCreateNode = async (name) => {
    const token = await getToken()

    try {
      const result = await createNodeApi(() => Promise.resolve(token), name)
      await loadNodes()
      await refreshPlanInfo()
      showToast(`Node "${name}" created successfully`, "success")
      // Stash a marker so the dashboard's HeartbeatBanner can pick it up
      // and celebrate the first heartbeat. Scoped by org to avoid leaking
      // across workspace switches.
      try {
        if (result?.node_id && organization?.id) {
          localStorage.setItem(
            `os.recentlyCreatedNode.${organization.id}`,
            JSON.stringify({
              node_id: result.node_id,
              name,
              created_at: Date.now(),
            })
          )
        }
      } catch (_) { /* localStorage unavailable — banner just won't show */ }
      return result
    } catch (err) {
      console.error("[SettingsPage] Failed to create node:", err)
      showToast(err.message || "Failed to create node", "error")
      throw err
    }
  }

  const handleDeleteNode = async (nodeId) => {
    setDeleting(true)
    try {
      const token = await getToken()
      await deleteNodeApi(() => Promise.resolve(token), nodeId)
      await loadNodes()
      await refreshPlanInfo()
      setDeleteConfirm(null)
      showToast("Node deleted and storage cleaned up", "success")
    } catch (err) {
      console.error("[SettingsPage] Failed to delete node:", err)
      showToast(err.message || "Failed to delete node", "error")
    } finally {
      setDeleting(false)
    }
  }

  const handleRotateKey = async (nodeId) => {
    const token = await getToken()
    try {
      const result = await rotateNodeKey(() => Promise.resolve(token), nodeId)
      await loadNodes()
      showToast("API key rotated — update your CloudNode config", "warning")
      return result
    } catch (err) {
      showToast(err.message || "Failed to rotate API key", "error")
      throw err
    }
  }

  const handleAddNodeClick = () => {
    if (planInfo && planInfo.usage.nodes >= planInfo.limits.max_nodes) {
      setUpgradeFeature("nodes")
    } else {
      setShowAddModal(true)
    }
  }

  const openRotateModal = (node) => {
    setSelectedNode(node)
    setShowRotateModal(true)
  }

  const dangerActions = {
    "wipe-logs": {
      title: "Wipe All Logs",
      description: "This will permanently delete all stream access logs, MCP activity logs, and statistics for your organization. This cannot be undone.",
      confirmPhrase: "wipe logs",
      handler: async () => {
        const token = await getToken()
        return await wipeStreamLogs(() => Promise.resolve(token))
      },
    },
    "full-reset": {
      title: "Full Organization Reset",
      description: "This will delete ALL nodes (notifying them to wipe local data), remove all cloud storage, clear all logs, and reset all settings. Your organization will be returned to a completely fresh state. This cannot be undone.",
      confirmPhrase: "reset everything",
      handler: async () => {
        const token = await getToken()
        const result = await fullReset(() => Promise.resolve(token))
        await loadNodes()
        return result
      },
    },
  }

  const handleDangerAction = async () => {
    const action = dangerActions[dangerAction]
    if (!action || dangerConfirmText !== action.confirmPhrase) return

    setDangerLoading(true)
    try {
      const result = await action.handler()
      setDangerResult(result)
      showToast(`${action.title} completed`, "success")
    } catch (err) {
      console.error("Danger action failed:", err)
      setDangerResult({ error: err.message })
      showToast(`${action.title} failed`, "error")
    } finally {
      setDangerLoading(false)
    }
  }

  const closeDangerModal = () => {
    setDangerAction(null)
    setDangerConfirmText("")
    setDangerResult(null)
    setDangerLoading(false)
  }

  if (!organization) {
    return (
      <div className="settings-container">
        <h1 className="page-title">Settings</h1>
        <p className="text-muted">Please select an organization to view settings.</p>
      </div>
    )
  }

  return (
    <div className="settings-container">
      <h1 className="page-title">Settings</h1>

      <div className="settings-section">
        <h2>Camera Nodes</h2>
        <p className="section-description">
          Manage your camera nodes. Each node can connect multiple cameras to your Command Center.
        </p>

        <div className="nodes-list">
          {nodesLoading ? (
            <div className="loading-spinner"></div>
          ) : nodes.length === 0 ? (
            <div className="empty-nodes">
              <p>No camera nodes configured yet.</p>
              <button
                className="btn btn-primary"
                onClick={handleAddNodeClick}
              >
                Add Your First Node
              </button>
            </div>
          ) : (
            <>
              {nodes.map((node) => (
                <div key={node.node_id} className="node-item">
                  <div className="node-info">
                    <div className="node-header-row">
                      <span className="node-name">{node.name || `Node ${node.node_id}`}</span>
                      <span className={`node-status status-${node.status}`}>
                        <span className="status-dot"></span>
                        {node.status}
                      </span>
                    </div>
                    <div className="node-meta">
                      <span className="node-id">ID: {node.node_id}</span>
                      {node.camera_count > 0 && (
                        <span className="node-cameras">{node.camera_count} camera{node.camera_count === 1 ? "" : "s"}</span>
                      )}
                      {node.node_version && (
                        <span
                          className="node-version"
                          title={`CloudNode v${node.node_version}`}
                        >
                          v{node.node_version}
                        </span>
                      )}
                      {node.last_seen && (
                        <span className="node-last-seen">
                          {formatRelativeTime(node.last_seen)}
                        </span>
                      )}
                    </div>
                    {node.update_available && (
                      <div className="node-update-available" role="status">
                        <span className="node-update-icon" aria-hidden="true">⬆</span>
                        <div className="node-update-body">
                          <strong>Update available: v{node.update_available}</strong>
                          {node.node_version && (
                            <span className="node-update-current">
                              {" "}(currently v{node.node_version})
                            </span>
                          )}
                          <p className="node-update-hint">
                            Re-run the installer on this node to upgrade.
                          </p>
                        </div>
                      </div>
                    )}
                    {node.key_rotated_at && (
                      <span className="node-key-rotated">
                        Key rotated {formatRelativeTime(node.key_rotated_at)}
                      </span>
                    )}
                    {node.last_register_error && (
                      <div className="node-register-error" role="alert">
                        <span className="node-register-error-icon">⚠️</span>
                        <div className="node-register-error-body">
                          <strong>Registration failing</strong>
                          <p>{node.last_register_error}</p>
                          {node.last_register_error_at && (
                            <span className="node-register-error-time">
                              {formatRelativeTime(node.last_register_error_at)}
                            </span>
                          )}
                          <button
                            type="button"
                            className="btn btn-small btn-primary"
                            onClick={() => openRotateModal(node)}
                          >
                            Rotate Key
                          </button>
                        </div>
                      </div>
                    )}
                    <NodeStorageBar storage={node.storage} />
                    {/* Per-camera recording-policy controls (v0.1.43+).
                        Cameras for this node, joined client-side from
                        the parallel /api/cameras fetch.  Renders one
                        small panel per camera with Continuous 24/7 +
                        Scheduled Recording toggles. */}
                    {cameras
                      .filter((c) => c.node_id === node.node_id)
                      .map((cam) => (
                        <CameraRecordingControls
                          key={cam.camera_id}
                          camera={cam}
                          timezone={orgTimezone}
                          onUpdated={(newPolicy) => {
                            // Mirror the server's authoritative state
                            // into the local cameras list so a re-render
                            // before the next poll reflects the toggle.
                            setCameras((prev) =>
                              prev.map((c) =>
                                c.camera_id === cam.camera_id
                                  ? { ...c, recording_policy: newPolicy }
                                  : c,
                              ),
                            )
                          }}
                        />
                      ))}
                  </div>
                  <div className="node-actions">
                    <button
                      className="btn btn-small btn-secondary"
                      onClick={() => openRotateModal(node)}
                    >
                      Rotate Key
                    </button>
                    <button
                      className="btn btn-small btn-danger"
                      onClick={() => setDeleteConfirm(node.node_id)}
                    >
                      Delete
                    </button>
                  </div>
                </div>
              ))}
              <button
                className="btn btn-primary add-node-btn"
                onClick={handleAddNodeClick}
              >
                Add Node
              </button>
            </>
          )}
        </div>

        {deleteConfirm && (
          <div className="modal-overlay" onClick={() => !deleting && setDeleteConfirm(null)}>
            <div className="modal-content small" onClick={(e) => e.stopPropagation()}>
              <div className="modal-header">
                <h2>{deleting ? "Deleting Node..." : "Delete Node?"}</h2>
              </div>
              <div className="modal-body">
                {deleting ? (
                  <div className="delete-progress">
                    <div className="loading-spinner" />
                    <p>Removing node and associated cameras...</p>
                  </div>
                ) : (
                  <p>Are you sure you want to delete this node? This will also remove all associated cameras and their stored footage.</p>
                )}
                <div className="modal-actions">
                  <button
                    className="btn btn-secondary"
                    onClick={() => setDeleteConfirm(null)}
                    disabled={deleting}
                  >
                    Cancel
                  </button>
                  <button
                    className="btn btn-danger"
                    onClick={() => handleDeleteNode(deleteConfirm)}
                    disabled={deleting}
                  >
                    {deleting ? "Deleting..." : "Delete"}
                  </button>
                </div>
              </div>
            </div>
          </div>
        )}
      </div>

      <div className="settings-section">
        <h2>Organization</h2>
        <div className="org-card">
          <div className="org-card-header">
            {organization?.imageUrl ? (
              <img src={organization.imageUrl} alt="" className="org-avatar" />
            ) : (
              <div className="org-avatar org-avatar-fallback">
                {(organization?.name || "O").charAt(0).toUpperCase()}
              </div>
            )}
            <div className="org-card-title">
              <h3>{organization?.name || "Unknown"}</h3>
              <span className="org-role-badge">
                {membership?.role === "org:admin" ? "Admin" : "Member"}
              </span>
            </div>
          </div>
          <div className="org-card-details">
            <div className="org-detail">
              <span className="org-detail-label">Members</span>
              <span className="org-detail-value">{organization?.membersCount || 1}</span>
            </div>
            <div className="org-detail">
              <span className="org-detail-label">Created</span>
              <span className="org-detail-value">
                {organization?.createdAt
                  ? new Date(organization.createdAt).toLocaleDateString()
                  : "—"}
              </span>
            </div>
            <div className="org-detail">
              <span className="org-detail-label">Nodes</span>
              <span className="org-detail-value">{nodes.length}</span>
            </div>
            <div className="org-detail">
              <span className="org-detail-label">Cameras</span>
              <span className="org-detail-value">
                {nodes.reduce((sum, n) => sum + (n.camera_count || 0), 0)}
              </span>
            </div>
          </div>
          <div className="org-card-id">
            <span className="org-detail-label">Org ID</span>
            <code>{organization?.id || "Unknown"}</code>
          </div>
        </div>
      </div>

      {/* Recording configuration moved per-camera in v0.1.43 — see
          the per-camera toggles inside each Camera Nodes card above.
          Org-level Continuous 24/7 / Scheduled Recording were removed
          because they never actually drove recording (they persisted
          to a Setting row but no consumer read them). */}

      {notifications && (
        <div className="settings-section">
          <h2>Notifications</h2>
          <p className="section-description">
            Choose which events show up in the bell inbox. Underlying motion
            events still record to history for incidents and analytics —
            turning a toggle off just stops the notification from appearing.
          </p>
          <div className="settings-toggles">
            <label className="toggle-row">
              <div className="toggle-info">
                <span className="toggle-label">Motion detection</span>
                <span className="toggle-desc">
                  Alert when a camera detects scene changes above its threshold
                </span>
              </div>
              <button
                type="button"
                className={`toggle-switch ${notifications.motion_notifications ? "active" : ""}`}
                onClick={() => handleNotificationToggle("motion_notifications")}
                disabled={notificationsSaving}
                aria-label="Toggle motion detection notifications"
                aria-pressed={notifications.motion_notifications}
              >
                <span className="toggle-knob" />
              </button>
            </label>
          </div>
        </div>
      )}

      {/*
        Email alerts — separate section so the visual hierarchy makes
        the inbox-vs-email distinction obvious.  Inbox toggles control
        the bell-icon panel; email toggles control whether the same
        event ALSO emails out.  An event can be in the inbox OR emailed
        OR both OR neither, which is the granularity operators have
        asked for.
      */}
      {emailPrefs && (
        <div className="settings-section" id="settings-notifications">
          <h2>Email Alerts</h2>
          <p className="section-description">
            Get an email when something operator-critical happens.
            The first six default ON for new orgs — turn off the
            ones you don't need.  <strong>Motion detection emails
            default OFF</strong> and must be opted in below; they
            ship with cooldown + digest behavior so you get one
            "first motion" email plus at most one summary per
            camera per cooldown window, not a flood.
          </p>
          {!emailGloballyEnabled && (
            <div
              style={{
                padding: "0.75rem 1rem",
                marginBottom: "1rem",
                background: "rgba(245, 158, 11, 0.1)",
                border: "1px solid rgba(245, 158, 11, 0.3)",
                borderRadius: "6px",
                color: "#f59e0b",
                fontSize: "0.9rem",
                lineHeight: 1.5,
              }}
            >
              <strong>Heads up:</strong> the platform-level email
              kill-switch is OFF on this Command Center. No emails
              will be sent regardless of the toggles below until an
              operator flips <code>EMAIL_ENABLED=true</code>. Per-org
              toggles still save and will activate the moment the
              kill-switch turns on.
            </div>
          )}
          <div className="settings-toggles">
            {[
              {
                key: "email_camera_offline",
                label: "Camera offline / recovered",
                desc:
                  "When a camera misses heartbeats for >90 seconds — " +
                  "AND the all-clear when it comes back.",
                audience: "All members",
              },
              {
                key: "email_node_offline",
                label: "CloudNode offline / recovered",
                desc:
                  "When a node loses uplink (every camera on it goes " +
                  "dark) — AND when it heartbeats again.",
                audience: "Admins only",
              },
              {
                key: "email_incident_created",
                label: "AI agent created an incident",
                desc:
                  "When a connected MCP agent (Claude, Cursor, etc.) " +
                  "opens a new incident report.",
                audience: "All members",
              },
              {
                key: "email_mcp_key_audit",
                label: "MCP API key audit",
                desc:
                  "When a new MCP key is generated OR an existing " +
                  "key is revoked.  Catches \"who just got " +
                  "programmatic access to my cameras?\" early.",
                audience: "Admins only",
              },
              {
                key: "email_cloudnode_disk_low",
                label: "CloudNode disk almost full",
                desc:
                  "When YOUR CloudNode hardware passes 90% disk " +
                  "use — recordings will fail when it caps out.  " +
                  "Different from our Command Center disk; this " +
                  "one is on the device you can act on.",
                audience: "Admins only",
              },
              {
                key: "email_member_audit",
                label: "Member added / role changed / removed",
                desc:
                  "Whenever your org's member list changes — new " +
                  "user added, role updated, or member removed.  " +
                  "Catches \"someone just got admin access to my " +
                  "cameras\" within seconds.",
                audience: "Admins only",
              },
              {
                key: "email_motion",
                label: "Motion detection (with digest)",
                desc:
                  "First motion event from each camera triggers an " +
                  "immediate email.  Any additional events in the " +
                  "next ~15 minutes are summarised in a single digest " +
                  "email (\"X more motion events on Front Door\") so " +
                  "a flappy outdoor camera doesn't flood your inbox.  " +
                  "Default OFF — opt in if you want it.",
                audience: "All members",
                // Only motion has a tooltip — the "why is this default
                // OFF when everything else is default ON?" question is
                // a real onboarding speed bump.  All other rows speak
                // for themselves.
                help: (
                  <>
                    Motion is the only email kind that defaults <strong>OFF</strong>.
                    Per-org motion volume varies wildly (1 indoor doorbell vs.
                    10 outdoor cameras with foliage triggers) — opting users
                    in by default risks day-one volume that drives spam-marks.
                    Spam-marks against our sender domain hurt deliverability
                    for <strong>every</strong> email kind across <strong>every</strong> customer,
                    so we let you opt in deliberately.  Cooldown + digest
                    caps you at 2 emails per camera per 15-minute window.
                  </>
                ),
              },
            ].map(({ key, label, desc, audience, help }) => (
              <label key={key} className="toggle-row">
                <div className="toggle-info">
                  <span className="toggle-label">
                    {label}
                    {help && (
                      <HelpTooltip label={`Help: ${label}`}>
                        {help}
                      </HelpTooltip>
                    )}
                  </span>
                  <span className="toggle-desc">
                    {desc}{" "}
                    <span style={{ color: "#9ca3af", fontSize: "0.8rem" }}>
                      · {audience}
                    </span>
                  </span>
                </div>
                <button
                  type="button"
                  className={`toggle-switch ${emailPrefs[key] ? "active" : ""}`}
                  onClick={() => handleEmailToggle(key)}
                  disabled={emailPrefsSaving}
                  aria-label={`Toggle email for ${label}`}
                  aria-pressed={Boolean(emailPrefs[key])}
                >
                  <span className="toggle-knob" />
                </button>
              </label>
            ))}
          </div>
        </div>
      )}

      <div className="settings-section">
        <h2>Time Zone</h2>
        <p className="section-description">
          The wall-clock time used to interpret per-camera scheduled
          recording windows. Pick the zone where your cameras live so
          "08:00–17:00" means 8am to 5pm local — DST is handled
          automatically. Defaults to UTC for new orgs.
        </p>
        <div style={{ display: "flex", alignItems: "center", gap: "0.75rem" }}>
          <select
            value={orgTimezone}
            onChange={(e) => saveTimezone(e.target.value)}
            disabled={timezoneSaving}
            style={{
              flex: 1,
              padding: "0.5rem 0.75rem",
              background: "var(--bg-secondary, #1a1a1a)",
              color: "var(--text-primary, #fff)",
              border: "1px solid var(--border, #333)",
              borderRadius: "6px",
              fontSize: "0.95rem",
              cursor: timezoneSaving ? "wait" : "pointer",
            }}
          >
            {/* List built from Intl.supportedValuesOf when available
                (Chrome 99+, Firefox 99+, Safari 15.4+).  Falls back
                to a curated list of common zones for older browsers. */}
            {(typeof Intl !== "undefined" && Intl.supportedValuesOf
              ? Intl.supportedValuesOf("timeZone")
              : [
                  "UTC",
                  "America/Los_Angeles",
                  "America/Denver",
                  "America/Chicago",
                  "America/New_York",
                  "America/Sao_Paulo",
                  "Europe/London",
                  "Europe/Paris",
                  "Europe/Berlin",
                  "Asia/Tokyo",
                  "Asia/Singapore",
                  "Australia/Sydney",
                ]
            ).map((tz) => (
              <option key={tz} value={tz}>
                {tz}
              </option>
            ))}
          </select>
          {orgTimezone === "UTC" && (
            <button
              type="button"
              onClick={() => {
                const browserTz =
                  Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC"
                if (browserTz !== "UTC") saveTimezone(browserTz)
              }}
              disabled={timezoneSaving}
              style={{
                padding: "0.5rem 0.9rem",
                background: "var(--accent-green, #22c55e)",
                color: "var(--bg-primary, #0a0a0a)",
                border: "none",
                borderRadius: "6px",
                fontWeight: 600,
                fontSize: "0.85rem",
                cursor: timezoneSaving ? "wait" : "pointer",
              }}
            >
              Use browser ({Intl.DateTimeFormat().resolvedOptions().timeZone})
            </button>
          )}
        </div>
      </div>

      {planInfo && (
        <div className="settings-section">
          <h2>Subscription</h2>
          <div className="plan-card">
            <div className="plan-card-header">
              <div className="plan-name-row">
                <h3>{planInfo.plan_name} Plan</h3>
                <span className={`plan-badge plan-badge-${planInfo.plan}`}>
                  {planInfo.plan === "free_org" ? "Free" : planInfo.plan_name}
                </span>
              </div>
              {planInfo.plan === "free_org" && (
                <Link to="/pricing" className="btn btn-primary btn-small">
                  Upgrade
                </Link>
              )}
              {planInfo.plan === "pro" && (
                <Link to="/pricing" className="btn btn-secondary btn-small">
                  Manage Plan
                </Link>
              )}
              {planInfo.plan === "pro_plus" && (
                <Link to="/pricing" className="btn btn-secondary btn-small">
                  Manage Plan
                </Link>
              )}
            </div>
            <div className="plan-usage">
              <div className="usage-item">
                <div className="usage-label">
                  <span>Cameras</span>
                  <span className="usage-count">
                    {planInfo.usage.cameras} / {planInfo.limits.max_cameras >= 999 ? "Unlimited" : planInfo.limits.max_cameras}
                  </span>
                </div>
                <div className="usage-bar">
                  <div
                    className={`usage-fill ${planInfo.usage.cameras >= planInfo.limits.max_cameras ? "usage-full" : ""}`}
                    style={{ width: `${Math.min(100, (planInfo.usage.cameras / planInfo.limits.max_cameras) * 100)}%` }}
                  />
                </div>
              </div>
              <div className="usage-item">
                <div className="usage-label">
                  <span>Nodes</span>
                  <span className="usage-count">
                    {planInfo.usage.nodes} / {planInfo.limits.max_nodes >= 999 ? "Unlimited" : planInfo.limits.max_nodes}
                  </span>
                </div>
                <div className="usage-bar">
                  <div
                    className={`usage-fill ${planInfo.usage.nodes >= planInfo.limits.max_nodes ? "usage-full" : ""}`}
                    style={{ width: `${Math.min(100, (planInfo.usage.nodes / Math.min(planInfo.limits.max_nodes, 50)) * 100)}%` }}
                  />
                </div>
              </div>
            </div>
          </div>
        </div>
      )}

      {/*
        Privacy & Data — sits just above Danger Zone because the
        themes are related (control over your org's data) but
        deliberately distinct: Export is benign + reversible (you
        get a ZIP, nothing changes server-side beyond an audit
        row).  Danger Zone actions are irreversible.  Putting them
        in separate sections keeps the visual hierarchy honest.
      */}
      <div className="settings-section">
        <h2>Privacy &amp; Data</h2>
        <p className="section-description">
          GDPR Article 20 (data portability) export.  Downloads a
          ZIP with one JSON file per data table in your organization
          &mdash; cameras, settings, audit log, motion events,
          notifications, MCP keys, email log, incidents, and the
          monthly usage counter.  Recordings live on your CloudNode
          devices, not Command Center, and are <strong>not</strong>
          included.  Admin only.  Rate-limited to 3 exports/hour.
        </p>
        <div className="privacy-actions">
          <button
            type="button"
            className="btn btn-secondary"
            onClick={handleExportGdpr}
            disabled={gdprExporting}
            title="Download a ZIP with all your organization's data"
          >
            {gdprExporting ? "Preparing export…" : "Download my data (ZIP)"}
          </button>
        </div>
      </div>

      <div className="settings-section danger-zone">
        <h2>Danger Zone</h2>
        <p className="section-description">
          Irreversible actions that affect your entire organization.
        </p>

        {planInfo && !planInfo.features?.includes("admin") ? (
          <div className="danger-locked">
            <div className="locked-icon">🔒</div>
            <p>Danger zone actions require a <strong>Pro</strong> or <strong>Pro Plus</strong> plan.</p>
            <button
              className="btn btn-primary btn-small"
              onClick={() => setUpgradeFeature("danger-zone")}
            >
              Upgrade
            </button>
          </div>
        ) : (
        <div className="danger-actions">
          <div className="danger-item">
            <div className="danger-info">
              <h3>Wipe All Logs</h3>
              <p>Delete all stream access logs, MCP activity logs, and usage statistics.</p>
            </div>
            <button
              className="btn btn-danger"
              onClick={() => setDangerAction("wipe-logs")}
            >
              Wipe Logs
            </button>
          </div>

          <div className="danger-item">
            <div className="danger-info">
              <h3>Full Organization Reset</h3>
              <p>Delete all nodes, cameras, cloud storage, logs, and settings. Nodes will be notified to wipe local data.</p>
            </div>
            <button
              className="btn btn-danger"
              onClick={() => setDangerAction("full-reset")}
            >
              Reset Everything
            </button>
          </div>
        </div>
        )}

        {dangerAction && (
          <div className="modal-overlay" onClick={() => !dangerLoading && closeDangerModal()}>
            <div className="modal-content small" onClick={(e) => e.stopPropagation()}>
              <div className="modal-header">
                <h2>{dangerActions[dangerAction].title}</h2>
              </div>
              <div className="modal-body">
                {dangerResult ? (
                  <div className="danger-result">
                    {dangerResult.error ? (
                      <p className="danger-error">Failed: {dangerResult.error}</p>
                    ) : (
                      <>
                        <p className="danger-success">Operation completed successfully.</p>
                        {dangerResult.nodes_deleted !== undefined && (
                          <ul className="danger-summary">
                            <li>{dangerResult.nodes_deleted} node(s) deleted ({dangerResult.nodes_wiped} notified)</li>
                            <li>{dangerResult.cameras_deleted} camera(s) removed</li>
                            <li>{dangerResult.storage_cleaned} storage object(s) cleaned</li>
                            <li>{dangerResult.logs_deleted} stream log(s) deleted</li>
                            <li>{dangerResult.mcp_logs_deleted || 0} MCP log(s) deleted</li>
                            <li>{dangerResult.settings_deleted} setting(s) reset</li>
                          </ul>
                        )}
                        {dangerResult.deleted_logs !== undefined && (
                          <ul className="danger-summary">
                            <li>{dangerResult.deleted_logs} stream log(s) deleted</li>
                            {dangerResult.deleted_mcp_logs > 0 && (
                              <li>{dangerResult.deleted_mcp_logs} MCP activity log(s) deleted</li>
                            )}
                          </ul>
                        )}
                      </>
                    )}
                    <div className="modal-actions">
                      <button className="btn btn-secondary" onClick={closeDangerModal}>
                        Close
                      </button>
                    </div>
                  </div>
                ) : dangerLoading ? (
                  <div className="delete-progress">
                    <div className="loading-spinner" />
                    <p>Processing... This may take a moment.</p>
                  </div>
                ) : (
                  <>
                    <p className="danger-warning">{dangerActions[dangerAction].description}</p>
                    <div className="danger-confirm-input">
                      <label>
                        Type <strong>{dangerActions[dangerAction].confirmPhrase}</strong> to confirm:
                      </label>
                      <input
                        type="text"
                        value={dangerConfirmText}
                        onChange={(e) => setDangerConfirmText(e.target.value)}
                        placeholder={dangerActions[dangerAction].confirmPhrase}
                        autoFocus
                      />
                    </div>
                    <div className="modal-actions">
                      <button className="btn btn-secondary" onClick={closeDangerModal}>
                        Cancel
                      </button>
                      <button
                        className="btn btn-danger"
                        onClick={handleDangerAction}
                        disabled={dangerConfirmText !== dangerActions[dangerAction].confirmPhrase}
                      >
                        {dangerActions[dangerAction].title}
                      </button>
                    </div>
                  </>
                )}
              </div>
            </div>
          </div>
        )}
      </div>

      <AddNodeModal
        isOpen={showAddModal}
        onClose={() => setShowAddModal(false)}
        onCreate={handleCreateNode}
      />

      <KeyRotationModal
        isOpen={showRotateModal}
        onClose={() => {
          setShowRotateModal(false)
          setSelectedNode(null)
        }}
        node={selectedNode}
        onRotate={handleRotateKey}
      />

      <UpgradeModal
        isOpen={!!upgradeFeature}
        onClose={() => setUpgradeFeature(null)}
        feature={upgradeFeature}
        currentPlan={planInfo?.plan}
      />
    </div>
  )
}

export default SettingsPage
