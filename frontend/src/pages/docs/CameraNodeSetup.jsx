import { OsTabs, useDocs } from "./context"


function CameraNodeSetup() {
  const { os, copyToClipboard } = useDocs()

  return (
    <section className="docs-section" id="cameranode-setup">
      <h2>CameraNode Setup<a href="#cameranode-setup" className="docs-anchor">#</a></h2>
      <p>
        CameraNode is a Rust application that captures video from USB cameras,
        encodes it as HLS segments with FFmpeg, and pushes them to the Command Center
        backend, which serves them to viewers from an in-memory cache.
      </p>

      <div className="docs-callout">
        <strong>Don't want a Command Center account?</strong> CameraNode also runs
        in <strong>Local-only mode</strong> — a free, LAN-only product with live
        viewing, snapshots, recording, and recording playback in any browser at{' '}
        <code>http://&lt;node-ip&gt;:8080/</code>. No account, no cloud, no pairing.
        Pick "Local-only" at the wizard's first prompt; everything below applies
        to <em>Connected mode</em> (the SaaS-paired flow). See the{' '}
        <a
          href="https://github.com/SourceBox-LLC/Sentinel-CameraNode/blob/master/docs/runbooks/local-mode-setup.md"
          target="_blank"
          rel="noopener noreferrer"
        >
          Local-mode setup runbook
        </a>{' '}
        for the standalone walkthrough.
      </div>

      <figure className="docs-diagram">
        <picture>
          <source srcSet="/images/setup-wizard.webp" type="image/webp" />
          <img
            src="/images/setup-wizard.jpg"
            alt="CameraNode setup wizard 5-step flow: Prerequisites, Configuration, Install, Verify, Launch — left to right. Annotation branches: 'IF FFMPEG MISSING' (winget on Windows, brew on macOS, apt/dnf/pacman on Linux) above step 1; 'ENCODER AUTO-DETECT' (NVENC/NVIDIA, QSV/Intel, AMF/AMD, libx264 fallback) below step 3."
            className="docs-diagram-image"
            width="2752"
            height="1536"
            loading="lazy"
          />
        </picture>
        <figcaption className="docs-diagram-caption">
          The wizard runs once per machine. Steps 1–3 set you up, step 4 round-trips a credential check against Command Center, and step 5 hands the node off to live operation. The two annotated branches show where the wizard makes choices for you — installing FFmpeg via the OS package manager and picking the best available hardware encoder.
        </figcaption>
      </figure>

      <h3>Installation</h3>
      <OsTabs id="cn" />
      <p style={{ marginTop: '0.75rem', fontSize: '0.9rem', color: 'var(--text-muted)' }}>
        {os === 'windows'
          ? 'After the MSI finishes, click the Sentinel CameraNode shortcut from the Start menu — first launch runs the setup wizard, every launch after streams cameras directly.'
          : 'Run in your terminal. The script downloads the binary and registers the node. After it finishes, run the binary directly to start the foreground dashboard — same recommended path as the Windows MSI Start menu shortcut.'}
      </p>

      <h3>Setup Wizard</h3>
      <p>
        On Windows the Start menu shortcut launches the wizard automatically the first time.
        On Linux/macOS the install script invokes it inline. To re-run the wizard later
        (e.g. to re-enrol or change the API URL):
      </p>
      <div className="docs-code-block">
        <code>{os === 'windows' ? 'sourcebox-sentry-cameranode.exe setup' : 'sourcebox-sentry-cameranode setup'}</code>
        <button className="docs-copy-btn" onClick={() => copyToClipboard(os === 'windows' ? 'sourcebox-sentry-cameranode.exe setup' : 'sourcebox-sentry-cameranode setup')}>Copy</button>
      </div>
      <p>The wizard walks through five steps:</p>
      <ol>
        <li>
          <strong>Prerequisites</strong> — detects platform, finds your USB cameras, verifies FFmpeg.
          If FFmpeg isn't on PATH the wizard offers to install it via the OS package manager:{' '}
          <code>winget install Gyan.FFmpeg</code> on Windows, <code>brew install ffmpeg</code> on
          macOS, the matching <code>apt</code> / <code>dnf</code> / <code>pacman</code> command
          on Linux. CameraNode always uses the system FFmpeg — there is no bundled copy.
        </li>
        <li><strong>Configuration</strong> — prompts for your Node ID + API key (from Command Center → Settings → Add Node).</li>
        <li><strong>Install</strong> — saves the encrypted config and detects the best video encoder (NVENC / QSV / AMF, or libx264 fallback).</li>
        <li><strong>Verify</strong> — round-trips a credential check against Command Center.</li>
        <li><strong>Launch</strong> — optionally auto-starts the node.</li>
      </ol>

      {os === 'windows' && (
        <>
          <h3>Running on Windows</h3>
          <p>
            The Start menu shortcut launches CameraNode as a foreground app — a terminal window
            opens with the live dashboard, FFmpeg starts pushing segments, and the node stays
            online for as long as the window is open. You can see what's happening, hit a slash
            command, and close it cleanly.
          </p>

          <h3>Uninstalling</h3>
          <p>
            Use <strong>Settings → Apps → Installed apps → Sentinel CameraNode → Uninstall</strong>.
            That removes the binary and wipes <code>C:\ProgramData\SourceBoxSentry\</code> —
            including your encrypted config and recordings. FFmpeg installed via{' '}
            <code>winget</code> stays put because it's a separate package owned by the OS
            package manager.
          </p>
          <p style={{ fontSize: '0.9rem', color: 'var(--text-muted)' }}>
            Upgrades (re-running a newer MSI) preserve everything under ProgramData; only an
            explicit uninstall wipes it.
          </p>

          <p>
            See the CameraNode <a href="https://github.com/SourceBox-LLC/Sentinel-CameraNode#quick-start" target="_blank" rel="noopener noreferrer">README</a> for the full reference.
          </p>
        </>
      )}

      {os !== 'windows' && (
        <>
          <h3>Running on Linux / macOS</h3>
          <p>
            After <code>install.sh</code> finishes, run the binary directly to launch the
            foreground TUI dashboard:
          </p>
          <div className="docs-code-block">
            <code>~/.sourcebox-sentry/sourcebox-sentry-cameranode</code>
            <button className="docs-copy-btn" onClick={() => copyToClipboard("~/.sourcebox-sentry/sourcebox-sentry-cameranode")}>Copy</button>
          </div>
          <p>
            A terminal dashboard opens with live cameras, segment counts, FFmpeg state, and
            slash commands. The node stays online for as long as the window is open — you can
            see what's happening, hit a slash command, and close it cleanly with Ctrl+C.
          </p>
        </>
      )}

      <h3>Configuration</h3>
      <p>
        CameraNode stores all configuration in a local SQLite database. Resolution order:
      </p>
      <ul>
        <li><code>$SOURCEBOX_SENTRY_DATA_DIR/node.db</code> if the env var is set (Docker)</li>
        <li><code>./data/node.db</code> if it already exists — Linux/macOS only, for legacy <code>cargo build</code> installs (Windows always uses the platform default below)</li>
        <li><code>C:\ProgramData\SourceBoxSentry\node.db</code> on Windows</li>
        <li><code>./data/node.db</code> otherwise (fresh manual install on Linux/macOS)</li>
      </ul>
      <p>The API key is encrypted at rest. Key settings:</p>
      <ul>
        <li><code>node_id</code> — Unique identifier assigned by Command Center</li>
        <li><code>api_key</code> — Authentication key (encrypted at rest)</li>
        <li><code>api_url</code> — Command Center URL</li>
        <li><code>encoder</code> — Hardware encoder auto-detected: NVENC, QSV, AMF, or falls back to libx264</li>
      </ul>

      <h3>Running</h3>
      <div className="docs-code-block">
        <code>{os === 'windows' ? '.\\sourcebox-sentry-cameranode.exe' : './sourcebox-sentry-cameranode'}</code>
        <button className="docs-copy-btn" onClick={() => copyToClipboard(os === 'windows' ? '.\\sourcebox-sentry-cameranode.exe' : './sourcebox-sentry-cameranode')}>Copy</button>
      </div>
      <p>CameraNode auto-detects connected USB cameras and starts streaming immediately.</p>

      <h3>What CameraNode Does</h3>
      <ul>
        <li>Discovers USB cameras and registers them with Command Center</li>
        <li>Captures video and encodes HLS segments (1-second chunks by default) via FFmpeg</li>
        <li>Pushes segments directly to the Command Center backend over authenticated HTTPS</li>
        <li>Sends heartbeats every 30 seconds to report camera status</li>
        <li>Auto-detects video/audio codecs and reports them to Command Center</li>
        <li>Supports hardware-accelerated encoding on NVIDIA, Intel, and AMD GPUs</li>
        <li>Stores recordings and snapshots locally in an encrypted SQLite database</li>
        <li>Runs a live terminal dashboard with slash commands and log viewer</li>
      </ul>
    </section>
  )
}

export default CameraNodeSetup
