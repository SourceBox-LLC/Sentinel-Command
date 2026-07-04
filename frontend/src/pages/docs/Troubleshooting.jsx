import { Link } from "react-router-dom"
import { useDocs } from "./context"


function Troubleshooting() {
  const { copyToClipboard } = useDocs()

  return (
    <section className="docs-section" id="troubleshooting">
      <h2>Troubleshooting<a href="#troubleshooting" className="docs-anchor">#</a></h2>
      <p>
        The most common things that go wrong and how to fix them. Click any
        symptom below to expand. Browser find-in-page (Ctrl/⌘+F) auto-opens
        whichever entry matches your search.
      </p>

      <details className="docs-accordion">
        <summary>
          <span className="docs-accordion-chevron" aria-hidden="true">▶</span>
          <span className="docs-accordion-title">No cameras detected</span>
        </summary>
        <p><strong>Linux:</strong> make sure your user can read video devices:</p>
        <div className="docs-code-block">
          <code>{`ls -l /dev/video*
# Should show crw-rw---- root video

sudo usermod -a -G video $USER
# Log out and back in`}</code>
          <button className="docs-copy-btn" onClick={() => copyToClipboard(`sudo usermod -a -G video $USER`)}>Copy</button>
        </div>
        <p><strong>Windows:</strong> close any other app using the camera (Zoom, Teams, browser tab with <code>getUserMedia</code>). DirectShow only allows one exclusive consumer per camera.</p>
        <p><strong>macOS:</strong> grant camera access in <strong>System Settings &gt; Privacy & Security &gt; Camera</strong> — you'll need to approve the terminal app running CameraNode.</p>
      </details>

      <details className="docs-accordion">
        <summary>
          <span className="docs-accordion-chevron" aria-hidden="true">▶</span>
          <span className="docs-accordion-title">FFmpeg not found</span>
        </summary>
        <p>CameraNode looks for FFmpeg on PATH. Install it via your OS package manager:</p>
        <div className="docs-code-block">
          <code>{`winget install Gyan.FFmpeg     # Windows
brew install ffmpeg            # macOS
sudo apt install ffmpeg        # Ubuntu / Debian
sudo dnf install ffmpeg        # Fedora
sudo pacman -S ffmpeg          # Arch`}</code>
          <button className="docs-copy-btn" onClick={() => copyToClipboard('winget install Gyan.FFmpeg')}>Copy</button>
        </div>
        <p>
          Re-run <code>sourcebox-sentry-cameranode setup</code> — the wizard offers to run the right
          command for your platform if FFmpeg still isn't on PATH. After a Windows winget install,
          open a new terminal so the updated PATH is picked up.
        </p>
      </details>

      <details className="docs-accordion">
        <summary>
          <span className="docs-accordion-chevron" aria-hidden="true">▶</span>
          <span className="docs-accordion-title">Stream won't play in the dashboard</span>
        </summary>
        <ol>
          <li>Confirm the node's local server is reachable: <code>curl http://localhost:8080/health</code></li>
          <li>Watch the dashboard log panel for FFmpeg errors (red lines)</li>
          <li>Confirm segments are being created: <code>ls data/hls/&#123;camera_id&#125;/</code> should show new <code>.ts</code> files every second</li>
          <li>Look for <code>Pushed segment …</code> lines — those confirm segments are reaching Command Center</li>
          <li>From settings, run <code>/export-logs</code> and open the file for a full diagnostic</li>
        </ol>
      </details>

      <details className="docs-accordion">
        <summary>
          <span className="docs-accordion-chevron" aria-hidden="true">▶</span>
          <span className="docs-accordion-title">Node shows offline in Command Center</span>
        </summary>
        <p>Command Center marks a node offline if no heartbeat arrives for 90 seconds. Things to check:</p>
        <ul>
          <li>Is the node process actually running? <code>ps aux | grep cameranode</code> or check the terminal dashboard</li>
          <li>Does outbound HTTPS to Command Center work? <code>curl -I https://sentinel-command.com/api/health</code> should return 200</li>
          <li>Is the API URL correct in the node config? Open settings via the dashboard <code>/settings</code> command</li>
          <li>Clock skew: if the machine's clock is wildly off, JWT auth may fail. Enable NTP.</li>
          <li>Firewall / egress filter dropping long-lived TLS connections? Confirm by tailing CameraNode logs for repeated connect-then-drop cycles, then ask your network admin to allow outbound 443 to Command Center.</li>
        </ul>
      </details>

      <details className="docs-accordion">
        <summary>
          <span className="docs-accordion-chevron" aria-hidden="true">▶</span>
          <span className="docs-accordion-title">Segments not appearing in the browser (but node says it's pushing)</span>
        </summary>
        <ul>
          <li>Refresh the live view — hls.js will retry playlist fetch on reload</li>
          <li>Open browser devtools &gt; Network, filter by <code>.ts</code>, and check for 401/403 errors — that means the viewer's JWT expired. Signing out and back in refreshes it.</li>
          <li>Confirm the camera is online in <code>list_cameras</code>; if it flipped to offline between the push and the fetch, the cache may have been evicted</li>
          <li>If those don't surface anything, open a GitHub issue with the <code>request_id</code> from the failing response (visible in any error body) — we can correlate that to backend logs.</li>
        </ul>
      </details>

      <details className="docs-accordion">
        <summary>
          <span className="docs-accordion-chevron" aria-hidden="true">▶</span>
          <span className="docs-accordion-title">MCP tool calls return 401</span>
        </summary>
        <ul>
          <li>Double-check the <code>Authorization: Bearer osc_...</code> header — keys start with the <code>osc_</code> prefix</li>
          <li>Confirm the key is still active in <Link to="/mcp">MCP Control Center</Link></li>
          <li>Your plan must be Pro or Pro Plus — Free accounts cannot use MCP</li>
          <li>If you rotated the key, update the AI client config to match</li>
        </ul>
      </details>

      <details className="docs-accordion">
        <summary>
          <span className="docs-accordion-chevron" aria-hidden="true">▶</span>
          <span className="docs-accordion-title">MCP tool calls return 429</span>
        </summary>
        <p>
          Two different limits can trigger this, and the error message
          tells you which:
        </p>
        <ul>
          <li><strong>Per-minute cap</strong> (30/min on Pro, 120/min on Pro Plus) — usually a burst of simultaneous calls from an agent. Wait 60 seconds and retry.</li>
          <li><strong>Per-day cap</strong> (5,000/day on Pro, 30,000/day on Pro Plus) — almost always a runaway loop. The 24-hour window resets from the first call, not midnight, so inspect what your agent is doing before blindly retrying.</li>
        </ul>
        <p>
          Rate limits are per API key — splitting an agent across multiple
          keys distributes the budget.
        </p>
      </details>

      <details className="docs-accordion">
        <summary>
          <span className="docs-accordion-chevron" aria-hidden="true">▶</span>
          <span className="docs-accordion-title">Hardware encoder won't initialize</span>
        </summary>
        <p>If CameraNode logs something like <code>h264_nvenc failed, falling back to libx264</code>:</p>
        <ul>
          <li><strong>NVIDIA:</strong> install the NVIDIA driver + <code>nvidia-cuda-toolkit</code>; confirm <code>nvidia-smi</code> works</li>
          <li><strong>Intel QSV:</strong> install <code>intel-media-va-driver</code> (or <code>intel-media-va-driver-non-free</code> for newer CPUs)</li>
          <li><strong>AMD AMF:</strong> only works on Windows with the AMD driver installed</li>
          <li><strong>Force software:</strong> set <code>SOURCEBOX_SENTRY_ENCODER=libx264</code> to skip HW probe entirely</li>
        </ul>
      </details>

      <details className="docs-accordion">
        <summary>
          <span className="docs-accordion-chevron" aria-hidden="true">▶</span>
          <span className="docs-accordion-title">High CPU usage</span>
        </summary>
        <ul>
          <li>The software encoder (<code>libx264</code>) is the biggest cost — install a HW encoder if available</li>
          <li>Motion detection runs a second FFmpeg per camera. If you don't need it, set <code>motion.enabled: false</code></li>
          <li>Reduce camera resolution at the source (most webcams can be set lower via their own driver)</li>
        </ul>
      </details>

      <details className="docs-accordion">
        <summary>
          <span className="docs-accordion-chevron" aria-hidden="true">▶</span>
          <span className="docs-accordion-title">Recordings take too much disk</span>
        </summary>
        <p>Lower <code>storage.max_size_gb</code>. CameraNode will delete the oldest first until it fits. Or switch to scheduled recording instead of continuous.</p>
      </details>

      <details className="docs-accordion">
        <summary>
          <span className="docs-accordion-chevron" aria-hidden="true">▶</span>
          <span className="docs-accordion-title">Where to grab logs for a bug report</span>
        </summary>
        <p>
          The terminal dashboard has an <code>/export-logs</code> command that writes a
          timestamped file with the full buffer — attach this to any bug report. For
          Command Center–side issues, include the <code>request_id</code> from the
          failing response (in the JSON error body) and any <code>X-Request-Id</code>
          response header — we use those to find the backend log line for your
          request.
        </p>
      </details>
    </section>
  )
}

export default Troubleshooting
