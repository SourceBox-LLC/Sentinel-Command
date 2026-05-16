import { Link } from "react-router-dom"


function Recording() {
  return (
    <section className="docs-section" id="recording">
      <h2>Recording &amp; Retention<a href="#recording" className="docs-anchor">#</a></h2>
      <p>
        Recording in Sentinel is <strong>node-local by design</strong>. Each
        camera has its own recording policy; CloudNode reconciles its in-memory
        recording state from the backend on every heartbeat (~30 s), and
        encrypted recording segments land in the SQLite database next to the
        binary. No video is uploaded for long-term storage — the cloud holds
        only the small in-memory segment buffer needed for live playback.
      </p>

      <h3>Recording modes</h3>
      <p>
        Per-camera since v0.1.43. The two modes are mutually exclusive — turning
        one on automatically turns the other off so the operator never has to
        reason about a conflicting policy.
      </p>
      <ul>
        <li><strong>Continuous 24/7</strong> — the camera records all the time the node is online. Best for high-value coverage and commercial deployments.</li>
        <li><strong>Scheduled</strong> — the camera records only during a defined wall-clock window (e.g. 18:00–06:00). Useful for residential after-hours coverage, business hours archiving, etc.</li>
        <li><strong>Off</strong> (default) — the camera streams live but writes nothing to durable storage.</li>
      </ul>

      <h3>Configure</h3>
      <ol>
        <li>Go to <Link to="/settings">Settings</Link> &gt; Camera Nodes.</li>
        <li>Each camera lives inside its node's card, below the storage bar.</li>
        <li>Toggle <strong>Continuous 24/7</strong> for always-on, or <strong>Scheduled Recording</strong> for a window.</li>
        <li>For Scheduled, the start/end inputs default to <code>08:00–17:00</code>; pick whatever you need.</li>
        <li>Nodes pick up the new policy on the next heartbeat (≤30 seconds), and segments start landing in the durable archive on the next 1-second segment rotation after that.</li>
      </ol>

      <h3>Time zone</h3>
      <p>
        Scheduled-recording windows are interpreted in the org's timezone, not
        UTC, so <code>08:00–17:00</code> means 8am to 5pm where the cameras
        physically live. Pick the zone in <Link to="/settings">Settings</Link>{" "}
        &gt; Time Zone (defaults to UTC for new orgs; one click to use the
        browser's detected zone). DST transitions are handled automatically —
        a schedule entered in <code>America/Los_Angeles</code> fires at the
        correct local hour year-round, no operator intervention at the
        spring-forward / fall-back boundaries.
      </p>

      <h3>Storage cap &amp; retention</h3>
      <p>
        Each node has a single <strong>storage cap</strong> chosen during the
        setup wizard. The wizard reads the host's free disk space and suggests
        a sensible default — 80% of free space, clamped to the historical
        64 GB ceiling, with a 5 GB floor. A node on a 32 GB Pi SD card gets
        ~25 GB; a node on a 1 TB drive gets the full 64 GB. The operator can
        override during setup or by re-running it.
      </p>
      <p>
        CloudNode enforces the cap every 5 minutes: when total stored bytes
        exceed it, oldest recording segments are deleted first (FIFO). The
        Settings dashboard surfaces a per-node usage bar with green / amber /
        red bands at 75% / 90% so operators can see how full each node is at
        a glance.
      </p>
      <p>
        Independently of the cap, CloudNode pauses recording writes when host
        free disk drops below a 1 GiB safety floor — a "don't blow up the
        host" guardrail that fires regardless of how the cap was set. Live
        streaming continues unchanged; only the durable archive is paused.
      </p>

      <h3>Manual record button</h3>
      <p>
        The per-camera record button on the dashboard is a thin wrapper that
        flips <code>continuous_24_7</code> on the camera. Same end state as
        toggling Continuous 24/7 in Settings — the heartbeat reconciler picks
        it up within one tick.
      </p>

      <h3>Playback</h3>
      <p>
        Recordings are browsable from the node's local HTTP server on port
        8080: <code>/recordings/list</code> returns the JSON list, and
        <code>/recordings/&#123;file&#125;</code> streams the bytes. Typically
        used from the Command Center dashboard in-app; for direct access you
        must be on the same local network as the node.
      </p>

      <div className="docs-callout docs-callout-info">
        <p>
          <span className="docs-callout-icon">🔒</span>
          <span>Because recordings never leave the node, even a Command Center
          compromise cannot expose your archive. Protect the node machine with
          the same rigor you'd apply to a physical NVR.</span>
        </p>
      </div>
    </section>
  )
}

export default Recording
