import { useEffect, useState } from "react"
import { Link } from "react-router-dom"

// Lightweight, dismissible cookie NOTICE — not a consent-management
// platform. We only set strictly-necessary authentication-session
// cookies (via Clerk) and run no analytics, ad, or tracking cookies,
// so under ePrivacy these are exempt from prior consent. This banner
// is therefore informational: it tells EEA/UK visitors what we use and
// links to the Cookies section of the Privacy Policy. If tracking
// cookies are ever added, this must be upgraded to real consent (with
// a reject option) BEFORE those cookies are set.
const STORAGE_KEY = "cookie_notice_dismissed_v1"

function CookieNotice() {
  const [visible, setVisible] = useState(false)

  useEffect(() => {
    try {
      if (localStorage.getItem(STORAGE_KEY) !== "1") setVisible(true)
    } catch {
      // localStorage blocked (private mode / cookies-off): show the
      // notice; it just won't persist dismissal. Harmless.
      setVisible(true)
    }
  }, [])

  const dismiss = () => {
    try {
      localStorage.setItem(STORAGE_KEY, "1")
    } catch {
      // ignore — dismissal won't persist, but the UI still closes
    }
    setVisible(false)
  }

  if (!visible) return null

  return (
    <div className="cookie-notice" role="region" aria-label="Cookie notice">
      <p className="cookie-notice-text">
        We use only strictly-necessary cookies to keep you signed in. No
        analytics, ad, or tracking cookies — ever.{" "}
        <Link to="/legal/privacy">Learn more</Link>.
      </p>
      <button type="button" className="cookie-notice-btn" onClick={dismiss}>
        Got it
      </button>
    </div>
  )
}

export default CookieNotice
