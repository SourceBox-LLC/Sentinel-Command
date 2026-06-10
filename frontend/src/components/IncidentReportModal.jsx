import { useEffect, useMemo, useRef, useState } from "react"
// hls.js is dynamically imported inside the clip-playback effect (same
// pattern as HlsPlayer): a static import here pulled the 508 KB /
// 157 KB-gzip chunk into the IncidentsPage bundle, fetched and parsed
// on EVERY /incidents visit before paint — even when no clip evidence
// is ever opened.
import { useAuth } from "@clerk/clerk-react"
import {
  getIncident,
  patchIncident,
  deleteIncident,
  fetchIncidentEvidenceBlobUrl,
  incidentEvidencePlaylistUrl,
} from "../services/api"
import { useToasts } from "../hooks/useToasts.jsx"
import { useSharedToken } from "../hooks/useSharedToken.jsx"

const SEVERITY_LABELS = {
  low: "Low",
  medium: "Medium",
  high: "High",
  critical: "Critical",
}

const STATUS_LABELS = {
  open: "Open",
  acknowledged: "Acknowledged",
  resolved: "Resolved",
  dismissed: "Dismissed",
}

function formatAbsolute(iso) {
  if (!iso) return "—"
  try {
    return new Date(iso).toLocaleString()
  } catch {
    return iso
  }
}

function formatRelative(iso) {
  if (!iso) return ""
  const ts = new Date(iso).getTime()
  const diffSec = Math.max(0, Math.floor((Date.now() - ts) / 1000))
  if (diffSec < 60) return "just now"
  if (diffSec < 3600) return `${Math.floor(diffSec / 60)}m ago`
  if (diffSec < 86400) return `${Math.floor(diffSec / 3600)}h ago`
  return `${Math.floor(diffSec / 86400)}d ago`
}

// Tiny markdown renderer — handles headings, bold, italic, ordered + unordered
// lists (with nesting), code, paragraphs.
// Deliberately minimal: no external dep, no HTML injection (we escape first).
function renderMarkdown(md) {
  if (!md) return null
  const escaped = md
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")

  const lines = escaped.split("\n")
  const blocks = []
  let para = []
  // Stack of list blocks being built. Each frame:
  //   { type: "list", ordered: bool, indent: number, items: [{ text, children }] }
  // listStack[0] is the root; deeper entries are nested inside the previous
  // entry's last item.
  let listStack = []
  let codeBlock = null

  const flushPara = () => {
    if (para.length) {
      blocks.push({ type: "p", content: para.join(" ") })
      para = []
    }
  }
  const flushListStack = () => {
    if (listStack.length === 0) return
    while (listStack.length > 1) {
      const top = listStack.pop()
      const parent = listStack[listStack.length - 1]
      parent.items[parent.items.length - 1].children = top
    }
    blocks.push(listStack.pop())
  }

  for (const raw of lines) {
    // Preserve leading whitespace for list indent detection, strip trailing only.
    const line = raw.replace(/\s+$/, "")

    // Code fence
    if (line.trimStart().startsWith("```")) {
      flushPara(); flushListStack()
      if (codeBlock === null) {
        codeBlock = []
      } else {
        blocks.push({ type: "code", content: codeBlock.join("\n") })
        codeBlock = null
      }
      continue
    }
    if (codeBlock !== null) {
      codeBlock.push(line)
      continue
    }

    // Headings
    const h = line.match(/^(#{1,4})\s+(.*)$/)
    if (h) {
      flushPara(); flushListStack()
      blocks.push({ type: "h", level: h[1].length, content: h[2] })
      continue
    }

    // List items: `  - foo`, `* foo`, `1. foo`, etc.
    const li = line.match(/^(\s*)([-*]|\d+\.)\s+(.*)$/)
    if (li) {
      flushPara()
      const indent = li[1].length
      const ordered = /^\d+\./.test(li[2])
      const text = li[3]
      const newItem = { text, children: null }

      if (listStack.length === 0) {
        listStack.push({ type: "list", ordered, indent, items: [newItem] })
      } else {
        const top = listStack[listStack.length - 1]
        if (indent > top.indent) {
          // Nested — start a new list inside the current top's last item.
          listStack.push({ type: "list", ordered, indent, items: [newItem] })
        } else {
          // Pop back until we find a frame with indent <= this one.
          while (
            listStack.length > 1 &&
            listStack[listStack.length - 1].indent > indent
          ) {
            const popped = listStack.pop()
            const parent = listStack[listStack.length - 1]
            parent.items[parent.items.length - 1].children = popped
          }
          const nowTop = listStack[listStack.length - 1]
          if (nowTop.indent === indent) {
            // If marker style changed at the same level, flush and start fresh
            // so a numbered list doesn't accidentally merge into an unordered one.
            if (nowTop.ordered !== ordered) {
              flushListStack()
              listStack.push({ type: "list", ordered, indent, items: [newItem] })
            } else {
              nowTop.items.push(newItem)
            }
          } else {
            // Fell through to a shallower indent that doesn't match any frame.
            flushListStack()
            listStack.push({ type: "list", ordered, indent, items: [newItem] })
          }
        }
      }
      continue
    }

    // Blank line
    if (!line.trim()) {
      flushPara(); flushListStack()
      continue
    }

    // Paragraph
    flushListStack()
    para.push(line)
  }
  flushPara(); flushListStack()
  if (codeBlock !== null) {
    blocks.push({ type: "code", content: codeBlock.join("\n") })
  }

  // Inline formatting: **bold**, *italic*, `code`
  const inline = (s) =>
    s
      .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
      .replace(/(^|\W)\*([^*\n]+)\*(\W|$)/g, "$1<em>$2</em>$3")
      .replace(/`([^`]+)`/g, "<code>$1</code>")

  const renderList = (block, key) => {
    const Tag = block.ordered ? "ol" : "ul"
    return (
      <Tag key={key}>
        {block.items.map((it, j) => (
          <li key={j}>
            <span dangerouslySetInnerHTML={{ __html: inline(it.text) }} />
            {it.children && renderList(it.children, `${key}-${j}`)}
          </li>
        ))}
      </Tag>
    )
  }

  return blocks.map((b, i) => {
    if (b.type === "h") {
      const Tag = `h${Math.min(6, 2 + b.level)}`
      return <Tag key={i} dangerouslySetInnerHTML={{ __html: inline(b.content) }} />
    }
    if (b.type === "list") {
      return renderList(b, i)
    }
    if (b.type === "code") {
      return <pre key={i}><code>{b.content}</code></pre>
    }
    return <p key={i} dangerouslySetInnerHTML={{ __html: inline(b.content) }} />
  })
}

function EvidenceImage({ incidentId, evidenceId, caption, getToken, onClick }) {
  const [src, setSrc] = useState(null)
  const [error, setError] = useState(false)

  useEffect(() => {
    let cancelled = false
    let url = null
    fetchIncidentEvidenceBlobUrl(getToken, incidentId, evidenceId)
      .then((blobUrl) => {
        if (cancelled) {
          URL.revokeObjectURL(blobUrl)
          return
        }
        url = blobUrl
        setSrc(blobUrl)
      })
      .catch(() => {
        if (!cancelled) setError(true)
      })
    return () => {
      cancelled = true
      if (url) URL.revokeObjectURL(url)
    }
  }, [incidentId, evidenceId, getToken])

  if (error) {
    return <div className="incident-evidence-error">Failed to load snapshot</div>
  }
  if (!src) {
    return <div className="incident-evidence-loading" />
  }
  return (
    <button
      type="button"
      className="incident-evidence-thumb"
      onClick={() => onClick(src, caption)}
      title={caption || "Click to enlarge"}
    >
      <img src={src} alt={caption || "Snapshot evidence"} />
      {caption && <span className="incident-evidence-caption">{caption}</span>}
    </button>
  )
}

function EvidenceVideo({ incidentId, evidenceId, caption }) {
  const videoRef = useRef(null)
  const hlsRef = useRef(null)
  const [error, setError] = useState(null)
  const { getCurrentToken, ready } = useSharedToken()

  useEffect(() => {
    const video = videoRef.current
    if (!video) return
    // Wait for the shared Clerk token before mounting hls.js — mirrors
    // HlsPlayer so the first playlist/segment XHR never goes out with
    // a null Authorization header.
    if (!ready) return undefined

    let cancelled = false
    const playlistUrl = incidentEvidencePlaylistUrl(incidentId, evidenceId)
    const ownOrigin = (import.meta.env.VITE_API_URL || "") || window.location.origin

    const setupClip = async () => {
      // Lazy-load hls.js — same pattern (and same cached chunk) as
      // HlsPlayer, so opening a clip after visiting the dashboard is
      // instant, and /incidents itself never pays the 157 KB gzip.
      const { default: Hls } = await import("hls.js")
      if (cancelled) return

      if (!Hls.isSupported()) {
        setError("HLS is not supported in this browser")
        return
      }

      const hls = new Hls({
        // Clip is a single short segment — VOD playback, no live tuning needed.
        xhrSetup: (xhr, url) => {
          const token = getCurrentToken()
          // hls.js may resolve segment URIs inside the playlist to
          // absolute URLs (matching ownOrigin) or hand us the relative
          // form verbatim.  Accept both; skip third-party origins.
          if (token && (url.startsWith(ownOrigin) || url.startsWith("/"))) {
            xhr.setRequestHeader("Authorization", `Bearer ${token}`)
            xhr.setRequestHeader("Cache-Control", "no-cache")
          }
        },
        // VOD-friendly buffering for very short clips
        maxBufferLength: 60,
        maxMaxBufferLength: 60,
      })
      hlsRef.current = hls

      hls.loadSource(playlistUrl)
      hls.attachMedia(video)

      hls.on(Hls.Events.ERROR, (_evt, data) => {
        if (cancelled) return
        if (data.fatal) {
          setError(`Playback error: ${data.details || data.type}`)
          try { hls.destroy() } catch { /* noop */ }
        }
      })
    }

    setupClip().catch((err) => {
      if (!cancelled) setError(err?.message || "Failed to load player")
    })

    return () => {
      cancelled = true
      if (hlsRef.current) {
        try { hlsRef.current.destroy() } catch { /* noop */ }
        hlsRef.current = null
      }
    }
  }, [incidentId, evidenceId, getCurrentToken, ready])

  return (
    <div className="incident-evidence-clip">
      {error ? (
        <div className="incident-evidence-error">{error}</div>
      ) : (
        <video
          ref={videoRef}
          className="incident-evidence-clip-video"
          controls
          playsInline
          muted
        />
      )}
      {caption && <div className="incident-evidence-caption">{caption}</div>}
    </div>
  )
}

function IncidentReportModal({ incidentId, onClose, onUpdated, onDeleted }) {
  const { getToken } = useAuth()
  const { showToast } = useToasts()
  const [incident, setIncident] = useState(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState(null)
  const [acting, setActing] = useState(null)
  const [lightbox, setLightbox] = useState(null)
  // Edit-mode state.  When `editing` is true, the header severity
  // badge becomes a dropdown, the Summary <p> becomes a textarea,
  // the Full Report markdown becomes a larger textarea, and the
  // action row swaps the status buttons for Save / Cancel.  We
  // keep an `editFields` snapshot so Cancel can revert without a
  // re-fetch.
  const [editing, setEditing] = useState(false)
  const [editFields, setEditFields] = useState({
    severity: "",
    summary: "",
    report: "",
  })

  // Load detail
  useEffect(() => {
    if (!incidentId) return
    let cancelled = false
    setLoading(true)
    setError(null)
    getIncident(getToken, incidentId)
      .then((data) => {
        if (!cancelled) {
          setIncident(data)
          setLoading(false)
        }
      })
      .catch((err) => {
        if (!cancelled) {
          setError(err.message || "Failed to load incident")
          setLoading(false)
        }
      })
    return () => {
      cancelled = true
    }
  }, [incidentId, getToken])

  // ESC key to close
  useEffect(() => {
    const onKey = (e) => {
      if (e.key === "Escape") {
        if (lightbox) setLightbox(null)
        else onClose()
      }
    }
    window.addEventListener("keydown", onKey)
    return () => window.removeEventListener("keydown", onKey)
  }, [onClose, lightbox])

  const handleStatus = async (newStatus) => {
    setActing(newStatus)
    try {
      const updated = await patchIncident(getToken, incidentId, { status: newStatus })
      setIncident(updated)
      showToast(`Incident ${STATUS_LABELS[newStatus].toLowerCase()}`, "success")
      if (onUpdated) onUpdated()
    } catch (err) {
      showToast(err.message || "Failed to update incident", "error")
    } finally {
      setActing(null)
    }
  }

  const handleEditStart = () => {
    if (!incident) return
    setEditFields({
      severity: incident.severity || "medium",
      summary: incident.summary || "",
      report: incident.report || "",
    })
    setEditing(true)
  }

  const handleEditCancel = () => {
    setEditing(false)
    // editFields stays in state but isn't read again until next edit
  }

  const handleEditSave = async () => {
    if (!incident) return
    // Send only the fields that actually changed — avoids overwriting
    // server-side updates we didn't know about (motion-cooldown patch
    // race etc.) and keeps the audit log clean.
    const patch = {}
    if (editFields.severity && editFields.severity !== incident.severity) {
      patch.severity = editFields.severity
    }
    if (editFields.summary !== (incident.summary || "")) {
      patch.summary = editFields.summary
    }
    if (editFields.report !== (incident.report || "")) {
      patch.report = editFields.report
    }
    if (Object.keys(patch).length === 0) {
      // No changes — just exit edit mode silently.
      setEditing(false)
      return
    }
    setActing("save")
    try {
      const updated = await patchIncident(getToken, incidentId, patch)
      setIncident(updated)
      setEditing(false)
      showToast("Incident updated", "success")
      if (onUpdated) onUpdated()
    } catch (err) {
      showToast(err.message || "Failed to update incident", "error")
    } finally {
      setActing(null)
    }
  }

  const handleDelete = async () => {
    // Native confirm — destructive op on a single record, real
    // confirmation prompt is the right friction.  Includes the
    // incident title so the operator can verify they're deleting
    // what they think they are.
    const confirmed = window.confirm(
      `Permanently delete this incident report?\n\n` +
      `"${incident?.title || `Incident #${incidentId}`}"\n\n` +
      `This deletes the report, all attached snapshots, video clips, ` +
      `and observations. The action cannot be undone.`,
    )
    if (!confirmed) return
    setActing("delete")
    try {
      await deleteIncident(getToken, incidentId)
      showToast("Incident deleted", "success")
      // Caller decides what to do on delete — IncidentsPage uses this
      // to close the modal AND drop the row from its in-memory list +
      // decrement the open/total counts.  We deliberately don't call
      // onClose() here because the parent's onDeleted handler does it.
      if (onDeleted) onDeleted(incidentId)
      else onClose()
    } catch (err) {
      showToast(err.message || "Failed to delete incident", "error")
      setActing(null)
    }
  }

  const snapshots = useMemo(
    () => (incident?.evidence || []).filter((e) => e.kind === "snapshot" && e.has_data),
    [incident]
  )
  const clips = useMemo(
    () => (incident?.evidence || []).filter((e) => e.kind === "clip" && e.has_data),
    [incident]
  )
  const observations = useMemo(
    () => (incident?.evidence || []).filter((e) => e.kind === "observation"),
    [incident]
  )

  // Combined timeline of all evidence in chronological order
  const timeline = useMemo(() => incident?.evidence || [], [incident])

  return (
    <div className="modal-overlay" onClick={onClose}>
      <div
        className="modal-content incident-modal"
        onClick={(e) => e.stopPropagation()}
      >
        {loading ? (
          <div className="incident-modal-loading">
            <div className="loading-spinner" />
            <p>Loading incident…</p>
          </div>
        ) : error ? (
          <div className="modal-body">
            <div className="error-message">{error}</div>
            <div className="modal-actions">
              <button className="btn btn-secondary" onClick={onClose}>Close</button>
            </div>
          </div>
        ) : incident ? (
          <>
            <div className="modal-header incident-modal-header">
              <div className="incident-modal-title">
                {editing ? (
                  <select
                    className={`incident-severity-edit incident-severity-${editFields.severity}`}
                    value={editFields.severity}
                    onChange={(e) =>
                      setEditFields((f) => ({ ...f, severity: e.target.value }))
                    }
                    disabled={acting !== null}
                    aria-label="Severity"
                  >
                    <option value="low">Low</option>
                    <option value="medium">Medium</option>
                    <option value="high">High</option>
                    <option value="critical">Critical</option>
                  </select>
                ) : (
                  <span className={`incident-severity-badge incident-severity-${incident.severity}`}>
                    {SEVERITY_LABELS[incident.severity] || incident.severity}
                  </span>
                )}
                <h2>{incident.title}</h2>
              </div>
              <button className="modal-close" onClick={onClose}>&times;</button>
            </div>

            <div className="modal-body">
              <div className="incident-modal-meta">
                {incident.camera_id && (
                  <span><strong>Camera:</strong> <code>{incident.camera_id}</code></span>
                )}
                <span><strong>Reported by:</strong> {incident.created_by}</span>
                <span><strong>Created:</strong> {formatAbsolute(incident.created_at)} ({formatRelative(incident.created_at)})</span>
                <span>
                  <strong>Status:</strong>{" "}
                  <span className={`incident-status-badge incident-status-${incident.status}`}>
                    {STATUS_LABELS[incident.status] || incident.status}
                  </span>
                </span>
                {incident.resolved_at && (
                  <span><strong>Resolved:</strong> {formatAbsolute(incident.resolved_at)} by {incident.resolved_by}</span>
                )}
              </div>

              <section className="incident-section">
                <h3 className="incident-section-title">Summary</h3>
                {editing ? (
                  <textarea
                    className="incident-edit-textarea incident-edit-summary"
                    value={editFields.summary}
                    onChange={(e) =>
                      setEditFields((f) => ({ ...f, summary: e.target.value }))
                    }
                    rows={3}
                    placeholder="One or two sentences — what was observed."
                    disabled={acting !== null}
                  />
                ) : (
                  <p className="incident-summary-text">{incident.summary}</p>
                )}
              </section>

              {(incident.report || editing) && (
                <section className="incident-section">
                  <h3 className="incident-section-title">Full Report</h3>
                  {editing ? (
                    <textarea
                      className="incident-edit-textarea incident-edit-report"
                      value={editFields.report}
                      onChange={(e) =>
                        setEditFields((f) => ({ ...f, report: e.target.value }))
                      }
                      rows={12}
                      placeholder="Long-form markdown report — observations, evidence walk-through, conclusions, recommended actions. Markdown formatting (## headings, **bold**, lists) renders in view mode."
                      disabled={acting !== null}
                    />
                  ) : (
                    <div className="incident-report-body">
                      {renderMarkdown(incident.report)}
                    </div>
                  )}
                </section>
              )}

              {snapshots.length > 0 && (
                <section className="incident-section">
                  <h3 className="incident-section-title">Evidence ({snapshots.length})</h3>
                  <div className="incident-evidence-grid">
                    {snapshots.map((e) => (
                      <EvidenceImage
                        key={e.id}
                        incidentId={incident.id}
                        evidenceId={e.id}
                        caption={e.text || e.camera_id}
                        getToken={getToken}
                        onClick={(src, caption) => setLightbox({ src, caption })}
                      />
                    ))}
                  </div>
                </section>
              )}

              {clips.length > 0 && (
                <section className="incident-section">
                  <h3 className="incident-section-title">Clips ({clips.length})</h3>
                  <div className="incident-evidence-clip-grid">
                    {clips.map((e) => (
                      <EvidenceVideo
                        key={e.id}
                        incidentId={incident.id}
                        evidenceId={e.id}
                        caption={e.text || e.camera_id}
                      />
                    ))}
                  </div>
                </section>
              )}

              {observations.length > 0 && (
                <section className="incident-section">
                  <h3 className="incident-section-title">Observations</h3>
                  <ul className="incident-observation-list">
                    {observations.map((e) => (
                      <li key={e.id}>
                        {e.camera_id && <code className="incident-obs-cam">{e.camera_id}</code>}
                        <span>{e.text}</span>
                        <span className="incident-obs-time">{formatRelative(e.timestamp)}</span>
                      </li>
                    ))}
                  </ul>
                </section>
              )}

              {timeline.length > 0 && (
                <section className="incident-section">
                  <h3 className="incident-section-title">Timeline</h3>
                  <ul className="incident-timeline">
                    {timeline.map((e) => (
                      <li key={e.id}>
                        <span className="incident-timeline-dot" />
                        <span className="incident-timeline-time">{formatAbsolute(e.timestamp)}</span>
                        <span className="incident-timeline-kind">{e.kind}</span>
                        <span className="incident-timeline-text">
                          {e.kind === "snapshot"
                            ? `snapshot of ${e.camera_id}${e.text ? ` — ${e.text}` : ""}`
                            : e.kind === "clip"
                              ? `clip from ${e.camera_id}${e.text ? ` — ${e.text}` : ""}`
                              : e.text}
                        </span>
                      </li>
                    ))}
                  </ul>
                </section>
              )}
            </div>

            <div className="modal-actions incident-modal-actions">
              {editing ? (
                /* Edit-mode actions: Cancel reverts the local field
                   snapshot without a re-fetch; Save sends only the
                   diff via patchIncident.  Lifecycle-status buttons
                   and Delete are hidden in edit mode to keep the
                   action surface unambiguous. */
                <>
                  <button
                    className="btn btn-secondary"
                    onClick={handleEditCancel}
                    disabled={acting !== null}
                  >
                    Cancel
                  </button>
                  <button
                    className="btn btn-primary incident-modal-save-btn"
                    onClick={handleEditSave}
                    disabled={acting !== null}
                  >
                    {acting === "save" ? "Saving…" : "Save changes"}
                  </button>
                </>
              ) : (
                <>
                  {incident.status !== "dismissed" && (
                    <button
                      className="btn btn-secondary"
                      onClick={() => handleStatus("dismissed")}
                      disabled={acting !== null}
                    >
                      {acting === "dismissed" ? "Dismissing…" : "Dismiss"}
                    </button>
                  )}
                  {incident.status === "open" && (
                    <button
                      className="btn btn-secondary"
                      onClick={() => handleStatus("acknowledged")}
                      disabled={acting !== null}
                    >
                      {acting === "acknowledged" ? "Acknowledging…" : "Acknowledge"}
                    </button>
                  )}
                  {incident.status !== "resolved" && (
                    <button
                      className="btn btn-primary"
                      onClick={() => handleStatus("resolved")}
                      disabled={acting !== null}
                    >
                      {acting === "resolved" ? "Resolving…" : "Mark Resolved"}
                    </button>
                  )}
                  {(incident.status === "resolved" || incident.status === "dismissed") && (
                    <button
                      className="btn btn-secondary"
                      onClick={() => handleStatus("open")}
                      disabled={acting !== null}
                    >
                      {acting === "open" ? "Reopening…" : "Reopen"}
                    </button>
                  )}
                  {/* Edit + Delete sit on the right side, visually
                      separated from the lifecycle-status buttons. */}
                  <button
                    className="btn btn-secondary incident-modal-edit-btn"
                    onClick={handleEditStart}
                    disabled={acting !== null}
                    title="Edit severity, summary, or report body"
                  >
                    Edit
                  </button>
                  <button
                    className="btn btn-danger"
                    onClick={handleDelete}
                    disabled={acting !== null}
                    title="Permanently delete this incident and all evidence"
                  >
                    {acting === "delete" ? "Deleting…" : "Delete"}
                  </button>
                </>
              )}
            </div>
          </>
        ) : null}

        {lightbox && (
          <div
            className="incident-lightbox"
            onClick={() => setLightbox(null)}
          >
            <img src={lightbox.src} alt={lightbox.caption || "Snapshot"} />
            {lightbox.caption && (
              <div className="incident-lightbox-caption">{lightbox.caption}</div>
            )}
          </div>
        )}
      </div>
    </div>
  )
}

export default IncidentReportModal
