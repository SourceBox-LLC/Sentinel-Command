function Dashboard() {
  return (
    <section className="docs-section" id="dashboard">
      <h2>Dashboard & Features<a href="#dashboard" className="docs-anchor">#</a></h2>
      <p>The Command Center web dashboard is where your team actually uses the system. It's organized into a few main areas.</p>

      <figure className="docs-diagram">
        <picture>
          <source srcSet="/images/dashboard-ia.webp" type="image/webp" />
          <img
            src="/images/dashboard-ia.jpg"
            alt="Dashboard information architecture tree. Root: Dashboard (opensentry-command.fly.dev). Four children: Live view (Camera tiles, Fullscreen, Snapshot capture, Manual record); Settings — admin-only (Node Management, Recording Policy, Organization, Subscription, Danger Zone); Admin — admin-only (Stream Access Logs, Usage Statistics, MCP Tool Activity, System Health); Incidents (Open incidents, Evidence viewer, Markdown reports, Triage actions)."
            className="docs-diagram-image"
            width="1920"
            height="1080"
            loading="lazy"
          />
        </picture>
        <figcaption className="docs-diagram-caption">
          The dashboard splits into four top-level sections. Admin-only branches are gated on the Pro / Pro Plus plan.
        </figcaption>
      </figure>

      <h3>Live view</h3>
      <p>
        The default page after sign-in. Every camera appears as a tile with a live
        status pill (online, recording, starting, offline, suspended, etc.) and an
        HLS player you can expand. Each tile shows its node name for quick
        identification, plus a color-coded group pill if the camera has been assigned
        to a <a href="#camera-groups">camera group</a>.
      </p>
      <ul>
        <li><strong>Live Streams</strong> — HLS video served same-origin through the Command Center proxy, JWT-authenticated per viewer. Starts at the live edge.</li>
        <li><strong>Snapshots</strong> — Click the camera icon to capture a single JPEG and save it on the node. Shows up in the node's snapshots list.</li>
        <li><strong>Recording</strong> — Manual start/stop per camera. Recordings are stored locally on the node in the encrypted SQLite database.</li>
        <li><strong>Fullscreen</strong> — Click a tile to expand to full screen.</li>
        <li><strong>Group filter</strong> — When the org has at least one camera group, a pill row above the grid lets you scope the live view to <em>All</em>, a specific group, or <em>Ungrouped</em>. Each grouped tile gets a colored top stripe and a group-name pill in its header so a 20-camera grid reads at a glance.</li>
      </ul>

      <h3>Settings</h3>
      <p>Configure your org, nodes, and recording policy. Admin-only.</p>
      <ul>
        <li><strong>Node Management</strong> — Create nodes, copy API keys at creation time, rotate keys, delete nodes (cascades to cameras).</li>
        <li><strong>Per-camera recording</strong> — each camera card inside its node has Continuous 24/7 + Scheduled Recording toggles (mutually exclusive). The node card shows a storage usage bar against the operator-chosen cap. See the <a href="#recording">Recording</a> section below.</li>
        <li><strong>Time Zone</strong> — set the org's IANA timezone so scheduled-recording windows are interpreted as local wall-clock time, not UTC. DST handled automatically.</li>
        <li><strong>Organization</strong> — Invite members, manage roles (Admin vs Member), view resource usage relative to plan caps.</li>
        <li><strong>Subscription</strong> — Current plan, usage bars for cameras and nodes, and an upgrade/downgrade flow.</li>
        <li><strong>Danger Zone</strong> — <em>Full Organization Reset</em> (the GDPR Article 17 right-to-erasure path) is available on every plan, including Free.  <em>Wipe Logs</em> (selective stream + MCP activity log purge, keeps the org running) is Pro / Pro Plus only.  Both require a typed confirmation in the modal.</li>
      </ul>

      <h3>Admin dashboard</h3>
      <p>Pro and Pro Plus plans unlock a separate Admin dashboard for auditing and analytics:</p>
      <ul>
        <li><strong>Stream Access Logs</strong> — Who watched which camera, from which IP, at what time. One row per user × camera × ~5-minute window.</li>
        <li><strong>Usage Statistics</strong> — Views by camera, by user, and by day. Useful to see which feeds matter and which are dormant.</li>
        <li><strong>MCP Tool Activity</strong> — Every tool call made by a connected AI client: which key, which tool, what it did, whether it succeeded.</li>
        <li><strong>System Health</strong> — Online vs offline camera counts, node heartbeat ages, segment cache status.</li>
      </ul>

      <h3>AI incident reports</h3>
      <p>
        Two paths produce AI-authored incidents: an external <a href="#mcp">MCP client</a>{" "}
        (Claude Code, Cursor, a custom agent) writing through the MCP tool surface, or
        the built-in <a href="#sentinel">Sentinel AI</a> firing autonomously on motion
        or incident_opened events. Each report has a severity, status, markdown
        write-up, attached snapshots, video clips, and a timeline of observations —
        all editable from the standalone <strong>Incident Reports</strong> page
        (<code>/incidents</code>), where AI- and human-filed reports sit side-by-side
        with a source filter.
      </p>
      <figure className="docs-diagram">
        <picture>
          <source srcSet="/images/incident-lifecycle.webp" type="image/webp" />
          <img
            src="/images/incident-lifecycle.jpg"
            alt="Incident lifecycle: Create (title, severity, camera) → Investigate (attach evidence + notes) → Finalize (markdown report body) → Review (human triage) → Resolve or Dismiss. Investigate has parallel branches: Evidence (snapshot, clip) above, and Notes & Revisions (observation, update_incident) below."
            className="docs-diagram-image"
            width="1920"
            height="1080"
            loading="lazy"
          />
        </picture>
        <figcaption className="docs-diagram-caption">
          An incident is an append-only record until it's reviewed. Agents attach evidence as they investigate; the finalize call seals the markdown report body. Humans make the close call.
        </figcaption>
      </figure>
      <ul>
        <li><strong>Create</strong> — Agents open an incident when they notice something worth
          flagging (possible intruder, equipment fault, unexpected motion).</li>
        <li><strong>Investigate</strong> — They can attach fresh JPEG snapshots from any camera,
          save short video clips from a camera's recent live buffer, and log text observations
          as they check other feeds.</li>
        <li><strong>Finalize</strong> — A markdown report is written at the end with what was
          seen, what was ruled out, and any recommended actions.</li>
        <li><strong>Review</strong> — Humans open <strong>Incident Reports</strong>, read the report, view the
          evidence thumbnails, play back the captured clips, and mark each incident
          acknowledged, resolved, or dismissed.</li>
        <li><strong>Look back</strong> — Agents can also list and re-read past incidents
          (including fetching their snapshots and clip metadata) so they can follow up without
          losing context.</li>
      </ul>
      <p className="docs-subtle">
        Requires MCP access (Pro or Pro Plus) and an MCP API key. See
        the <a href="#mcp">MCP Integration</a> section for setup.
      </p>
    </section>
  )
}

export default Dashboard
