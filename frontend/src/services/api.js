const API_URL = import.meta.env.VITE_API_URL || ""

// ── Central 401 (dead-session) handling ──────────────────────────────
//
// api.js is a plain module, not a React component, so it can't reach
// Clerk's signOut() or the router directly.  The app registers a handler
// once at mount (see App.jsx) and we invoke it the first time any request
// comes back 401.
//
// Why 401 only, never 403:
//   - 401 means the *credential itself* was rejected — expired/revoked
//     session, deleted org, bad signature.  The session is dead; the only
//     sane recovery is to end it locally and send the user to sign-in.
//     Before this, a dead session left every screen throwing its own 401
//     toast while the UI sat broken behind them.
//   - 403 means "authenticated but not allowed" (e.g. a viewer hitting an
//     admin-only endpoint).  That's a legitimate permission denial the
//     calling component should surface — forcing a logout on a 403 would
//     be hostile and confusing.  So 403 falls through to parseErrorBody
//     like any other error.
let _onUnauthorized = null
// Latch so a burst of in-flight requests all 401-ing at once fires the
// handler (and any redirect) exactly once.  Cleared on the next success
// so a genuine later expiry still triggers a fresh sign-out.
let _unauthorizedHandled = false

/**
 * Register the app-level reaction to a 401 (typically: Clerk signOut +
 * redirect to /sign-in).  Pass null to unregister (effect cleanup).
 */
export function setUnauthorizedHandler(handler) {
  _onUnauthorized = handler
}

function _handleUnauthorized() {
  if (_unauthorizedHandled) return
  _unauthorizedHandled = true
  if (typeof _onUnauthorized === "function") {
    try {
      _onUnauthorized()
    } catch (e) {
      console.error("[API] Unauthorized handler threw:", e)
    }
  }
}

/**
 * Parse a non-2xx response body into a usable Error.
 *
 * Three shapes exist on the wire today; the parser handles each cleanly
 * so consumers never see "[object Object]" in a toast regardless of
 * which backend pattern produced the response:
 *
 *   1. ApiError envelope — ``{detail: {error, message, ...extras}}``
 *      (see backend/app/core/errors.py — also matches the existing 402
 *      plan-limit-hit body and the new 422 validation handler)
 *   2. Plain string detail — ``{detail: "string"}``
 *      (most legacy ``raise HTTPException(detail="...")`` sites)
 *   3. Top-level envelope without ``detail`` —
 *      ``{error, message, ...}``
 *      (rate_limit_exceeded_handler in main.py emits this shape directly
 *      via JSONResponse rather than HTTPException)
 *
 * Plus the catch-all "no body / empty / non-JSON" fallback.
 *
 * The returned Error always has:
 *   - .message  — human-readable, ready to drop in a toast
 *   - .code     — machine-readable string when available, else null
 *                 (call sites can do ``if (e.code === "plan_limit_hit")``)
 *   - .status   — the HTTP status code
 *   - .detail   — the raw structured detail when one was sent, so
 *                 callers that branch on extra fields (plan,
 *                 max_cameras, etc.) can read them directly
 */
function parseErrorBody(body, status) {
  const detail = body?.detail

  // Shape 1: ApiError-style structured envelope under .detail.
  // Some backend sites send a machine code WITHOUT a human message —
  // e.g. {"detail": {"error": "sentinel_dispatch_disabled"}} and the
  // monthly-cap 429 {"detail": {"error": "monthly_cap_reached", ...}}.
  // Those must still surface .code (call sites branch on it) with a
  // humanized fallback message instead of falling through to the
  // generic "Request failed with status NNN".
  if (
    detail &&
    typeof detail === "object" &&
    !Array.isArray(detail) &&
    (detail.message || detail.error)
  ) {
    const fallback =
      typeof detail.error === "string" && detail.error
        ? detail.error.replace(/_/g, " ")
        : `Request failed with status ${status}`
    const err = new Error(detail.message || fallback)
    err.code = detail.error ?? null
    err.detail = detail
    err.status = status
    return err
  }

  // Shape 2: plain-string detail (legacy ``raise HTTPException`` pattern)
  if (typeof detail === "string") {
    const err = new Error(detail)
    err.code = null
    err.detail = null
    err.status = status
    return err
  }

  // Pydantic 422 fallback: array of {loc, msg, type}. Should be rare now
  // that main.py rewrites 422s through the validation handler, but
  // in-flight deploys (and the dev server when the handler hasn't
  // reloaded yet) can still surface this — handle defensively.
  if (Array.isArray(detail) && detail.length > 0) {
    const first = detail[0] || {}
    const loc = Array.isArray(first.loc)
      ? first.loc.filter((p) => p !== "body").join(".")
      : ""
    const msg = first.msg || "Validation failed"
    const err = new Error(loc ? `${msg} (${loc})` : msg)
    err.code = "validation_failed"
    err.detail = { errors: detail }
    err.status = status
    return err
  }

  // Shape 3: top-level envelope without .detail (rate-limit handler).
  // The body itself carries error/message at the top.
  if (body && typeof body === "object" && body.message) {
    const err = new Error(body.message)
    err.code = body.error ?? null
    err.detail = body
    err.status = status
    return err
  }

  // Last-resort fallback: nothing useful in the body at all.
  const err = new Error(`Request failed with status ${status}`)
  err.code = null
  err.detail = null
  err.status = status
  return err
}

export async function fetchWithAuth(endpoint, getToken, options = {}) {
  const token = getToken ? await getToken() : null

  const headers = {
    'Content-Type': 'application/json',
    ...options.headers
  }

  if (token) {
    headers['Authorization'] = `Bearer ${token}`
  }

  let response
  try {
    response = await fetch(
      `${API_URL}${endpoint}`,
      {
        ...options,
        headers
      }
    )
  } catch (fetchError) {
    console.error("[API] Fetch error:", fetchError)
    throw fetchError
  }

  if (!response.ok) {
    if (response.status === 401) _handleUnauthorized()
    const body = await response.json().catch(() => null)
    throw parseErrorBody(body, response.status)
  }

  // A success means the session is valid again — clear the latch so a
  // future genuine expiry re-triggers the sign-out flow.
  _unauthorizedHandled = false

  if (response.status === 204) {
    return null
  }

  return await response.json()
}

export async function getCameras(getToken) {
  return fetchWithAuth("/api/cameras", getToken)
}

export async function getSettings(getToken) {
  return fetchWithAuth("/api/settings", getToken)
}

// Per-camera recording policy (v0.1.43+).  Replaced the org-level
// updateRecordingSettings, which was wired to a backend endpoint that
// persisted but never actually drove recording.  PATCH semantics —
// only the fields you pass get updated.
export async function updateCameraRecordingPolicy(getToken, cameraId, policy) {
  return fetchWithAuth(
    `/api/cameras/${encodeURIComponent(cameraId)}/recording-settings`,
    getToken,
    {
      method: "PATCH",
      body: JSON.stringify(policy),
    },
  )
}

export async function updateNotificationSettings(getToken, settings) {
  return fetchWithAuth("/api/settings/notifications", getToken, {
    method: "POST",
    body: JSON.stringify(settings)
  })
}

// Per-org timezone (v0.1.43+ scheduled-recording feature).  IANA
// names like "America/Los_Angeles" or "UTC".  Backend validates
// against zoneinfo.available_timezones — typos 422 here, not later.
export async function updateOrgTimezone(getToken, tzName) {
  return fetchWithAuth("/api/settings/timezone", getToken, {
    method: "POST",
    body: JSON.stringify({ timezone: tzName })
  })
}

// Per-org per-kind email notification preferences.  Companion to the
// inbox-level updateNotificationSettings above — a notification can
// be enabled in the inbox AND emailed, just inbox-only, or off
// entirely.  See backend/app/api/notifications.py::email_enabled_for_kind
// and the kind→setting map for the contract.
export async function getEmailPreferences(getToken) {
  return fetchWithAuth("/api/notifications/email/preferences", getToken)
}

export async function updateEmailPreferences(getToken, prefs) {
  return fetchWithAuth("/api/notifications/email/preferences", getToken, {
    method: "POST",
    body: JSON.stringify(prefs)
  })
}

export async function getCameraGroups(getToken) {
  return fetchWithAuth("/api/camera-groups", getToken)
}

export async function createCameraGroup(getToken, name, color, icon) {
  return fetchWithAuth("/api/camera-groups", getToken, {
    method: "POST",
    body: JSON.stringify({ name, color, icon })
  })
}

export async function deleteCameraGroup(getToken, groupId) {
  return fetchWithAuth(`/api/camera-groups/${groupId}`, getToken, {
    method: "DELETE"
  })
}

// Assign a camera to a group (or pass null/undefined to unassign).
// Backend takes group_id as a query param — see
// `assign_camera_group` in backend/app/api/cameras.py.
export async function assignCameraGroup(getToken, cameraId, groupId) {
  const url = groupId !== null && groupId !== undefined
    ? `/api/cameras/${cameraId}/group?group_id=${encodeURIComponent(groupId)}`
    : `/api/cameras/${cameraId}/group`
  return fetchWithAuth(url, getToken, { method: "PUT" })
}

// Node management
export async function getNodes(getToken) {
  return fetchWithAuth("/api/nodes", getToken)
}

export async function getNode(getToken, nodeId) {
  return fetchWithAuth(`/api/nodes/${nodeId}`, getToken)
}

export async function createNode(getToken, name) {
  return fetchWithAuth("/api/nodes", getToken, {
    method: "POST",
    body: JSON.stringify({ name })
  })
}

// Member-initiated request to be promoted to admin role.
// Fires inbox + email notifications to every admin in the org.
// Backend returns 400 if the caller is already an admin (no useless
// self-notification) and 429 after 3 requests/hour from the same
// org bucket (prevents spam).
export async function requestAdminPromotion(getToken) {
  return fetchWithAuth("/api/notifications/request-admin-promotion", getToken, {
    method: "POST",
  })
}

export async function rotateNodeKey(getToken, nodeId) {
  return fetchWithAuth(`/api/nodes/${nodeId}/rotate-key`, getToken, {
    method: "POST"
  })
}

export async function deleteNode(getToken, nodeId) {
  return fetchWithAuth(`/api/nodes/${nodeId}`, getToken, {
    method: "DELETE"
  })
}

// Snapshot (saved on the camera node)
export async function requestSnapshot(getToken, cameraId) {
  return fetchWithAuth(`/api/cameras/${cameraId}/snapshot`, getToken, {
    method: "POST"
  })
}

// Recording (start/stop on the camera node)
export async function setRecording(getToken, cameraId, recording) {
  return fetchWithAuth(`/api/cameras/${cameraId}/recording`, getToken, {
    method: "POST",
    body: JSON.stringify({ recording })
  })
}

// Audit logs (admin only)
export async function getStreamLogs(getToken, params = {}) {
  const queryString = new URLSearchParams(
    Object.entries(params).filter(([_, v]) => v != null && v !== "")
  ).toString()
  return fetchWithAuth(`/api/audit/stream-logs?${queryString}`, getToken)
}

// Organization audit log — write_audit() rows for member changes,
// MCP key gen, settings changes, danger-zone actions, etc.
// Returns {total, limit, offset, logs}.
export async function getOrgAuditLogs(getToken, params = {}) {
  const queryString = new URLSearchParams(
    Object.entries(params).filter(([_, v]) => v != null && v !== "")
  ).toString()
  return fetchWithAuth(`/api/audit-logs?${queryString}`, getToken)
}

// CSV export for the org audit log.  Same blob-download flow as the
// other CSV exports.
export async function downloadOrgAuditLogsCsv(getToken, params = {}) {
  const cleanParams = Object.entries(params).filter(([_, v]) => v != null && v !== "")
  cleanParams.push(["format", "csv"])
  const qs = new URLSearchParams(cleanParams).toString()
  return _downloadCsv(`/api/audit-logs?${qs}`, getToken, "audit-log.csv")
}

// GDPR Article 20 — full org data export as a ZIP.
// Same blob-download flow as the CSV exports below; the backend
// streams a ZIP containing one JSON file per org-scoped table
// (cameras, settings, audit, motion events, notifications, MCP
// keys, email logs, incidents, etc.) plus a manifest.json.
export async function downloadGdprExport(getToken) {
  return _downloadFile(
    "/api/gdpr/export",
    getToken,
    "gdpr-export.zip",
    "application/zip",
    { method: "POST" },
  )
}

// CSV export for stream access logs.  Triggers a browser download via
// blob → object URL → hidden anchor click.  We can't just point the
// browser at the endpoint with window.open() because that wouldn't
// send the Clerk JWT — auth has to ride on a fetch().  Filename
// comes from the backend's Content-Disposition header so the date
// stamping stays server-controlled.
export async function downloadStreamLogsCsv(getToken, params = {}) {
  const cleanParams = Object.entries(params).filter(([_, v]) => v != null && v !== "")
  cleanParams.push(["format", "csv"])
  const qs = new URLSearchParams(cleanParams).toString()
  return _downloadCsv(`/api/audit/stream-logs?${qs}`, getToken, "stream-access-log.csv")
}

export async function getStreamStats(getToken, days = 7) {
  return fetchWithAuth(`/api/audit/stream-logs/stats?days=${days}`, getToken)
}

// Plan info
export async function getPlanInfo(getToken) {
  return fetchWithAuth("/api/nodes/plan", getToken)
}

// Danger Zone
export async function wipeStreamLogs(getToken) {
  return fetchWithAuth("/api/settings/danger/wipe-logs", getToken, {
    method: "POST"
  })
}

export async function fullReset(getToken) {
  return fetchWithAuth("/api/settings/danger/full-reset", getToken, {
    method: "POST"
  })
}

// MCP API Keys
export async function getMcpKeys(getToken) {
  return fetchWithAuth("/api/mcp/keys", getToken)
}

export async function createMcpKey(getToken, { name, scopeMode = "all", scopeTools = null } = {}) {
  const body = { name, scope_mode: scopeMode }
  if (scopeMode === "custom") {
    body.scope_tools = Array.isArray(scopeTools) ? scopeTools : []
  }
  return fetchWithAuth(`/api/mcp/keys`, getToken, {
    method: "POST",
    body: JSON.stringify(body)
  })
}

export async function revokeMcpKey(getToken, keyId) {
  return fetchWithAuth(`/api/mcp/keys/${keyId}`, getToken, {
    method: "DELETE"
  })
}

// Integration API keys (osi_) — REST keys for external integrations like
// Home Assistant. Separate from MCP keys (osc_); see /api/integration/keys.
export async function getIntegrationKeys(getToken) {
  return fetchWithAuth("/api/integration/keys", getToken)
}

export async function createIntegrationKey(getToken, { name } = {}) {
  return fetchWithAuth("/api/integration/keys", getToken, {
    method: "POST",
    body: JSON.stringify({ name }),
  })
}

export async function revokeIntegrationKey(getToken, keyId) {
  return fetchWithAuth(`/api/integration/keys/${keyId}`, getToken, {
    method: "DELETE",
  })
}

export async function getMcpToolCatalog(getToken) {
  return fetchWithAuth(`/api/mcp/tools`, getToken)
}

// MCP Activity
export async function getMcpActivity(getToken, limit = 50) {
  return fetchWithAuth(`/api/mcp/activity/recent?limit=${limit}`, getToken)
}

export async function getMcpSessions(getToken) {
  return fetchWithAuth("/api/mcp/activity/sessions", getToken)
}

export async function getMcpStats(getToken) {
  return fetchWithAuth("/api/mcp/activity/stats", getToken)
}

// MCP Activity Logs (DB-backed, for admin dashboard)
export async function getMcpLogs(getToken, params = {}) {
  const queryString = new URLSearchParams(
    Object.entries(params).filter(([_, v]) => v != null && v !== "")
  ).toString()
  return fetchWithAuth(`/api/mcp/activity/logs?${queryString}`, getToken)
}

// CSV export for MCP activity logs.  Same blob-download flow as the
// stream-logs CSV — see downloadStreamLogsCsv for the rationale.
export async function downloadMcpLogsCsv(getToken, params = {}) {
  const cleanParams = Object.entries(params).filter(([_, v]) => v != null && v !== "")
  cleanParams.push(["format", "csv"])
  const qs = new URLSearchParams(cleanParams).toString()
  return _downloadCsv(`/api/mcp/activity/logs?${qs}`, getToken, "mcp-activity-log.csv")
}

export async function getMcpLogStats(getToken, days = 7) {
  return fetchWithAuth(`/api/mcp/activity/logs/stats?days=${days}`, getToken)
}

// Incident reports (both AI-authored via MCP and human-authored via this API)
export async function getIncidents(getToken, params = {}) {
  const queryString = new URLSearchParams(
    Object.entries(params).filter(([_, v]) => v != null && v !== "")
  ).toString()
  const suffix = queryString ? `?${queryString}` : ""
  return fetchWithAuth(`/api/incidents${suffix}`, getToken)
}

// Operator-filed incident.  Body shape: { title, summary, severity?, camera_id? }.
export async function createIncident(getToken, body) {
  return fetchWithAuth("/api/incidents", getToken, {
    method: "POST",
    body: JSON.stringify(body),
  })
}

export async function getIncidentCounts(getToken) {
  return fetchWithAuth("/api/incidents/counts", getToken)
}

export async function getIncident(getToken, incidentId) {
  return fetchWithAuth(`/api/incidents/${incidentId}`, getToken)
}

export async function patchIncident(getToken, incidentId, patch) {
  return fetchWithAuth(`/api/incidents/${incidentId}`, getToken, {
    method: "PATCH",
    body: JSON.stringify(patch),
  })
}

export async function deleteIncident(getToken, incidentId) {
  return fetchWithAuth(`/api/incidents/${incidentId}`, getToken, {
    method: "DELETE",
  })
}

// Returns a Blob URL for an evidence snapshot. Caller must URL.revokeObjectURL when done.
export async function fetchIncidentEvidenceBlobUrl(getToken, incidentId, evidenceId) {
  const token = getToken ? await getToken() : null
  const headers = {}
  if (token) headers["Authorization"] = `Bearer ${token}`
  const response = await fetch(
    `${API_URL}/api/incidents/${incidentId}/evidence/${evidenceId}`,
    { headers }
  )
  if (!response.ok) {
    if (response.status === 401) _handleUnauthorized()
    throw new Error(`Failed to load evidence (${response.status})`)
  }
  const blob = await response.blob()
  return URL.createObjectURL(blob)
}

// Absolute URL to the synthetic HLS playlist for a clip evidence item.
// hls.js loads this and resolves the segment URL inside it (which points back
// at the regular blob endpoint). Auth is added via xhrSetup, same as the
// live HlsPlayer.
export function incidentEvidencePlaylistUrl(incidentId, evidenceId) {
  return `${API_URL}/api/incidents/${incidentId}/evidence/${evidenceId}/playlist.m3u8`
}

// ── Notifications (bell inbox) ─────────────────────────────────────

export async function getNotifications(getToken, params = {}) {
  const queryString = new URLSearchParams(
    Object.entries(params).filter(([_, v]) => v != null && v !== "")
  ).toString()
  const suffix = queryString ? `?${queryString}` : ""
  return fetchWithAuth(`/api/notifications${suffix}`, getToken)
}

export async function getUnreadNotificationCount(getToken) {
  return fetchWithAuth("/api/notifications/unread-count", getToken)
}

export async function markNotificationsViewed(getToken) {
  return fetchWithAuth("/api/notifications/mark-viewed", getToken, {
    method: "POST",
  })
}

export async function clearAllNotifications(getToken) {
  return fetchWithAuth("/api/notifications/clear-all", getToken, {
    method: "POST",
  })
}

// ── Generic file-download helper ─────────────────────────────────────
//
// Streams an arbitrary attachment response from a backend endpoint
// and triggers a browser download.  Used by downloadStreamLogsCsv,
// downloadMcpLogsCsv, and downloadGdprExport.
//
// Why not just window.open(url)?  Because the browser navigation
// wouldn't carry the Clerk JWT — we'd get a 401.  Auth has to ride
// on a fetch().  We collect the response as a Blob (which streams
// internally), wrap it in an object URL, and click a hidden anchor
// to fire the browser's standard download UI.
//
// Filename precedence:
//   1. The backend's Content-Disposition `filename=`  (preferred —
//      includes the org id + date stamped server-side)
//   2. The fallback name passed by the caller — only used if the
//      header is missing or unparseable.
async function _downloadFile(
  endpoint, getToken, fallbackFilename, _expectedMime = null, fetchOptions = {},
) {
  const token = getToken ? await getToken() : null
  const headers = { ...(fetchOptions.headers || {}) }
  if (token) headers["Authorization"] = `Bearer ${token}`

  const response = await fetch(`${API_URL}${endpoint}`, {
    ...fetchOptions,
    headers,
  })
  if (!response.ok) {
    if (response.status === 401) _handleUnauthorized()
    // Surface the API's error envelope via the SAME parser the JSON
    // client uses — `body?.detail` alone produced "[object Object]"
    // toasts for structured envelopes (422 arrays, ApiError objects).
    let body = null
    try {
      body = await response.json()
    } catch { /* not JSON — fall through to status code */ }
    if (body !== null) throw parseErrorBody(body, response.status)
    throw new Error(`HTTP ${response.status}`)
  }

  // Pull filename from Content-Disposition; fall back to caller default.
  // Header looks like: attachment; filename="audit-log-org_xxx-20260505.csv"
  let filename = fallbackFilename
  const cd = response.headers.get("Content-Disposition") || ""
  const match = cd.match(/filename="?([^";]+)"?/i)
  if (match && match[1]) filename = match[1]

  const blob = await response.blob()
  const url = URL.createObjectURL(blob)
  const a = document.createElement("a")
  a.href = url
  a.download = filename
  // Some browsers (Safari historically) require the anchor in the DOM.
  document.body.appendChild(a)
  a.click()
  document.body.removeChild(a)
  // Free the object URL — the browser holds onto it until revoked.
  // setTimeout gives the click a tick to actually fire before we yank
  // the URL out from under it.
  setTimeout(() => URL.revokeObjectURL(url), 0)
}

// Backward-compat shim — existing CSV callers still reach for the old name.
async function _downloadCsv(endpoint, getToken, fallbackFilename) {
  return _downloadFile(endpoint, getToken, fallbackFilename, "text/csv")
}

// ── Sentinel ─────────────────────────────────────────────────────────
// Slice 1 of the Sentinel rollout: config + run history persistence.
// The agent itself isn't yet wired up, so getSentinelRuns() returns
// an empty list for new orgs.  See plans/ for the 7-slice roadmap.

// GET — always returns 200, even for non-Pro-Plus orgs.  Look at
// `plan_gated` in the response to decide whether to render in
// read-only mode with an upgrade banner.
export async function getSentinelConfig(getToken) {
  return fetchWithAuth("/api/sentinel/config", getToken)
}

// PATCH — partial update.  Only fields present in the patch are
// touched.  Returns 402 for non-Pro-Plus orgs; the optimistic-update
// caller is responsible for rolling back local state on failure.
export async function updateSentinelConfig(getToken, patch) {
  return fetchWithAuth("/api/sentinel/config", getToken, {
    method: "PATCH",
    body: JSON.stringify(patch),
  })
}

// Paginated run history with small inline stats.  Filters: trigger
// (motion|incident_opened|manual|scheduled), since (ISO datetime).
export async function getSentinelRuns(
  getToken,
  { limit = 50, offset = 0, trigger, since } = {},
) {
  const qs = new URLSearchParams()
  qs.set("limit", String(limit))
  qs.set("offset", String(offset))
  if (trigger) qs.set("trigger", trigger)
  if (since) qs.set("since", since)
  return fetchWithAuth(`/api/sentinel/runs?${qs}`, getToken)
}

// Single run detail with full tool trace (for the run-detail drawer).
export async function getSentinelRun(getToken, runId) {
  return fetchWithAuth(
    `/api/sentinel/runs/${encodeURIComponent(runId)}`,
    getToken,
  )
}

// Operator-initiated agent run.  Creates a pending sentinel_runs row;
// the agent (when it ships) picks it up and posts back via /complete.
// Pro Plus only — 402 otherwise.  Cap-enforced — 429 if the org is
// at the monthly limit.
export async function dispatchSentinelManualRun(getToken, { prompt, cameraId } = {}) {
  return fetchWithAuth("/api/sentinel/runs/manual", getToken, {
    method: "POST",
    body: JSON.stringify({
      prompt: prompt || "",
      camera_id: cameraId || null,
    }),
  })
}