import { useState, useCallback, useEffect, useRef, memo } from "react"
import { useAuth } from "@clerk/clerk-react"
import { requestSnapshot, setRecording } from "../services/api"
import { useToasts } from "../hooks/useToasts.jsx"
import HlsPlayer from "./HlsPlayer.jsx"

function CameraCard({
  cameraId,
  camera,
  group,
  onRequestUpgrade,
}) {
  const { getToken } = useAuth()
  const { showToast } = useToasts()
  const [snapshotLoading, setSnapshotLoading] = useState(false)
  const [snapshotMsg, setSnapshotMsg] = useState(null)
  // Initialized from the server's view, not a blind `false`: after a
  // refresh — or when policy/another admin started recording — the
  // status badge said "recording" while this button still read
  // "Record" (and clicking sent recording:true to an already-recording
  // camera).
  const [recording, setRecordingState] = useState(camera.status === "recording")
  const [recordLoading, setRecordLoading] = useState(false)

  // Resync from the server when ITS value changes (5s poll), same
  // last-server-value ref pattern as CameraRecordingControls: only a
  // genuine status transition overwrites the local optimistic state,
  // so our own just-clicked toggle isn't clobbered by a stale poll.
  const serverRecording = camera.status === "recording"
  const lastServerRecordingRef = useRef(serverRecording)
  useEffect(() => {
    if (serverRecording !== lastServerRecordingRef.current) {
      lastServerRecordingRef.current = serverRecording
      setRecordingState(serverRecording)
    }
  }, [serverRecording])

  const takeSnapshot = useCallback(async () => {
    setSnapshotLoading(true)
    setSnapshotMsg(null)
    try {
      await requestSnapshot(getToken, cameraId)
      setSnapshotMsg("Saved to node")
      showToast("Snapshot saved to node", "success")
    } catch (err) {
      setSnapshotMsg(err.message || "Snapshot failed")
      showToast(err.message || "Snapshot failed", "error")
    } finally {
      setSnapshotLoading(false)
      setTimeout(() => setSnapshotMsg(null), 3000)
    }
  }, [cameraId, getToken, showToast])

  const toggleRecording = useCallback(async () => {
    setRecordLoading(true)
    try {
      await setRecording(getToken, cameraId, !recording)
      setRecordingState(!recording)
      showToast(!recording ? "Recording started" : "Recording stopped", !recording ? "info" : "success")
    } catch (err) {
      console.error("Recording toggle failed:", err)
      showToast(err.message || "Recording toggle failed", "error")
    } finally {
      setRecordLoading(false)
    }
  }, [cameraId, getToken, recording, showToast])

  // "failed" (supervisor gave up) and "error" mean the pipeline is
  // producing nothing — treat them as offline for playback purposes.
  // "restarting" and "starting" are transient; the HLS player will just
  // show its buffering state until segments arrive.
  const status = camera.status
  // Plan-cap suspension takes precedence over every other state: the
  // backend is 402-ing every push, so even if FFmpeg is running locally
  // the viewer has no stream to watch. Render the card as "down" and
  // swap the normal offline copy for an upgrade CTA.
  const isSuspendedByPlan = Boolean(camera.disabled_by_plan)
  const isDown = isSuspendedByPlan || status === "offline" || status === "failed" || status === "error"
  const isTransient = !isSuspendedByPlan && (status === "starting" || status === "restarting")

  const nodeTypeLabel = camera.node_type || "Camera"
  const nodeTypeIcon = (
    <svg
      width="15"
      height="15"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.8"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <path d="M23 7l-7 5 7 5V7z" />
      <rect x="1" y="5" width="15" height="14" rx="2" ry="2" />
    </svg>
  )

  const statusClass = isSuspendedByPlan
    ? "suspended"
    : status === "online"     ? "online" :
      status === "streaming"  ? "streaming" :
      status === "recording"  ? "recording" :
      status === "starting"   ? "starting" :
      status === "restarting" ? "restarting" :
      // "idle"/"discovered" fell through to the red offline style even
      // though index.css ships amber/blue badge styles for them — a
      // healthy-but-not-streaming camera read as an outage.
      status === "idle"       ? "idle" :
      status === "discovered" ? "discovered" :
      status === "failed"     ? "failed" :
      status === "error"      ? "error" : "offline"

  const statusText = isSuspendedByPlan ? "suspended" : (status || "unknown")

  // Down-state messages used inside the feed placeholder. "failed" and
  // "error" render the supervisor's last_error if we have it so the user
  // isn't left guessing why the camera went dark. A plan-suspended camera
  // gets its own dedicated copy so the operator knows exactly why the
  // feed is dark — it isn't broken, it's just outside their plan.
  const downLabel = isSuspendedByPlan
    ? "Suspended — Plan Limit"
    : status === "failed" ? "Pipeline Failed" :
      status === "error"  ? "Pipeline Error"  : "Camera Offline"
  const downDetail = isSuspendedByPlan
    ? "Upgrade your plan to resume streaming."
    : (status === "failed" || status === "error") && camera.last_error
      ? camera.last_error
      : null

  // Tooltip content for the status badge. Shows the reason inline so the
  // user doesn't have to hover into the feed to see what's wrong.
  const badgeTitle = isSuspendedByPlan
    ? "Suspended by plan limit — upgrade to resume streaming"
    : camera.last_error
      ? `${status}: ${camera.last_error}`
      : status || "unknown"

  const cardClasses = `camera-card ${isDown ? "offline" : ""}`

  // Camera Groups Phase 3: a thin colored stripe at the top of the
  // card encodes the group color so a 20-camera grid reads at a
  // glance, plus a small group pill in the header so the name is
  // visible.  Both no-op when the camera isn't grouped.
  const stripeStyle = group?.color
    ? { borderTop: `3px solid ${group.color}` }
    : undefined

  return (
    <div className={cardClasses} style={stripeStyle}>
      <div className="camera-header">
        <div className="camera-info">
          <div className="camera-icon">{nodeTypeIcon}</div>
          <div className="camera-details">
            <h3>
              {camera.name || `Camera ${cameraId.slice(-4)}`}
              {group && (
                <span
                  className="camera-group-pill"
                  style={{ "--group-color": group.color }}
                  title={`Group: ${group.name}`}
                >
                  {group.icon ? `${group.icon} ` : ""}
                  {group.name}
                </span>
              )}
            </h3>
            {/*
                Node association line.  The camera name itself is
                auto-generated from the USB descriptor and is often
                duplicated across cameras ("USB Webcam" × N), so the
                operator-chosen node name is what tells the user
                which physical box this camera lives on.  Only render
                when we actually have a node_name — an orphaned
                camera (mid-delete) just shows the tech line below.
            */}
            {camera.node_name && (
              <span className="camera-node">
                <span className="camera-node-prefix">on</span>
                {camera.node_name}
              </span>
            )}
            <span className="camera-tech">
              {cameraId}
              <span className="camera-tech-sep"> · </span>
              <span className="camera-tech-type">{nodeTypeLabel}</span>
            </span>
          </div>
        </div>
        <div className={`status-badge ${statusClass}`} title={badgeTitle}>
          <span className="dot"></span>
          <span className="status-text">{statusText}</span>
        </div>
      </div>

      <div className="camera-feed-container">
        {isDown ? (
          <div className={`feed-loading error${isSuspendedByPlan ? " suspended" : ""}`}>
            <span className="status-icon">{isSuspendedByPlan ? "🔒" : "⚠️"}</span>
            <span>{downLabel}</span>
            {downDetail && <span className="feed-detail">{downDetail}</span>}
            {isSuspendedByPlan && onRequestUpgrade && (
              <button
                type="button"
                className="btn btn-primary btn-small"
                onClick={onRequestUpgrade}
                style={{ marginTop: "0.5rem" }}
              >
                Upgrade plan
              </button>
            )}
          </div>
        ) : (
          <HlsPlayer
            cameraId={cameraId}
            cameraName={camera.name || `Camera ${cameraId.slice(-4)}`}
            status={status}
          />
        )}
        {isTransient && (
          <div className="camera-feed-overlay-banner">
            {status === "restarting"
              ? `Reconnecting${camera.last_error ? ` — ${camera.last_error}` : "…"}`
              : "Starting up…"}
          </div>
        )}
      </div>

      <div className="camera-controls">
        <button
          className={`btn btn-record${recording ? " recording" : ""}`}
          onClick={toggleRecording}
          disabled={recordLoading}
          title={recording ? "Stop recording" : "Start recording on node"}
        >
          <span className={`record-dot${recording ? " active" : ""}`} />
          {recordLoading ? "..." : recording ? "Recording" : "Record"}
        </button>
        <button
          className="btn btn-snapshot"
          onClick={takeSnapshot}
          disabled={snapshotLoading}
          title="Take Snapshot (saved on camera node)"
        >
          <svg width="14" height="14" viewBox="0 0 24 24" fill="currentColor">
            <circle cx="12" cy="12" r="3.2"/>
            <path d="M9 2L7.17 4H4c-1.1 0-2 .9-2 2v12c0 1.1.9 2 2 2h16c0 1.1-.9-2-2V6c0-1.1-.9-2-2-2h-3.17L15 2H9zm3 15c-2.76 0-5-2.24-5-5s2.24-5 5-5 5 2.24 5 5-2.24 5-5 5z"/>
          </svg>
          {snapshotLoading ? "Capturing…" : snapshotMsg || "Snapshot"}
        </button>
      </div>

    </div>
  )
}

export default memo(CameraCard, (prevProps, nextProps) => {
  return (
    prevProps.cameraId === nextProps.cameraId &&
    prevProps.camera.status === nextProps.camera.status &&
    prevProps.camera.name === nextProps.camera.name &&
    prevProps.camera.last_error === nextProps.camera.last_error &&
    prevProps.camera.disabled_by_plan === nextProps.camera.disabled_by_plan &&
    // Group identity (re-render when reassigned) and color (re-render
    // when the group itself is recolored).  null === null short-circuit
    // covers ungrouped cameras with no allocation churn.
    (prevProps.group?.id ?? null) === (nextProps.group?.id ?? null) &&
    (prevProps.group?.color ?? null) === (nextProps.group?.color ?? null) &&
    (prevProps.group?.name ?? null) === (nextProps.group?.name ?? null) &&
    prevProps.onRequestUpgrade === nextProps.onRequestUpgrade
  )
})