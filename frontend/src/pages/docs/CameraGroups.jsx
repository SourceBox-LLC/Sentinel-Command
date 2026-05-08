function CameraGroups() {
  return (
    <section className="docs-section" id="camera-groups">
      <h2>Camera Groups<a href="#camera-groups" className="docs-anchor">#</a></h2>
      <p>
        Camera groups are an organizational primitive — "Front yard", "Workshop",
        "Main floor" — that bundle cameras together so AI agents (and
        eventually a settings UI) can talk about a location instead of a
        list of camera IDs.
      </p>

      <div className="docs-callout docs-callout-info">
        <p>
          <span className="docs-callout-icon">ℹ️</span>
          <span>
            <strong>Today, groups are MCP / API-only.</strong> A management
            UI inside Settings is planned but not yet shipped. Until then,
            groups are created and assigned via the REST API directly — see
            below — or simply ignored if you don't have an agent workflow
            that needs them.
          </span>
        </p>
      </div>

      <h3>What groups do today</h3>
      <ul>
        <li>
          <strong>MCP navigation</strong> — Agents call <code>list_camera_groups</code> to
          resolve a natural-language location ("check the workshop") to a
          set of <code>camera_id</code>s. This is the primary use case
          right now.
        </li>
        <li>
          <strong>Sentinel scoping</strong> — Camera scope on the Sentinel
          page is per-camera; group-aware scope selectors are on the
          roadmap.
        </li>
      </ul>

      <h3>Managing groups via the API</h3>
      <p>
        With an admin Clerk session token, the following endpoints are
        available:
      </p>
      <ul>
        <li><code>GET /api/camera-groups</code> — list groups in your org</li>
        <li><code>POST /api/camera-groups</code> — create a group (<code>name</code>, <code>color</code>, <code>icon</code>)</li>
        <li><code>DELETE /api/camera-groups/&#123;id&#125;</code> — delete a group; member cameras are unassigned</li>
        <li><code>PUT /api/cameras/&#123;camera_id&#125;/group</code> — assign a camera to a group (or pass <code>group_id=null</code> to unassign)</li>
      </ul>

      <h3>Tips for when the UI lands</h3>
      <ul>
        <li>
          Name groups by <em>place</em>, not purpose. "Driveway" stays
          meaningful as cameras come and go; "Vehicle monitoring" doesn't.
        </li>
        <li>
          A camera can only be in one group. If you need multi-group
          overlap, that's a planned feature ("saved views").
        </li>
      </ul>
    </section>
  )
}

export default CameraGroups
