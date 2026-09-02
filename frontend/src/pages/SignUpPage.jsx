import { SignUp } from "@clerk/clerk-react"
import { Navigate, Link } from "react-router-dom"
import { LogoMark } from "../components/Logo.jsx"
import { IS_LOCAL_AUTH } from "../auth/index.jsx"

function SignUpPage() {
  // Self-hosted: a single fixed admin account, provisioned once via
  // backend/scripts/hash_local_admin_password.py — there's nothing to
  // sign up for.
  if (IS_LOCAL_AUTH) {
    return <Navigate to="/sign-in" replace />
  }

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
        <SignUp
          routing="path"
          path="/sign-up"
          signInUrl="/sign-in"
          redirectUrl="/dashboard"
          afterSignUpUrl="/dashboard"
        />
        <p className="auth-legal-consent">
          By creating an account you agree to our{" "}
          <a href="https://sentinel-command.com/legal/terms">Terms of Service</a> and{" "}
          <a href="https://sentinel-command.com/legal/privacy">Privacy Policy</a>.
        </p>
      </div>
    </div>
  )
}

export default SignUpPage
