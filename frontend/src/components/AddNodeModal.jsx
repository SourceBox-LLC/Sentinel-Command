import { useState, useRef, useEffect } from "react"

function AddNodeModal({ isOpen, onClose, onCreate }) {
  const [step, setStep] = useState(1)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState(null)
  const [credentials, setCredentials] = useState(null)
  const [os, setOs] = useState("linux")
  const inputRef = useRef(null)

  useEffect(() => {
    const ua = navigator.userAgent.toLowerCase()
    if (ua.includes('win')) setOs('windows')
    else if (ua.includes('mac')) setOs('macos')
    else setOs('linux')
  }, [])

  async function handleCreateClick() {
    const name = inputRef.current?.value

    if (!name || !name.trim()) {
      setError("Please enter a node name")
      return
    }

    setLoading(true)
    setError(null)

    try {
      const result = await onCreate(name.trim())
      setCredentials(result)
      setStep(2)
    } catch (err) {
      setError(err.message || "Failed to create node")
    } finally {
      setLoading(false)
    }
  }

  const base = window.location.origin
  // Windows installs via the MSI, not a one-liner — see the Windows
  // branch in the install-tab content below.
  //
  // Linux/macOS: install.sh accepts --url/--node-id/--key for
  // non-interactive registration.  The result is a registered node
  // ready to be launched via the foreground TUI dashboard
  // (`~/.sourcebox-sentry/sourcebox-sentry-cloudnode`).  Foreground
  // is the recommended primary experience — matches the Windows MSI
  // model where the Start menu shortcut launches the TUI and the
  // service is an explicit second step.
  //
  // For 24/7 unattended operation operators can append
  // `--install-service` to the one-liner.  We surface this as a
  // separate command rather than a checkbox so the foreground path
  // remains the obvious default — the goal is for everyone to verify
  // the foreground dashboard works before opting into a service that
  // hides failures behind journalctl.
  const linuxInstallCmd = credentials
    ? `curl -fsSL ${base}/install.sh | bash -s -- --url "${base}" --node-id ${credentials.node_id} --key ${credentials.api_key}`
    : `curl -fsSL ${base}/install.sh | bash`
  const linuxStartCmd = `~/.sourcebox-sentry/sourcebox-sentry-cloudnode`
  const linuxUnattendedCmd = credentials
    ? `curl -fsSL ${base}/install.sh | bash -s -- --url "${base}" --node-id ${credentials.node_id} --key ${credentials.api_key} --install-service`
    : ''
  const installCommands = {
    linux: linuxInstallCmd,
    macos: linuxInstallCmd,
  }
  const MSI_DOWNLOAD_URL =
    'https://github.com/SourceBox-LLC/Sentinel-CameraNode/releases/latest/download/sourcebox-sentry-cloudnode-windows-x86_64.msi'

  const exe = os === 'windows' ? 'sourcebox-sentry-cloudnode.exe' : 'sourcebox-sentry-cloudnode'
  // Quick setup command — kept for the Windows path (after MSI install,
  // operators run this in PowerShell to register the node) and as a
  // fallback / re-registration option on Linux/macOS.  The Linux
  // one-liner above already includes these args, so step 2 is now
  // optional / informational on Linux, mandatory on Windows.
  const quickSetupCmd = credentials
    ? `${exe} setup --url "${base}" --node-id ${credentials.node_id} --key ${credentials.api_key}`
    : ''

  const handleCopy = (text) => {
    navigator.clipboard.writeText(text)
  }

  const handleClose = () => {
    // Don't close while the create is in flight: the node is still
    // created server-side (silently burning a plan slot), and the late
    // setCredentials/setStep(2) would leak the earlier node's one-time
    // key into the next open of this still-mounted modal.
    if (loading) return
    setStep(1)
    setError(null)
    setCredentials(null)
    onClose()
  }

  // Once the one-time credentials are showing, require the explicit
  // Done button — an accidental overlay click would discard a key that
  // can only be recovered by rotating.
  const handleDismissAttempt = () => {
    if (step === 2) return
    handleClose()
  }

  if (!isOpen) return null

  return (
    <div className="modal-overlay" onClick={handleDismissAttempt}>
      <div className="modal-content" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <h2>{step === 1 ? "Add Camera Node" : "Node Created"}</h2>
          <button className="modal-close" onClick={handleDismissAttempt}>&times;</button>
        </div>

        {step === 1 && (
          <div className="modal-body">
            <p className="modal-description">
              Give your camera node a name to identify it (e.g., "Home", "Office", "Garage").
            </p>
            
            <div className="form-group">
              <label className="form-label" htmlFor="nodeName">Node Name</label>
              <input
                ref={inputRef}
                id="nodeName"
                name="nodeName"
                className="form-input"
                type="text"
                placeholder="e.g., Home"
                autoFocus
                disabled={loading}
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    e.preventDefault()
                    handleCreateClick()
                  }
                }}
              />
            </div>

            {error && (
              <div className="error-message">{error}</div>
            )}

            <div className="modal-actions">
              <button
                type="button"
                className="btn btn-secondary"
                onClick={handleClose}
                disabled={loading}
              >
                Cancel
              </button>
              <button
                type="button"
                className="btn btn-primary"
                onClick={handleCreateClick}
                disabled={loading}
              >
                {loading ? "Creating..." : "Create Node"}
              </button>
            </div>
          </div>
        )}

        {step === 2 && credentials && (
          <div className="modal-body">
            <div className="warning-banner">
              <span className="warning-icon">⚠️</span>
              <div>
                <strong>Save These Credentials</strong>
                <p>You won't be able to see the API key again!</p>
              </div>
            </div>

            <div className="credentials-box">
              <div className="credential-item">
                <label>Node ID</label>
                <div className="credential-value">
                  <code>{credentials.node_id}</code>
                  <button
                    className="btn btn-small"
                    onClick={() => handleCopy(credentials.node_id)}
                  >
                    Copy
                  </button>
                </div>
              </div>

              <div className="credential-item">
                <label>API Key</label>
                <div className="credential-value">
                  <code>{credentials.api_key}</code>
                  <button
                    className="btn btn-small"
                    onClick={() => handleCopy(credentials.api_key)}
                  >
                    Copy
                  </button>
                </div>
              </div>
            </div>

            <div className="command-section">
              <h4>Deploy Your Node</h4>

              <div className="deployment-content">
                <div className="command-box">
                  <h5>{os === 'windows' ? '1. Install CloudNode:' : '1. Install + Register:'}</h5>
                  <div className="install-tabs">
                    <div className="install-tab-buttons">
                      <button className={`install-tab-btn${os === 'linux' ? ' active' : ''}`} onClick={() => setOs('linux')}>Linux</button>
                      <button className={`install-tab-btn${os === 'macos' ? ' active' : ''}`} onClick={() => setOs('macos')}>macOS</button>
                      <button className={`install-tab-btn${os === 'windows' ? ' active' : ''}`} onClick={() => setOs('windows')}>Windows</button>
                    </div>
                  </div>
                  {os !== 'windows' ? (
                    <>
                      <code>{installCommands[os]}</code>
                      <button className="btn btn-small" onClick={() => handleCopy(installCommands[os])}>Copy</button>
                    </>
                  ) : (
                    <div style={{ marginTop: '0.5rem' }}>
                      <a
                        href={MSI_DOWNLOAD_URL}
                        target="_blank"
                        rel="noopener noreferrer"
                        className="btn btn-primary btn-small"
                        style={{ textDecoration: 'none', display: 'inline-block' }}
                      >
                        ⬇ Download Windows MSI
                      </a>
                      <p style={{ marginTop: '0.5rem', fontSize: '0.85rem', color: 'var(--text-muted)' }}>
                        Run the MSI (UAC). SmartScreen → <strong>More info → Run anyway</strong>.
                      </p>
                    </div>
                  )}
                </div>
                {/*
                    Step 2 differs by platform:
                      - Windows: post-MSI setup wizard (or quick-setup with creds)
                      - Linux/macOS: launch the foreground TUI dashboard
                    Both converge on the same model — the foreground TUI is
                    the recommended primary experience; the service is opt-in.
                */}
                {os === 'windows' ? (
                  <div className="command-box quick-setup-box">
                    <h5>2. Quick Setup (one command in PowerShell):</h5>
                    <code className="quick-setup-cmd">{quickSetupCmd}</code>
                    <button className="btn btn-small" onClick={() => handleCopy(quickSetupCmd)}>Copy</button>
                  </div>
                ) : (
                  <div className="command-box quick-setup-box">
                    <h5>2. Start CloudNode (foreground dashboard):</h5>
                    <code className="quick-setup-cmd">{linuxStartCmd}</code>
                    <button className="btn btn-small" onClick={() => handleCopy(linuxStartCmd)}>Copy</button>
                  </div>
                )}
                <div className="command-note">
                  {os === 'windows'
                    ? <>The MSI registers a Start menu shortcut for the foreground TUI dashboard — that's the recommended way to run CloudNode. The setup command above registers the node before the first launch. <br/><br/>For 24/7 unattended operation, the MSI also installs an optional Windows Service named <code>SourceBoxSentryCloudNode</code> (manual-start by default). Switch it to automatic from <strong>services.msc</strong> after you've verified the foreground works.</>
                    : <>The foreground dashboard shows live cameras, segment counts, FFmpeg state, and slash commands. Same primary experience as the Windows MSI's Start menu shortcut.<br/><br/>For 24/7 unattended operation (camera-in-a-closet, no SSH session), append <code>--install-service</code> to the install command:<br/>
                      <code style={{ display: 'block', marginTop: '0.4rem', padding: '0.5rem', fontSize: '0.85em', wordBreak: 'break-all' }}>{linuxUnattendedCmd}</code>
                      <button className="btn btn-small" onClick={() => handleCopy(linuxUnattendedCmd)}>Copy unattended command</button></>
                  }
                </div>
              </div>
            </div>

            <div className="modal-actions">
              <button
                type="button"
                className="btn btn-primary"
                onClick={handleClose}
              >
                Done
              </button>
            </div>
          </div>
        )}
      </div>
    </div>
  )
}

export default AddNodeModal