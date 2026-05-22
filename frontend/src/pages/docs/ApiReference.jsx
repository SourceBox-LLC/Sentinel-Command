import { useDocs } from "./context"


function ApiReference() {
  const { base, copyToClipboard } = useDocs()

  return (
    <section className="docs-section" id="api-reference">
      <h2>API Reference<a href="#api-reference" className="docs-anchor">#</a></h2>
      <p>
        Command Center exposes a REST API at <code>{base}</code>. Three
        auth schemes cover three audiences — CloudNode uses API key headers, the web
        dashboard uses Clerk JWTs, and the MCP endpoint uses a dedicated bearer token.
      </p>

      <h3>Authentication</h3>
      <div className="docs-plans-table">
        <table>
          <thead>
            <tr><th>Scheme</th><th>Header</th><th>Used by</th></tr>
          </thead>
          <tbody>
            <tr><td>Node API key</td><td><code>X-Node-API-Key: nak_...</code></td><td>Every CloudNode → Command Center call (register, heartbeat, push-segment, playlist, motion, codec, decommission)</td></tr>
            <tr><td>Clerk JWT</td><td><code>Authorization: Bearer &lt;jwt&gt;</code></td><td>Web dashboard, authenticated viewers</td></tr>
            <tr><td>MCP key</td><td><code>Authorization: Bearer osc_...</code></td><td>AI clients talking to <code>/mcp</code></td></tr>
          </tbody>
        </table>
      </div>

      <h3>Error format</h3>
      <p>All errors return a JSON body with a stable shape:</p>
      <div className="docs-code-block">
        <code>{`{
  "error": "camera_not_found",
  "message": "No camera with id cam_xyz in this organization",
  "request_id": "req_ab12cd34"
}`}</code>
        <button className="docs-copy-btn" onClick={() => copyToClipboard(`{
  "error": "camera_not_found",
  "message": "No camera with id cam_xyz in this organization",
  "request_id": "req_ab12cd34"
}`)}>Copy</button>
      </div>
      <p>Standard HTTP status codes apply:</p>
      <ul>
        <li><strong>400</strong> — malformed request body or missing required query param</li>
        <li><strong>401</strong> — missing or invalid auth header</li>
        <li><strong>403</strong> — authenticated but not authorized (wrong org, insufficient role, plan gate)</li>
        <li><strong>404</strong> — resource not found in the caller's org</li>
        <li><strong>429</strong> — rate-limit exceeded. Applies to both REST routes (per-route limits, see <a href="#api-rate-limits">API Rate Limits</a>) and MCP tool calls (per-key budget on Pro/Pro Plus). The response body includes an <code>error: "rate_limit_exceeded"</code> field, the matched <code>limit</code>, and a <code>retry_after_seconds</code> hint; clients should honour the <code>Retry-After</code> header.</li>
        <li><strong>5xx</strong> — server error; <code>request_id</code> in the body is what to include in bug reports</li>
      </ul>

      <h3>Example request</h3>
      <p>Listing cameras from a shell with a signed-in user's JWT:</p>
      <div className="docs-code-block">
        <code>{`curl -H "Authorization: Bearer $CLERK_JWT" \\
     ${base}/api/cameras`}</code>
        <button className="docs-copy-btn" onClick={() => copyToClipboard(`curl -H "Authorization: Bearer $CLERK_JWT" ${base}/api/cameras`)}>Copy</button>
      </div>

      <details className="docs-accordion">
        <summary>
          <span className="docs-accordion-chevron" aria-hidden="true">▶</span>
          <span className="docs-accordion-title">Node Endpoints</span>
          <span className="docs-accordion-count">7 endpoints</span>
        </summary>
        <p className="docs-accordion-intro">Used by CloudNode. Authenticate with the <code>X-Node-API-Key: {"your_api_key"}</code> header.</p>

        <div className="docs-endpoint"><span className="docs-endpoint-method post">POST</span><span className="docs-endpoint-path">/api/nodes/register</span></div>
        <p>Register a node and its cameras. Returns camera ID mappings.</p>

        <div className="docs-endpoint"><span className="docs-endpoint-method post">POST</span><span className="docs-endpoint-path">/api/nodes/heartbeat</span></div>
        <p>Send periodic heartbeat with camera status updates.</p>

        <div className="docs-endpoint"><span className="docs-endpoint-method post">POST</span><span className="docs-endpoint-path">/api/cameras/{"{camera_id}"}/push-segment</span></div>
        <p>Push a raw HLS <code>.ts</code> segment into the Command Center's in-memory cache. Body is the binary segment, <code>filename</code> is a query param.</p>

        <div className="docs-endpoint"><span className="docs-endpoint-method post">POST</span><span className="docs-endpoint-path">/api/cameras/{"{camera_id}"}/playlist</span></div>
        <p>Push the rolling HLS playlist text. The backend rewrites segment URLs to its own proxy paths and caches the result.</p>

        <div className="docs-endpoint"><span className="docs-endpoint-method post">POST</span><span className="docs-endpoint-path">/api/cameras/{"{camera_id}"}/codec</span></div>
        <p>Report detected video/audio codec information.</p>

        <div className="docs-endpoint"><span className="docs-endpoint-method post">POST</span><span className="docs-endpoint-path">/api/cameras/{"{camera_id}"}/motion</span></div>
        <p>Report a motion event scored above the per-camera threshold. Body: <code>{"{score, timestamp, segment_seq}"}</code>. Per-camera cooldown is enforced on the node side, so the backend trusts every event it receives.</p>

        <div className="docs-endpoint"><span className="docs-endpoint-method post">POST</span><span className="docs-endpoint-path">/api/nodes/self/decommission</span></div>
        <p>Node-initiated factory reset — fired from the TUI&apos;s <code>/wipe confirm</code> flow before the local data is erased. Deletes the <code>CameraNode</code> row (cascades to cameras and any in-memory segment cache) so a freshly-wiped box doesn&apos;t reappear as a stale offline node.</p>
      </details>

      <details className="docs-accordion">
        <summary>
          <span className="docs-accordion-chevron" aria-hidden="true">▶</span>
          <span className="docs-accordion-title">User Endpoints</span>
          <span className="docs-accordion-count">10 endpoints</span>
        </summary>
        <p className="docs-accordion-intro">Used by the web dashboard. Authenticate with Clerk JWT in <code>Authorization: Bearer</code> header.</p>

      <div className="docs-endpoint"><span className="docs-endpoint-method get">GET</span><span className="docs-endpoint-path">/api/cameras</span></div>
      <p>List all cameras in the organization.</p>

      <div className="docs-endpoint"><span className="docs-endpoint-method get">GET</span><span className="docs-endpoint-path">/api/cameras/{"{camera_id}"}/stream.m3u8</span></div>
      <p>Get the cached HLS playlist for browser playback. Segment URLs point at the same-origin segment proxy.</p>

      <div className="docs-endpoint"><span className="docs-endpoint-method get">GET</span><span className="docs-endpoint-path">/api/cameras/{"{camera_id}"}/segment/{"{filename}"}</span></div>
      <p>Serve a single cached HLS segment from memory. JWT-authenticated, same-origin.</p>

      <div className="docs-endpoint"><span className="docs-endpoint-method get">GET</span><span className="docs-endpoint-path">/api/nodes</span></div>
      <p>List all nodes in the organization. Admin only.</p>

      <div className="docs-endpoint"><span className="docs-endpoint-method post">POST</span><span className="docs-endpoint-path">/api/nodes</span></div>
      <p>Create a new node. Returns the API key (shown once).</p>

      <div className="docs-endpoint"><span className="docs-endpoint-method get">GET</span><span className="docs-endpoint-path">/api/settings</span></div>
      <p>Org-level settings: notifications + timezone. Recording is per-camera since v0.1.43 — see the camera endpoints below.</p>

      <div className="docs-endpoint"><span className="docs-endpoint-method post">POST</span><span className="docs-endpoint-path">/api/settings/timezone</span></div>
      <p>Set the org's IANA timezone (e.g. <code>"America/Los_Angeles"</code>). Drives the wall-clock interpretation of per-camera scheduled-recording windows. Admin only. 422 on unknown zones.</p>

      <div className="docs-endpoint"><span className="docs-endpoint-method patch">PATCH</span><span className="docs-endpoint-path">/api/cameras/{"{camera_id}"}/recording-settings</span></div>
      <p>Update a camera's recording policy. Optional fields: <code>continuous_24_7</code>, <code>scheduled_recording</code>, <code>scheduled_start</code>, <code>scheduled_end</code> (HH:MM 24-hour in the org's timezone). The two modes are mutually exclusive — passing both as <code>true</code> returns 422. Admin only.</p>

      <div className="docs-endpoint"><span className="docs-endpoint-method post">POST</span><span className="docs-endpoint-path">/api/cameras/{"{camera_id}"}/recording</span></div>
      <p>Manual record button — thin wrapper that flips <code>continuous_24_7</code> on the camera. The heartbeat reconciler picks it up within one tick. Body: <code>{"{recording: bool}"}</code>. Admin only.</p>

        <div className="docs-endpoint"><span className="docs-endpoint-method get">GET</span><span className="docs-endpoint-path">/api/audit/stream-logs</span></div>
        <p>Stream access history. Admin only. Filterable by camera and user.</p>
      </details>

      <details className="docs-accordion">
        <summary>
          <span className="docs-accordion-chevron" aria-hidden="true">▶</span>
          <span className="docs-accordion-title">Incident Reports</span>
          <span className="docs-accordion-count">7 endpoints</span>
        </summary>
        <p className="docs-accordion-intro">
          AI-generated incident reports (written by the <a href="#sentinel">Sentinel
          agent</a> or by direct MCP-API calls) and human-filed reports — all
          reviewed and managed from the dashboard. Endpoints require admin
          permission.
        </p>

      <div className="docs-endpoint"><span className="docs-endpoint-method get">GET</span><span className="docs-endpoint-path">/api/incidents</span></div>
      <p>List incidents for the org (newest first). Supports <code>status</code>, <code>severity</code>, <code>camera_id</code>, <code>limit</code>, and <code>offset</code> query params.</p>

      <div className="docs-endpoint"><span className="docs-endpoint-method get">GET</span><span className="docs-endpoint-path">/api/incidents/counts</span></div>
      <p>Aggregate counts for the stat bar and badges — total, open, open critical, open high.</p>

      <div className="docs-endpoint"><span className="docs-endpoint-method get">GET</span><span className="docs-endpoint-path">/api/incidents/{"{incident_id}"}</span></div>
      <p>Fetch a single incident with its full markdown report and all evidence metadata.</p>

      <div className="docs-endpoint"><span className="docs-endpoint-method patch">PATCH</span><span className="docs-endpoint-path">/api/incidents/{"{incident_id}"}</span></div>
      <p>Acknowledge, resolve, dismiss, or otherwise edit an incident's status, severity, summary, or report.</p>

      <div className="docs-endpoint"><span className="docs-endpoint-method delete">DELETE</span><span className="docs-endpoint-path">/api/incidents/{"{incident_id}"}</span></div>
      <p>Permanently delete an incident and all of its evidence (cascades).</p>

      <div className="docs-endpoint"><span className="docs-endpoint-method get">GET</span><span className="docs-endpoint-path">/api/incidents/{"{incident_id}"}/evidence/{"{evidence_id}"}</span></div>
      <p>Stream a snapshot or clip blob attached as evidence — used by the dashboard to render thumbnails and play back clips in the incident report modal.</p>

        <div className="docs-endpoint"><span className="docs-endpoint-method get">GET</span><span className="docs-endpoint-path">/api/incidents/{"{incident_id}"}/evidence/{"{evidence_id}"}/playlist.m3u8</span></div>
        <p>Synthetic single-segment HLS playlist for a clip evidence item, so the dashboard can reuse hls.js to play back captured video with the same JWT auth as the live player.</p>
      </details>

      <h3 id="api-health">Health & Status</h3>
      <p>Two unauthenticated endpoints for monitoring. Use the minimal one for load balancers; use the detailed one for status pages and on-call diagnostics.</p>

      <div className="docs-endpoint"><span className="docs-endpoint-method get">GET</span><span className="docs-endpoint-path">/api/health</span></div>
      <p>Always-200 liveness check. Returns <code>{`{ "status": "healthy", "version": "2.1.2" }`}</code>. Right shape for load-balancer probes — keep it cheap.</p>

      <div className="docs-endpoint"><span className="docs-endpoint-method get">GET</span><span className="docs-endpoint-path">/api/health/detailed</span></div>
      <p>Verbose status — process uptime, DB ping latency in ms, HLS cache occupancy, pending viewer-usage flush queue depth, SSE subscriber counts. Overall <code>status</code> rolls up to <code>healthy</code> / <code>degraded</code> / <code>unhealthy</code>. Public on purpose so a status-page tool can poll it from off-net, but every value is a number — never an org, camera, or user identifier (pinned by a privacy regression test).</p>

      <details className="docs-accordion">
        <summary>
          <span className="docs-accordion-chevron" aria-hidden="true">▶</span>
          <span className="docs-accordion-title">Sentinel AI Endpoints</span>
          <span className="docs-accordion-count">8 endpoints</span>
        </summary>
        <p className="docs-accordion-intro">
          Per-org config and run lifecycle for the <a href="#sentinel">Sentinel
          AI agent</a>. Admin endpoints surface plan-gated config and run
          history to the dashboard. Agent-side endpoints are gated by the
          shared <code>SENTINEL_AGENT_KEY</code> service-to-service header
          rather than a user JWT — they're called by the agent process
          itself, not by humans.
        </p>

      <div className="docs-endpoint"><span className="docs-endpoint-method get">GET</span><span className="docs-endpoint-path">/api/sentinel/config</span></div>
      <p>Get the org&#39;s Sentinel AI config plus a <code>plan_gated</code> flag and the plan-aware monthly cap (100 for Pro, 500 for Pro Plus, 0 for free / past-due-too-long). Always returns 200 — non-eligible orgs get a read-only payload so the frontend can render the upgrade banner.</p>

      <div className="docs-endpoint"><span className="docs-endpoint-method patch">PATCH</span><span className="docs-endpoint-path">/api/sentinel/config</span></div>
      <p>Partial update — toggles, schedule mode + window + active days, motion cooldown, camera scope. Pro / Pro Plus only (returns 402 otherwise).</p>

      <div className="docs-endpoint"><span className="docs-endpoint-method get">GET</span><span className="docs-endpoint-path">/api/sentinel/runs</span></div>
      <p>List recent runs with stats — runs today / this month / total / pending / incidents filed, plus the plan-aware <code>monthly_cap</code> and <code>remaining_this_month</code>. Supports <code>limit</code>, <code>offset</code>, <code>trigger</code>, and <code>since</code> query params.</p>

      <div className="docs-endpoint"><span className="docs-endpoint-method get">GET</span><span className="docs-endpoint-path">/api/sentinel/runs/{"{run_id}"}</span></div>
      <p>Single run with the full agent tool trace — every tool call with arguments and (truncated) results.</p>

      <div className="docs-endpoint"><span className="docs-endpoint-method post">POST</span><span className="docs-endpoint-path">/api/sentinel/runs/manual</span></div>
      <p>Operator "Run now" — creates a pending manual run with an optional prompt and camera. Skips the schedule + scope gates (the operator overrode them by clicking) but still cap-enforced; returns 429 with the plan-aware cap when the org is at or over its monthly limit.</p>

      <div className="docs-endpoint"><span className="docs-endpoint-method get">GET</span><span className="docs-endpoint-path">/api/sentinel/runs/pending</span></div>
      <p>Service-to-service. The Sentinel AI polls this on wakeup to drain pending runs across all orgs (FIFO, oldest-first). Auth: <code>X-Sentinel-Agent-Key</code> header.</p>

      <div className="docs-endpoint"><span className="docs-endpoint-method post">POST</span><span className="docs-endpoint-path">/api/sentinel/runs/{"{run_id}"}/start</span></div>
      <p>Service-to-service. Agent claims a pending run — transitions <code>pending → running</code>. Idempotent.</p>

        <div className="docs-endpoint"><span className="docs-endpoint-method post">POST</span><span className="docs-endpoint-path">/api/sentinel/runs/{"{run_id}"}/complete</span></div>
        <p>Service-to-service. Agent posts the terminal outcome — <code>incident</code> (with severity + incident_id), <code>no_action</code>, or <code>error</code> — plus the full tool trace. Cross-checks that <code>incident_id</code> belongs to the run's org. Idempotent on terminal rows.</p>
      </details>

      <h3>MCP Endpoint</h3>
      <p>Streamable HTTP transport at <code>/mcp</code>. Authenticate with <code>Authorization: Bearer osc_...</code> header.</p>
      <p>See the <a href="#mcp">MCP Integration</a> section for setup and available tools.</p>
    </section>
  )
}

export default ApiReference
