/**
 * Local (self-hosted) auth implementation — a single fixed admin
 * account, no Clerk involved at all. Selected by ../index.jsx when
 * VITE_AUTH_PROVIDER=local.
 *
 * Every hook/component here mirrors the shape of its @clerk/clerk-react
 * counterpart closely enough that the ~25 files consuming useAuth() /
 * useOrganization() / etc. via the facade in ../index.jsx don't need to
 * know which mode is active.
 */

import { createContext, useCallback, useContext, useEffect, useState } from "react"
import { API_URL } from "../services/api.js"

const TOKEN_KEY = "sentinel_local_token"

// Refresh once less than this much life remains on the token, mirroring
// backend/app/core/local_auth.py's REFRESH_THRESHOLD_SECONDS — an open
// tab never actually reaches expiry, matching how Clerk refreshes its
// own tokens invisibly.
const REFRESH_THRESHOLD_MS = 24 * 3600 * 1000
const REFRESH_CHECK_INTERVAL_MS = 60 * 1000

// Stable identity — a single fixed org, never re-created, so consumers
// that use organization.id as an effect/fetch dependency don't churn on
// every re-render. There is no local Organization table (see
// backend/app/core/config.py's LOCAL_ORG_ID) — this object exists only
// to satisfy the shape Clerk's useOrganization() returns.
const FAKE_ORG = {
  id: "self-host",
  name: "Local Install",
  imageUrl: null,
  membersCount: 1,
  createdAt: new Date(0),
}
const FAKE_MEMBERSHIP = { role: "org:admin" }

function readToken() {
  try {
    return localStorage.getItem(TOKEN_KEY)
  } catch {
    return null
  }
}

function writeToken(token) {
  try {
    localStorage.setItem(TOKEN_KEY, token)
  } catch {
    // Private browsing / storage disabled — session just won't survive
    // a reload, which is a degraded-but-safe fallback, not a crash.
  }
}

function clearToken() {
  try {
    localStorage.removeItem(TOKEN_KEY)
  } catch {
    // ignore
  }
}

function decodeJwtExpMs(token) {
  try {
    const payload = token.split(".")[1]
    const json = atob(payload.replace(/-/g, "+").replace(/_/g, "/"))
    const { exp } = JSON.parse(json)
    return typeof exp === "number" ? exp * 1000 : null
  } catch {
    return null
  }
}

const LocalAuthContext = createContext(null)

function useLocalAuthContext() {
  const ctx = useContext(LocalAuthContext)
  if (!ctx) {
    throw new Error("Local auth hooks must be used within AuthProvider")
  }
  return ctx
}

export function LocalAuthProvider({ children }) {
  const [isSignedIn, setIsSignedIn] = useState(() => !!readToken())

  const login = useCallback((token) => {
    writeToken(token)
    setIsSignedIn(true)
  }, [])

  const logout = useCallback(async () => {
    clearToken()
    setIsSignedIn(false)
  }, [])

  // Keep the session alive across a long-open tab (this is a
  // security-camera dashboard plausibly left open on a wall-mounted
  // display) — refresh the token well before its 30-day expiry rather
  // than forcing a re-login.
  useEffect(() => {
    if (!isSignedIn) return undefined

    let cancelled = false

    const maybeRefresh = async () => {
      const token = readToken()
      if (!token) return
      const expMs = decodeJwtExpMs(token)
      if (expMs == null || expMs - Date.now() > REFRESH_THRESHOLD_MS) return
      try {
        const res = await fetch(`${API_URL}/api/auth/local/refresh`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ token }),
        })
        if (cancelled) return
        if (!res.ok) {
          // Refresh token itself is invalid/expired — sign out so route
          // guards redirect to /sign-in instead of every subsequent API
          // call silently failing with 401.
          await logout()
          return
        }
        const data = await res.json()
        writeToken(data.token)
      } catch {
        // Network blip — try again on the next tick.
      }
    }

    maybeRefresh()
    const interval = setInterval(maybeRefresh, REFRESH_CHECK_INTERVAL_MS)
    return () => {
      cancelled = true
      clearInterval(interval)
    }
  }, [isSignedIn, logout])

  return (
    <LocalAuthContext.Provider value={{ isSignedIn, login, logout }}>
      {children}
    </LocalAuthContext.Provider>
  )
}

export function useAuth() {
  const { isSignedIn } = useLocalAuthContext()
  return {
    isLoaded: true,
    isSignedIn,
    getToken: async () => readToken(),
    // Single fixed admin is always org:admin — no Clerk permission
    // bitmap to check against.
    has: () => true,
  }
}

export function useOrganization() {
  const { isSignedIn } = useLocalAuthContext()
  return {
    isLoaded: true,
    organization: isSignedIn ? FAKE_ORG : null,
    membership: isSignedIn ? FAKE_MEMBERSHIP : null,
  }
}

export function useClerk() {
  const { logout } = useLocalAuthContext()
  return { signOut: logout }
}

export function useLocalLogin() {
  const { login } = useLocalAuthContext()
  return login
}

export function SignedIn({ children }) {
  const { isSignedIn } = useLocalAuthContext()
  return isSignedIn ? children : null
}

export function SignedOut({ children }) {
  const { isSignedIn } = useLocalAuthContext()
  return isSignedIn ? null : children
}

// Single fixed org — nothing to switch between.
export function OrganizationSwitcher() {
  return null
}

// Unreachable in local mode: useOrganization() never returns a null
// organization while signed in, so App.jsx's RequireOrg never renders
// this. Exported anyway so consumer files can import it unconditionally.
export function CreateOrganization() {
  return null
}

export function UserButton() {
  const [open, setOpen] = useState(false)
  const { logout } = useLocalAuthContext()

  return (
    <div className="local-user-button">
      <button
        type="button"
        className="local-user-button-trigger"
        onClick={() => setOpen((o) => !o)}
        aria-label="Account menu"
        aria-expanded={open}
      >
        A
      </button>
      {open && (
        <div className="local-user-button-menu" role="menu">
          <div className="local-user-button-label">Local Admin</div>
          <button
            type="button"
            className="local-user-button-signout"
            role="menuitem"
            onClick={() => {
              setOpen(false)
              logout()
            }}
          >
            Sign out
          </button>
        </div>
      )}
    </div>
  )
}
