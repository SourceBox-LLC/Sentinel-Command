import { useState } from "react"
import { SignIn } from "@clerk/clerk-react"
import { useNavigate, Link } from "react-router-dom"
import { LogoMark } from "../components/Logo.jsx"
import { IS_LOCAL_AUTH, useLocalLogin } from "../auth/index.jsx"
import { API_URL } from "../services/api.js"

// Self-hosted (AUTH_PROVIDER=local): a single fixed admin account, no
// Clerk involved. There's no sign-up flow to match — the admin account
// is provisioned once via backend/scripts/hash_local_admin_password.py.
// Named export (default is SignInPage) so tests can render it directly
// without depending on the build-time VITE_AUTH_PROVIDER branch below.
export function LocalSignInForm() {
  const login = useLocalLogin()
  const navigate = useNavigate()
  const [username, setUsername] = useState("")
  const [password, setPassword] = useState("")
  const [error, setError] = useState("")
  const [submitting, setSubmitting] = useState(false)

  const handleSubmit = async (event) => {
    event.preventDefault()
    setError("")
    setSubmitting(true)
    try {
      const res = await fetch(`${API_URL}/api/auth/local/login`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ username, password }),
      })
      if (!res.ok) {
        setError("Invalid username or password.")
        return
      }
      const data = await res.json()
      login(data.token)
      navigate("/dashboard", { replace: true })
    } catch {
      setError("Could not reach the server. Please try again.")
    } finally {
      setSubmitting(false)
    }
  }

  return (
    <form className="local-signin-form" onSubmit={handleSubmit}>
      <h1 className="local-signin-title">Sign in</h1>
      <label className="local-signin-label" htmlFor="local-signin-username">
        Username
      </label>
      <input
        id="local-signin-username"
        type="text"
        className="local-signin-input"
        value={username}
        onChange={(event) => setUsername(event.target.value)}
        autoComplete="username"
        autoFocus
        required
      />
      <label className="local-signin-label" htmlFor="local-signin-password">
        Password
      </label>
      <input
        id="local-signin-password"
        type="password"
        className="local-signin-input"
        value={password}
        onChange={(event) => setPassword(event.target.value)}
        autoComplete="current-password"
        required
      />
      {error && <p className="local-signin-error">{error}</p>}
      <button type="submit" className="btn btn-primary local-signin-submit" disabled={submitting}>
        {submitting ? "Signing in…" : "Sign in"}
      </button>
    </form>
  )
}

function SignInPage() {
  return (
    <div className="auth-layout">
      <div className="bg-grid"></div>
      <div className="bg-glow bg-glow-1"></div>
      <div className="bg-glow bg-glow-2"></div>

      <Link to="/" className="auth-logo">
        <span className="auth-logo-icon"><LogoMark size={30} /></span>
        <span className="auth-logo-highlight">Sentinel</span>
        <span className="auth-logo-text"> by SourceBox</span>
      </Link>

      <div className="auth-page">
        {IS_LOCAL_AUTH ? (
          <LocalSignInForm />
        ) : (
          <SignIn
            routing="path"
            path="/sign-in"
            signUpUrl="/sign-up"
            redirectUrl="/dashboard"
            afterSignInUrl="/dashboard"
          />
        )}
      </div>
    </div>
  )
}

export default SignInPage
