import { Link } from "react-router-dom"

// React Router renders NOTHING for an unmatched path, and the backend's
// SPA middleware serves index.html for every unknown URL — so before this
// existed, any typo'd or stale link produced a blank black page returning
// HTTP 200. Silent, and indistinguishable from a broken app.
//
// It bit hardest on links to pages that moved to the standalone marketing
// site (/security, /legal/*, /docs) and were left behind as relative
// hrefs in the app.
function NotFoundPage() {
  return (
    <div className="notfound-container">
      <div className="notfound-code">404</div>
      <h1>Page not found</h1>
      <p>
        This URL doesn&apos;t exist in Command Center. If you followed a link
        from an older page, it may have moved to{" "}
        <a href="https://sentinel-command.com" rel="noopener noreferrer">
          sentinel-command.com
        </a>
        .
      </p>
      <Link to="/dashboard" className="notfound-cta">
        Back to dashboard
      </Link>
    </div>
  )
}

export default NotFoundPage
