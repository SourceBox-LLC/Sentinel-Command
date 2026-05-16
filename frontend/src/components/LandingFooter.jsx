import { Link } from "react-router-dom"
import { LogoMark } from "./Logo.jsx"

// Support / contact channel.  Until `sourceboxsentry.com` is
// provisioned for incoming mail (no MX records yet) we route every
// contact path through the public GitHub issue tracker — same
// pattern LegalPage.jsx uses for the `legal@` placeholder.  When the
// real `support@` mailbox lands, swap to a `mailto:` link.
const SUPPORT_URL =
  "https://github.com/SourceBox-LLC/OpenSentry-Command/issues/new?labels=support&template=support.md"
const STATUS_URL = "https://github.com/SourceBox-LLC/OpenSentry-Command/issues?q=is%3Aissue+label%3Aoutage"

function LandingFooter() {
  return (
    <footer className="landing-footer">
      <div className="landing-container">
        <div className="landing-footer-content">
          <div className="landing-footer-brand">
            <div className="landing-logo">
              <LogoMark size={28} className="landing-logo-icon" />
              <span className="landing-logo-text">Sentinel</span>
              <span> by SourceBox</span>
            </div>
            <p>Open-source security for everyone.</p>
          </div>

          <div className="landing-footer-links">
            <div className="landing-footer-col">
              <h5>Product</h5>
              <Link to="/#features">Features</Link>
              <Link to="/#architecture">Architecture</Link>
              <Link to="/#quickstart">Quick Start</Link>
              <Link to="/docs">Documentation</Link>
            </div>

            <div className="landing-footer-col">
              <h5>Resources</h5>
              <a href="https://github.com/SourceBox-LLC/OpenSentry-Command" target="_blank" rel="noopener noreferrer">
                Command Center
              </a>
              <a href="https://github.com/SourceBox-LLC/opensentry-cloud-node" target="_blank" rel="noopener noreferrer">
                CloudNode
              </a>
              <a href="https://github.com/SourceBox-LLC" target="_blank" rel="noopener noreferrer">
                SourceBox LLC
              </a>
            </div>

            <div className="landing-footer-col">
              <h5>Support</h5>
              <a href={SUPPORT_URL} target="_blank" rel="noopener noreferrer">
                Get help
              </a>
              <a href={STATUS_URL} target="_blank" rel="noopener noreferrer">
                Outage status
              </a>
              <a
                href="https://github.com/SourceBox-LLC/OpenSentry-Command/issues/new?labels=bug"
                target="_blank"
                rel="noopener noreferrer"
              >
                Report a bug
              </a>
            </div>

            <div className="landing-footer-col">
              <h5>Legal</h5>
              <Link to="/security">Security</Link>
              <Link to="/legal/terms">Terms of Service</Link>
              <Link to="/legal/privacy">Privacy Policy</Link>
              <a href="https://github.com/SourceBox-LLC/OpenSentry-Command/blob/master/LICENSE" target="_blank" rel="noopener noreferrer">
                AGPL-3.0 License
              </a>
            </div>
          </div>
        </div>

        <div className="landing-footer-bottom">
          <p>© {new Date().getFullYear()} SourceBox LLC. Open source under AGPL-3.0.</p>
        </div>
      </div>
    </footer>
  )
}

export default LandingFooter