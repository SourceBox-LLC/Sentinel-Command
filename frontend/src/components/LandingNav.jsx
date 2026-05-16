import { useState } from "react"
import { Link } from "react-router-dom"
import { SignedIn, SignedOut, UserButton, OrganizationSwitcher, useOrganization } from "@clerk/clerk-react"
import { LogoMark } from "./Logo.jsx"

// Extracted into its own component so useOrganization() only runs when there
// is an active Clerk session — otherwise Clerk logs a console warning on every
// public-page render.
function SignedInActions() {
  const { organization, isLoaded: orgLoaded, membership } = useOrganization()
  const isAdmin = orgLoaded && membership?.role === "org:admin"

  return (
    <>
      {orgLoaded && organization && (
        <>
          <OrganizationSwitcher
            hidePersonal
            afterCreateOrganizationUrl="/dashboard"
            afterSelectOrganizationUrl="/dashboard"
            createOrganizationMode="modal"
          />
          <nav className="nav-links">
            <Link to="/dashboard" className="nav-link">
              Dashboard
            </Link>
            {isAdmin && (
              <>
                <Link to="/settings" className="nav-link">
                  Settings
                </Link>
                <Link to="/admin" className="nav-link">
                  Admin
                </Link>
              </>
            )}
            <Link to="/docs" className="nav-link">
              Docs
            </Link>
          </nav>
        </>
      )}
      <UserButton afterSignOutUrl="/" />
    </>
  )
}

function LandingNav() {
  const [mobileMenuOpen, setMobileMenuOpen] = useState(false)

  return (
    <nav className="landing-nav">
      <div className="landing-nav-container">
        <Link to="/" className="landing-logo">
          <LogoMark size={28} className="landing-logo-icon" />
          <span className="landing-logo-text">Sentinel</span>
          <span> by SourceBox</span>
        </Link>

        {/* Marketing nav. Hidden when signed in — the in-product nav
            (Dashboard / Settings / Admin / Docs from SignedInActions) takes
            over so the row doesn't overflow. Docs is the only marketing
            link still useful in-product, so it appears in both nav modes. */}
        <SignedOut>
          <ul className={`landing-nav-links ${mobileMenuOpen ? 'active' : ''}`}>
            <li>
              <Link to="/#features" onClick={() => setMobileMenuOpen(false)}>Features</Link>
            </li>
            <li>
              <Link to="/#architecture" onClick={() => setMobileMenuOpen(false)}>Architecture</Link>
            </li>
            <li>
              <Link to="/#quickstart" onClick={() => setMobileMenuOpen(false)}>Quick Start</Link>
            </li>
            <li>
              <Link to="/docs" onClick={() => setMobileMenuOpen(false)}>Docs</Link>
            </li>
            <li>
              <Link to="/security" onClick={() => setMobileMenuOpen(false)}>Security</Link>
            </li>
            <li>
              <Link to="/pricing" onClick={() => setMobileMenuOpen(false)}>Pricing</Link>
            </li>
            {/* Mobile-only auth CTAs. The desktop nav has these in
                .landing-nav-actions (hidden via media query at ≤1024px); the
                dropdown duplicates them so the hamburger menu is a complete
                navigation surface on phones / narrow tablets. */}
            <li className="landing-nav-mobile-cta">
              <Link
                to="/sign-in"
                className="landing-btn-ghost"
                onClick={() => setMobileMenuOpen(false)}
              >
                Sign In
              </Link>
            </li>
            <li className="landing-nav-mobile-cta">
              <Link
                to="/sign-up"
                className="landing-btn-primary"
                onClick={() => setMobileMenuOpen(false)}
              >
                Get Started
              </Link>
            </li>
          </ul>
        </SignedOut>

        <div className="landing-nav-actions">
          <SignedOut>
            <Link to="/sign-in" className="landing-btn landing-btn-ghost">
              Sign In
            </Link>
            <Link to="/sign-up" className="landing-btn landing-btn-primary">
              Get Started
            </Link>
          </SignedOut>

          <SignedIn>
            <SignedInActions />
          </SignedIn>
        </div>

        <button
          className="landing-mobile-menu-btn"
          onClick={() => setMobileMenuOpen(!mobileMenuOpen)}
          aria-label="Toggle menu"
        >
          <span></span>
          <span></span>
          <span></span>
        </button>
      </div>
    </nav>
  )
}

export default LandingNav
