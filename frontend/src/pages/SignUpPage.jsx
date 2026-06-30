import { SignUp } from "@clerk/clerk-react"
import { Link } from "react-router-dom"

function SignUpPage() {
  return (
    <div className="auth-layout">
      <div className="bg-grid"></div>
      <div className="bg-glow bg-glow-1"></div>
      <div className="bg-glow bg-glow-2"></div>
      
      <Link to="/" className="auth-logo">
        <span className="auth-logo-icon">🛡️</span>
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
          <Link to="/legal/terms">Terms of Service</Link> and{" "}
          <Link to="/legal/privacy">Privacy Policy</Link>.
        </p>
      </div>
    </div>
  )
}

export default SignUpPage