import { SignIn } from "@clerk/clerk-react"
import { Link } from "react-router-dom"

function SignInPage() {
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
        <SignIn 
          routing="path" 
          path="/sign-in" 
          signUpUrl="/sign-up"
          redirectUrl="/dashboard"
          afterSignInUrl="/dashboard"
        />
      </div>
    </div>
  )
}

export default SignInPage