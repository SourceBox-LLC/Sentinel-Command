import { useDocs } from "./context"


function Mcp() {
  const { base, copyToClipboard } = useDocs()

  return (
    <section className="docs-section" id="mcp">
      <h2>MCP Integration<a href="#mcp" className="docs-anchor">#</a></h2>
      <div className="docs-callout docs-callout-info">
        <p>
          <span className="docs-callout-icon">✨</span>
          <span>MCP integration requires a <strong>Pro</strong> or <strong>Pro Plus</strong> plan.</span>
        </p>
      </div>
      <p>
        SourceBox Sentry supports the <strong>Model Context Protocol (MCP)</strong>, letting AI tools like
        Claude Code, Cursor, or custom agents interact with your cameras, nodes, and settings
        through natural language.
      </p>

      <h3>What is MCP?</h3>
      <p>
        MCP is an open protocol that lets AI assistants connect to external tools and data sources.
        When you connect an AI tool to SourceBox Sentry via MCP, it can list your cameras, check node
        status, get stream URLs, manage recording settings, and more — all through conversation.
      </p>

      <h3>Agent workflow</h3>
      <p>
        A typical agent session flows through three lanes. The agent drives the
        conversation and calls MCP tools; Command Center authenticates each call
        and routes it; CloudNode produces physical data (JPEGs, clip bytes)
        whenever a tool needs a live view of a camera.
      </p>
      <figure className="docs-diagram">
        <picture>
          <source srcSet="/images/mcp-workflow.webp" type="image/webp" />
          <img
            src="/images/mcp-workflow.jpg"
            alt="MCP agent workflow swimlane. AI AGENT lane drives 7 numbered tool calls: list_cameras, view_camera, watch_camera, create_incident, attach_snapshot, attach_clip, finalize_incident. COMMAND CENTER lane authenticates and dispatches each call. CLOUDNODE lane produces physical data (JPEG snapshot, JPEG burst, JPEG → DB, clip from cache) only when the tool needs it. Legend: green READ, amber VISUAL, purple WRITE."
            className="docs-diagram-image"
            width="1920"
            height="1080"
            loading="lazy"
          />
        </picture>
        <figcaption className="docs-diagram-caption">
          A typical agent loop. Each numbered step is a tool call. The agent drives the conversation; Command Center authenticates and dispatches; CloudNode produces image / clip bytes when the tool needs them.
        </figcaption>
      </figure>

      <h3>Setup</h3>
      <div className="docs-steps">
        <div className="docs-step">
          <div className="docs-step-number">1</div>
          <div className="docs-step-content">
            <h4>Generate an MCP API key</h4>
            <p>Go to the <strong>MCP</strong> page in your dashboard and click <strong>Generate Key</strong>. Save it — you won't see it again.</p>
          </div>
        </div>
        <div className="docs-step">
          <div className="docs-step-number">2</div>
          <div className="docs-step-content">
            <h4>Add to your AI tool</h4>
            <p>Add the following config to your Claude Code settings (<code>~/.claude.json</code>) or project <code>.mcp.json</code>:</p>
            <div className="docs-code-block">
              <code>{`{
  "mcpServers": {
    "opensentry": {
      "type": "http",
      "url": "${base}/mcp",
      "headers": {
        "Authorization": "Bearer osc_your_key_here"
      }
    }
  }
}`}</code>
              <button className="docs-copy-btn" onClick={() => copyToClipboard(`{
  "mcpServers": {
    "opensentry": {
      "type": "http",
      "url": "${base}/mcp",
      "headers": {
        "Authorization": "Bearer osc_your_key_here"
      }
    }
  }
}`)}>Copy</button>
            </div>
            <p>Or via CLI:</p>
            <div className="docs-code-block">
              <code>{`claude mcp add --transport http opensentry ${base}/mcp --header "Authorization: Bearer osc_your_key"`}</code>
              <button className="docs-copy-btn" onClick={() => copyToClipboard(`claude mcp add --transport http opensentry ${base}/mcp --header "Authorization: Bearer osc_your_key"`)}>Copy</button>
            </div>
          </div>
        </div>
        <div className="docs-step">
          <div className="docs-step-number">3</div>
          <div className="docs-step-content">
            <h4>Start using it</h4>
            <p>Ask your AI tool things like "list my cameras" or "get the stream URL for the garage cam".</p>
          </div>
        </div>
      </div>

      <h3>Available Tools</h3>
      <p className="docs-subtle">
        23 tools grouped by capability. VISUAL tools return images the model can look at,
        READ tools return structured data, and WRITE tools create or update state.
      </p>

      <details className="docs-mcp-category" open>
        <summary>
          <span className="docs-mcp-category-chevron" aria-hidden="true">▶</span>
          <span className="docs-mcp-category-title">Live viewing</span>
          <span className="docs-mcp-category-count">2 tools</span>
        </summary>
        <div className="docs-mcp-tools">
          <div className="docs-endpoint">
            <span className="docs-endpoint-method get">VISUAL</span>
            <span className="docs-endpoint-path">view_camera</span>
          </div>
          <p>See what a camera sees <em>right now</em> — returns a single live JPEG the agent can actually look at. Use for a one-shot situational check ("is anyone in the workshop?"). For motion or change over time, use <code>watch_camera</code>. To preserve what was seen, follow up with <code>attach_snapshot</code>.</p>

          <div className="docs-endpoint">
            <span className="docs-endpoint-method get">VISUAL</span>
            <span className="docs-endpoint-path">watch_camera</span>
          </div>
          <p>Burst of 2–10 snapshots from one camera, 1–30s apart. Use when a single <code>view_camera</code> frame isn't enough — to confirm whether a subject is moving, whether motion is sustained or fleeting, or whether something is returning to a scene. For longer evidence retention on an incident, use <code>attach_clip</code>.</p>
        </div>
      </details>

      <details className="docs-mcp-category">
        <summary>
          <span className="docs-mcp-category-chevron" aria-hidden="true">▶</span>
          <span className="docs-mcp-category-title">Cameras, nodes &amp; groups</span>
          <span className="docs-mcp-category-count">6 tools</span>
        </summary>
        <div className="docs-mcp-tools">
        <div className="docs-endpoint">
          <span className="docs-endpoint-method get">READ</span>
          <span className="docs-endpoint-path">list_cameras</span>
        </div>
        <p>Every camera in the org with status, codec, and group assignment. Start here when the agent doesn't yet know what cameras exist — most other camera tools take a <code>camera_id</code> from this output.</p>

        <div className="docs-endpoint">
          <span className="docs-endpoint-method get">READ</span>
          <span className="docs-endpoint-path">get_camera</span>
        </div>
        <p>Full metadata for one camera (status, codec, node, group, last seen). Use after <code>list_cameras</code> to inspect one closely. Returns text only — for the actual image, use <code>view_camera</code>.</p>

        <div className="docs-endpoint">
          <span className="docs-endpoint-method get">READ</span>
          <span className="docs-endpoint-path">get_stream_url</span>
        </div>
        <p>Returns the authenticated HLS playlist URL for a camera. This is a URL a human or HLS player can open — the agent <em>cannot</em> watch video from it. Use only when handing a stream URL back to the user. To see a frame, use <code>view_camera</code> or <code>watch_camera</code>.</p>

        <div className="docs-endpoint">
          <span className="docs-endpoint-method get">READ</span>
          <span className="docs-endpoint-path">list_nodes</span>
        </div>
        <p>Every CloudNode (the physical box running cameras on the local network) with status, hostname, and camera count. Use when troubleshooting at the box level — e.g. whether a whole node is offline vs whether one of its cameras is. For per-camera state, use <code>list_cameras</code>.</p>

        <div className="docs-endpoint">
          <span className="docs-endpoint-method get">READ</span>
          <span className="docs-endpoint-path">get_node</span>
        </div>
        <p>Full detail for one CloudNode by <code>node_id</code> (hostname, IP, port, status, camera count). Use after <code>list_nodes</code> when you need detail on one specific box — e.g. to confirm which physical device the user should power-cycle.</p>

        <div className="docs-endpoint">
          <span className="docs-endpoint-method get">READ</span>
          <span className="docs-endpoint-path">list_camera_groups</span>
        </div>
        <p>Camera groups defined in the dashboard — user-defined zones (e.g. "Front yard", "Workshop") that bundle cameras together. Use when the user names a place and you need to find which cameras live there.</p>
        </div>
      </details>

      <details className="docs-mcp-category">
        <summary>
          <span className="docs-mcp-category-chevron" aria-hidden="true">▶</span>
          <span className="docs-mcp-category-title">Settings, logs &amp; system</span>
          <span className="docs-mcp-category-count">5 tools</span>
        </summary>
        <div className="docs-mcp-tools">
        <div className="docs-endpoint">
          <span className="docs-endpoint-method get">READ</span>
          <span className="docs-endpoint-path">get_camera_recording_policy</span>
        </div>
        <p>A single camera's recording policy: <code>continuous_24_7</code>, <code>scheduled_recording</code>, and the scheduled start/end times (HH:MM in the org's timezone). Per-camera since v0.1.43 — replaced the previous org-level <code>get_recording_settings</code>. Use when the user asks "is the garage cam recording right now?" or before filing an incident if it matters whether the moment was being archived locally.</p>

        <div className="docs-endpoint">
          <span className="docs-endpoint-method post">WRITE</span>
          <span className="docs-endpoint-path">set_camera_recording_policy</span>
        </div>
        <p>Set the recording policy for a specific camera. Any field omitted is left unchanged (PATCH semantics). Use when the user asks "turn on recording for the garage cam" or "set the front door cam to record from 6pm to 6am". Times are HH:MM 24-hour in the org's timezone. Continuous and scheduled are mutually exclusive — passing both as <code>true</code> returns <code>{"{error: 'modes_conflict'}"}</code>.</p>

        <div className="docs-endpoint">
          <span className="docs-endpoint-method get">READ</span>
          <span className="docs-endpoint-path">get_stream_logs</span>
        </div>
        <p>Recent stream-access log entries (one row per user × camera × ~5min window). Use to audit who watched a sensitive camera, check whether a user reviewed a feed during a time of interest, or investigate suspicious viewing activity. Filter by <code>camera_id</code> to scope to one feed.</p>

        <div className="docs-endpoint">
          <span className="docs-endpoint-method get">READ</span>
          <span className="docs-endpoint-path">get_stream_stats</span>
        </div>
        <p>Aggregated stream-viewing stats over the last N days: totals, by-camera, and by-user. Use to find the most-watched cameras, build a usage summary, or establish a baseline before deciding whether a viewing pattern looks unusual. For per-event detail, use <code>get_stream_logs</code>.</p>

        <div className="docs-endpoint">
          <span className="docs-endpoint-method get">READ</span>
          <span className="docs-endpoint-path">get_system_status</span>
        </div>
        <p>High-level snapshot of the org's deployment: camera count with online/offline split, node count with online/offline split, and the active plan. Good first call to orient before drilling in. For per-camera detail, use <code>list_cameras</code>.</p>
        </div>
      </details>

      <details className="docs-mcp-category">
        <summary>
          <span className="docs-mcp-category-chevron" aria-hidden="true">▶</span>
          <span className="docs-mcp-category-title">Incident reports</span>
          <span className="docs-mcp-category-count">10 tools</span>
        </summary>
        <p className="docs-subtle docs-mcp-category-intro">
          Let the agent file, investigate, and read back structured incident reports.
          Everything written by these tools shows up on the <strong>Incident Reports</strong> page
          of the dashboard, alongside human-filed reports, for review.
        </p>
        <div className="docs-mcp-tools">
        <div className="docs-endpoint">
          <span className="docs-endpoint-method post">WRITE</span>
          <span className="docs-endpoint-path">create_incident</span>
        </div>
        <p>Open a new incident with a title, summary, severity, and (optionally) a primary camera. Returns the new <code>incident_id</code> to pass to follow-up tools.</p>

        <div className="docs-endpoint">
          <span className="docs-endpoint-method post">WRITE</span>
          <span className="docs-endpoint-path">attach_snapshot</span>
        </div>
        <p>Capture a fresh JPEG from a camera and store it as evidence on an incident. Good for freezing what you saw at the moment of investigation.</p>

        <div className="docs-endpoint">
          <span className="docs-endpoint-method post">WRITE</span>
          <span className="docs-endpoint-path">attach_clip</span>
        </div>
        <p>Save the most recent ~15 seconds of a camera's live buffer as a video clip on an incident. Pulls from the in-memory HLS cache (no recording is started) and stores a single .ts blob the human reviewer can play back from the dashboard.</p>

        <div className="docs-endpoint">
          <span className="docs-endpoint-method post">WRITE</span>
          <span className="docs-endpoint-path">add_observation</span>
        </div>
        <p>Append a free-form text observation (what you checked, what you ruled out) to an incident as you investigate.</p>

        <div className="docs-endpoint">
          <span className="docs-endpoint-method post">WRITE</span>
          <span className="docs-endpoint-path">update_incident</span>
        </div>
        <p>Edit fields on an existing incident: status, severity, short summary, or the long-form markdown report body. Pass only the fields to change. The <code>report</code> parameter REPLACES the existing body, so include the full revised text. Use this for revisions after new evidence — the first report write should go through <code>finalize_incident</code>.</p>

        <div className="docs-endpoint">
          <span className="docs-endpoint-method post">WRITE</span>
          <span className="docs-endpoint-path">finalize_incident</span>
        </div>
        <p>Write the long-form markdown report body for the <em>first</em> time at the end of an investigation, after snapshots/clips and observations are attached. For later revisions, use <code>update_incident</code> with its <code>report</code> parameter instead.</p>

        <div className="docs-endpoint">
          <span className="docs-endpoint-method get">READ</span>
          <span className="docs-endpoint-path">list_incidents</span>
        </div>
        <p>List previous incidents (most recent first) with optional filters for status, severity, or camera. Skips the full report body — use <code>get_incident</code> for detail.</p>

        <div className="docs-endpoint">
          <span className="docs-endpoint-method get">READ</span>
          <span className="docs-endpoint-path">get_incident</span>
        </div>
        <p>Fetch one incident's full detail: summary, markdown report, observations, and evidence metadata (with ids to pass to <code>get_incident_snapshot</code>).</p>

        <div className="docs-endpoint">
          <span className="docs-endpoint-method get">VISUAL</span>
          <span className="docs-endpoint-path">get_incident_snapshot</span>
        </div>
        <p>Fetch a snapshot image that was previously attached to an incident as evidence so the agent can actually see what was captured.</p>

        <div className="docs-endpoint">
          <span className="docs-endpoint-method get">READ</span>
          <span className="docs-endpoint-path">get_incident_clip</span>
        </div>
        <p>Read metadata about a clip (size, approximate duration, mime, source camera) previously attached with <code>attach_clip</code>. Agents can't watch video, but this confirms the clip is saved and tells the human reviewer what to expect.</p>
        </div>
      </details>
    </section>
  )
}

export default Mcp
