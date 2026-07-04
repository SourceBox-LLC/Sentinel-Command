import { lazy, Suspense, useEffect } from "react"
import { Routes, Route, Navigate, useNavigate } from "react-router-dom"
import { useAuth, useClerk, useOrganization, CreateOrganization } from "@clerk/clerk-react"
import Layout from "./components/Layout.jsx"
import LoadingSpinner from "./components/LoadingSpinner.jsx"
import ErrorBoundary from "./components/ErrorBoundary.jsx"
import CookieNotice from "./components/CookieNotice.jsx"
import { setUnauthorizedHandler } from "./services/api.js"

// Lazy-load pages to reduce initial bundle size
const SignInPage = lazy(() => import("./pages/SignInPage.jsx"))
const SignUpPage = lazy(() => import("./pages/SignUpPage.jsx"))
const DashboardPage = lazy(() => import("./pages/DashboardPage.jsx"))
const SettingsPage = lazy(() => import("./pages/SettingsPage.jsx"))
const AdminPage = lazy(() => import("./pages/AdminPage.jsx"))
const TestHlsPage = lazy(() => import("./pages/TestHlsPage.jsx"))
const McpPage = lazy(() => import("./pages/McpPage.jsx"))
const IntegrationsPage = lazy(() => import("./pages/IntegrationsPage.jsx"))
const IncidentsPage = lazy(() => import("./pages/IncidentsPage.jsx"))
const PricingPage = lazy(() => import("./pages/PricingPage.jsx"))

function RequireOrg({ children }) {
  const { organization, isLoaded } = useOrganization()
  const { isSignedIn } = useAuth()

  if (!isLoaded) {
    return (
      <div className="loading-container">
        <div className="loading-spinner"></div>
      </div>
    )
  }

  if (!isSignedIn) {
    return <Navigate to="/sign-in" replace />
  }

  if (!organization) {
    return (
      <div className="org-creation-page">
        <div className="org-creation-card">
          <div className="org-creation-icon">🏢</div>
          <h1 className="org-creation-title">Create Your Organization</h1>
          <p className="org-creation-subtitle">
            Organizations help you manage cameras and collaborate with your team.
            You can invite members and control permissions.
          </p>
          <div className="org-creation-form">
            <CreateOrganization afterCreateOrganizationUrl="/dashboard" />
          </div>
        </div>
      </div>
    )
  }

  return children
}

function RequireAdmin({ children }) {
  const { organization, membership, isLoaded } = useOrganization()
  const { isSignedIn, has } = useAuth()

  if (!isLoaded) {
    return (
      <div className="loading-container">
        <div className="loading-spinner"></div>
      </div>
    )
  }

  if (!isSignedIn) {
    return <Navigate to="/sign-in" replace />
  }

  if (!organization) {
    return <Navigate to="/dashboard" replace />
  }

  const isAdmin = has?.({ role: "org:admin" }) ||
    membership?.role === "org:admin" ||
    membership?.role === "admin"

  if (!isAdmin) {
    return <Navigate to="/dashboard" replace />
  }

  return children
}

// The marketing landing page, documentation, pricing, legal, and security
// pages now live on the standalone website at sentinel-command.com.
// Redirect any visit to the bare root there.
const STANDALONE_SITE = "https://sentinel-command.com"

function RedirectToStandalone() {
  useEffect(() => {
    window.location.replace(STANDALONE_SITE)
  }, [])
  return (
    <div className="loading-container">
      <LoadingSpinner />
    </div>
  )
}

function App() {
  const { signOut } = useClerk()
  const navigate = useNavigate()

  // Register the app-wide reaction to a 401 from any API call (see
  // setUnauthorizedHandler in services/api.js).  A 401 means the session
  // credential was rejected — revoked, expired beyond refresh, or the org
  // was deleted — so we end the Clerk session and bounce to sign-in.
  // Without this, a dead session left every component throwing its own
  // 401 error while the UI sat broken behind the toasts.
  useEffect(() => {
    setUnauthorizedHandler(() => {
      signOut().finally(() => navigate("/sign-in", { replace: true }))
    })
    return () => setUnauthorizedHandler(null)
  }, [signOut, navigate])

  return (
    <ErrorBoundary>
      <Suspense fallback={<div className="loading-container"><LoadingSpinner /></div>}>
        <Routes>
        {/* Root redirects to the standalone marketing/docs site */}
        <Route path="/" element={<RedirectToStandalone />} />

        {/* Auth routes (public but use Clerk components) */}
        <Route path="/sign-in/*" element={<SignInPage />} />
        <Route path="/sign-up/*" element={<SignUpPage />} />

        {/* Test route (admin only, gated by VITE_ENABLE_TEST_ROUTES build flag).
            The page contains hardcoded localhost URLs and dev-only console
            logging — never registered in production builds. */}
        {import.meta.env.VITE_ENABLE_TEST_ROUTES === "true" && (
          <Route element={<Layout />}>
            <Route
              path="/test-hls"
              element={
                <RequireAdmin>
                  <TestHlsPage />
                </RequireAdmin>
              }
            />
          </Route>
        )}

        {/* Authenticated routes with Layout */}
        <Route element={<Layout />}>
          <Route
            path="/dashboard"
            element={
              <RequireOrg>
                <DashboardPage />
              </RequireOrg>
            }
          />
          <Route
            path="/settings"
            element={
              <RequireAdmin>
                <SettingsPage />
              </RequireAdmin>
            }
          />
          <Route
            path="/admin"
            element={
              <RequireAdmin>
                <AdminPage />
              </RequireAdmin>
            }
          />
          <Route
            path="/mcp"
            element={
              <RequireAdmin>
                <McpPage />
              </RequireAdmin>
            }
          />
          <Route
            path="/integrations"
            element={
              <RequireAdmin>
                <IntegrationsPage />
              </RequireAdmin>
            }
          />
          {/* /incidents and /incidents/:incidentId share the same page;
              the page reads useParams().incidentId to open the report
              modal on mount when present.  Lets notification deep-links
              resolve. */}
          <Route
            path="/incidents"
            element={
              <RequireAdmin>
                <IncidentsPage />
              </RequireAdmin>
            }
          />
          <Route
            path="/incidents/:incidentId"
            element={
              <RequireAdmin>
                <IncidentsPage />
              </RequireAdmin>
            }
          />
          <Route
            path="/pricing"
            element={
              <RequireOrg>
                <PricingPage />
              </RequireOrg>
            }
          />
        </Route>
        </Routes>
      </Suspense>
      <CookieNotice />
    </ErrorBoundary>
  )
}

export default App