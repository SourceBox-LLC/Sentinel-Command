import { useEffect, useRef, useState } from "react"
import { useAuth } from "@clerk/clerk-react"
import { updateCameraRecordingPolicy, assignCameraGroup } from "../services/api"
import HelpTooltip from "./HelpTooltip.jsx"

/**
 * Per-camera recording-policy controls (v0.1.43+).
 *
 * Shown inside each Camera Nodes card on Settings, one row per
 * camera under the storage bar.  Replaces the previous org-wide
 * Settings → Recording section, which never actually drove
 * recording (its toggles persisted to a Setting row but no consumer
 * read them).  Per-camera here is the granularity that matches how
 * recording state is keyed at runtime in CloudNode and lets
 * operators record some cameras 24/7 while leaving others off for
 * privacy / storage reasons.
 *
 * Camera Groups Phase 2 (added 2026-05-09): also hosts the per-camera
 * Group selector — a small `<select>` that assigns the camera to one
 * of the org's camera groups.  Groups are managed in
 * Settings > Camera Groups (Phase 1) and are how AI agents resolve
 * natural-language locations to camera_ids via the
 * `list_camera_groups` MCP tool.
 *
 * Props:
 *   - camera: the camera object (must include `recording_policy` and
 *     optionally `group_id`)
 *   - onUpdated: optional callback invoked with the new policy after
 *     a successful PATCH; parent uses this to refresh local state
 *     so the toggle is immediately reflected in the card.
 *   - onGroupChanged: optional callback invoked with the new group_id
 *     after a successful group assignment.  Same purpose as
 *     onUpdated, scoped to group changes.
 *   - groups: array of {id, name, color, icon} for this org's camera
 *     groups.  Empty array hides the selector entirely (no point
 *     showing a dropdown with nothing to pick).
 *   - canManageGroups: false hides the group selector for non-admin
 *     members (the backend rejects with 403 anyway, but better UX
 *     to not surface a control they can't use).
 *
 * Optimistic UI: the toggle flips immediately; if the PATCH fails
 * we roll back to the previous state and surface a toast.  Avoids
 * the lag where a user clicks twice because nothing visually
 * changed for half a second.
 */
function CameraRecordingControls({
  camera,
  onUpdated,
  onGroupChanged,
  timezone,
  groups = [],
  canManageGroups = true,
}) {
  const { getToken } = useAuth()
  const policy = camera.recording_policy || {
    continuous_24_7: false,
    scheduled_recording: false,
    scheduled_start: null,
    scheduled_end: null,
  }

  // Local mirror of policy for optimistic updates.  Initialized from
  // props but diverges briefly during a PATCH-in-flight.
  const [local, setLocal] = useState(policy)
  const [saving, setSaving] = useState(false)

  // Group state mirror — kept separate from `local` because the group
  // selector posts to a different endpoint and uses a different
  // optimistic-rollback model.  Treats `null` and `undefined` the same
  // as "(no group)" — backend stores group_id as nullable FK.
  const [localGroupId, setLocalGroupId] = useState(camera.group_id ?? null)
  const [savingGroup, setSavingGroup] = useState(false)

  // Resync the local mirrors when the SERVER value genuinely changes
  // (Settings polls every 30s; an MCP agent can flip recording policy
  // out from under us).  useState initializers run once, so without
  // this an external change would never render — and the next local
  // edit would PATCH the full stale snapshot back, silently reverting
  // it.  We compare against the last-seen server value in a ref (NOT
  // "resync whenever not saving"): the optimistic local value must
  // survive the window where our own save has landed but the parent
  // hasn't re-propped yet — only an actual prop transition wins.
  const policyKey = `${policy.continuous_24_7}|${policy.scheduled_recording}|${policy.scheduled_start}|${policy.scheduled_end}`
  const lastServerPolicyRef = useRef(policyKey)
  useEffect(() => {
    if (policyKey !== lastServerPolicyRef.current) {
      lastServerPolicyRef.current = policyKey
      setLocal(policy)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [policyKey])

  const serverGroupId = camera.group_id ?? null
  const lastServerGroupRef = useRef(serverGroupId)
  useEffect(() => {
    if (serverGroupId !== lastServerGroupRef.current) {
      lastServerGroupRef.current = serverGroupId
      setLocalGroupId(serverGroupId)
    }
  }, [serverGroupId])

  const persist = async (next) => {
    const previous = local
    setLocal(next)
    setSaving(true)
    try {
      const token = await getToken()
      const resp = await updateCameraRecordingPolicy(
        () => Promise.resolve(token),
        camera.camera_id,
        next,
      )
      // Server-canonical state may differ slightly (e.g., null
      // normalization) — sync to whatever the server actually saved.
      if (resp?.recording_policy) {
        setLocal(resp.recording_policy)
        if (onUpdated) onUpdated(resp.recording_policy)
      } else if (onUpdated) {
        onUpdated(next)
      }
    } catch (err) {
      // Roll back on failure.
      setLocal(previous)
      console.error("Recording policy update failed:", err)
    } finally {
      setSaving(false)
    }
  }

  // Continuous and Scheduled are mutually exclusive — having both on
  // means scheduled is silently ignored (the heartbeat handler picks
  // continuous in that case), which is a confusing UI state.  Toggling
  // one ON explicitly turns the other OFF in the same PATCH.  The
  // backend enforces the same invariant as a 422 — see
  // `update_camera_recording_policy` in api/cameras.py — so a direct
  // API caller can't bypass the rule either.
  const onToggleContinuous = () => {
    const turningOn = !local.continuous_24_7
    persist({
      ...local,
      continuous_24_7: turningOn,
      // When turning continuous ON, ensure scheduled is OFF.
      // When turning continuous OFF, leave scheduled alone.
      ...(turningOn ? { scheduled_recording: false } : {}),
    })
  }

  const onToggleScheduled = () => {
    const turningOn = !local.scheduled_recording
    const next = {
      ...local,
      scheduled_recording: turningOn,
      // When turning scheduled ON, ensure continuous is OFF.
      ...(turningOn ? { continuous_24_7: false } : {}),
    }
    // Default the window to a reasonable 8am–5pm if scheduled is being
    // turned on for the first time and no window has been set.
    if (turningOn) {
      next.scheduled_start = local.scheduled_start || "08:00"
      next.scheduled_end = local.scheduled_end || "17:00"
    }
    persist(next)
  }

  const onChangeTime = (field, value) => {
    persist({ ...local, [field]: value })
  }

  // Group assignment handler — separate save flow from recording-policy
  // since it hits a different endpoint.  Optimistic UI: flip the local
  // selection immediately; on failure roll back and let the parent
  // know nothing changed.
  const onChangeGroup = async (e) => {
    const raw = e.target.value
    // The "(no group)" option uses an empty-string value; coerce to null
    // so we send null instead of "" to the backend.
    const next = raw === "" ? null : Number(raw)
    if (next === localGroupId) return
    const previous = localGroupId
    setLocalGroupId(next)
    setSavingGroup(true)
    try {
      const token = await getToken()
      await assignCameraGroup(
        () => Promise.resolve(token),
        camera.camera_id,
        next,
      )
      if (onGroupChanged) onGroupChanged(next)
    } catch (err) {
      console.error("Group assignment failed:", err)
      setLocalGroupId(previous)
    } finally {
      setSavingGroup(false)
    }
  }

  return (
    <div
      className="camera-recording-controls"
      style={{
        marginTop: "0.5rem",
        padding: "0.6rem 0.75rem",
        background: "var(--bg-primary, #0a0a0a)",
        border: "1px solid var(--border, #2a2a2a)",
        borderRadius: "6px",
        opacity: saving ? 0.7 : 1,
        transition: "opacity 0.2s ease",
      }}
    >
      <div
        style={{
          fontSize: "0.85rem",
          fontWeight: 600,
          marginBottom: "0.5rem",
        }}
      >
        {camera.name || camera.camera_id}
      </div>

      {/* Group selector (Camera Groups Phase 2) — render only when the
          org has at least one group AND the user can manage them.  No
          group + admin = small "Create one in Camera Groups above" hint. */}
      {canManageGroups && groups.length > 0 && (
        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            fontSize: "0.85rem",
            color: "var(--text-muted, #888)",
            marginBottom: "0.5rem",
            gap: "0.5rem",
          }}
        >
          <label
            htmlFor={`group-select-${camera.camera_id}`}
            style={{ flexShrink: 0 }}
          >
            Group
            <HelpTooltip label="Help: camera group">
              Bundle this camera with others in the same physical zone.
              AI agents (Sentinel, Claude, etc.) use group names to
              resolve queries like &ldquo;check the workshop&rdquo; into
              the right set of camera_ids.  Manage groups in
              Settings &gt; Camera Groups.
            </HelpTooltip>
          </label>
          <select
            id={`group-select-${camera.camera_id}`}
            className="form-input camera-group-select"
            value={localGroupId ?? ""}
            onChange={onChangeGroup}
            disabled={savingGroup}
            aria-label={`Camera group for ${camera.name || camera.camera_id}`}
          >
            <option value="">(no group)</option>
            {groups.map((g) => (
              <option key={g.id} value={g.id}>
                {g.icon ? `${g.icon} ` : ""}
                {g.name}
              </option>
            ))}
          </select>
        </div>
      )}

      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          fontSize: "0.85rem",
          color: "var(--text-muted, #888)",
          marginBottom: "0.4rem",
        }}
      >
        <span>
          Continuous 24/7
          <HelpTooltip label="Help: continuous recording">
            Records <strong>every frame</strong>, all day, every day, until
            you turn it off.  Highest storage cost — a single 1080p camera
            on continuous fills ~30 GB/day.  Use for high-stakes feeds
            (front door, register) where missing a moment is worse than
            paying for storage.  Mutually exclusive with Scheduled.
          </HelpTooltip>
        </span>
        <button
          type="button"
          className={`toggle-switch ${local.continuous_24_7 ? "active" : ""}`}
          onClick={onToggleContinuous}
          disabled={saving}
          aria-label={`Toggle continuous recording for ${camera.name || camera.camera_id}`}
          aria-pressed={local.continuous_24_7}
        >
          <span className="toggle-knob" />
        </button>
      </div>

      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          fontSize: "0.85rem",
          color: "var(--text-muted, #888)",
          marginBottom: local.scheduled_recording ? "0.4rem" : 0,
        }}
      >
        <span>
          Scheduled Recording
          <HelpTooltip label="Help: scheduled recording">
            Records only during the time window you set below
            (defaults to 8am–5pm).  Storage cost scales with the
            window — a 9-hour daily schedule is roughly 1/3 the
            cost of Continuous.  Use for business-hours-only feeds.
            Mutually exclusive with Continuous.
          </HelpTooltip>
        </span>
        <button
          type="button"
          className={`toggle-switch ${local.scheduled_recording ? "active" : ""}`}
          onClick={onToggleScheduled}
          disabled={saving}
          aria-label={`Toggle scheduled recording for ${camera.name || camera.camera_id}`}
          aria-pressed={local.scheduled_recording}
        >
          <span className="toggle-knob" />
        </button>
      </div>

      {local.scheduled_recording && (
        <div style={{ marginTop: "0.3rem" }}>
          <div
            style={{
              display: "flex",
              gap: "0.5rem",
              alignItems: "center",
              fontSize: "0.85rem",
            }}
          >
            <input
              type="time"
              value={local.scheduled_start || ""}
              onChange={(e) => onChangeTime("scheduled_start", e.target.value)}
              disabled={saving}
              style={{
                padding: "0.25rem 0.5rem",
                background: "var(--bg-secondary, #1a1a1a)",
                border: "1px solid var(--border, #333)",
                borderRadius: "4px",
                color: "var(--text-primary, #fff)",
                fontSize: "0.85rem",
              }}
            />
            <span style={{ color: "var(--text-muted, #888)" }}>to</span>
            <input
              type="time"
              value={local.scheduled_end || ""}
              onChange={(e) => onChangeTime("scheduled_end", e.target.value)}
              disabled={saving}
              style={{
                padding: "0.25rem 0.5rem",
                background: "var(--bg-secondary, #1a1a1a)",
                border: "1px solid var(--border, #333)",
                borderRadius: "4px",
                color: "var(--text-primary, #fff)",
                fontSize: "0.85rem",
              }}
            />
          </div>
          {timezone && (
            <p
              style={{
                fontSize: "0.75rem",
                color: "var(--text-muted, #888)",
                marginTop: "0.25rem",
                marginBottom: 0,
              }}
            >
              Times in {timezone}. Change in Settings → Time Zone.
            </p>
          )}
        </div>
      )}
    </div>
  )
}

export default CameraRecordingControls
