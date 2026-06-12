import { useState, useEffect, useCallback, useRef } from "react"
import { Link } from "react-router-dom"
import { useAuth, useOrganization } from "@clerk/clerk-react"
import { getNode } from "../services/api"

// Shows a dismissible banner on the dashboard right after a new node has been
// created (via the Add Node modal on the Settings page). Polls the backend
// every 5s to detect the first heartbeat, then flashes a green "is live"
// celebration and auto-dismisses after 30s.
//
// State is persisted in localStorage under `os.recentlyCreatedNode.<orgId>`
// so it survives a navigation from /settings to /dashboard and a page reload.
// Entries older than RECENT_WINDOW_MS are treated as stale and ignored.

const RECENT_WINDOW_MS = 10 * 60 * 1000  // 10 min — banner disappears after this
const POLL_INTERVAL_MS = 5000
const SUCCESS_DISMISS_MS = 30_000
const STALLED_AFTER_MS = 2 * 60 * 1000  // flip "waiting" copy to "still waiting" after 2 min

function storageKey(orgId) {
  return `os.recentlyCreatedNode.${orgId}`
}

function readMarker(orgId) {
  if (!orgId) return null
  try {
    const raw = localStorage.getItem(storageKey(orgId))
    if (!raw) return null
    const parsed = JSON.parse(raw)
    if (!parsed?.node_id || !parsed?.created_at) return null
    // Stale — purge and ignore
    if (Date.now() - parsed.created_at > RECENT_WINDOW_MS) {
      localStorage.removeItem(storageKey(orgId))
      return null
    }
    return parsed
  } catch (_) {
    return null
  }
}

function clearMarker(orgId) {
  if (!orgId) return
  try { localStorage.removeItem(storageKey(orgId)) } catch (_) { /* ignore */ }
}

function HeartbeatBanner() {
  const { getToken } = useAuth()
  const { organization } = useOrganization()
  const orgId = organization?.id

  const [marker, setMarker] = useState(() => readMarker(orgId))
  const [node, setNode] = useState(null)
  const [live, setLive] = useState(false)
  const [dismissed, setDismissed] = useState(false)
  // Derived from wall-clock time inside the polling tick rather than during
  // render — keeps the component pure and lets ESLint react-hooks/purity
  // stay happy. The polling interval (5 s) is finer than the 2-min stalled
  // threshold so the flag flips within one tick of crossing.
  const [stalled, setStalled] = useState(false)
  const successTimeoutRef = useRef(null)

  // Re-read marker when org context changes — someone switching workspaces
  // shouldn't see the other workspace's banner.
  useEffect(() => {
    setMarker(readMarker(orgId))
    setNode(null)
    setLive(false)
    setDismissed(false)
    setStalled(false)
  }, [orgId])

  // Poll the backend until the node reports its first real heartbeat
  // (status transitions off "pending" and last_seen becomes non-null).
  // Same tick also drives the stalled flag.
  useEffect(() => {
    if (!marker || dismissed || live) return
    if (!orgId) return

    let cancelled = false

    const tick = async () => {
      // The 10-minute onboarding window is enforced HERE, not just at
      // marker-read time: once the marker was in state, nothing ever
      // re-checked expiry, so an installer that never ran kept this
      // banner polling getNode every 5s for the life of the tab.
      if (Date.now() - marker.created_at > RECENT_WINDOW_MS) {
        clearMarker(orgId)
        setMarker(null)
        return
      }
      // Skip fetches while hidden — the banner is invisible; the next
      // visible tick (≤5s away) catches up.
      if (document.hidden) return
      // Update the stalled flag first so even a failed fetch still flips
      // the banner copy at the right time.
      if (!cancelled) {
        setStalled(Date.now() - marker.created_at > STALLED_AFTER_MS)
      }
      try {
        const token = await getToken()
        const data = await getNode(() => Promise.resolve(token), marker.node_id)
        if (cancelled) return
        setNode(data)
        // First real heartbeat: effective status is no longer "pending" AND
        // we've seen at least one last_seen timestamp.
        if (data && data.status && data.status !== "pending" && data.last_seen) {
          setLive(true)
        }
      } catch (err) {
        // Node 404 → user deleted it before it heartbeated; clear marker.
        if (cancelled) return
        const msg = String(err?.message || "")
        if (msg.includes("404") || msg.toLowerCase().includes("not found")) {
          clearMarker(orgId)
          setMarker(null)
        }
        // Any other error: silently retry next tick. Polling shouldn't
        // spam the toast system.
      }
    }

    tick()
    const id = setInterval(tick, POLL_INTERVAL_MS)
    return () => { cancelled = true; clearInterval(id) }
  }, [marker, orgId, getToken, dismissed, live])

  // On success, auto-dismiss after SUCCESS_DISMISS_MS.
  useEffect(() => {
    if (!live) return
    clearMarker(orgId)  // belt & suspenders — clear once we've celebrated
    successTimeoutRef.current = setTimeout(() => {
      setDismissed(true)
    }, SUCCESS_DISMISS_MS)
    return () => {
      if (successTimeoutRef.current) clearTimeout(successTimeoutRef.current)
    }
  }, [live, orgId])

  const handleDismiss = useCallback(() => {
    clearMarker(orgId)
    setDismissed(true)
  }, [orgId])

  if (!marker || dismissed) return null

  const nodeName = node?.name || marker.name || marker.node_id

  if (live) {
    return (
      <div className="heartbeat-banner heartbeat-banner-success" role="status">
        <span className="heartbeat-banner-icon" aria-hidden="true">🎉</span>
        <div className="heartbeat-banner-body">
          <strong>{nodeName} is live!</strong>
          <span className="heartbeat-banner-subtext">
            Your node is heartbeating. Cameras will appear below as they start streaming.
          </span>
        </div>
        <button
          type="button"
          className="heartbeat-banner-close"
          onClick={handleDismiss}
          aria-label="Dismiss"
        >
          ×
        </button>
      </div>
    )
  }

  // While waiting, the success view above doesn't render — so `stalled`
  // here is meaningful (the live / dismissed paths short-circuit before
  // we reach this branch).
  const showStalled = !live && stalled

  return (
    <div
      className={`heartbeat-banner heartbeat-banner-waiting${showStalled ? " heartbeat-banner-stalled" : ""}`}
      role="status"
    >
      <div className="heartbeat-banner-spinner" aria-hidden="true">
        <div className="heartbeat-dot"></div>
        <div className="heartbeat-dot"></div>
        <div className="heartbeat-dot"></div>
      </div>
      <div className="heartbeat-banner-body">
        {showStalled ? (
          <>
            <strong>Still waiting for {nodeName}…</strong>
            <span className="heartbeat-banner-subtext">
              Haven&rsquo;t run the install command yet?{" "}
              <Link to="/settings" className="heartbeat-banner-link">
                View setup instructions
              </Link>
            </span>
          </>
        ) : (
          <>
            <strong>Waiting for {nodeName} to come online…</strong>
            <span className="heartbeat-banner-subtext">
              Run the install command on your device &mdash; we&rsquo;ll detect the first heartbeat automatically.
            </span>
          </>
        )}
      </div>
      <button
        type="button"
        className="heartbeat-banner-close"
        onClick={handleDismiss}
        aria-label="Dismiss"
      >
        ×
      </button>
    </div>
  )
}

export default HeartbeatBanner
