import { useEffect, useState } from "react"
import { useAuth } from "@clerk/clerk-react"

import { createIncident, getCameras } from "../services/api"
import { useToasts } from "../hooks/useToasts.jsx"

const SEVERITY_OPTIONS = [
  { value: "low", label: "Low" },
  { value: "medium", label: "Medium" },
  { value: "high", label: "High" },
  { value: "critical", label: "Critical" },
]

/**
 * Operator-filed incident creation form.  Mirrors the MCP
 * `create_incident` tool's input shape so the resulting row renders
 * identically in the existing IncidentReportModal — title, summary,
 * severity (default medium), optional camera_id.
 *
 * On success: calls onCreated(incident) so the parent can refresh
 * the list AND open the just-created incident in the report modal
 * for follow-up edits (writing the long-form report, attaching
 * evidence later, etc).
 */
export default function NewIncidentModal({ onClose, onCreated }) {
  const { getToken } = useAuth()
  const { showToast } = useToasts()

  const [title, setTitle] = useState("")
  const [summary, setSummary] = useState("")
  const [severity, setSeverity] = useState("medium")
  const [cameraId, setCameraId] = useState("")
  const [cameras, setCameras] = useState([])
  const [submitting, setSubmitting] = useState(false)
  const [error, setError] = useState(null)

  // Load cameras for the dropdown.  Failing soft is fine — operator
  // can still file an incident with no camera attached, the field is
  // optional.
  useEffect(() => {
    const onKey = (e) => { if (e.key === "Escape" && !submitting) onClose() }
    window.addEventListener("keydown", onKey)
    return () => window.removeEventListener("keydown", onKey)
  }, [submitting, onClose])

  useEffect(() => {
    let cancelled = false
    getCameras(getToken)
      .then((data) => {
        if (!cancelled) setCameras(Array.isArray(data) ? data : [])
      })
      .catch(() => {
        if (!cancelled) setCameras([])
      })
    return () => {
      cancelled = true
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const submit = async (event) => {
    event.preventDefault()
    if (submitting) return

    const trimmedTitle = title.trim()
    const trimmedSummary = summary.trim()
    if (!trimmedTitle) {
      setError("Title is required.")
      return
    }
    if (!trimmedSummary) {
      setError("Summary is required.")
      return
    }

    setError(null)
    setSubmitting(true)
    try {
      const created = await createIncident(getToken, {
        title: trimmedTitle,
        summary: trimmedSummary,
        severity,
        camera_id: cameraId || null,
      })
      showToast("Incident filed", "success")
      onCreated(created)
    } catch (err) {
      setError(err?.message || "Failed to create incident")
    } finally {
      setSubmitting(false)
    }
  }

  return (
    <div className="modal-overlay" onClick={onClose}>
      <div
        className="modal-content new-incident-modal"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="modal-header">
          <h2>File a new incident</h2>
          <button className="modal-close" onClick={onClose} aria-label="Close">
            &times;
          </button>
        </div>

        <form className="modal-body" onSubmit={submit}>
          <div className="new-incident-field">
            <label htmlFor="new-incident-title">Title</label>
            <input
              id="new-incident-title"
              type="text"
              value={title}
              onChange={(e) => setTitle(e.target.value)}
              placeholder="Short headline (e.g. 'Front gate left open overnight')"
              maxLength={200}
              required
              autoFocus
            />
          </div>

          <div className="new-incident-field">
            <label htmlFor="new-incident-summary">Summary</label>
            <textarea
              id="new-incident-summary"
              value={summary}
              onChange={(e) => setSummary(e.target.value)}
              placeholder="One or two sentences describing what was observed."
              rows={4}
              required
            />
          </div>

          <div className="new-incident-row">
            <div className="new-incident-field">
              <label htmlFor="new-incident-severity">Severity</label>
              <select
                id="new-incident-severity"
                value={severity}
                onChange={(e) => setSeverity(e.target.value)}
              >
                {SEVERITY_OPTIONS.map((opt) => (
                  <option key={opt.value} value={opt.value}>
                    {opt.label}
                  </option>
                ))}
              </select>
            </div>

            <div className="new-incident-field">
              <label htmlFor="new-incident-camera">Camera (optional)</label>
              <select
                id="new-incident-camera"
                value={cameraId}
                onChange={(e) => setCameraId(e.target.value)}
              >
                <option value="">— None —</option>
                {cameras.map((c) => (
                  <option key={c.camera_id} value={c.camera_id}>
                    {c.name || c.camera_id}
                  </option>
                ))}
              </select>
            </div>
          </div>

          {error && <div className="new-incident-error">{error}</div>}

          <div className="modal-actions">
            <button
              type="button"
              className="btn btn-secondary"
              onClick={onClose}
              disabled={submitting}
            >
              Cancel
            </button>
            <button
              type="submit"
              className="btn btn-primary"
              disabled={submitting}
            >
              {submitting ? "Filing…" : "File incident"}
            </button>
          </div>
        </form>
      </div>
    </div>
  )
}
