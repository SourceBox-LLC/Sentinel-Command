import { useCallback, useEffect, useRef, useState } from "react"
import { useAuth } from "@clerk/clerk-react"
import {
  clearAllNotifications,
  getNotifications,
  getUnreadNotificationCount,
  markNotificationsViewed,
} from "../services/api.js"

const API_URL = import.meta.env.VITE_API_URL || ""

/**
 * Hook backing the notification bell in the top bar.
 *
 * Responsibilities:
 *  - Fetch recent notifications + unread count on mount
 *  - Subscribe to `/api/notifications/stream` SSE so new events
 *    appear live without a refresh
 *  - Expose `markAllViewed()` for the bell panel to call when opened
 *
 * Uses fetch + manual line parsing (same as useMotionAlerts) because
 * `EventSource` doesn't support custom Authorization headers.
 *
 * Reconnects automatically on disconnect with exponential backoff.
 */
export function useNotifications() {
  const { getToken } = useAuth()
  const [notifications, setNotifications] = useState([])
  const [unreadCount, setUnreadCount] = useState(0)
  const [lastViewedAt, setLastViewedAt] = useState(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState(null)

  // Keep latest lastViewedAt accessible inside the SSE callback without
  // re-subscribing every time it changes.
  const lastViewedRef = useRef(null)
  lastViewedRef.current = lastViewedAt

  // Initial load: fetch the inbox and the unread count in parallel.
  const refresh = useCallback(async () => {
    try {
      const [list, count] = await Promise.all([
        getNotifications(getToken, { limit: 50 }),
        getUnreadNotificationCount(getToken),
      ])
      setNotifications(list.notifications || [])
      setLastViewedAt(list.last_viewed_at || count.last_viewed_at || null)
      setUnreadCount(count.unread || 0)
      setError(null)
    } catch (e) {
      setError(e.message || "Failed to load notifications")
    } finally {
      setLoading(false)
    }
  }, [getToken])

  useEffect(() => {
    refresh()
  }, [refresh])

  // Bump last_viewed_at on the server and zero the badge locally.
  const markAllViewed = useCallback(async () => {
    // Optimistic — the UI should feel instant
    setUnreadCount(0)
    try {
      const result = await markNotificationsViewed(getToken)
      if (result?.last_viewed_at) {
        setLastViewedAt(result.last_viewed_at)
        // Update per-item unread flags on existing items
        setNotifications((prev) =>
          prev.map((n) => ({ ...n, unread: false })),
        )
      }
    } catch {
      // Fall back to a refresh so state isn't lying
      refresh()
    }
  }, [getToken, refresh])

  // Soft-hide every currently-visible notification from this user.
  // Per-user, not per-org — other members still see their inbox intact,
  // and rows stay in the DB for audit/incidents.  Anything created after
  // this call still shows up normally.
  const clearAll = useCallback(async () => {
    // Stash the current list so we can roll back if the server rejects.
    const previous = notifications
    // Optimistic — empty the panel and zero the badge right away.
    setNotifications([])
    setUnreadCount(0)
    try {
      const result = await clearAllNotifications(getToken)
      if (result?.last_viewed_at) {
        setLastViewedAt(result.last_viewed_at)
      }
    } catch {
      // Rollback + resync so the UI reflects reality.
      setNotifications(previous)
      refresh()
    }
  }, [getToken, notifications, refresh])

  // SSE subscription for live updates.
  useEffect(() => {
    let cancelled = false
    let reconnectTimer = null
    let controller = null
    let backoff = 5000
    const MAX_BACKOFF = 30000

    async function connect() {
      if (cancelled) return

      let token
      try {
        token = await getToken()
      } catch {
        reconnectTimer = setTimeout(connect, backoff)
        return
      }

      controller = new AbortController()

      try {
        const res = await fetch(`${API_URL}/api/notifications/stream`, {
          headers: { Authorization: `Bearer ${token}` },
          signal: controller.signal,
        })

        if (!res.ok) {
          reconnectTimer = setTimeout(connect, backoff)
          backoff = Math.min(backoff * 2, MAX_BACKOFF)
          return
        }

        backoff = 5000

        const reader = res.body.getReader()
        const decoder = new TextDecoder()
        let buffer = ""

        while (true) {
          const { done, value } = await reader.read()
          if (done) break

          buffer += decoder.decode(value, { stream: true })
          const lines = buffer.split("\n")
          buffer = lines.pop()

          for (const line of lines) {
            if (!line.startsWith("data: ")) continue
            try {
              const event = JSON.parse(line.slice(6))
              if (event.type === "notification") {
                // A notification is "unread" if it was created after
                // the user's last_viewed_at on the server.  Live events
                // are always newer than the last snapshot we fetched,
                // so flag them unread and bump the badge.
                const item = { ...event, unread: true }
                setNotifications((prev) => {
                  // Cap locally at 200 to avoid unbounded memory growth
                  // on a long-lived tab.
                  const next = [item, ...prev]
                  return next.length > 200 ? next.slice(0, 200) : next
                })
                setUnreadCount((n) => (n >= 99 ? 99 : n + 1))
              }
            } catch {
              // Ignore malformed lines
            }
          }
        }
      } catch (err) {
        if (err.name === "AbortError") return
      }

      if (!cancelled) {
        reconnectTimer = setTimeout(connect, backoff)
        backoff = Math.min(backoff * 2, MAX_BACKOFF)
      }
    }

    connect()

    return () => {
      cancelled = true
      clearTimeout(reconnectTimer)
      controller?.abort()
    }
  }, [getToken])

  return {
    notifications,
    unreadCount,
    lastViewedAt,
    loading,
    error,
    markAllViewed,
    clearAll,
    refresh,
  }
}
