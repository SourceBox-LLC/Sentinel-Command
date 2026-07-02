// Shared state for the split DocsPage.
//
// The pre-split DocsPage held three pieces of state that >half of the section
// bodies needed: the user's currently-selected OS (drives the install-command
// snippet rendering), a "Copied!" toast flag, and the install-command map
// keyed by OS. After splitting one file per <section>, prop-drilling those
// three pieces into ~19 sections would be noise; React Context is the right
// fit.
//
// `OsTabs` reads from this context internally so callers don't have to thread
// anything through. The Getting-Started and CameraNode-Setup sections both
// render an OsTabs instance — they share a single `os` value, so flipping
// the tab in one updates the other (and the inline ``sourcebox-sentry-cameranode``
// invocation a few lines below it).
//
// Keeping this in /pages/docs/context.jsx (not /hooks/) signals that it's
// scoped to the docs route — it isn't, and shouldn't be, used elsewhere.

import { createContext, useContext, useEffect, useState } from "react"


const DocsContext = createContext(null)


export function DocsProvider({ children }) {
  // Default to linux until the userAgent sniff has run; that gives the
  // Copy buttons a sensible target on first paint without a flash of
  // "windows" content for non-Windows visitors.
  const [os, setOs] = useState("linux")
  const [copied, setCopied] = useState(false)

  useEffect(() => {
    const ua = navigator.userAgent.toLowerCase()
    if (ua.includes("win")) setOs("windows")
    else if (ua.includes("mac")) setOs("macos")
    else setOs("linux")
  }, [])

  const copyToClipboard = (text) => {
    navigator.clipboard.writeText(text)
    setCopied(true)
    setTimeout(() => setCopied(false), 2000)
  }

  // ``base`` resolves at module-init time on the browser, so we capture it
  // once per provider mount. Captured inside the provider rather than at
  // module top-level so test runners that stub window.location can produce
  // consistent results.
  const base = window.location.origin
  // Windows is intentionally absent — that platform installs via the MSI
  // from GitHub Releases (rendered as a download button in OsTabs) rather
  // than a curl-style one-liner. There used to be a PowerShell installer
  // alongside the MSI; it was retired in v0.1.31 because the MSI is the
  // only Windows install path that handles upgrades + Add/Remove Programs
  // cleanly.
  const installCommands = {
    linux: `curl -fsSL ${base}/install.sh | bash`,
    macos: `curl -fsSL ${base}/install.sh | bash`,
  }
  const msiDownloadUrl =
    "https://github.com/SourceBox-LLC/Sentinel-CameraNode/releases/latest/download/sourcebox-sentry-cameranode-windows-x86_64.msi"

  const value = {
    os,
    setOs,
    copied,
    copyToClipboard,
    base,
    installCommands,
    msiDownloadUrl,
  }
  return <DocsContext.Provider value={value}>{children}</DocsContext.Provider>
}


export function useDocs() {
  const ctx = useContext(DocsContext)
  if (!ctx) {
    throw new Error("useDocs() must be used inside <DocsProvider>")
  }
  return ctx
}


// Reusable install-command tabs widget.
// Renders three OS tabs and the appropriate install action for the
// selected platform. Two callsites today (Getting Started + CameraNode
// Setup) so the switch propagates between them via the shared `os` state.
//
// Linux / macOS render a copy-able shell one-liner. Windows renders an
// MSI download button instead — there's no command to copy, the MSI is
// the install. Conditional rendering inside the same widget keeps both
// consumers consistent without forcing them to know about the platform
// difference.
export function OsTabs({ id }) {
  const {
    os,
    setOs,
    copied,
    copyToClipboard,
    installCommands,
    msiDownloadUrl,
  } = useDocs()
  return (
    <div className="install-tabs" key={id}>
      <div className="install-tab-buttons">
        {["linux", "macos", "windows"].map((o) => (
          <button
            key={o}
            className={`install-tab-btn${os === o ? " active" : ""}`}
            onClick={() => setOs(o)}
          >
            {o === "macos" ? "macOS" : o.charAt(0).toUpperCase() + o.slice(1)}
          </button>
        ))}
      </div>
      <div className="install-tab-content">
        {os !== "windows" ? (
          <div className="docs-code-block">
            <code>{installCommands[os]}</code>
            <button
              className="docs-copy-btn"
              onClick={() => copyToClipboard(installCommands[os])}
            >
              {copied ? "Copied!" : "Copy"}
            </button>
          </div>
        ) : (
          <div style={{ marginTop: "0.5rem" }}>
            <a
              href={msiDownloadUrl}
              target="_blank"
              rel="noopener noreferrer"
              style={{
                display: "inline-flex",
                alignItems: "center",
                gap: "0.5rem",
                padding: "0.6rem 1.2rem",
                background: "var(--accent-green)",
                color: "var(--bg-primary)",
                fontWeight: "600",
                borderRadius: "6px",
                textDecoration: "none",
              }}
            >
              ⬇  Download Windows MSI
            </a>
            <p style={{ marginTop: "0.75rem", fontSize: "0.9rem", color: "var(--text-muted)" }}>
              Run the MSI (UAC prompt; SmartScreen → <strong>More info → Run anyway</strong>),
              then open the <strong>Sentinel CameraNode</strong> shortcut from the
              Start menu. First launch runs the setup wizard; every launch after streams
              cameras directly.
            </p>
          </div>
        )}
      </div>
    </div>
  )
}
