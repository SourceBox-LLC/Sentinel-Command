// Interaction test for the local (self-hosted) sign-in form
// (src/pages/SignInPage.jsx's LocalSignInForm) — the actual username/
// password form a self-hosted admin sees at /sign-in.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest"
import { render, screen, waitFor } from "@testing-library/react"
import userEvent from "@testing-library/user-event"
import { MemoryRouter } from "react-router-dom"
import { LocalSignInForm } from "../../src/pages/SignInPage.jsx"
import { LocalAuthProvider, useAuth } from "../../src/auth/local.jsx"

const mockNavigate = vi.fn()
vi.mock("react-router-dom", async (importOriginal) => {
  const actual = await importOriginal()
  return { ...actual, useNavigate: () => mockNavigate }
})

function SignedInProbe() {
  const { isSignedIn } = useAuth()
  return <span data-testid="signed-in">{String(isSignedIn)}</span>
}

function renderForm() {
  return render(
    <MemoryRouter>
      <LocalAuthProvider>
        <LocalSignInForm />
        <SignedInProbe />
      </LocalAuthProvider>
    </MemoryRouter>,
  )
}

beforeEach(() => {
  localStorage.clear()
  mockNavigate.mockClear()
})

afterEach(() => {
  vi.unstubAllGlobals()
})

describe("LocalSignInForm", () => {
  it("shows an inline error and does not navigate on wrong credentials", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok: false, status: 401 }))
    const user = userEvent.setup()
    renderForm()

    await user.type(screen.getByLabelText(/username/i), "admin")
    await user.type(screen.getByLabelText(/password/i), "wrong-password")
    await user.click(screen.getByRole("button", { name: /sign in/i }))

    await waitFor(() =>
      expect(screen.getByText(/invalid username or password/i)).toBeInTheDocument(),
    )
    expect(mockNavigate).not.toHaveBeenCalled()
    expect(screen.getByTestId("signed-in")).toHaveTextContent("false")
  })

  it("logs in and navigates to /dashboard on correct credentials", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({ ok: true, json: async () => ({ token: "a.b.c" }) }),
    )
    const user = userEvent.setup()
    renderForm()

    await user.type(screen.getByLabelText(/username/i), "admin")
    await user.type(screen.getByLabelText(/password/i), "testpass123")
    await user.click(screen.getByRole("button", { name: /sign in/i }))

    await waitFor(() => expect(mockNavigate).toHaveBeenCalledWith("/dashboard", { replace: true }))
    expect(screen.getByTestId("signed-in")).toHaveTextContent("true")
    expect(localStorage.getItem("sentinel_local_token")).toBe("a.b.c")

    expect(fetch).toHaveBeenCalledWith(
      "/api/auth/local/login",
      expect.objectContaining({
        method: "POST",
        body: JSON.stringify({ username: "admin", password: "testpass123" }),
      }),
    )
  })

  it("shows an error when the backend is unreachable", async () => {
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("network down")))
    const user = userEvent.setup()
    renderForm()

    await user.type(screen.getByLabelText(/username/i), "admin")
    await user.type(screen.getByLabelText(/password/i), "testpass123")
    await user.click(screen.getByRole("button", { name: /sign in/i }))

    await waitFor(() =>
      expect(screen.getByText(/could not reach the server/i)).toBeInTheDocument(),
    )
    expect(mockNavigate).not.toHaveBeenCalled()
  })
})
