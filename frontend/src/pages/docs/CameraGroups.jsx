import { Link } from "react-router-dom"


function CameraGroups() {
  return (
    <section className="docs-section" id="camera-groups">
      <h2>Camera Groups<a href="#camera-groups" className="docs-anchor">#</a></h2>
      <p>
        Camera groups are user-defined zones — &ldquo;Front yard&rdquo;,
        &ldquo;Workshop&rdquo;, &ldquo;Main floor&rdquo; — that bundle cameras
        together so AI agents can talk about a location instead of a list of
        camera IDs.
      </p>

      <h3>Creating a group</h3>
      <ol>
        <li>Open <Link to="/settings#settings-camera-groups">Settings &gt; Camera Groups</Link> (admin only).</li>
        <li>Click <strong>Create Your First Group</strong> (or <strong>+ New Group</strong> if some already exist), give it a name, color, and an optional emoji icon.</li>
        <li>Submit. The group becomes immediately visible to the agent via <code>list_camera_groups</code>.</li>
      </ol>

      <div className="docs-callout docs-callout-info">
        <p>
          <span className="docs-callout-icon">ℹ️</span>
          <span>
            <strong>Per-camera assignment is on the roadmap.</strong> Phase 1
            ships group creation + deletion. Phase 2 will add a group selector
            on each camera card so you can move cameras between groups from
            the dashboard. For now you can assign cameras programmatically via{" "}
            <code>PUT /api/cameras/&#123;camera_id&#125;/group</code>.
          </span>
        </p>
      </div>

      <h3>What groups do today</h3>
      <ul>
        <li>
          <strong>MCP navigation</strong> — Agents call{" "}
          <code>list_camera_groups</code> to resolve a natural-language
          location (&ldquo;check the workshop&rdquo;) to a set of{" "}
          <code>camera_id</code>s. This is the primary use case right now and
          the reason groups exist as a first-class concept.
        </li>
        <li>
          <strong>Sentinel scoping</strong> — Today the Sentinel page&apos;s
          camera scope is per-camera. Group-aware scope selectors are on the
          roadmap; once cameras can be assigned to groups, Sentinel will
          inherit group membership.
        </li>
      </ul>

      <h3>Managing groups via the API</h3>
      <p>
        With an admin Clerk session token, the same operations the Settings UI
        uses are available directly:
      </p>
      <ul>
        <li><code>GET /api/camera-groups</code> — list groups in your org (also available to members)</li>
        <li><code>POST /api/camera-groups</code> — create a group (<code>name</code>, <code>color</code>, <code>icon</code>)</li>
        <li><code>DELETE /api/camera-groups/&#123;id&#125;</code> — delete a group; member cameras are unassigned</li>
        <li><code>PUT /api/cameras/&#123;camera_id&#125;/group</code> — assign a camera to a group (or pass <code>group_id=null</code> to unassign)</li>
      </ul>

      <h3>Tips</h3>
      <ul>
        <li>
          Name groups by <em>place</em>, not purpose. &ldquo;Driveway&rdquo;
          stays meaningful as cameras come and go; &ldquo;Vehicle
          monitoring&rdquo; doesn&apos;t.
        </li>
        <li>
          Pick contrasting colors for adjacent zones — once dashboard tile
          color tagging ships (Phase 3), a 20-camera grid reads at a glance.
        </li>
        <li>
          A camera can only be in one group. If you need multi-group overlap,
          that&apos;s a planned feature (&ldquo;saved views&rdquo;).
        </li>
      </ul>
    </section>
  )
}

export default CameraGroups
