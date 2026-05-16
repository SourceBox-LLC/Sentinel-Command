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
      </div>
    </div>
  )
}

export default SignUpPage