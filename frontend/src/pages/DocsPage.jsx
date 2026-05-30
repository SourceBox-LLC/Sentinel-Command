import { useEffect, useState } from "react"
import { Link } from "react-router-dom"

import { DocsProvider } from "./docs/context"
import { useToasts } from "../hooks/useToasts.jsx"

// One file per <section>. Each component is self-contained — sections that
// need shared state (the OS-tabs choice, the Copy-button toast) pull it from
// useDocs() rather than props. See pages/docs/context.jsx for the provider.
import GettingStarted from "./docs/GettingStarted"
import CloudNodeSetup from "./docs/CloudNodeSetup"
import Configuration from "./docs/Configuration"
import Deployment from "./docs/Deployment"
import MotionDetection from "./docs/MotionDetection"
import TerminalDashboard from "./docs/TerminalDashboard"
import Dashboard from "./docs/Dashboard"
import Recording from "./docs/Recording"
import CameraGroups from "./docs/CameraGroups"
import Notifications from "./docs/Notifications"
import Mcp from "./docs/Mcp"
import HomeAssistant from "./docs/HomeAssistant"
import Sentinel from "./docs/Sentinel"
import Plans from "./docs/Plans"
import Architecture from "./docs/Architecture"
import SecurityProcedures from "./docs/SecurityProcedures"
import Troubleshooting from "./docs/Troubleshooting"
import Faq from "./docs/Faq"
import ApiReference from "./docs/ApiReference"
import ApiRateLimits from "./docs/ApiRateLimits"
import Resources from "./docs/Resources"


// Order matches the sidebar order — drives the scrollspy's "topmost
// visible section" calculation.  Each entry is the section's id.
const DOC_SECTIONS = [
  "getting-started",
  "architecture",
  "cloudnode-setup",
  "configuration",
  "deployment",
  "motion-detection",
  "terminal-dashboard",
  "dashboard",
  "recording",
  "camera-groups",
  "notifications",
  "mcp",
  "home-assistant",
  "sentinel",
  "plans",
  "security-procedures",
  "troubleshooting",
  "faq",
  "api-reference",
  "api-rate-limits",
]


/**
 * Highlights the sidebar link matching whichever section is currently
 * topmost-visible in the viewport.  Uses a single IntersectionObserver
 * scoped to the section IDs in DOC_SECTIONS.
 *
 * The "active" section is the one with the smallest non-negative
 * `boundingClientRect.top` — i.e. the one that just entered (or is
 * about to enter) the top of the viewport.  Falls back to the
 * highest-positioned section among the currently-intersecting set
 * when nothing has entered yet (e.g. when the page loads scrolled
 * mid-document).
 */
function useScrollspy(sectionIds) {
  const [activeId, setActiveId] = useState(sectionIds[0] || "")

  useEffect(() => {
    if (typeof IntersectionObserver === "undefined") return

    const visible = new Map()

    const recompute = () => {
      // Pick the section whose top is closest to (but not far below)
      // the viewport top.  Sections above the fold have negative top
      // values; we want the largest of those (closest to zero).
      let bestId = null
      let bestTop = -Infinity
      for (const [id, top] of visible) {
        if (top <= 80 && top > bestTop) {
          bestTop = top
          bestId = id
        }
      }
      // If nothing is above the fold (we're at the top of the page),
      // pick the topmost intersecting section.
      if (!bestId) {
        let lowestTop = Infinity
        for (const [id, top] of visible) {
          if (top < lowestTop) {
            lowestTop = top
            bestId = id
          }
        }
      }
      if (bestId) setActiveId(bestId)
    }

    const observer = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          const id = entry.target.id
          if (entry.isIntersecting) {
            visible.set(id, entry.boundingClientRect.top)
          } else {
            visible.delete(id)
          }
        }
        recompute()
      },
      {
        // rootMargin top -80px so a section is considered "active" once
        // its heading scrolls under the page's sticky chrome (the docs
        // page has no sticky bar today, but this leaves headroom for
        // future "back to top" / breadcrumb additions).
        rootMargin: "-80px 0px -60% 0px",
        threshold: [0, 1],
      },
    )

    const elements = sectionIds
      .map((id) => document.getElementById(id))
      .filter(Boolean)
    elements.forEach((el) => observer.observe(el))

    // Recompute on plain scroll too — IntersectionObserver only fires on
    // boundary crossings, so a slow scroll between two intersecting
    // sections wouldn't update the active state otherwise.
    const onScroll = () => {
      for (const el of elements) {
        const rect = el.getBoundingClientRect()
        visible.set(el.id, rect.top)
      }
      recompute()
    }
    window.addEventListener("scroll", onScroll, { passive: true })

    return () => {
      observer.disconnect()
      window.removeEventListener("scroll", onScroll)
    }
  }, [sectionIds])

  return activeId
}


function DocsSidebar({ activeId }) {
  // Render each link with an active class when its href matches the
  // currently-spied section.  className is computed at render time so
  // React doesn't have to re-mount the <a>s on every active change.
  const linkClass = (id) =>
    `docs-sidebar-link${activeId === id ? " active" : ""}`

  return (
    <aside className="docs-sidebar">
      <div className="docs-sidebar-header">
        <h2>Sentinel</h2>
        <p>Documentation</p>
      </div>
      <nav className="docs-sidebar-nav">
        <div className="docs-sidebar-group">
          <div className="docs-sidebar-group-label">Introduction</div>
          <a href="#getting-started" className={linkClass("getting-started")}>Getting Started</a>
          <a href="#architecture" className={linkClass("architecture")}>Architecture</a>
        </div>
        <div className="docs-sidebar-group">
          <div className="docs-sidebar-group-label">CloudNode</div>
          <a href="#cloudnode-setup" className={linkClass("cloudnode-setup")}>Setup</a>
          <a href="#configuration" className={linkClass("configuration")}>Configuration</a>
          <a href="#deployment" className={linkClass("deployment")}>Deployment</a>
          <a href="#motion-detection" className={linkClass("motion-detection")}>Motion Detection</a>
          <a href="#terminal-dashboard" className={linkClass("terminal-dashboard")}>Terminal Dashboard</a>
        </div>
        <div className="docs-sidebar-group">
          <div className="docs-sidebar-group-label">Command Center</div>
          <a href="#dashboard" className={linkClass("dashboard")}>Dashboard & Features</a>
          <a href="#recording" className={linkClass("recording")}>Recording & Retention</a>
          <a href="#camera-groups" className={linkClass("camera-groups")}>Camera Groups</a>
          <a href="#notifications" className={linkClass("notifications")}>Notifications</a>
        </div>
        <div className="docs-sidebar-group">
          <div className="docs-sidebar-group-label">Integrations</div>
          <a href="#mcp" className={linkClass("mcp")}>MCP Integration</a>
          <a href="#home-assistant" className={linkClass("home-assistant")}>Home Assistant</a>
        </div>
        <div className="docs-sidebar-group">
          <div className="docs-sidebar-group-label">AI Agent</div>
          <a href="#sentinel" className={linkClass("sentinel")}>Sentinel</a>
        </div>
        <div className="docs-sidebar-group">
          <div className="docs-sidebar-group-label">Account & Security</div>
          <a href="#plans" className={linkClass("plans")}>Plans & Limits</a>
          <a href="#security-procedures" className={linkClass("security-procedures")}>Security Procedures</a>
        </div>
        <div className="docs-sidebar-group">
          <div className="docs-sidebar-group-label">Help</div>
          <a href="#troubleshooting" className={linkClass("troubleshooting")}>Troubleshooting</a>
          <a href="#faq" className={linkClass("faq")}>FAQ</a>
        </div>
        <div className="docs-sidebar-group">
          <div className="docs-sidebar-group-label">Reference</div>
          <a href="#api-reference" className={linkClass("api-reference")}>API Reference</a>
          <a href="#api-rate-limits" className={linkClass("api-rate-limits")}>API Rate Limits</a>
        </div>
      </nav>
      <div className="docs-sidebar-footer">
        <Link to="/sign-up" className="docs-sidebar-btn">
          Get Started Free
        </Link>
      </div>
    </aside>
  )
}


/**
 * Floating Back-to-top button.  Fades in once the page is scrolled
 * past ~600px; smooth-scrolls back on click.  Position is fixed
 * bottom-right with a generous mobile-safe inset.  Single instance
 * mounted by DocsPage — not shared across the app, since other pages
 * are short enough not to need it.
 */
function BackToTopButton() {
  const [visible, setVisible] = useState(false)

  useEffect(() => {
    const onScroll = () => setVisible(window.scrollY > 600)
    onScroll()
    window.addEventListener("scroll", onScroll, { passive: true })
    return () => window.removeEventListener("scroll", onScroll)
  }, [])

  return (
    <button
      type="button"
      className={`docs-back-to-top${visible ? " visible" : ""}`}
      onClick={() => window.scrollTo({ top: 0, behavior: "smooth" })}
      aria-label="Back to top"
      title="Back to top"
    >
      <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
        <line x1="12" y1="19" x2="12" y2="5" />
        <polyline points="5 12 12 5 19 12" />
      </svg>
    </button>
  )
}


function DocsPage() {
  const { showToast } = useToasts()
  const activeId = useScrollspy(DOC_SECTIONS)

  // Delegated click handler for any `.docs-anchor` link inside a docs
  // heading.  Three things on click:
  //   1. Copy the full URL+hash to the clipboard.
  //   2. Update the URL bar via history.pushState (so the link IS
  //      shareable from the address bar too).
  //   3. Smooth-scroll the target heading into view.
  // Single handler bound to the document instead of touching 19
  // individual section files.
  useEffect(() => {
    const handler = (event) => {
      const anchor = event.target.closest(".docs-anchor")
      if (!anchor) return
      event.preventDefault()
      const href = anchor.getAttribute("href") || ""
      const targetId = href.startsWith("#") ? href.slice(1) : ""
      const fullUrl = `${window.location.origin}${window.location.pathname}${href}`

      // Update history first so the URL bar shows the deep-link.  We
      // use pushState rather than assigning location.hash because the
      // latter triggers a default scroll-jump that competes with our
      // smooth-scroll.
      window.history.pushState(null, "", href)

      // Smooth-scroll to the target.  Falls back silently if the
      // element doesn't exist (shouldn't happen — every `.docs-anchor`
      // we render points at its parent section's id).
      if (targetId) {
        const target = document.getElementById(targetId)
        if (target) target.scrollIntoView({ behavior: "smooth", block: "start" })
      }

      // Copy to clipboard + toast.  Clipboard API requires a secure
      // context; if we don't have it (rare — local file:// preview),
      // we silently skip the copy and rely on the URL bar update.
      if (navigator.clipboard?.writeText) {
        navigator.clipboard.writeText(fullUrl).then(
          () => showToast("Link copied to clipboard", "success"),
          () => showToast("Couldn't copy — clipboard blocked", "error"),
        )
      } else {
        showToast("Link in URL bar — copy manually", "info")
      }
    }
    document.addEventListener("click", handler)
    return () => document.removeEventListener("click", handler)
  }, [showToast])

  return (
    <DocsProvider>
      <div className="docs-layout">
        <DocsSidebar activeId={activeId} />
        <main className="docs-content">
          <div className="docs-content-inner">
            <div className="docs-hero-banner" aria-hidden="true">
              <picture>
                <source srcSet="/images/docs-hero.webp" type="image/webp" />
                <img
                  src="/images/docs-hero.jpg"
                  alt=""
                  className="docs-hero-banner-image"
                  width="2240"
                  height="960"
                  loading="eager"
                />
              </picture>
            </div>

            <div className="docs-header">
              <h1>Documentation</h1>
              <p>How to use Sentinel — installing CloudNode on your camera machine, working with the cloud dashboard, and connecting AI tools over MCP.</p>
            </div>

            <GettingStarted />
            <CloudNodeSetup />
            <Configuration />
            <Deployment />
            <MotionDetection />
            <TerminalDashboard />
            <Dashboard />
            <Recording />
            <CameraGroups />
            <Notifications />
            <Mcp />
            <HomeAssistant />
            <Sentinel />
            <Plans />
            <Architecture />
            <SecurityProcedures />
            <Troubleshooting />
            <Faq />
            <ApiReference />
            <ApiRateLimits />
            <Resources />

            <div className="docs-cta">
              <p>Ready to set up your security camera system?</p>
              <Link to="/sign-up" className="docs-cta-btn">Create Free Account</Link>
            </div>
          </div>
        </main>
        <BackToTopButton />
      </div>
    </DocsProvider>
  )
}

export default DocsPage
