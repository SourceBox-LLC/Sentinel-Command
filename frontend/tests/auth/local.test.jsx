// Interaction tests for the local (self-hosted) auth implementation
// (src/auth/local.jsx) — the code path that actually runs when a real
// user opens the app with AUTH_PROVIDER=local. These render real
// components through @testing-library/react (not just unit-test plain
// functions) so a login/logout click goes through the same DOM event
// pipeline a browser would use.
//
// Tested directly against local.jsx rather than through the
// ../auth/index.jsx facade because IS_LOCAL there is fixed by
// VITE_AUTH_PROVIDER at Vite build/config time, not per-test-file
// switchable — but local.jsx's exports are exactly what the facade
// selects in local mode, so this exercises the real implementation.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import { useState } from "react"
import { render, screen, waitFor } from "@testing-library/react"
import userEvent from "@testing-library/user-event"
import {
  LocalAuthProvider,
  useAuth,
  useOrganization,
  useClerk,
  SignedIn,
  SignedOut,
  UserButton,
} from "../../src/auth/local.jsx"

const TOKEN_KEY = "sentinel_local_token"

// Well beyond local.jsx's 24h refresh threshold, so mounting a
// provider with one of these doesn't trigger the background refresh
// effect and interfere with an unrelated assertion. Tests that
// exercise the refresh effect itself use a short expiry explicitly.
const FAR_FUTURE_SECONDS = 30 * 24 * 3600

function makeToken(expInSeconds) {
  const header = btoa(JSON.stringify({ alg: "none" }))
  const payload = btoa(JSON.stringify({ exp: Math.floor(Date.now() / 1000) + expInSeconds }))
  return `${header}.${payload}.sig`
}

function Probe() {
  const { isSignedIn, getToken } = useAuth()
  const { organization, membership } = useOrganization()
  const [capturedToken, setCapturedToken] = useState(null)
  return (
    <div>
      <span data-testid="signed-in">{String(isSignedIn)}</span>
      <span data-testid="org-id">{organization?.id ?? "none"}</span>
      <span data-testid="role">{membership?.role ?? "none"}</span>
      <span data-testid="captured-token">{capturedToken ?? "none"}</span>
      <button onClick={async () => setCapturedToken(await getToken())}>
        capture token
      </button>
    </div>
  )
}

beforeEach(() => {
  localStorage.clear()
  // Default stub: no test in this file (other than the dedicated
  // refresh-effect ones below) expects a network call, so any surprise
  // call fails loudly instead of silently mutating localStorage.
  vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("unexpected fetch call")))
})

afterEach(() => {
  vi.unstubAllGlobals()
})

describe("LocalAuthProvider / useAuth / useOrganization", () => {
  it("starts signed out with no organization when localStorage has no token", () => {
    render(
      <LocalAuthProvider>
        <Probe />
      </LocalAuthProvider>,
    )
    expect(screen.getByTestId("signed-in")).toHaveTextContent("false")
    expect(screen.getByTestId("org-id")).toHaveTextContent("none")
    expect(screen.getByTestId("role")).toHaveTextContent("none")
  })

  it("starts signed in when localStorage already has a token (page reload case)", () => {
    localStorage.setItem(TOKEN_KEY, makeToken(FAR_FUTURE_SECONDS))
    render(
      <LocalAuthProvider>
        <Probe />
      </LocalAuthProvider>,
    )
    expect(screen.getByTestId("signed-in")).toHaveTextContent("true")
    expect(screen.getByTestId("org-id")).toHaveTextContent("self-host")
    expect(screen.getByTestId("role")).toHaveTextContent("org:admin")
  })

  it("getToken() returns the token currently in localStorage", async () => {
    const token = makeToken(FAR_FUTURE_SECONDS)
    localStorage.setItem(TOKEN_KEY, token)
    const user = userEvent.setup()
    render(
      <LocalAuthProvider>
        <Probe />
      </LocalAuthProvider>,
    )
    await user.click(screen.getByText("capture token"))
    await waitFor(() => expect(screen.getByTestId("captured-token")).toHaveTextContent(token))
  })
})

describe("useClerk().signOut()", () => {
  function SignOutButton() {
    const { signOut } = useClerk()
    return <button onClick={signOut}>sign out</button>
  }

  it("clears the stored token and flips isSignedIn to false", async () => {
    localStorage.setItem(TOKEN_KEY, makeToken(FAR_FUTURE_SECONDS))
    const user = userEvent.setup()
    render(
      <LocalAuthProvider>
        <Probe />
        <SignOutButton />
      </LocalAuthProvider>,
    )
    expect(screen.getByTestId("signed-in")).toHaveTextContent("true")

    await user.click(screen.getByText("sign out"))

    await waitFor(() => expect(screen.getByTestId("signed-in")).toHaveTextContent("false"))
    expect(localStorage.getItem(TOKEN_KEY)).toBeNull()
  })
})

describe("SignedIn / SignedOut", () => {
  it("renders only the matching branch based on session state", () => {
    render(
      <LocalAuthProvider>
        <SignedIn>
          <span>visible-when-in</span>
        </SignedIn>
        <SignedOut>
          <span>visible-when-out</span>
        </SignedOut>
      </LocalAuthProvider>,
    )
    expect(screen.queryByText("visible-when-in")).not.toBeInTheDocument()
    expect(screen.getByText("visible-when-out")).toBeInTheDocument()
  })
})

describe("UserButton", () => {
  it("opens a menu with Sign out on click, and signing out clears the session", async () => {
    localStorage.setItem(TOKEN_KEY, makeToken(FAR_FUTURE_SECONDS))
    const user = userEvent.setup()
    render(
      <LocalAuthProvider>
        <Probe />
        <UserButton />
      </LocalAuthProvider>,
    )

    expect(screen.queryByText("Sign out")).not.toBeInTheDocument()

    await user.click(screen.getByRole("button", { name: /account menu/i }))
    expect(screen.getByText("Local Admin")).toBeInTheDocument()
    expect(screen.getByText("Sign out")).toBeInTheDocument()

    await user.click(screen.getByText("Sign out"))

    await waitFor(() => expect(screen.getByTestId("signed-in")).toHaveTextContent("false"))
    expect(localStorage.getItem(TOKEN_KEY)).toBeNull()
  })
})

// The background token-refresh effect (see local.jsx's LocalAuthProvider
// useEffect) fires on mount for any token under the 24h threshold, so
// these use a short expiry deliberately.
describe("background token refresh", () => {
  it("replaces the stored token when a near-expiry token is refreshed", async () => {
    localStorage.setItem(TOKEN_KEY, makeToken(3600)) // 1h — under the 24h threshold
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({ ok: true, json: async () => ({ token: "refreshed.token.value" }) }),
    )

    render(
      <LocalAuthProvider>
        <Probe />
      </LocalAuthProvider>,
    )

    await waitFor(() => expect(localStorage.getItem(TOKEN_KEY)).toBe("refreshed.token.value"))
    expect(fetch).toHaveBeenCalledWith(
      "/api/auth/local/refresh",
      expect.objectContaining({ method: "POST" }),
    )
    // Still signed in with the fresh token — refresh doesn't bounce the session.
    expect(screen.getByTestId("signed-in")).toHaveTextContent("true")
  })

  it("signs out when the refresh call itself is rejected (token expired/invalid)", async () => {
    localStorage.setItem(TOKEN_KEY, makeToken(3600))
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: false, status: 401 }))

    render(
      <LocalAuthProvider>
        <Probe />
      </LocalAuthProvider>,
    )

    await waitFor(() => expect(screen.getByTestId("signed-in")).toHaveTextContent("false"))
    expect(localStorage.getItem(TOKEN_KEY)).toBeNull()
  })

  it("does not touch a token that's far from expiry", async () => {
    const token = makeToken(FAR_FUTURE_SECONDS)
    localStorage.setItem(TOKEN_KEY, token)
    const fetchSpy = vi.fn()
    vi.stubGlobal("fetch", fetchSpy)

    render(
      <LocalAuthProvider>
        <Probe />
      </LocalAuthProvider>,
    )

    // Nothing to await on success here — assert the negative holds
    // after yielding a tick for any (incorrect) effect to have fired.
    await new Promise((resolve) => setTimeout(resolve, 0))
    expect(fetchSpy).not.toHaveBeenCalled()
    expect(localStorage.getItem(TOKEN_KEY)).toBe(token)
  })
})
