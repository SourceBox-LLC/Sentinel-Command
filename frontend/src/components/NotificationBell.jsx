import { useEffect, useRef, useState } from "react"
import { useNavigate } from "react-router-dom"
import { useNotifications } from "../hooks/useNotifications.jsx"

/**
 * Bell icon in the top-right with an unread badge and dropdown inbox.
 *
 * Opening the panel marks everything viewed on the server so the badge
 * clears.  New notifications arriving *while* the panel is open still
 * appear in the list and bump the badge back up — which is the
 * intended "there's something new right now" signal.
 */
export default function NotificationBell() {
  const { notifications, unreadCount, loading, markAllViewed, clearAll } = useNotifications()
  const [open, setOpen] = useState(false)
  const wrapperRef = useRef(null)
  const navigate = useNavigate()

  // Close when clicking outside
  useEffect(() => {
    if (!open) return
    function handleClick(e) {
      if (wrapperRef.current && !wrapperRef.current.contains(e.target)) {
        setOpen(false)
      }
    }
    document.addEventListener("mousedown", handleClick)
    return () => document.removeEventListener("mousedown", handleClick)
  }, [open])

  // Close on Escape
  useEffect(() => {
    if (!open) return
    function handleKey(e) {
      if (e.key === "Escape") setOpen(false)
    }
    document.addEventListener("keydown", handleKey)
    return () => document.removeEventListener("keydown", handleKey)
  }, [open])

  function togglePanel() {
    const willOpen = !open
    setOpen(willOpen)
    if (willOpen && unreadCount > 0) {
      // Opening the panel = "I've seen these" — clear the badge.
      markAllViewed()
    }
  }

  function handleItemClick(notif) {
    setOpen(false)
    if (notif.link) {
      navigate(notif.link)
    }
  }

  const badgeLabel = unreadCount > 99 ? "99+" : String(unreadCount)

  return (
    <div className="notif-bell-wrapper" ref={wrapperRef}>
      <button
        type="button"
        className={`notif-bell-btn${unreadCount > 0 ? " has-unread" : ""}`}
        onClick={togglePanel}
        aria-label={
          unreadCount > 0
            ? `Notifications, ${unreadCount} unread`
            : "Notifications"
        }
        aria-expanded={open}
      >
        <BellIcon />
        {unreadCount > 0 && (
          <span className="notif-bell-badge" aria-hidden="true">
            {badgeLabel}
          </span>
        )}
      </button>

      {open && (
        <div className="notif-panel" role="dialog" aria-label="Notifications">
          <div className="notif-panel-header">
            <div className="notif-panel-title">Notifications</div>
            <div className="notif-panel-actions">
              {/*
                Opening the panel already marks everything viewed (see
                togglePanel), so "Mark all read" is a no-op unless new
                items arrive while the panel is open.  Gate on
                unreadCount so users don't click a button that does
                nothing visible.
              */}
              {unreadCount > 0 && (
                <button
                  type="button"
                  className="notif-mark-all-btn"
                  onClick={markAllViewed}
                >
                  Mark all read
                </button>
              )}
              {/*
                "Clear all" soft-hides every visible row from *this*
                user's inbox.  Notifications stay in the DB for audit
                and for other users in the same org — see the backend
                /clear-all endpoint for the per-user semantics.
              */}
              {notifications.length > 0 && (
                <button
                  type="button"
                  className="notif-clear-all-btn"
                  onClick={clearAll}
                >
                  Clear all
                </button>
              )}
            </div>
          </div>

          <div className="notif-panel-list">
            {loading && (
              <div className="notif-panel-empty">Loading…</div>
            )}
            {!loading && notifications.length === 0 && (
              <div className="notif-panel-empty">
                No notifications yet.
                <br />
                <span className="notif-panel-empty-hint">
                  Motion events and system alerts will appear here.
                </span>
              </div>
            )}
            {notifications.map((n) => (
              <NotificationItem
                key={n.id}
                notification={n}
                onClick={() => handleItemClick(n)}
              />
            ))}
          </div>
        </div>
      )}
    </div>
  )
}

function NotificationItem({ notification, onClick }) {
  const { kind, title, body, severity, created_at, unread } = notification
  const hasLink = Boolean(notification.link)

  return (
    <button
      type="button"
      className={`notif-item notif-item-${severity}${unread ? " is-unread" : ""}${hasLink ? " has-link" : ""}`}
      onClick={onClick}
      disabled={!hasLink}
    >
      <span className={`notif-item-icon notif-icon-${severity}`}>
        {iconForKind(kind)}
      </span>
      <div className="notif-item-body">
        <div className="notif-item-title">
          {title}
          {unread && <span className="notif-item-dot" aria-hidden="true" />}
        </div>
        {body && <div className="notif-item-desc">{body}</div>}
        <div className="notif-item-time">{relativeTime(created_at)}</div>
      </div>
    </button>
  )
}

function iconForKind(kind) {
  switch (kind) {
    case "incident_created":
      return "⚠"
    case "motion":
    case "motion_digest":
      return "◉"
    case "camera_offline":
    case "node_offline":
      return "⚠"
    case "cameranode_disk_low":
    case "plan_limit_reached":
      return "⚠"
    case "camera_online":
    case "node_online":
      return "✓"
    case "error":
      return "✕"
    default:
      // welcome, member_added/removed/role_changed, mcp/integration
      // key events, and anything new → neutral bullet.
      return "•"
  }
}

function relativeTime(iso) {
  if (!iso) return ""
  const then = new Date(iso).getTime()
  if (Number.isNaN(then)) return ""
  const diff = Math.max(0, Date.now() - then)
  const s = Math.floor(diff / 1000)
  if (s < 60) return "just now"
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m ago`
  const h = Math.floor(m / 60)
  if (h < 24) return `${h}h ago`
  const d = Math.floor(h / 24)
  if (d < 7) return `${d}d ago`
  return new Date(iso).toLocaleDateString()
}

function BellIcon() {
  // Simple inline SVG — no icon library dependency needed.
  return (
    <svg
      xmlns="http://www.w3.org/2000/svg"
      width="18"
      height="18"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <path d="M6 8a6 6 0 0 1 12 0c0 7 3 9 3 9H3s3-2 3-9" />
      <path d="M10.3 21a1.94 1.94 0 0 0 3.4 0" />
    </svg>
  )
}
