import { useState, useEffect } from "react"
import { Link, useLocation } from "react-router-dom"
import {
  CloudIcon,
  ShieldLockIcon,
  MemoryIcon,
  UsersIcon,
  VideoIcon,
  KeyIcon,
  CodeIcon,
  ShieldCheckIcon,
} from "../components/FeatureIcons.jsx"

function LandingPage() {
  const [os, setOs] = useState('linux')
  const [copied, setCopied] = useState(false)
  const location = useLocation()

  useEffect(() => {
    const ua = navigator.userAgent.toLowerCase()
    if (ua.includes('win')) setOs('windows')
    else if (ua.includes('mac')) setOs('macos')
    else setOs('linux')
  }, [])

  useEffect(() => {
    if (location.hash) {
      const el = document.querySelector(location.hash)
      if (el) el.scrollIntoView({ behavior: "smooth" })
    }
  }, [location.hash])

  const copyToClipboard = (text) => {
    navigator.clipboard.writeText(text)
    setCopied(true)
    setTimeout(() => setCopied(false), 2000)
  }

  const base = window.location.origin
  // Windows is intentionally absent — that platform installs via the
  // MSI from GitHub Releases (rendered as a download button below)
  // rather than a curl-style one-liner. The PowerShell installer
  // (`install.ps1`) was retired when the MSI shipped: the MSI registers
  // a Windows Service, which is the right execution model for an
  // always-on camera node and what `install.ps1` couldn't cleanly do.
  const installCommands = {
    linux: `curl -fsSL ${base}/install.sh | bash`,
    macos: `curl -fsSL ${base}/install.sh | bash`,
  }
  const MSI_DOWNLOAD_URL =
    'https://github.com/SourceBox-LLC/opensentry-cloud-node/releases/latest/download/sourcebox-sentry-cloudnode-windows-x86_64.msi'

  return (
    <>
      {/* Hero Section */}
      <section className="landing-hero">
        <div className="landing-hero-bg"></div>
        <div className="landing-hero-container">
          <div className="landing-hero-content">
            <div className="landing-hero-badge">☁️ Cloud-Hosted • Multi-Tenant</div>
            <h1 className="landing-hero-title">
              Private Security<br />Camera System
            </h1>
            <p className="landing-hero-subtitle">
              Watch your cameras from anywhere — without handing your footage to a vendor's
              AI. Motion detection runs on your hardware. <strong>Recordings stay yours.</strong>
            </p>
            <div className="landing-hero-actions">
              <Link to="/sign-up" className="landing-btn landing-btn-primary landing-btn-lg">
                Get Started Free
                <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
                  <path d="M5 12h14M12 5l7 7-7 7"/>
                </svg>
              </Link>
              <Link to="/#architecture" className="landing-btn landing-btn-outline landing-btn-lg">
                How It Works
              </Link>
            </div>
            <div className="landing-hero-stats">
              <div className="landing-stat">
                <span className="landing-stat-value">Open</span>
                <span className="landing-stat-label">Source</span>
              </div>
              <div className="landing-stat">
                <span className="landing-stat-value">AES-256</span>
                <span className="landing-stat-label">At Rest</span>
              </div>
              <div className="landing-stat">
                <span className="landing-stat-value">&lt;10s</span>
                <span className="landing-stat-label">Live Latency</span>
              </div>
            </div>
          </div>
          <div className="landing-hero-visual">
            <picture>
              <source srcSet="/images/landing-hero.webp" type="image/webp" />
              <img
                src="/images/landing-hero.jpg"
                alt="A private home study at dusk — USB webcam clipped to a monitor showing the Sentinel dashboard with green and purple camera tiles, on a warm wooden desk with a plant and steaming mug, soft golden window light"
                className="landing-hero-image"
                width="1600"
                height="1200"
                loading="eager"
                fetchpriority="high"
              />
            </picture>
          </div>
        </div>
      </section>

      {/* Features Section */}
      <section id="features" className="landing-section-alt">
        <div className="landing-container">
          <div className="landing-section-header">
            <h2 className="landing-section-title">Why Sentinel?</h2>
            <p className="landing-section-subtitle">
              Designed for modern security. Cloud-hosted, globally accessible, and secure by default.
            </p>
          </div>
          <div className="landing-features-grid">
            <div className="landing-feature-card">
              <div className="landing-feature-icon"><CloudIcon /></div>
              <h3>Hosted by Us</h3>
              <p>
                Sign up and connect cameras — no servers to run on your end. No
                port forwarding, no static IPs, no VPN required.
              </p>
            </div>
            <div className="landing-feature-card">
              <div className="landing-feature-icon"><ShieldLockIcon /></div>
              <h3>Encrypted End to End to Disk</h3>
              <p>
                HTTPS from camera to browser. Recordings on the CloudNode are sealed at
                rest with AES-256-GCM using a machine-id-derived key — a stolen drive
                is unreadable elsewhere.
              </p>
            </div>
            <div className="landing-feature-card">
              <div className="landing-feature-icon"><MemoryIcon /></div>
              <h3>In-Memory Streaming</h3>
              <p>
                Live HLS segments are cached in RAM by the Command Center and served directly
                to viewers — fast, simple, and no per-request object-store fees.
              </p>
            </div>
            <div className="landing-feature-card">
              <div className="landing-feature-icon"><UsersIcon /></div>
              <h3>Multi-Tenant</h3>
              <p>
                Organizations with role-based permissions. Admin and member roles.
                Invite team members to monitor cameras together.
              </p>
            </div>
            <div className="landing-feature-card">
              <div className="landing-feature-icon"><VideoIcon /></div>
              <h3>Real-Time Streaming</h3>
              <p>
                HLS streaming with sub-10-second latency. CloudNode uploads segments
                in real-time for near-live viewing experience.
              </p>
            </div>
            <div className="landing-feature-card">
              <div className="landing-feature-icon"><KeyIcon /></div>
              <h3>Simple Setup</h3>
              <p>
                Install CloudNode on any device with a USB camera. Enter API key.
                That's it. No network configuration required.
              </p>
            </div>
            <div className="landing-feature-card landing-feature-highlight">
              <div className="landing-feature-icon"><CodeIcon /></div>
              <h3>MCP Integration</h3>
              <p>
                Give AI tools direct visual access to your cameras via the
                Model Context Protocol. See what cameras see and control everything through natural language.
              </p>
              <span className="landing-feature-badge">PRO</span>
            </div>
            <div className="landing-feature-card landing-feature-highlight">
              <div className="landing-feature-icon"><ShieldCheckIcon /></div>
              <h3>Sentinel AI Agent</h3>
              <p>
                Autonomous AI that investigates motion events on your behalf — views
                the camera, decides whether what it sees warrants attention, and
                files a full incident report with snapshot evidence. 100 runs/mo on
                Pro, 500 on Pro Plus.
              </p>
              <span className="landing-feature-badge">PRO</span>
            </div>
          </div>
        </div>
      </section>

      {/* Privacy-first — positioned after the features grid so a scanning
          visitor sees it right after "why us". Breaks the alt/non-alt rhythm
          on purpose; the colour treatment signals "read this one". */}
      <section id="privacy" className="landing-section-privacy">
        <div className="landing-container">
          <div className="landing-privacy-grid">
            <div className="landing-privacy-copy">
              <span className="landing-privacy-eyebrow">Privacy by design</span>
              <h2 className="landing-privacy-title">
                Your recordings never leave your CloudNode.
              </h2>
              <p className="landing-privacy-lede">
                Cloud security cameras stream every frame to a vendor's servers
                for storage and analysis. Your CloudNode is different — it sits
                on your network, records to its own encrypted disk, and runs
                motion analysis locally. The cloud only ever holds a rolling
                60-second live buffer in memory.
              </p>
              <ul className="landing-privacy-points">
                <li><strong>Motion detection runs locally.</strong> FFmpeg threshold detection on your CloudNode. Pixel math, not ML.</li>
                <li><strong>Recordings are encrypted at rest.</strong> AES-256-GCM with a key derived from your device's OS machine ID.</li>
                <li><strong>No analytics, no ad networks, no data brokers.</strong> Verifiable in our source with a single grep.</li>
                <li><strong>We don't hold your video.</strong> Recordings live on your CloudNode, not our cloud. If law enforcement asks for footage, they have to ask you.</li>
              </ul>
              <div className="landing-privacy-ctas">
                <Link to="/security" className="landing-privacy-cta primary">
                  How we compare →
                </Link>
                <Link to="/legal/privacy" className="landing-privacy-cta secondary">
                  Read the Privacy Policy
                </Link>
              </div>
            </div>

            <div className="landing-privacy-visual">
              <picture>
                <source srcSet="/images/privacy.webp" type="image/webp" />
                <img
                  src="/images/privacy.jpg"
                  alt=""
                  className="landing-privacy-image"
                  width="1200"
                  height="1200"
                  loading="lazy"
                />
              </picture>
              <div className="landing-privacy-image-caption" aria-hidden="true">
                Your CloudNode. Your hardware. Your data.
              </div>
            </div>
          </div>
        </div>
      </section>

      {/* Architecture Section */}
      <section id="architecture" className="landing-section">
        <div className="landing-container">
          <div className="landing-section-header">
            <h2 className="landing-section-title">Two-Component Architecture</h2>
            <p className="landing-section-subtitle">
              CloudNode at your premises. Command Center in the cloud. Secure by design.
            </p>
          </div>
          <div className="landing-arch-grid">
            <div className="landing-arch-card node">
              <div className="landing-arch-header">
                <div className="landing-arch-icon">📹</div>
                <div className="landing-arch-badge">Customer Premises</div>
              </div>
              <h3>CloudNode</h3>
              <p className="landing-arch-desc">
                Runs on any device with a USB camera. Captures video, generates HLS segments,
                and pushes them straight to the Command Center over authenticated HTTPS.
              </p>
              <ul className="landing-arch-features">
                <li>
                  <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
                    <polyline points="20 6 9 17 4 12"/>
                  </svg>
                  USB camera support
                </li>
                <li>
                  <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
                    <polyline points="20 6 9 17 4 12"/>
                  </svg>
                  FFmpeg HLS generation
                </li>
                <li>
                  <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
                    <polyline points="20 6 9 17 4 12"/>
                  </svg>
                  Real-time uploads
                </li>
                <li>
                  <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
                    <polyline points="20 6 9 17 4 12"/>
                  </svg>
                  Windows, Linux, macOS
                </li>
              </ul>
              <a 
                href="https://github.com/SourceBox-LLC/opensentry-cloud-node"
                target="_blank"
                rel="noopener noreferrer"
                className="landing-arch-cta landing-arch-cta-primary"
              >
                <svg width="18" height="18" viewBox="0 0 24 24" fill="currentColor">
                  <path d="M12 0C5.37 0 0 5.37 0 12c0 5.31 3.435 9.795 8.205 11.385.6.105.825-.255.825-.57 0-.285-.015-1.23-.015-2.235-3.015.555-3.795-.735-4.035-1.41-.135-.345-.72-1.41-1.23-1.695-.42-.225-1.02-.78-.015-.795.945-.015 1.62.87 1.845 1.23 1.08 1.815 2.805 1.305 3.495.99.105-.78.42-1.305.765-1.605-2.67-.3-5.46-1.335-5.46-5.925 0-1.305.465-2.385 1.23-3.225-.12-.3-.54-1.53.12-3.18 0 0 1.005-.315 3.3 1.23.96-.27 1.98-.405 3-.405s2.04.135 3 .405c2.295-1.56 3.3-1.23 3.3-1.23.66 1.65.24 2.88.12 3.18.765.84 1.23 1.905 1.23 3.225 0 4.605-2.805 5.625-5.475 5.925.435.375.81 1.095.81 2.22 0 1.605-.015 2.895-.015 3.3 0 .315.225.69.825.57A12.02 12.02 0 0024 12c0-6.63-5.37-12-12-12z"/>
                </svg>
                View Repository
              </a>
            </div>

            <div className="landing-arch-connector">
              <div className="landing-connector-line"></div>
              <div className="landing-connector-protocols">
                <span>HTTPS</span>
                <span>HLS</span>
                <span>REST API</span>
              </div>
              <div className="landing-connector-line"></div>
            </div>

            <div className="landing-arch-card cloud">
              <div className="landing-arch-header">
                <div className="landing-arch-icon">🖥️</div>
                <div className="landing-arch-badge">Cloud Hosted</div>
              </div>
              <h3>Command Center</h3>
              <p className="landing-arch-desc">
                Web dashboard for viewing all cameras, managing organizations, and
                controlling access. Operated by SourceBox — you sign up, we run it.
              </p>
              <ul className="landing-arch-features">
                <li>
                  <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
                    <polyline points="20 6 9 17 4 12"/>
                  </svg>
                  HTTPS web interface
                </li>
                <li>
                  <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
                    <polyline points="20 6 9 17 4 12"/>
                  </svg>
                  Clerk multi-tenant auth
                </li>
                <li>
                  <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
                    <polyline points="20 6 9 17 4 12"/>
                  </svg>
                  SQLite metadata storage
                </li>
                <li>
                  <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
                    <polyline points="20 6 9 17 4 12"/>
                  </svg>
                  In-memory HLS segment cache
                </li>
              </ul>
              <a 
                href="https://github.com/SourceBox-LLC/OpenSentry-Command" 
                target="_blank" 
                rel="noopener noreferrer"
                className="landing-arch-cta landing-arch-cta-primary"
              >
                <svg width="18" height="18" viewBox="0 0 24 24" fill="currentColor">
                  <path d="M12 0C5.37 0 0 5.37 0 12c0 5.31 3.435 9.795 8.205 11.385.6.105.825-.255.825-.57 0-.285-.015-1.23-.015-2.235-3.015.555-3.795-.735-4.035-1.41-.135-.345-.72-1.41-1.23-1.695-.42-.225-1.02-.78-.015-.795.945-.015 1.62.87 1.845 1.23 1.08 1.815 2.805 1.305 3.495.99.105-.78.42-1.305.765-1.605-2.67-.3-5.46-1.335-5.46-5.925 0-1.305.465-2.385 1.23-3.225-.12-.3-.54-1.53.12-3.18 0 0 1.005-.315 3.3 1.23.96-.27 1.98-.405 3-.405s2.04.135 3 .405c2.295-1.56 3.3-1.23 3.3-1.23.66 1.65.24 2.88.12 3.18.765.84 1.23 1.905 1.23 3.225 0 4.605-2.805 5.625-5.475 5.925.435.375.81 1.095.81 2.22 0 1.605-.015 2.895-.015 3.3 0 .315.225.69.825.57A12.02 12.02 0 0024 12c0-6.63-5.37-12-12-12z"/>
                </svg>
                View Repository
              </a>
            </div>
          </div>
        </div>
      </section>

      {/* Quick Start Section */}
      <section id="quickstart" className="landing-section-alt">
        <div className="landing-container">
          <div className="landing-section-header">
            <h2 className="landing-section-title">Quick Start</h2>
            <p className="landing-section-subtitle">
              Get your first camera online in under 5 minutes.
            </p>
          </div>
          <div className="landing-install-steps">
            <div className="landing-step">
              <div className="landing-step-number">1</div>
              <div className="landing-step-content">
                <h4>Create a free account</h4>
                <Link to="/sign-up" className="landing-btn landing-btn-primary">
                  Sign up — it's free
                </Link>
                <p className="landing-step-note">
                  Takes about 30 seconds. You'll create your organization on the way in.
                </p>
              </div>
            </div>
            <div className="landing-step">
              <div className="landing-step-number">2</div>
              <div className="landing-step-content">
                <h4>Install CloudNode on your device</h4>
                <div className="install-tabs install-tabs-landing">
                  <div className="install-tab-buttons">
                    <button
                      className={`install-tab-btn${os === 'linux' ? ' active' : ''}`}
                      onClick={() => setOs('linux')}
                    >
                      Linux
                    </button>
                    <button
                      className={`install-tab-btn${os === 'macos' ? ' active' : ''}`}
                      onClick={() => setOs('macos')}
                    >
                      macOS
                    </button>
                    <button
                      className={`install-tab-btn${os === 'windows' ? ' active' : ''}`}
                      onClick={() => setOs('windows')}
                    >
                      Windows
                    </button>
                  </div>
                  {os !== 'windows' ? (
                    <>
                      <div className="landing-code-block">
                        <code>{installCommands[os]}</code>
                        <button className="landing-copy-btn" onClick={() => copyToClipboard(installCommands[os])}>
                          {copied ? 'Copied!' : 'Copy'}
                        </button>
                      </div>
                      <p className="landing-step-note">
                        One command. Downloads CloudNode, checks dependencies, and guides you through setup.
                      </p>
                    </>
                  ) : (
                    <>
                      <a
                        href={MSI_DOWNLOAD_URL}
                        target="_blank"
                        rel="noopener noreferrer"
                        className="landing-btn landing-btn-primary"
                        style={{ display: 'inline-flex', alignItems: 'center', gap: '0.5rem' }}
                      >
                        ⬇  Download Windows MSI
                      </a>
                      <p className="landing-step-note" style={{ marginTop: '1rem' }}>
                        Run the MSI (one UAC prompt), then open PowerShell as Administrator and run{' '}
                        <code>sourcebox-sentry-cloudnode setup</code>. Registers as a Windows Service that
                        auto-starts on boot. MSI is currently unsigned — SmartScreen will warn:
                        click <strong>More info → Run anyway</strong>. See{' '}
                        <Link to="/docs#cloudnode-setup">install notes</Link> for full details.
                      </p>
                    </>
                  )}
                </div>
              </div>
            </div>
            <div className="landing-step">
              <div className="landing-step-number">3</div>
              <div className="landing-step-content">
                <h4>Connect your USB camera</h4>
                <p className="landing-step-note">
                  CloudNode will automatically detect connected USB cameras. No manual configuration needed.
                </p>
              </div>
            </div>
            <div className="landing-step">
              <div className="landing-step-number">4</div>
              <div className="landing-step-content">
                <h4>Enter your API key</h4>
                <p className="landing-step-note">
                  Generate an API key in your Command Center settings and enter it during CloudNode setup. 
                  Your camera will appear in the dashboard instantly.
                </p>
              </div>
            </div>
          </div>
          <div style={{ textAlign: 'center', marginTop: '3rem' }}>
            <Link to="/sign-up" className="landing-btn landing-btn-primary landing-btn-lg">
              Get Started Free
            </Link>
          </div>
        </div>
      </section>

      {/* Security Section */}
      <section className="landing-section">
        <div className="landing-container">
          <div className="landing-security-content">
            <div className="landing-security-text">
              <h2>Security First Design</h2>
              <p>
                Every connection is encrypted. Every stream is authenticated. No inbound ports required.
              </p>
              <ul className="landing-security-list">
                <li>
                  <span className="landing-security-check">✓</span>
                  <div>
                    <strong>HTTPS</strong> — All web traffic encrypted with TLS
                  </div>
                </li>
                <li>
                  <span className="landing-security-check">✓</span>
                  <div>
                    <strong>Clerk Auth</strong> — Multi-tenant organizations with role-based permissions
                  </div>
                </li>
                <li>
                  <span className="landing-security-check">✓</span>
                  <div>
                    <strong>Same-origin HLS</strong> — Live segments served from the authenticated backend, never a third-party bucket
                  </div>
                </li>
                <li>
                  <span className="landing-security-check">✓</span>
                  <div>
                    <strong>API Keys</strong> — Node authentication with SHA256 hashing
                  </div>
                </li>
                <li>
                  <span className="landing-security-check">✓</span>
                  <div>
                    <strong>No Inbound Ports</strong> — CloudNode pushes to cloud, no router config needed
                  </div>
                </li>
              </ul>
            </div>
            <div className="landing-security-visual">
              <div className="landing-encryption-diagram">
                <div className="landing-diagram-node node">
                  <span>CloudNode</span>
                  <div style={{ fontSize: '0.75rem', color: 'var(--text-muted)' }}>USB Camera + FFmpeg</div>
                </div>
                <div className="landing-diagram-arrow">
                  <span>HTTPS</span>
                </div>
                <div className="landing-diagram-node cloud">
                  <span>Command Center</span>
                  <div style={{ fontSize: '0.75rem', color: 'var(--text-muted)' }}>Hosted by SourceBox · in-memory cache</div>
                </div>
                <div className="landing-diagram-arrow">
                  <span>HTTPS</span>
                </div>
                <div className="landing-diagram-node" style={{ borderColor: 'var(--accent-purple)' }}>
                  <span>Browser</span>
                  <div style={{ fontSize: '0.75rem', color: 'var(--text-muted)' }}>Any Device</div>
                </div>
              </div>
            </div>
          </div>
        </div>
      </section>

      {/* MCP Section */}
      <section className="landing-section-alt">
        <div className="landing-container">
          <div className="landing-section-header">
            <h2 className="landing-section-title">AI-Powered with MCP</h2>
            <p className="landing-section-subtitle">
              Give AI tools like Claude Code direct access to your security system
              through the Model Context Protocol.
            </p>
          </div>
          <div className="landing-mcp-showcase">
            <div className="landing-mcp-left">
              <div className="landing-mcp-badge">Model Context Protocol</div>
              <h3>Control cameras with natural language</h3>
              <p>
                Pro and Pro Plus users can generate an MCP API key and connect any
                compatible AI tool — Claude Code, Cursor, or custom agents — directly
                to their organization's cameras, nodes, and system data.
              </p>
              <div className="landing-mcp-examples">
                <div className="landing-mcp-example">"Show me what the front door camera sees"</div>
                <div className="landing-mcp-example">"Watch the garage cam for 30 seconds"</div>
                <div className="landing-mcp-example">"Are any cameras offline right now?"</div>
              </div>
              <Link to="/pricing" className="landing-btn landing-btn-primary">
                Try with Pro
              </Link>
            </div>
            <div className="landing-mcp-right">
              <div className="landing-mcp-config">
                <div className="landing-mcp-config-header">
                  <span className="landing-mcp-dot red" />
                  <span className="landing-mcp-dot yellow" />
                  <span className="landing-mcp-dot green" />
                  <span className="landing-mcp-config-title">.mcp.json</span>
                </div>
                <pre className="landing-mcp-code">{`{
  "mcpServers": {
    "opensentry": {
      "type": "http",
      "url": "${base}/mcp",
      "headers": {
        "Authorization": "Bearer osc_..."
      }
    }
  }
}`}</pre>
              </div>
              <div className="landing-mcp-tools-count">
                <span>20+</span> tools available
              </div>
            </div>
          </div>
        </div>
      </section>

      {/* CTA Section */}
      <section className="landing-cta">
        <div className="landing-container">
          <div className="landing-cta-content">
            <h2>Ready to Get Started?</h2>
            <p>
              Sign up for Sentinel today. Cloud-hosted, globally accessible, and free to start.
            </p>
            <div className="landing-cta-actions">
              <Link to="/sign-up" className="landing-btn landing-btn-primary landing-btn-lg">
                Get Started Free
                <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
                  <path d="M5 12h14M12 5l7 7-7 7"/>
                </svg>
              </Link>
              <a 
                href="https://github.com/SourceBox-LLC/opensentry-cloud-node"
                target="_blank"
                rel="noopener noreferrer"
                className="landing-btn landing-btn-outline landing-btn-lg"
              >
                <svg width="20" height="20" viewBox="0 0 24 24" fill="currentColor">
                  <path d="M12 0C5.37 0 0 5.37 0 12c0 5.31 3.435 9.795 8.205 11.385.6.105.825-.255.825-.57 0-.285-.015-1.23-.015-2.235-3.015.555-3.795-.735-4.035-1.41-.135-.345-.72-1.41-1.23-1.695-.42-.225-1.02-.78-.015-.795.945-.015 1.62.87 1.845 1.23 1.08 1.815 2.805 1.305 3.495.99.105-.78.42-1.305.765-1.605-2.67-.3-5.46-1.335-5.46-5.925 0-1.305.465-2.385 1.23-3.225-.12-.3-.54-1.53.12-3.18 0 0 1.005-.315 3.3 1.23.96-.27 1.98-.405 3-.405s2.04.135 3 .405c2.295-1.56 3.3-1.23 3.3-1.23.66 1.65.24 2.88.12 3.18.765.84 1.23 1.905 1.23 3.225 0 4.605-2.805 5.625-5.475 5.925.435.375.81 1.095.81 2.22 0 1.605-.015 2.895-.015 3.3 0 .315.225.69.825.57A12.02 12.02 0 0024 12c0-6.63-5.37-12-12-12z"/>
                </svg>
                Get CloudNode
              </a>
            </div>
          </div>
        </div>
      </section>
    </>
  )
}

export default LandingPage