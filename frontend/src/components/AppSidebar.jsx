import { Link, useLocation } from "react-router-dom"
import { useOrganization } from "@clerk/clerk-react"
import { usePlanInfo } from "../hooks/usePlanInfo.jsx"

/* 18px stroke icons for the nav rail. Inline (not FeatureIcons) because
   these are nav-specific glyphs sized/weighted for 0.9rem labels. */
const iconProps = {
  width: 18,
  height: 18,
  viewBox: "0 0 24 24",
  fill: "none",
  stroke: "currentColor",
  strokeWidth: 1.8,
  strokeLinecap: "round",
  strokeLinejoin: "round",
  "aria-hidden": "true",
}

const NAV_ICONS = {
  dashboard: (
    <svg {...iconProps}>
      <rect x="3" y="3" width="7" height="7" rx="1.5" />
      <rect x="14" y="3" width="7" height="7" rx="1.5" />
      <rect x="3" y="14" width="7" height="7" rx="1.5" />
      <rect x="14" y="14" width="7" height="7" rx="1.5" />
    </svg>
  ),
  incidents: (
    <svg {...iconProps}>
      <path d="M12 3l9 16H3l9-16z" />
      <line x1="12" y1="10" x2="12" y2="14" />
      <circle cx="12" cy="16.6" r="0.4" fill="currentColor" stroke="none" />
    </svg>
  ),
  settings: (
    <svg {...iconProps}>
      <circle cx="12" cy="12" r="3" />
      <path d="M19.4 15a1.6 1.6 0 0 0 .33 1.77l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.6 1.6 0 0 0-1.77-.33 1.6 1.6 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09a1.6 1.6 0 0 0-1-1.51 1.6 1.6 0 0 0-1.77.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.6 1.6 0 0 0 .33-1.77 1.6 1.6 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09a1.6 1.6 0 0 0 1.51-1 1.6 1.6 0 0 0-.33-1.77l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.6 1.6 0 0 0 1.77.33h.01a1.6 1.6 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.6 1.6 0 0 0 1 1.51h.01a1.6 1.6 0 0 0 1.77-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.6 1.6 0 0 0-.33 1.77v.01a1.6 1.6 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.6 1.6 0 0 0-1.51 1z" />
    </svg>
  ),
  admin: (
    <svg {...iconProps}>
      <line x1="4" y1="20" x2="4" y2="12" />
      <line x1="10" y1="20" x2="10" y2="4" />
      <line x1="16" y1="20" x2="16" y2="9" />
      <line x1="22" y1="20" x2="2" y2="20" />
    </svg>
  ),
  mcp: (
    <svg {...iconProps}>
      <path d="M12 3l1.9 5.1L19 10l-5.1 1.9L12 17l-1.9-5.1L5 10l5.1-1.9L12 3z" />
      <path d="M19 15l.8 2.2L22 18l-2.2.8L19 21l-.8-2.2L16 18l2.2-.8L19 15z" />
    </svg>
  ),
  integrations: (
    <svg {...iconProps}>
      <path d="M10 4.5a2 2 0 1 1 4 0V6h3a1 1 0 0 1 1 1v3h1.5a2 2 0 1 1 0 4H18v3a1 1 0 0 1-1 1h-3v-1.5a2 2 0 1 0-4 0V18H7a1 1 0 0 1-1-1v-3H4.5a2 2 0 1 1 0-4H6V7a1 1 0 0 1 1-1h3V4.5z" />
    </svg>
  ),
  pricing: (
    <svg {...iconProps}>
      <rect x="2.5" y="5" width="19" height="14" rx="2.5" />
      <line x1="2.5" y1="10" x2="21.5" y2="10" />
      <line x1="6.5" y1="15" x2="10.5" y2="15" />
    </svg>
  ),
}

function SidebarNavItem({ to, label, icon, badge, badgeClass, locked, active, onNavigate }) {
  const base = active ? "nav-link active" : "nav-link"
  const className = locked ? `${base} nav-link-locked` : base
  return (
    <Link to={to} className={className} onClick={onNavigate}>
      {icon && <span className="nav-icon">{NAV_ICONS[icon]}</span>}
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

  const navSections = [
    {
      kicker: "Operations",
      items: [
        { to: "/dashboard", label: "Dashboard", icon: "dashboard" },
        isAdmin && { to: "/incidents", label: "Incidents", icon: "incidents" },
      ].filter(Boolean),
    },
    {
      kicker: "Workspace",
      items: [
        isAdmin && { to: "/settings", label: "Settings", icon: "settings" },
        // No PRO lock — Home Assistant integration is available on every tier.
        isAdmin && { to: "/integrations", label: "Integrations", icon: "integrations" },
        // Admin-gated like Settings/Admin — the /mcp route is RequireAdmin,
        // so showing it to members just silently bounced them to /dashboard.
        isAdmin && { to: "/mcp", label: "MCP", icon: "mcp" },
        isAdmin && {
          to: "/admin",
          label: "Admin",
          icon: "admin",
          ...(hasAdminFeature ? {} : { locked: true, badge: "PRO", badgeClass: "nav-pro-badge" }),
        },
      ].filter(Boolean),
    },
    {
      kicker: "Account",
      items: [{ to: "/pricing", label: "Pricing", icon: "pricing" }],
    },
    // Sentinel and Docs now live on the standalone website at
    // sentinel-command.com — removed from the in-app sidebar.
  ].filter((section) => section.items.length > 0)

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
        {navSections.map((section) => (
          <div key={section.kicker}>
            <div className="app-sidebar-kicker">{section.kicker}</div>
            {section.items.map((item) => (
              <SidebarNavItem
                key={item.to}
                to={item.to}
                label={item.label}
                icon={item.icon}
                badge={item.badge}
                badgeClass={item.badgeClass}
                locked={item.locked}
                active={isActive(item.to)}
                onNavigate={onClose}
              />
            ))}
          </div>
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
