import { OsTabs } from "./context"


function GettingStarted() {
  return (
    <section className="docs-section" id="getting-started">
      <h2>Getting Started<a href="#getting-started" className="docs-anchor">#</a></h2>
      <p>
        Sentinel turns any USB webcam into a cloud-connected security camera without
        router changes, VPNs, or third-party cloud storage in the live video path.
        You bring the camera and a machine to plug it into; we handle streaming,
        storage, access control, and agentic review.
      </p>

      <h3>The two pieces</h3>
      <ul>
        <li><strong>Command Center</strong> — The web dashboard at <code>opensentry-command.fly.dev</code>.
          Lives in the cloud. Hosts your account, cameras, settings, recording
          schedule, audit logs, incident reports, and the MCP endpoint for AI
          assistants. You don't install anything for Command Center — just sign in.</li>
        <li><strong>CloudNode</strong> — A small Rust application you install on a machine
          next to your cameras. It detects USB webcams, encodes their video with
          FFmpeg, and pushes 1-second HLS segments over outbound HTTPS to Command
          Center. It also runs a local terminal dashboard so you can watch what
          it's doing.</li>
      </ul>

      <h3>Core concepts</h3>
      <div className="docs-concepts-grid">
        <div className="docs-concept">
          <h4>Organization</h4>
          <p>A tenant in Command Center. Every camera, node, user, incident, and
          MCP key is scoped to one org. Admins can invite members and manage
          billing; members can view and operate cameras.</p>
        </div>
        <div className="docs-concept">
          <h4>Node</h4>
          <p>A single CloudNode install. A node has a unique <code>node_id</code>, an
          encrypted API key, and one or more cameras. One node per machine is
          the normal deployment.</p>
        </div>
        <div className="docs-concept">
          <h4>Camera</h4>
          <p>A USB webcam discovered by a node. Cameras appear in the dashboard
          automatically when the node comes online and register their codec on
          first segment.</p>
        </div>
        <div className="docs-concept">
          <h4>Segment</h4>
          <p>A <code>.ts</code> HLS video chunk — 1 second by default. CloudNode emits
          a new one every second and pushes it to Command Center, which caches
          roughly 60 at a time per camera in RAM for low-latency playback.</p>
        </div>
        <div className="docs-concept">
          <h4>Incident</h4>
          <p>A structured report file opened by a human or an AI agent. Holds a
          severity, status, markdown write-up, attached snapshots and clips, and
          a timeline of observations. Shows up on the Incident Reports page.</p>
        </div>
        <div className="docs-concept">
          <h4>MCP key</h4>
          <p>An API key that authorizes an outside AI client (Claude Code, Cursor,
          custom agents) to call the org's MCP tools. Separate from CloudNode
          API keys. Revocable and auditable.</p>
        </div>
      </div>

      <h3>Prerequisites</h3>
      <ul>
        <li>A USB webcam (built-in laptop cameras work too)</li>
        <li>A Sentinel account (free tier covers up to 5 cameras across 2 nodes, with 30 viewer-hours/month of live playback)</li>
        <li>A Linux, Windows, or macOS machine for CloudNode</li>
        <li>FFmpeg installed (or Docker) — the setup wizard offers to install it via your OS package manager (<code>winget</code> on Windows, <code>brew</code> on macOS, <code>apt</code>/<code>dnf</code>/<code>pacman</code> on Linux)</li>
        <li>Outbound HTTPS access from the CloudNode machine to the internet</li>
      </ul>

      <h3>Quick Setup</h3>
      <div className="docs-steps">
        <div className="docs-step">
          <div className="docs-step-number">1</div>
          <div className="docs-step-content">
            <h4>Create an account</h4>
            <p>Visit <code>opensentry-command.fly.dev</code>, sign up, and create your organization. You can invite team members later.</p>
          </div>
        </div>
        <div className="docs-step">
          <div className="docs-step-number">2</div>
          <div className="docs-step-content">
            <h4>Create a node and get your API key</h4>
            <p>Go to <strong>Settings</strong>, click <strong>Add Node</strong>, name it, and copy the API key. Save it — you won't see it again.</p>
          </div>
        </div>
        <div className="docs-step">
          <div className="docs-step-number">3</div>
          <div className="docs-step-content">
            <h4>Install CloudNode</h4>
            <OsTabs id="qs" />
            <p>The installer downloads CloudNode and walks you through setup. If FFmpeg isn't already on the system, the wizard offers to install it via your OS package manager.</p>
          </div>
        </div>
        <div className="docs-step">
          <div className="docs-step-number">4</div>
          <div className="docs-step-content">
            <h4>View your camera</h4>
            <p>Once CloudNode is running, your camera appears on the dashboard automatically. Click it to watch the live HLS stream.</p>
          </div>
        </div>
      </div>

      <div className="docs-callout docs-callout-info">
        <p>
          <span className="docs-callout-icon">ℹ️</span>
          <span>CloudNode only makes <strong>outbound</strong> HTTPS connections. No inbound ports, no port forwarding, no VPN required.</span>
        </p>
      </div>
    </section>
  )
}

export default GettingStarted
