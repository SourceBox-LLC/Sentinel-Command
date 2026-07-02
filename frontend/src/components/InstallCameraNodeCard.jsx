import { useMemo, useState } from "react"
import { useAuth } from "@clerk/clerk-react"
import { createNode } from "../services/api"

/*
 * In-app CameraNode install widget.
 *
 * Replaces the previous "click out to GitHub README to figure out how to
 * install" flow on the empty-state dashboard.  The single biggest moment
 * of customer drop-off is right here — they signed up, they see an
 * empty grid, and the next thing they need to do is run a CLI command.
 * Anything that takes them OUT of the app at this moment loses them.
 *
 * Flow:
 *
 *   1. Initial: button "Get my install command →" (no API call yet —
 *      we don't want a hot reload to spam the org with orphan nodes).
 *   2. User clicks → POST /api/nodes auto-names "First CameraNode",
 *      returns {node_id, api_key} for one-time display.
 *   3. Display the FULLY-credentialed one-liner (matches what
 *      AddNodeModal generates) with Copy button + OS tabs.
 *   4. Live "Waiting for CameraNode to connect…" pulse — the parent
 *      dashboard polls cameras every 5s and unmounts this hero when
 *      a camera arrives, so the auto-advance is implicit.
 *
 * Why we wait for an explicit click instead of auto-creating on mount:
 * the React strict-mode double-invoke during dev would create two
 * orphan nodes, and a user reloading the dashboard repeatedly while
 * troubleshooting their network would burn through the plan's
 * max_nodes limit (default 3 on free).  Explicit click = explicit intent.
 */

const ORIGIN = (typeof window !== "undefined" && window.location?.origin) || ""

// Map the platform string to one of our supported tabs.  Defaults to
// Linux because the Linux + macOS commands are identical so a wrong
// default for a macOS user still produces a working command, and
// Linux is the most common server OS for camera deployments.
function detectOS() {
  if (typeof navigator === "undefined") return "linux"
  const ua = (navigator.userAgent || "") + " " + (navigator.platform || "")
  if (/Win/i.test(ua)) return "windows"
  if (/Mac/i.test(ua)) return "macos"
  return "linux"
}

const TABS = [
  { id: "linux", label: "Linux" },
  { id: "macos", label: "macOS" },
  { id: "windows", label: "Windows" },
]

// Build the install one-liner.  Linux + macOS use install.sh with
// args; Windows points at the MSI download.  These match exactly
// what AddNodeModal generates so there's a single canonical "this
// is the install command" surface.
function buildLinuxMacCommand(creds) {
  if (!creds) return ""
  const { node_id, api_key } = creds
  return `curl -fsSL ${ORIGIN}/install.sh | bash -s -- --url "${ORIGIN}" --node-id ${node_id} --key ${api_key}`
}

function CopyButton({ text }) {
  const [copied, setCopied] = useState(false)

  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(text)
      setCopied(true)
      // Reset after 2s so the button is reusable without page reload
      // and the user gets unambiguous "yes that worked" feedback.
      setTimeout(() => setCopied(false), 2000)
    } catch {
      // Clipboard access can fail in non-HTTPS contexts or restricted
      // iframes.  Rare in our deploy (HTTPS-only on Fly), but if it
      // happens the user can still triple-click the command and copy
      // manually — they're not blocked.
    }
  }

  return (
    <button
      type="button"
      className="install-copy-btn"
      onClick={handleCopy}
      aria-label={copied ? "Copied to clipboard" : "Copy command to clipboard"}
    >
      {copied ? "Copied ✓" : "Copy"}
    </button>
  )
}

function LinuxMacInstructions({ creds, os }) {
  const cmd = buildLinuxMacCommand(creds)
  return (
    <div className="install-tab-body">
      <p className="install-step-text">
        Open a terminal on the {os === "macos" ? "Mac" : "Linux machine"} where
        your cameras live and run this one-liner.  CameraNode auto-registers,
        downloads its binary, and starts heartbeating in ~30 seconds.
      </p>
      <div className="install-command-row">
        <pre className="install-command"><code>{cmd}</code></pre>
        <CopyButton text={cmd} />
      </div>
      <p className="install-helper-text">
        Requires <code>curl</code> and <code>bash</code> (present on every
        modern {os === "macos" ? "macOS" : "Linux"} system).  The command
        contains your one-time credentials — keep it private.
      </p>
    </div>
  )
}

function WindowsInstructions({ creds }) {
  // Windows uses the MSI installer + a separate post-install setup
  // command (Powershell can't curl|bash).  Both lines together do
  // what the Linux one-liner does in one.
  const setupCmd = creds
    ? `sourcebox-sentry-cameranode.exe setup --url "${ORIGIN}" --node-id ${creds.node_id} --key ${creds.api_key}`
    : ""
  return (
    <div className="install-tab-body">
      <p className="install-step-text">
        Two steps on Windows: download the MSI installer, then register
        this CameraNode with your one-time credentials.
      </p>

      <div className="install-command-row">
        {/* MUST be x86_64 — the backend's arch allow-list rejects
            "x64" with a JSON 404, which made this button (the
            highest-drop-off onboarding moment) a dead link. */}
        <a
          href={`${ORIGIN}/downloads/windows/x86_64`}
          className="btn btn-primary install-download-btn"
        >
          ↓ Download CameraNode for Windows
        </a>
      </div>

      <p className="install-step-text" style={{ marginTop: 14 }}>
        After the MSI install completes, open PowerShell and run:
      </p>
      <div className="install-command-row">
        <pre className="install-command"><code>{setupCmd}</code></pre>
        <CopyButton text={setupCmd} />
      </div>

      <p className="install-helper-text">
        ~30 MB MSI installer.  Registers as a Windows Service so CameraNode
        survives reboots.  The setup command contains your one-time
        credentials — keep it private.
      </p>
    </div>
  )
}

export default function InstallCameraNodeCard() {
  const { getToken } = useAuth()
  const detected = useMemo(detectOS, [])
  const [activeTab, setActiveTab] = useState(detected)

  const [creds, setCreds] = useState(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState(null)

  const handleGenerate = async () => {
    setLoading(true)
    setError(null)
    try {
      // Use the shared createNode helper (services/api.js) so this widget
      // gets the same auth-token wiring + base-URL handling as the rest
      // of the app.  The helper already throws Error(<api detail>) on
      // non-2xx responses, so the catch below just surfaces the message.
      const data = await createNode(getToken, "First CameraNode")
      setCreds({ node_id: data.node_id, api_key: data.api_key })
    } catch (e) {
      setError(e.message || "Failed to generate credentials")
    } finally {
      setLoading(false)
    }
  }

  // Re-render the install card based on whether creds exist yet.
  // Three states: initial (CTA button), loading (spinner-text),
  // ready (tabs + commands).  Error renders inline at the bottom.

  if (!creds) {
    return (
      <div className="install-cameranode-card">
        <p className="install-step-text" style={{ marginBottom: 14 }}>
          We&rsquo;ll generate one-time credentials for this org and bake them
          into a single install command — paste it on the machine where your
          cameras live and CameraNode comes online in ~30 seconds.
        </p>
        <button
          type="button"
          className="btn btn-primary"
          onClick={handleGenerate}
          disabled={loading}
        >
          {loading ? "Generating credentials…" : "Get my install command →"}
        </button>
        {error && (
          <p className="install-error" role="alert">
            {error}
          </p>
        )}
      </div>
    )
  }

  return (
    <div className="install-cameranode-card">
      <div className="install-tabs" role="tablist" aria-label="Operating system">
        {TABS.map((t) => (
          <button
            key={t.id}
            type="button"
            role="tab"
            aria-selected={activeTab === t.id}
            className={`install-tab ${activeTab === t.id ? "install-tab-active" : ""}`}
            onClick={() => setActiveTab(t.id)}
          >
            {t.label}
            {detected === t.id && (
              <span className="install-tab-detected" aria-label="Detected">
                {" "}· detected
              </span>
            )}
          </button>
        ))}
      </div>

      <div role="tabpanel">
        {activeTab === "linux" && <LinuxMacInstructions creds={creds} os="linux" />}
        {activeTab === "macos" && <LinuxMacInstructions creds={creds} os="macos" />}
        {activeTab === "windows" && <WindowsInstructions creds={creds} />}
      </div>

      <div className="install-waiting" aria-live="polite">
        <span className="install-waiting-dot" />
        <span className="install-waiting-text">
          Waiting for CameraNode to connect&hellip;  This panel will
          update automatically when the first heartbeat arrives.
        </span>
      </div>
    </div>
  )
}

