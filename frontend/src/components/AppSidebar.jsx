import { Link, useLocation } from "react-router-dom"
import { useOrganization } from "@clerk/clerk-react"
import { usePlanInfo } from "../hooks/usePlanInfo.jsx"

function SidebarNavItem({ to, label, badge, badgeClass, locked, active, onNavigate }) {
  const base = active ? "nav-link active" : "nav-link"
  const className = locked ? `${base} nav-link-locked` : base
  return (
    <Link to={to} className={className} onClick={onNavigate}>
      <span className="app-sidebar-label">{label}</span>
      {badge && <span className={badgeClass}>{badge}</span>}
    </Link>
  )
}

function AppSidebar({ open, onClose }) {
  const { organization, isLoaded: orgLoaded, membership } = useOrganization()
  const { planInfo } = usePlanInfo()
  const location = useLocation()

  if (!organization) return null

  const isAdmin = orgLoaded && membership?.role === "org:admin"
  const planFeatures = planInfo?.features || []
  const hasAdminFeature = planFeatures.includes("admin")
  const isActive = (path) => location.pathname === path

  const navItems = [
    { to: "/dashboard", label: "Dashboard" },
    isAdmin && { to: "/settings", label: "Settings" },
    isAdmin && {
      to: "/admin",
      label: "Admin",
      ...(hasAdminFeature ? {} : { locked: true, badge: "PRO", badgeClass: "nav-pro-badge" }),
    },
    // Admin-gated like Settings/Admin — the /mcp route is RequireAdmin,
    // so showing it to members just silently bounced them to /dashboard.
    isAdmin && { to: "/mcp", label: "MCP" },
    // No PRO lock — Home Assistant integration is available on every tier.
    isAdmin && { to: "/integrations", label: "Integrations" },
    isAdmin && { to: "/incidents", label: "Incidents" },
    { to: "/sentinel", label: "Sentinel" },
    { to: "/docs", label: "Help" },
    { to: "/pricing", label: "Pricing" },
  ].filter(Boolean)

  const showPlanBanner = !!(planInfo && hasAdminFeature)
  const showUsage = !!(planInfo && typeof planInfo.usage?.viewer_hours_limit === "number")
  const isProPlus = planInfo?.plan === "pro_plus"
  const planClass = isProPlus ? "pro-plus" : "pro"

  let usageUsed = 0
  let usageLimit = 0
  let usagePct = 0
  let usageState = "ok"
  if (showUsage) {
    usageUsed = planInfo.usage.viewer_hours_used || 0
    usageLimit = planInfo.usage.viewer_hours_limit
    usagePct = usageLimit > 0 ? Math.min(100, (usageUsed / usageLimit) * 100) : 0
    usageState = usagePct >= 100 ? "full" : usagePct >= 80 ? "warn" : "ok"
  }

  return (
    <aside className="app-sidebar" data-open={open ? "true" : "false"}>
      <nav className="app-sidebar-nav">
        {navItems.map((item) => (
          <SidebarNavItem
            key={item.to}
            to={item.to}
            label={item.label}
            badge={item.badge}
            badgeClass={item.badgeClass}
            locked={item.locked}
            active={isActive(item.to)}
            onNavigate={onClose}
          />
        ))}
      </nav>

      {(showPlanBanner || showUsage) && <div className="app-sidebar-divider" />}

      {showPlanBanner && (
        <div className={`pro-status-bar pro-status-${planClass}`}>
          <div className="pro-status-left">
            <span className="pro-status-badge">{isProPlus ? "PRO PLUS" : "PRO"}</span>
            <span className="pro-status-text">
              {planInfo.usage.cameras} / {planInfo.limits.max_cameras >= 999 ? "∞" : planInfo.limits.max_cameras} cameras
              {" · "}
              {planInfo.usage.nodes} / {planInfo.limits.max_nodes >= 999 ? "∞" : planInfo.limits.max_nodes} nodes
              {" · "}
              MCP + Admin + Analytics
            </span>
          </div>
          <Link to="/settings" className="pro-status-link" onClick={onClose}>Manage Plan</Link>
        </div>
      )}

      {showUsage && (
        <div className={`usage-panel usage-${usageState}`} role="status" aria-live="polite">
          <div className="usage-panel-head">
            <div>
              <div className="usage-panel-title">Viewer hours this month</div>
              <div className="usage-panel-subtitle">
                Live video playback counts against your monthly cap. Recordings on your node do not.
              </div>
            </div>
            <div className="usage-panel-count">
              <strong>{usageUsed.toFixed(1)}</strong>
              <span className="usage-panel-slash">/</span>
              <span>{usageLimit}h</span>
            </div>
          </div>
          <div className="usage-panel-bar">
            <div className="usage-panel-fill" style={{ width: `${usagePct}%` }} />
          </div>
          {usageState === "warn" && (
            <div className="usage-panel-hint">
              Approaching your monthly cap — consider upgrading to keep streaming uninterrupted.
            </div>
          )}
          {usageState === "full" && (
            <div className="usage-panel-hint">
              Monthly cap reached. Live playback resumes on the 1st of next month, or upgrade for more viewing time.
            </div>
          )}
        </div>
      )}
    </aside>
  )
}

export default AppSidebar
