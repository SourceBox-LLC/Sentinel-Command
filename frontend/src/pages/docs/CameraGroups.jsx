import { Link } from "react-router-dom"


function CameraGroups() {
  return (
    <section className="docs-section" id="camera-groups">
      <h2>Camera Groups<a href="#camera-groups" className="docs-anchor">#</a></h2>
      <p>
        Camera groups are user-defined zones — &ldquo;Front yard&rdquo;,
        &ldquo;Workshop&rdquo;, &ldquo;Main floor&rdquo; — that bundle cameras
        together for filtering, color tagging, and AI-agent navigation.
      </p>

      <h3>Creating a group</h3>
      <ol>
        <li>Open <Link to="/settings#settings-camera-groups">Settings &gt; Camera Groups</Link> (admin only).</li>
        <li>Click <strong>Create Your First Group</strong> (or <strong>+ New Group</strong> if some already exist), give it a name, color, and an optional emoji icon.</li>
        <li>Submit. The group becomes immediately visible to the agent via <code>list_camera_groups</code>.</li>
      </ol>

      <h3>Assigning cameras</h3>
      <ol>
        <li>Stay in <Link to="/settings#nodes">Settings &gt; Camera Nodes</Link>.</li>
        <li>Find the camera you want to group. Each camera card has a <strong>Group</strong> dropdown alongside its recording-policy toggles.</li>
        <li>Pick a group from the dropdown — the assignment saves immediately. Pick &ldquo;(no group)&rdquo; to unassign.</li>
      </ol>

      <h3>What groups do for you</h3>
      <ul>
        <li><strong>Live view layout</strong> — Camera tiles on the dashboard get a colored top stripe matching their group. A 20-camera grid reads at a glance.</li>
        <li><strong>Color-coded pill</strong> — Each tile shows a small pill in its header with the group icon + name, tinted in the group color. The dashboard filter row above the grid uses the same color swatches.</li>
        <li><strong>Filter</strong> — A pill row at the top of the dashboard lets you scope the live view to <em>All</em>, a specific group, or <em>Ungrouped</em>. State is local to the session — no setting to save.</li>
        <li><strong>MCP navigation</strong> — Agents call <code>list_camera_groups</code> to resolve a natural-language location (&ldquo;check the workshop&rdquo;) to a set of <code>camera_id</code>s. Sentinel uses this for location-aware investigations.</li>
      </ul>

      <h3>Tips</h3>
      <ul>
        <li>
          Name groups by <em>place</em>, not purpose. &ldquo;Driveway&rdquo;
          stays meaningful as cameras come and go; &ldquo;Vehicle
          monitoring&rdquo; doesn&apos;t.
        </li>
        <li>
          Pick contrasting colors for adjacent zones — the dashboard top
          stripes are the easiest signal at a glance.
        </li>
        <li>
          A camera can only be in one group. If you need multi-group
          overlap, that&apos;s a planned feature (&ldquo;saved views&rdquo;).
        </li>
        <li>
          Deleting a group unassigns its cameras (they don&apos;t get
          deleted) — useful when reorganizing zones.
        </li>
      </ul>

      <h3>Managing groups via the API</h3>
      <p>
        For automation, the same operations the UI uses are exposed as REST
        endpoints (admin-scoped Clerk session token required for writes):
      </p>
      <ul>
        <li><code>GET /api/camera-groups</code> — list groups in your org</li>
        <li><code>POST /api/camera-groups</code> — create a group (<code>name</code>, <code>color</code>, <code>icon</code>)</li>
        <li><code>DELETE /api/camera-groups/&#123;id&#125;</code> — delete a group; member cameras are unassigned</li>
        <li><code>PUT /api/cameras/&#123;camera_id&#125;/group?group_id=N</code> — assign a camera to a group (omit <code>group_id</code> to unassign)</li>
      </ul>
    </section>
  )
}

export default CameraGroups
