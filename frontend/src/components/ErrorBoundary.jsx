import { Component } from "react"

/**
 * Top-level error boundary.
 *
 * React renders a blank screen if any component throws during render
 * with no boundary above it. That's the worst-case UX — the user has
 * no idea what happened, no way to recover except a manual page
 * refresh, and we get no signal in production. Wrapping <App /> in
 * this catches every unhandled render error, shows a usable fallback
 * with a refresh button, and (when Sentry is configured) reports the
 * error to Sentry's React handler so we hear about it.
 *
 * Class component because error boundaries require it — React's
 * function-component error hook still hasn't shipped as of React 19.
 *
 * Note: this only catches errors during render, lifecycle methods,
 * and constructors. Async errors (Promise rejections in handlers,
 * setTimeout callbacks) bypass it. For those, see the fetch wrappers
 * in services/api.js and the .catch() in IncidentReportModal etc.
 */
class ErrorBoundary extends Component {
  constructor(props) {
    super(props)
    this.state = { hasError: false, error: null }
  }

  static getDerivedStateFromError(error) {
    return { hasError: true, error }
  }

  componentDidCatch(error, info) {
    // Log to console for dev visibility, and surface to Sentry if it's
    // initialized (the SDK auto-attaches a global handler but the
    // explicit call here gives us a structured component-stack frame).
    console.error("[ErrorBoundary] caught:", error, info)
    if (typeof window !== "undefined" && window.Sentry?.captureException) {
      window.Sentry.captureException(error, {
        contexts: { react: { componentStack: info?.componentStack } },
      })
    }
  }

  handleReload = () => {
    window.location.reload()
  }

  render() {
    if (!this.state.hasError) return this.props.children

    const message = this.state.error?.message || "Something went wrong."
    return (
      <div
        style={{
          minHeight: "100vh",
          display: "flex",
          alignItems: "center",
          justifyContent: "center",
          background: "var(--bg-primary, #0a0a0a)",
          color: "var(--text-primary, #fff)",
          padding: "2rem",
          fontFamily: "system-ui, sans-serif",
        }}
      >
        <div style={{ maxWidth: "32rem", textAlign: "center" }}>
          <div style={{ fontSize: "3rem", marginBottom: "1rem" }}>⚠️</div>
          <h1 style={{ fontSize: "1.5rem", marginBottom: "0.75rem" }}>
            Something broke on this page
          </h1>
          <p
            style={{
              color: "var(--text-muted, #888)",
              marginBottom: "1.5rem",
              fontSize: "0.95rem",
            }}
          >
            The error has been logged. Try reloading — if it keeps
            happening, email{" "}
            <a
              href="mailto:support@sentinel-command.com"
              style={{ color: "var(--accent-green, #22c55e)" }}
            >
              support@sentinel-command.com
            </a>{" "}
            with this message:
          </p>
          <pre
            style={{
              textAlign: "left",
              padding: "0.75rem 1rem",
              background: "var(--bg-secondary, #1a1a1a)",
              border: "1px solid var(--border, #333)",
              borderRadius: "6px",
              fontSize: "0.85rem",
              overflow: "auto",
              marginBottom: "1.5rem",
            }}
          >
            {message}
          </pre>
          <button
            onClick={this.handleReload}
            style={{
              padding: "0.6rem 1.5rem",
              background: "var(--accent-green, #22c55e)",
              color: "var(--bg-primary, #0a0a0a)",
              border: "none",
              borderRadius: "6px",
              fontWeight: 600,
              cursor: "pointer",
            }}
          >
            Reload page
          </button>
        </div>
      </div>
    )
  }
}

export default ErrorBoundary
