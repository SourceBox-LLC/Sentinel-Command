// Motion event history for the Admin dashboard.
//
// The backend has served GET /api/motion/events and /events/stats since
// motion ingestion shipped, and nothing in the SPA ever called them. The
// only motion surface was the live SSE toast in useMotionAlerts — so an
// operator could see motion happening *right now* but had no way to
// answer "what triggered overnight?", which for a security product is
// the question the product exists to answer.
//
// Mirrors the Stream Access tab's shape deliberately (filter row →
// summary → table → pager) so it reads as part of the same dashboard
// rather than a bolted-on view.

import { useState, useEffect, useCallback } from "react"
import { useAuth } from "../auth/index.jsx"
import { getMotionEvents, getMotionStats, getCameras } from "../services/api"

const PAGE_SIZE = 50

// Matches the backend's Query(le=168) ceiling — offering a window the
// API would reject is a worse experience than not offering it.
const WINDOWS = [
  { hours: 1, label: "Last hour" },
  { hours: 24, label: "Last 24 hours" },
  { hours: 72, label: "Last 3 days" },
  { hours: 168, label: "Last 7 days" },
]

function scoreClass(score) {
  if (score == null) return ""
  if (score >= 0.75) return "motion-score-high"
  if (score >= 0.4) return "motion-score-mid"
  return "motion-score-low"
}

function MotionEventsPanel() {
  const { getToken } = useAuth()
  const [events, setEvents] = useState([])
  const [stats, setStats] = useState(null)
  const [cameras, setCameras] = useState([])
  const [hours, setHours] = useState(24)
  const [cameraId, setCameraId] = useState("")
  const [offset, setOffset] = useState(0)
  const [total, setTotal] = useState(0)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState(null)

  // Camera list is fetched once — it populates the filter dropdown and
  // lets the table show names instead of raw ids.
  useEffect(() => {
    let cancelled = false
    getCameras(getToken)
      .then((d) => { if (!cancelled) setCameras(d?.cameras || d || []) })
      .catch(() => { /* filter degrades to ids; not worth surfacing */ })
    return () => { cancelled = true }
  }, [getToken])

  const load = useCallback(async () => {
    setLoading(true)
    setError(null)
    try {
      const [ev, st] = await Promise.all([
        getMotionEvents(getToken, {
          hours,
          limit: PAGE_SIZE,
          offset,
          camera_id: cameraId || null,
        }),
        getMotionStats(getToken, hours),
      ])
      setEvents(ev?.events || [])
      setTotal(ev?.total || 0)
      setStats(st)
    } catch (e) {
      setError(e?.message || "Could not load motion events.")
    } finally {
      setLoading(false)
    }
  }, [getToken, hours, offset, cameraId])

  useEffect(() => { load() }, [load])

  // Any filter change invalidates the current page — staying on offset
  // 300 of a narrower result set shows an empty table that looks broken.
  const changeWindow = (h) => { setHours(h); setOffset(0) }
  const changeCamera = (id) => { setCameraId(id); setOffset(0) }

  const cameraName = (id) => {
    const c = cameras.find((x) => String(x.camera_id) === String(id))
    return c?.name || id
  }

  const page = Math.floor(offset / PAGE_SIZE) + 1
  const pages = Math.max(1, Math.ceil(total / PAGE_SIZE))

  return (
    <div className="audit-section">
      <div className="audit-section-header">
        <div>
          <h2>Motion Events</h2>
          <p className="section-description">
            Every motion detection recorded by your cameras. Recordings stay on
            your CameraNode — this is the index of when something moved.
          </p>
        </div>
      </div>

      <div className="audit-filters">
        <div className="filter-group">
          <label htmlFor="motion-window">Window</label>
          <select
            id="motion-window"
            value={hours}
            onChange={(e) => changeWindow(Number(e.target.value))}
          >
            {WINDOWS.map((w) => (
              <option key={w.hours} value={w.hours}>{w.label}</option>
            ))}
          </select>
        </div>
        <div className="filter-group">
          <label htmlFor="motion-camera">Camera</label>
          <select
            id="motion-camera"
            value={cameraId}
            onChange={(e) => changeCamera(e.target.value)}
          >
            <option value="">All Cameras</option>
            {cameras.map((c) => (
              <option key={c.camera_id} value={c.camera_id}>
                {c.name || c.camera_id}
              </option>
            ))}
          </select>
        </div>
      </div>

      {stats?.cameras?.length > 0 && (
        <div className="motion-stats-strip">
          {stats.cameras
            .slice()
            .sort((a, b) => b.event_count - a.event_count)
            .slice(0, 4)
            .map((c) => (
              <div className="motion-stat-card" key={c.camera_id}>
                <div className="motion-stat-name">{cameraName(c.camera_id)}</div>
                <div className="motion-stat-count">
                  {c.event_count.toLocaleString()}
                </div>
                <div className="motion-stat-meta">
                  events · peak{" "}
                  {c.peak_score != null ? c.peak_score.toFixed(2) : "—"}
                </div>
              </div>
            ))}
        </div>
      )}

      {error && <div className="audit-error">{error}</div>}

      {loading ? (
        <div className="audit-empty">Loading motion events…</div>
      ) : events.length === 0 ? (
        <div className="audit-empty">
          <div className="audit-empty-icon" aria-hidden="true">🎞️</div>
          No motion events in this window.
        </div>
      ) : (
        <>
          <div className="audit-table-wrap">
            <table className="audit-table">
              <thead>
                <tr>
                  <th>Time</th>
                  <th>Camera</th>
                  <th>Score</th>
                  <th>Segment</th>
                </tr>
              </thead>
              <tbody>
                {events.map((e) => (
                  <tr key={e.id}>
                    <td>
                      {e.timestamp
                        ? new Date(e.timestamp + "Z").toLocaleString()
                        : "—"}
                    </td>
                    <td>{cameraName(e.camera_id)}</td>
                    <td>
                      <span className={scoreClass(e.score)}>
                        {e.score != null ? e.score.toFixed(2) : "—"}
                      </span>
                    </td>
                    <td className="audit-mono">
                      {e.segment_seq != null ? `#${e.segment_seq}` : "—"}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          {pages > 1 && (
            <div className="audit-pager">
              <button
                type="button"
                disabled={offset === 0}
                onClick={() => setOffset(Math.max(0, offset - PAGE_SIZE))}
              >
                ← Previous
              </button>
              <span>
                Page {page} of {pages} · {total.toLocaleString()} events
              </span>
              <button
                type="button"
                disabled={offset + PAGE_SIZE >= total}
                onClick={() => setOffset(offset + PAGE_SIZE)}
              >
                Next →
              </button>
            </div>
          )}
        </>
      )}
    </div>
  )
}

export default MotionEventsPanel
