import { useEffect, useRef } from "react"
import { useAuth, useOrganization } from "@clerk/clerk-react"
import { useToasts } from "./useToasts.jsx"

const API_URL = import.meta.env.VITE_API_URL || ""

/**
 * Subscribe to the motion-events SSE stream and show a toast for each
 * motion detection event.  `cameras` is the dashboard's cameras map so
 * we can resolve camera_id -> friendly name.
 *
 * Uses `fetch` + manual line parsing instead of the `EventSource` API
 * because EventSource doesn't support custom Authorization headers
 * (Clerk JWT).
 *
 * Reconnects automatically on disconnect with exponential backoff
 * (5 s -> 10 s -> 20 s -> 30 s max).
 */
export function useMotionAlerts(cameras) {
  const { getToken } = useAuth()
  // Key the subscription to the active org — getToken is referentially
  // stable in clerk-react 5, so without orgId in the deps an org
  // switch would leave the OLD org's motion stream connected and
  // toasting (with raw camera ids, since the cameras map is the new
  // org's).  Same rationale as useNotifications.
  const { organization } = useOrganization()
  const orgId = organization?.id || null
  const { showToast } = useToasts()
  const abortRef = useRef(null)
  // Keep cameras ref current so the SSE callback always sees the latest
  // map. Updating the ref inside an effect (rather than during render)
  // satisfies React's purity rules — refs aren't supposed to be mutated
  // during render. The SSE callback fires asynchronously after commit so
  // it always reads the post-commit value.
  const camerasRef = useRef(cameras)
  useEffect(() => {
    camerasRef.current = cameras
  }, [cameras])

  useEffect(() => {
    let cancelled = false
    let reconnectTimer = null
    let backoff = 5000
    const MAX_BACKOFF = 30000

    async function connect() {
      if (cancelled) return

      let token
      try {
        token = await getToken()
      } catch {
        // Not signed in yet — retry later
        reconnectTimer = setTimeout(connect, backoff)
        return
      }
      // Cleanup may have fired during the token await (StrictMode
      // double-mount, org switch, page nav) — abortRef pointed at the
      // PREVIOUS controller then, so without this check we'd open a
      // stream that nothing can abort (zombie: duplicate toasts, a
      // leaked slot against the per-org SSE cap).
      if (cancelled) return

      const controller = new AbortController()
      abortRef.current = controller

      try {
        const res = await fetch(`${API_URL}/api/motion/events/stream`, {
          headers: { Authorization: `Bearer ${token}` },
          signal: controller.signal,
        })
        if (cancelled) {
          try { res.body?.cancel() } catch { /* already closed */ }
          return
        }

        if (!res.ok) {
          reconnectTimer = setTimeout(connect, backoff)
          backoff = Math.min(backoff * 2, MAX_BACKOFF)
          return
        }

        // Connected — reset backoff
        backoff = 5000

        const reader = res.body.getReader()
        const decoder = new TextDecoder()
        let buffer = ""

        while (true) {
          const { done, value } = await reader.read()
          if (done || cancelled) break

          buffer += decoder.decode(value, { stream: true })
          const lines = buffer.split("\n")
          buffer = lines.pop() // keep incomplete line in buffer

          for (const line of lines) {
            if (!line.startsWith("data: ")) continue
            try {
              const event = JSON.parse(line.slice(6))
              if (event.type === "motion") {
                const cam = camerasRef.current[event.camera_id]
                const name = cam?.name || event.camera_id
                const score = event.score ?? 0
                showToast(
                  `Motion detected on "${name}" (${score}%)`,
                  "motion",
                  6000,
                )
              }
            } catch {
              // Ignore malformed lines
            }
          }
        }
      } catch (err) {
        if (err.name === "AbortError") return // intentional disconnect
      }

      // Stream ended or errored — reconnect with backoff
      if (!cancelled) {
        reconnectTimer = setTimeout(connect, backoff)
        backoff = Math.min(backoff * 2, MAX_BACKOFF)
      }
    }

    connect()

    return () => {
      cancelled = true
      clearTimeout(reconnectTimer)
      abortRef.current?.abort()
    }
    // orgId: tear down + reconnect the stream under the new org's token.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [getToken, showToast, orgId])
}
