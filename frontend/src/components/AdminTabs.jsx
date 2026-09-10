// Tab strip for the Admin dashboard's log surfaces (Stream Access /
// Organization Audit / MCP Activity / Motion). Cuts the page from
// "five sections stacked vertically" down to "one section at a time"
// and lets us put a red badge on the MCP tab when there are errors
// the admin should look at.

const TAB_DEFS = [
  { id: "stream", label: "Stream Access", icon: "📺", accent: "green" },
  { id: "audit", label: "Organization Audit", icon: "📋", accent: "amber" },
  { id: "mcp", label: "MCP Activity", icon: "🤖", accent: "purple" },
  { id: "motion", label: "Motion", icon: "🎞️", accent: "blue" },
]

function AdminTabs({ activeTab, onTabChange, streamCount, mcpCount, mcpErrors = 0 }) {
  // OrgAuditLogPanel owns its own pagination state and doesn't expose
  // a count up — leaving the audit tab without a count is the right
  // tradeoff vs. piping callbacks through that component just for the
  // pill.
  const counts = { stream: streamCount, mcp: mcpCount }

  return (
    <div className="admin-tabs" role="tablist" aria-label="Admin log views">
      {TAB_DEFS.map((tab) => {
        const isActive = activeTab === tab.id
        const count = counts[tab.id]
        const showErrorBadge = tab.id === "mcp" && mcpErrors > 0
        return (
          <button
            key={tab.id}
            type="button"
            role="tab"
            id={`admin-tab-${tab.id}`}
            aria-selected={isActive}
            aria-controls={`admin-panel-${tab.id}`}
            className={`admin-tab admin-tab-${tab.accent}${isActive ? " active" : ""}`}
            onClick={() => onTabChange(tab.id)}
          >
            <span className="admin-tab-icon" aria-hidden="true">{tab.icon}</span>
            <span className="admin-tab-label">{tab.label}</span>
            {typeof count === "number" && (
              <span className="admin-tab-count">{count.toLocaleString()}</span>
            )}
            {showErrorBadge && (
              <span
                className="admin-tab-error-badge"
                title={`${mcpErrors} ${mcpErrors === 1 ? "failure" : "failures"} in window`}
                aria-label={`${mcpErrors} errors`}
              >
                {mcpErrors}
              </span>
            )}
          </button>
        )
      })}
    </div>
  )
}

export default AdminTabs
