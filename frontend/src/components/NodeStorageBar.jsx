/**
 * Per-node storage usage bar shown on the Settings → Camera Nodes
 * card.  Reads CameraNode v0.1.41+ heartbeat-reported storage stats
 * and renders ONE thing: how full the node's allocated cap is.
 *
 * Bar fills against `max_size_gb` (default 64 GB).  100% means the
 * retention loop is now actively deleting oldest segments to keep
 * usage at the cap — not a system error, just "node is at its
 * allocated limit, oldest recordings are being aged out."
 *
 * Host filesystem details (disk_free / disk_total) are deliberately
 * NOT shown here — operators care about node-allocated space, not
 * the underlying disk.  CameraNode still pauses recordings if the
 * host disk drops below 1 GiB free (safety floor in
 * storage::stats) but that's a self-protective server-side
 * behaviour, not something the dashboard needs to surface.
 *
 * Renders nothing if the node hasn't reported stats yet (older
 * CameraNode, brand-new install) — better than an empty 0% bar that
 * lies about the data we don't have.
 *
 * "Change" edits the cap remotely (POST /api/nodes/{id}/storage-cap,
 * CameraNode 0.1.79+). A Connected node's own dashboard listens only on
 * localhost, so for a headless machine this is the way to do it. Only
 * offered while the node is connected; the server explains anything
 * else (an older node, a cap larger than its disk).
 */

import { useState } from "react"
import { useAuth } from "../auth/index.jsx"
import { useToasts } from "../hooks/useToasts.jsx"
import { setNodeStorageCap } from "../services/api"

const GIB = 1024 * 1024 * 1024

function formatGb(bytes) {
  if (!bytes && bytes !== 0) return "—"
  if (bytes < GIB) {
    return `${(bytes / (1024 * 1024)).toFixed(0)} MB`
  }
  return `${(bytes / GIB).toFixed(1)} GB`
}

function CapEditor({ nodeId, used, currentGb, onDone, onCancel }) {
  const { getToken } = useAuth()
  const { showToast } = useToasts()
  const [value, setValue] = useState(String(currentGb || ""))
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState(null)

  const requested = Number(value)
  const valid = Number.isInteger(requested) && requested >= 1
  const deletes = valid && used > requested * GIB ? used - requested * GIB : 0

  const save = async (e) => {
    e.preventDefault()
    if (!valid) return
    setSaving(true)
    setError(null)
    try {
      const result = await setNodeStorageCap(getToken, nodeId, requested)
      const freed = result?.freed_bytes ?? 0
      showToast(
        freed > 0
          ? `Storage cap set to ${requested} GB. Deleted ${formatGb(freed)} of the oldest recordings.`
          : `Storage cap set to ${requested} GB.`,
        "success",
      )
      onDone()
    } catch (err) {
      setError(err.message || "The storage cap could not be changed.")
      setSaving(false)
    }
  }

  return (
    <form onSubmit={save} className="node-storage-cap-form" aria-label="Change storage cap">
      <label>
        <span>Keep up to</span>
        <input
          type="number"
          inputMode="numeric"
          min={1}
          step={1}
          value={value}
          onChange={(e) => setValue(e.target.value)}
          disabled={saving}
          autoFocus
        />
        <span>GB</span>
      </label>
      <button type="submit" className="btn btn-small btn-primary" disabled={saving || !valid || requested === currentGb}>
        {saving ? "Saving…" : "Save"}
      </button>
      <button type="button" className="btn btn-small btn-secondary" onClick={onCancel} disabled={saving}>
        Cancel
      </button>
      {deletes > 0 && (
        <p className="node-storage-cap-note">
          This node holds {formatGb(used)}. Saving deletes about {formatGb(deletes)} of its oldest
          recordings now.
        </p>
      )}
      {error && (
        <p className="node-storage-cap-error" role="alert">
          {error}
        </p>
      )}
    </form>
  )
}

function NodeStorageBar({ storage, nodeId, online = false, onChanged }) {
  const [editing, setEditing] = useState(false)
  if (!storage) return null

  const used = storage.used_bytes ?? 0
  const max = storage.max_bytes ?? 0

  // 100% means "at cap" — clamp so the bar doesn't overflow visually
  // when the writer sneaks past the cap between retention ticks.
  const rawPct = max > 0 ? (used / max) * 100 : 0
  const displayPct = Math.min(rawPct, 100)
  const overCap = rawPct > 100

  // Color band: green < 75, amber 75-90, red 90+.  At-cap (>=100)
  // is the retention loop doing its job, not a failure, but worth
  // surfacing visually so the operator sees "this node is full"
  // without having to read the number.
  const barClass = displayPct >= 90 ? "danger" : displayPct >= 75 ? "warn" : "ok"
  const fillColor =
    barClass === "danger"
      ? "var(--accent-red, #ef4444)"
      : barClass === "warn"
        ? "var(--accent-amber, #f59e0b)"
        : "var(--accent-green, #22c55e)"

  return (
    <div className="node-storage" style={{ marginTop: "0.6rem" }}>
      <div
        style={{
          display: "flex",
          justifyContent: "space-between",
          alignItems: "baseline",
          fontSize: "0.85rem",
          color: "var(--text-muted, #888)",
          marginBottom: "0.35rem",
        }}
      >
        <span>Storage</span>
        <span>
          {formatGb(used)} / {formatGb(max)}
          <span style={{ marginLeft: "0.5rem" }}>
            ({displayPct.toFixed(0)}%)
          </span>
          {nodeId && !editing && (
            <button
              type="button"
              className="node-storage-cap-link"
              onClick={() => setEditing(true)}
              disabled={!online}
              title={online ? "Change this node's storage cap" : "Connect the node to change its storage cap"}
            >
              Change
            </button>
          )}
        </span>
      </div>
      {/* Track + fill.  Track height bumped to 12px and given a
          visible 1px border so it reads as a real progress bar
          even at 0% used (the empty state is the most common one
          for fresh installs).  Without the border the empty track
          on dark background is invisible and operators wonder
          where the bar is. */}
      <div
        style={{
          height: "12px",
          width: "100%",
          background: "var(--bg-primary, #0a0a0a)",
          border: "1px solid var(--border, #2a2a2a)",
          borderRadius: "6px",
          overflow: "hidden",
        }}
      >
        <div
          style={{
            width: `${displayPct}%`,
            height: "100%",
            background: fillColor,
            transition: "width 0.3s ease, background 0.3s ease",
          }}
        />
      </div>
      {editing && (
        <CapEditor
          nodeId={nodeId}
          used={used}
          currentGb={max > 0 ? Math.round(max / GIB) : 0}
          onCancel={() => setEditing(false)}
          onDone={() => {
            setEditing(false)
            onChanged?.()
          }}
        />
      )}
      {overCap && (
        <p
          style={{
            fontSize: "0.78rem",
            color: "var(--text-muted, #888)",
            marginTop: "0.3rem",
            marginBottom: 0,
          }}
        >
          Over cap — retention is deleting oldest recordings to free space.
        </p>
      )}
    </div>
  )
}

export default NodeStorageBar
