import { useState, useEffect, useCallback, useMemo, useRef } from "react"
import { Link } from "react-router-dom"
import { useAuth, useOrganization } from "@clerk/clerk-react"
import { getCameras, getCameraGroups } from "../services/api"
import { useToasts } from "../hooks/useToasts.jsx"
import { usePlanInfo } from "../hooks/usePlanInfo.jsx"
import { useMotionAlerts } from "../hooks/useMotionAlerts.jsx"
import CameraCard from "../components/CameraCard.jsx"
import UpgradeModal from "../components/UpgradeModal.jsx"
import HeartbeatBanner from "../components/HeartbeatBanner.jsx"
import { AdminWelcomeHero, MemberWelcomeHero } from "../components/WelcomeHero.jsx"

function DashboardPage() {
  const { getToken } = useAuth()
  const { organization, membership } = useOrganization()
  const { showToast } = useToasts()
  const { planInfo } = usePlanInfo()
  const [cameras, setCameras] = useState({})
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState(null)
  const [showUpgrade, setShowUpgrade] = useState(false)
  const prevCamerasRef = useRef(null)
  const toastedOfflinesRef = useRef(new Set())
  // Camera Groups Phase 3: groups list drives both the color-tag
  // lookup on each tile and the filter row above the grid.  Loaded
  // once on org switch (and refreshed by the camera-poll cycle below
  // so a group created in another tab shows up within ~5 s).
  // groupFilter is null for "show all", a numeric id for a specific
  // group, or the sentinel "ungrouped" for cameras with no group.
  const [groups, setGroups] = useState([])
  const [groupFilter, setGroupFilter] = useState(null)

  const isAdmin = membership?.role === "org:admin"

  // Real-time motion detection notifications via SSE
  useMotionAlerts(cameras)

  const loadCameras = useCallback(async () => {
    if (!organization) return
    
    try {
      setError(null)
      const token = await getToken()
      const data = await getCameras(() => Promise.resolve(token))
      
      const camerasMap = Array.isArray(data)
        ? data.reduce((acc, camera) => {
            if (camera.camera_id) {
              acc[camera.camera_id] = camera
            }
            return acc
          }, {})
        : data
      
      // Detect cameras that just went offline
      if (prevCamerasRef.current) {
        const newlyOffline = []
        for (const [id, cam] of Object.entries(camerasMap)) {
          const prev = prevCamerasRef.current[id]
          if (prev && prev.status !== "offline" && cam.status === "offline") {
            newlyOffline.push(cam.name || id)
          }
        }
        if (newlyOffline.length > 0) {
          // Only toast each camera once per offline event
          const fresh = newlyOffline.filter(n => !toastedOfflinesRef.current.has(n))
          if (fresh.length > 0) {
            fresh.forEach(n => toastedOfflinesRef.current.add(n))
            const msg = fresh.length === 1
              ? `Camera "${fresh[0]}" went offline`
              : `${fresh.length} cameras went offline`
            showToast(msg, "warning")
          }
        }
        // Clear from toasted set when cameras come back online
        for (const [id, cam] of Object.entries(camerasMap)) {
          if (cam.status !== "offline") {
            const name = cam.name || id
            if (toastedOfflinesRef.current.has(name)) {
              toastedOfflinesRef.current.delete(name)
              showToast(`Camera "${name}" is back online`, "success")
            }
          }
        }
      }

      // Only update state if data actually changed (shallow field comparison
      // instead of JSON.stringify — avoids blocking the main thread).
      const prev = prevCamerasRef.current
      let changed = !prev || Object.keys(camerasMap).length !== Object.keys(prev).length
      if (!changed) {
        for (const [id, cam] of Object.entries(camerasMap)) {
          const p = prev[id]
          if (!p || p.status !== cam.status || p.name !== cam.name || p.last_seen !== cam.last_seen) {
            changed = true
            break
          }
        }
      }
      if (changed) {
        prevCamerasRef.current = camerasMap
        setCameras(camerasMap)
      }
    } catch (err) {
      console.error("[Dashboard] Error loading cameras:", err)
      // Only toast on first error, not every poll cycle
      if (!error) showToast("Failed to load cameras", "error")
      setError(err.message)
    } finally {
      setLoading(false)
    }
  }, [organization, getToken])

  // Group fetch — runs alongside the camera fetch but doesn't need
  // 5-second cadence.  Refresh on org switch + every 30 s so a group
  // created in another tab shows up without a hard reload.  Soft-fails
  // (empty array) since the dashboard works fine without groups.
  const loadGroups = useCallback(async () => {
    if (!organization) return
    try {
      const token = await getToken()
      const data = await getCameraGroups(() => Promise.resolve(token))
      if (Array.isArray(data)) setGroups(data)
    } catch (err) {
      // Silent — groups are an enhancement, not a blocker.
      console.error("[Dashboard] Failed to load camera groups:", err)
    }
  }, [organization, getToken])

  useEffect(() => {
    if (!organization) return

    loadCameras()
    loadGroups()
    const cameraInterval = setInterval(loadCameras, 5000)
    const groupsInterval = setInterval(loadGroups, 30000)
    return () => {
      clearInterval(cameraInterval)
      clearInterval(groupsInterval)
    }
  }, [organization, loadCameras, loadGroups])

  // Map of group_id → group object for O(1) color lookup per tile.
  const groupById = useMemo(() => {
    const m = new Map()
    for (const g of groups) m.set(g.id, g)
    return m
  }, [groups])

  // Apply the active group filter.  null → all cameras.  "ungrouped" →
  // only cameras with no group_id.  Number → only that group.
  const filteredCameraEntries = useMemo(() => {
    const entries = Object.entries(cameras)
    if (groupFilter === null) return entries
    if (groupFilter === "ungrouped") {
      return entries.filter(([, c]) => !c.group_id)
    }
    return entries.filter(([, c]) => c.group_id === groupFilter)
  }, [cameras, groupFilter])


  // No manual refresh handler — the auto-refresh interval above
  // (loadCameras every 5s) keeps the dashboard live without user
  // action.  Manual button was removed once it became clear it was
  // a placebo: every Refresh click did the same fetch the background
  // poll already runs twice per 10s.

  const getStats = () => {
    const cameraList = Object.values(cameras)
    // "Active" = anything that's NOT in a known-down state.  Mirrors
    // the isDown logic used per-card in CameraCard.jsx so the stat
    // count and the per-card UI agree on what "down" means.
    // Includes streaming / online / recording / starting / restarting;
    // excludes offline / failed / error / plan-suspended.  Previous
    // version only counted streaming + online and undercounted any
    // camera in `recording` mode (continuous-24/7), which a 24/7
    // recording camera definitely is — that was a real bug.
    const active = cameraList.filter(c =>
      !(c.disabled_by_plan ||
        c.status === "offline" ||
        c.status === "failed" ||
        c.status === "error")
    ).length
    const total = cameraList.length
    const systemOk = total > 0
    return { active, total, systemOk }
  }

  if (!organization) {
    return (
      <div className="home-container">
        <div className="no-org-container">
          <h1 className="hero-title">No Organization Selected</h1>
          <p className="no-org-text">
            Create or join an organization to start managing your security cameras.
          </p>
        </div>
      </div>
    )
  }

  const stats = getStats()

  return (
    <div className="dashboard-container">
      <HeartbeatBanner />

      {isAdmin && planInfo?.payment_past_due && (() => {
        // Grace countdown: backend returns grace_days_remaining + grace_expires_at.
        // When the timestamp parses cleanly we show the live number; otherwise
        // fall back to the ToS-guaranteed window (grace_window_days) so the
        // copy still reads correctly for older backends that haven't shipped
        // the countdown fields yet.
        const daysLeft = planInfo.grace_days_remaining
        const windowDays = planInfo.grace_window_days ?? 7
        let copy
        if (daysLeft === 0) {
          copy = (
            <>
              <strong>Grace period expired.</strong> Cameras beyond the free-tier
              limit are now suspended. Update your payment method to restore them.
            </>
          )
        } else if (typeof daysLeft === "number") {
          copy = (
            <>
              <strong>Payment past due — {daysLeft} day{daysLeft === 1 ? "" : "s"} left.</strong>
              {" "}After that, cameras beyond the free-tier limit will be suspended.
              Update your payment method now to avoid interruption.
            </>
          )
        } else {
          copy = (
            <>
              Your payment is past due. Cameras beyond your free-tier limit will be
              suspended after a {windowDays}-day grace period — update your payment method to
              keep streaming.
            </>
          )
        }
        return (
          <div
            className={`payment-past-due-banner${daysLeft === 0 ? " payment-past-due-expired" : ""}`}
            role="status"
            aria-live="polite"
          >
            <span>{copy}</span>
            <Link to="/pricing" className="btn btn-primary btn-small">
              Manage Billing
            </Link>
          </div>
        )
      })()}

      <div className="stats-grid">
        <div className="stat-card">
          <div className="stat-label">Active Cameras</div>
          <div className="stat-value green">{stats.active}</div>
        </div>
        <div className="stat-card">
          <div className="stat-label">Total Cameras</div>
          <div className="stat-value blue">{stats.total}</div>
        </div>
        <div className="stat-card">
          <div className="stat-label">System Status</div>
          <div className={`stat-value ${stats.systemOk ? "green" : "amber"}`}>
            {stats.systemOk ? "Ready" : "Offline"}
          </div>
        </div>
      </div>

      {isAdmin && planInfo && planInfo.usage.cameras >= planInfo.limits.max_cameras && (
        <div className="plan-limit-banner">
          <span className="plan-limit-text">
            You've reached your camera limit ({planInfo.limits.max_cameras} on the {planInfo.plan_name} plan).
            New cameras won't be added until you upgrade.
          </span>
          <button className="btn btn-primary btn-small" onClick={() => setShowUpgrade(true)}>
            Upgrade
          </button>
        </div>
      )}

      {isAdmin && planInfo && planInfo.usage.cameras >= Math.floor(planInfo.limits.max_cameras * 0.8) && planInfo.usage.cameras < planInfo.limits.max_cameras && (
        <div className="plan-limit-banner plan-limit-warning">
          <span className="plan-limit-text">
            You're using {planInfo.usage.cameras} of {planInfo.limits.max_cameras} cameras on the {planInfo.plan_name} plan.
          </span>
          <button className="btn btn-secondary btn-small" onClick={() => setShowUpgrade(true)}>
            View Plans
          </button>
        </div>
      )}

      <div className="section-header">
        <h2 className="section-title">Camera Feeds</h2>
      </div>

      {/* Group filter pills (Camera Groups Phase 3).  Hidden when the
          org has no groups defined — there's nothing to filter against. */}
      {groups.length > 0 && Object.keys(cameras).length > 0 && (
        <div className="dashboard-group-filter" role="tablist" aria-label="Filter cameras by group">
          <button
            type="button"
            role="tab"
            aria-selected={groupFilter === null}
            className={`dashboard-group-pill ${groupFilter === null ? "active" : ""}`}
            onClick={() => setGroupFilter(null)}
          >
            All
          </button>
          {groups.map((g) => (
            <button
              key={g.id}
              type="button"
              role="tab"
              aria-selected={groupFilter === g.id}
              className={`dashboard-group-pill ${groupFilter === g.id ? "active" : ""}`}
              onClick={() => setGroupFilter(g.id)}
              style={{ "--group-color": g.color }}
            >
              <span className="dashboard-group-pill-swatch" aria-hidden="true" />
              {g.icon ? `${g.icon} ` : ""}
              {g.name}
            </button>
          ))}
          <button
            type="button"
            role="tab"
            aria-selected={groupFilter === "ungrouped"}
            className={`dashboard-group-pill ${groupFilter === "ungrouped" ? "active" : ""}`}
            onClick={() => setGroupFilter("ungrouped")}
          >
            Ungrouped
          </button>
        </div>
      )}

      {loading ? (
        <div className="empty-state">
          <div className="loading-spinner"></div>
          <p>Loading cameras...</p>
        </div>
      ) : error ? (
        <div className="empty-state">
          <div className="empty-icon">⚠️</div>
          <h3>Error Loading Cameras</h3>
          <p>{error}</p>
          <button onClick={loadCameras} className="btn btn-primary">
            Retry
          </button>
        </div>
      ) : Object.keys(cameras).length === 0 ? (
        isAdmin
          ? <AdminWelcomeHero />
          : <MemberWelcomeHero orgName={organization?.name} />
      ) : filteredCameraEntries.length === 0 ? (
        // Cameras exist but the filter hides them all — show a neutral
        // empty state with a CTA to clear the filter, NOT the
        // first-run welcome hero (which would imply zero cameras).
        <div className="empty-state">
          <div className="empty-icon">🔍</div>
          <h3>No cameras match this filter</h3>
          <p>
            {groupFilter === "ungrouped"
              ? "Every camera is currently assigned to a group."
              : "No cameras are assigned to this group yet."}
          </p>
          <button onClick={() => setGroupFilter(null)} className="btn btn-secondary">
            Show all cameras
          </button>
        </div>
      ) : (
        <div className="camera-grid">
          {filteredCameraEntries.map(([cameraId, camera]) => (
            <CameraCard
              key={cameraId}
              cameraId={cameraId}
              camera={camera}
              group={camera.group_id ? groupById.get(camera.group_id) : null}
              onRequestUpgrade={() => setShowUpgrade(true)}
            />
          ))}
        </div>
      )}

      <UpgradeModal
        isOpen={showUpgrade}
        onClose={() => setShowUpgrade(false)}
        feature="cameras"
        currentPlan={planInfo?.plan}
      />
    </div>
  )
}

export default DashboardPage