import { Link } from "react-router-dom"

const UPGRADE_MESSAGES = {
  nodes: {
    title: "Node Limit Reached",
    icon: "🖥️",
    description: "You've hit the maximum number of camera nodes on your current plan.",
    benefit: "Upgrade to connect more nodes and expand your security coverage.",
  },
  cameras: {
    title: "Camera Limit Reached",
    icon: "📹",
    description: "You've reached the maximum number of cameras for your plan.",
    benefit: "Upgrade to monitor more cameras across all your locations.",
  },
  admin: {
    title: "Pro Feature",
    icon: "📊",
    description: "The Admin Dashboard with stream access logs and analytics is a Pro feature.",
    benefit: "Upgrade to get full visibility into who's accessing your streams.",
  },
  "danger-zone": {
    title: "Pro Feature",
    icon: "⚙️",
    description: "Advanced management tools like log wiping and full resets are Pro features.",
    benefit: "Upgrade for complete control over your organization data.",
  },
  mcp: {
    title: "MCP Integration",
    icon: "</>",
    description: "MCP lets AI tools like Claude Code directly control your cameras, nodes, and settings.",
    benefit: "Upgrade to connect your security system to AI-powered workflows.",
  },
}

// Numeric values mirror PLAN_LIMITS in backend/app/core/plans.py — keep them in
// sync. Viewer-hours / month is the real tier differentiator (the binding cap
// at runtime); cameras / nodes / seats are abuse rails that almost no
// legitimate customer hits, but they're listed here because the upgrade-decision
// audience cares about both sides of the picture.
const PLAN_COMPARISON = [
  { label: "Viewer-hours / month", free: "30", pro: "300", proPlus: "1,500" },
  { label: "Cameras", free: "5", pro: "25", proPlus: "200" },
  { label: "Nodes", free: "2", pro: "10", proPlus: "Unlimited" },
  { label: "Team members", free: "2", pro: "10", proPlus: "20" },
  { label: "Admin Dashboard", free: false, pro: true, proPlus: true },
  { label: "Stream Analytics", free: false, pro: true, proPlus: true },
  { label: "Danger Zone Tools", free: false, pro: true, proPlus: true },
  { label: "MCP Integration", free: false, pro: true, proPlus: true },
  { label: "Priority Support", free: false, pro: false, proPlus: true },
]

const isProPlus = (slug) => slug === "pro_plus"

function UpgradeModal({ isOpen, onClose, feature, currentPlan }) {
  if (!isOpen) return null

  const msg = UPGRADE_MESSAGES[feature] || UPGRADE_MESSAGES.nodes
  // self_host falls through to "Free" without this branch — this modal
  // is unlikely to trigger for self-host in practice (its caps are
  // effectively unlimited), but a self-hosted admin who does hit it
  // shouldn't be told they're on a Clerk plan they've never had.
  const planName =
    currentPlan === "free_org" ? "Free" :
    currentPlan === "pro" ? "Pro" :
    isProPlus(currentPlan) ? "Pro Plus" :
    currentPlan === "self_host" ? "Self-Hosted" : "Free"

  return (
    <div className="modal-overlay" onClick={onClose}>
      <div className="modal-content upgrade-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <h2>{msg.title}</h2>
          <button className="modal-close" onClick={onClose}>&times;</button>
        </div>

        <div className="modal-body">
          <div className="upgrade-modal-hero">
            <div className="upgrade-modal-icon">{msg.icon}</div>
            <p className="upgrade-modal-desc">{msg.description}</p>
            <p className="upgrade-modal-benefit">{msg.benefit}</p>
            <div className="upgrade-modal-current">
              Currently on the <strong>{planName}</strong> plan
            </div>
          </div>

          <div className="upgrade-comparison">
            <table className="comparison-table">
              <thead>
                <tr>
                  <th></th>
                  <th className={currentPlan === "free_org" ? "current-col" : ""}>Free</th>
                  <th className={currentPlan === "pro" ? "current-col" : "highlight-col"}>Pro</th>
                  <th className={isProPlus(currentPlan) ? "current-col" : ""}>Pro Plus</th>
                </tr>
              </thead>
              <tbody>
                {PLAN_COMPARISON.map((row) => (
                  <tr key={row.label}>
                    <td className="comparison-label">{row.label}</td>
                    <td className={currentPlan === "free_org" ? "current-col" : ""}>
                      {typeof row.free === "boolean" ? (row.free ? "✓" : "—") : row.free}
                    </td>
                    <td className={currentPlan === "pro" ? "current-col" : "highlight-col"}>
                      {typeof row.pro === "boolean" ? (row.pro ? "✓" : "—") : row.pro}
                    </td>
                    <td className={isProPlus(currentPlan) ? "current-col" : ""}>
                      {typeof row.proPlus === "boolean" ? (row.proPlus ? "✓" : "—") : row.proPlus}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          <div className="modal-actions">
            <button className="btn btn-secondary" onClick={onClose}>
              Maybe Later
            </button>
            <Link to="/pricing" className="btn btn-primary" onClick={onClose}>
              View Plans
            </Link>
          </div>
        </div>
      </div>
    </div>
  )
}

export default UpgradeModal
