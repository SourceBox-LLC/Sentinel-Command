import { useState } from "react"
import { Link } from "react-router-dom"
import { useAuth } from "@clerk/clerk-react"
import InstallCameraNodeCard from "./InstallCameraNodeCard.jsx"
import { requestAdminPromotion } from "../services/api"

// Dashboard empty-state heroes, differentiated by role. Admins get the
// "set up your first camera" checklist; members get a capability-focused
// welcome that tells them what they *can* do today instead of showing
// them a checklist they can't act on.
//
// Both variants share the .welcome-hero / .welcome-step CSS defined in
// index.css — no new styles needed for the split.

function CheckMarkIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="3" strokeLinecap="round" strokeLinejoin="round">
      <polyline points="20 6 9 17 4 12"/>
    </svg>
  )
}

export function AdminWelcomeHero() {
  return (
    <div className="welcome-hero">
      <div className="welcome-hero-header">
        <div className="welcome-hero-icon" aria-hidden="true">👋</div>
        <h2 className="welcome-hero-title">Welcome to Sentinel</h2>
        <p className="welcome-hero-subtitle">
          Your control plane is ready. Sentinel runs on your own hardware &mdash; let&rsquo;s get the first camera online.
        </p>
      </div>

      <ol className="welcome-checklist" role="list">
        <li className="welcome-step welcome-step-done">
          <span className="welcome-step-marker" aria-hidden="true">
            <CheckMarkIcon />
          </span>
          <div className="welcome-step-body">
            <div className="welcome-step-title">Workspace created</div>
            <div className="welcome-step-desc">You&rsquo;re signed in and ready to go.</div>
          </div>
        </li>

        <li className="welcome-step welcome-step-active">
          <span className="welcome-step-marker" aria-hidden="true">2</span>
          <div className="welcome-step-body">
            <div className="welcome-step-title">Install a CameraNode</div>
            <div className="welcome-step-desc">
              Run one command on the computer where your cameras live. CameraNode
              auto-registers with this org &mdash; no credentials to copy or paste.
            </div>
            {/*
              In-app install widget replaces the old "Add your first node"
              + "Installation guide ↗ (GitHub)" button pair.  The biggest
              moment of customer drop-off is here — they signed up, they
              see an empty grid, they need to run a CLI command.  Anything
              that takes them OUT of the app at this moment loses them.
              The widget auto-detects OS, shows the exact one-liner with
              a copy button, and animates a "waiting for connection"
              indicator so they know we'll notice when CameraNode comes up.
            */}
            <InstallCameraNodeCard />
            {/* No secondary "Generate node credentials manually" link
                here anymore — the widget itself calls POST /api/nodes
                on the user's first click, and the Settings link in
                the header is always available for users who want to
                see existing nodes or manage them. */}
          </div>
        </li>

        <li className="welcome-step welcome-step-pending">
          <span className="welcome-step-marker" aria-hidden="true">3</span>
          <div className="welcome-step-body">
            <div className="welcome-step-title">Camera goes live</div>
            <div className="welcome-step-desc">
              Once the node starts heartbeating, streams appear here automatically &mdash; usually within 30 seconds.
            </div>
          </div>
        </li>
      </ol>
    </div>
  )
}

export function MemberWelcomeHero({ orgName }) {
  const workspace = orgName || "this workspace"
  return (
    <div className="welcome-hero">
      <div className="welcome-hero-header">
        <div className="welcome-hero-icon" aria-hidden="true">👋</div>
        <h2 className="welcome-hero-title">
          {orgName ? `Welcome to ${orgName}` : "Welcome"}
        </h2>
        <p className="welcome-hero-subtitle">
          You&rsquo;ve joined as a member. Live camera feeds will appear here as an admin adds them &mdash; no setup on your end.
        </p>
      </div>

      <ol className="welcome-checklist" role="list">
        <li className="welcome-step welcome-step-done">
          <span className="welcome-step-marker" aria-hidden="true">
            <CheckMarkIcon />
          </span>
          <div className="welcome-step-body">
            <div className="welcome-step-title">Live monitoring</div>
            <div className="welcome-step-desc">
              Camera feeds appear on this page and auto-refresh every 5 seconds as they come online.
            </div>
          </div>
        </li>

        <li className="welcome-step welcome-step-done">
          <span className="welcome-step-marker" aria-hidden="true">
            <CheckMarkIcon />
          </span>
          <div className="welcome-step-body">
            <div className="welcome-step-title">Real-time motion alerts</div>
            <div className="welcome-step-desc">
              When a camera detects motion, you&rsquo;ll get a notification in the bell icon at the top right.
            </div>
          </div>
        </li>

        <li className="welcome-step welcome-step-done">
          <span className="welcome-step-marker" aria-hidden="true">
            <CheckMarkIcon />
          </span>
          <div className="welcome-step-body">
            <div className="welcome-step-title">Team workspace</div>
            <div className="welcome-step-desc">
              You&rsquo;re collaborating securely in {workspace}. An admin manages cameras and access; you focus on watching.
            </div>
          </div>
        </li>
      </ol>

      <RequestAdminAccessFootnote />
    </div>
  )
}


// In-app "Request admin access" CTA for members.  Replaces the
// previous static "Ask a workspace admin to promote you" text —
// gives the member a single click that fires inbox + email
// notifications to every admin in the org with the requester's
// identity attached.  Backend rate-limits to 3/hour per org so
// a member can't blast admins.
//
// State machine: idle → submitting → sent (sticky for the rest
// of the session — no point letting them re-spam admins after a
// successful request).  Errors render inline so the member can
// see what went wrong (e.g. they're already an admin via JWT
// they didn't realise had refreshed, or the rate limit kicked in
// from a hot-reload double-fire during dev).
function RequestAdminAccessFootnote() {
  const { getToken } = useAuth()
  const [status, setStatus] = useState("idle")  // idle | submitting | sent
  const [error, setError] = useState(null)

  const handleClick = async () => {
    setStatus("submitting")
    setError(null)
    try {
      await requestAdminPromotion(getToken)
      setStatus("sent")
    } catch (e) {
      setError(e.message || "Failed to send request — please try again later.")
      setStatus("idle")
    }
  }

  if (status === "sent") {
    return (
      <div className="welcome-hero-footnote welcome-hero-footnote-sent">
        ✓ Your request has been sent to your organization&rsquo;s admins.
        They&rsquo;ll see it in their inbox and by email.
      </div>
    )
  }

  return (
    <div className="welcome-hero-footnote">
      Need admin access?
      {" "}
      <button
        type="button"
        className="link-button"
        onClick={handleClick}
        disabled={status === "submitting"}
      >
        {status === "submitting" ? "Sending request…" : "Request it"}
      </button>
      {" "}
      and your org&rsquo;s admins will be notified.
      {error && (
        <div className="welcome-hero-footnote-error" role="alert">
          {error}
        </div>
      )}
    </div>
  )
}
