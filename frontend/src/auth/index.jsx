/**
 * Auth facade — Clerk (hosted SaaS) or local (self-hosted, single admin)
 * depending on VITE_AUTH_PROVIDER, selected once at module load. Every
 * consumer imports useAuth/useOrganization/etc. from here instead of
 * "@clerk/clerk-react" directly, so the same ~25 files work unmodified
 * in both modes.
 *
 * Note this is a SELECTION, not a runtime wrapper: in a Clerk build,
 * `useAuth` below literally IS Clerk's useAuth — local.jsx's hooks are
 * never called at all in that build (and vice versa), so there's no
 * cross-mode fallback behavior to reason about.
 */

import * as Clerk from "@clerk/clerk-react"
import * as Local from "./local.jsx"

const IS_LOCAL = import.meta.env.VITE_AUTH_PROVIDER === "local"

// Match Clerk's UI to our dark brand. Without this, SignIn / SignUp /
// PricingTable / OrganizationSwitcher render with Clerk's default light
// theme on top of our near-black page background — looks broken. Using
// `variables` (not @clerk/themes) keeps us off the extra dependency.
const CLERK_APPEARANCE = {
  variables: {
    colorBackground: "#12141c",
    colorPrimary: "#22c55e",
    colorText: "#f4f4f5",
    colorTextSecondary: "#a1a1aa",
    colorTextOnPrimaryBackground: "#04170b",
    colorInputBackground: "rgba(0, 0, 0, 0.35)",
    colorInputText: "#f4f4f5",
    colorNeutral: "#ffffff",
    colorDanger: "#ef4444",
    colorSuccess: "#22c55e",
    colorWarning: "#f59e0b",
    borderRadius: "10px",
    fontFamily: "'Inter', -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif",
  },
}

function ClerkAuthProvider({ children }) {
  const publishableKey = import.meta.env.VITE_CLERK_PUBLISHABLE_KEY
  if (!publishableKey) {
    throw new Error("Missing VITE_CLERK_PUBLISHABLE_KEY. Please add it to your .env file.")
  }
  return (
    <Clerk.ClerkProvider publishableKey={publishableKey} appearance={CLERK_APPEARANCE}>
      {children}
    </Clerk.ClerkProvider>
  )
}

export function AuthProvider({ children }) {
  return IS_LOCAL ? (
    <Local.LocalAuthProvider>{children}</Local.LocalAuthProvider>
  ) : (
    <ClerkAuthProvider>{children}</ClerkAuthProvider>
  )
}

// `pick` resolves which implementation to use at CALL time (inside the
// wrapper below), not at module-eval time — this matters for tests:
// component test files mock "@clerk/clerk-react" narrowly (only the
// hook that component actually calls), and vitest's mock guard throws
// on any property read the mock factory didn't declare, even one this
// facade would otherwise touch unconditionally for components that
// never use it. Routing the selection through a plain helper (instead
// of a bare `IS_LOCAL ? Local.useX(...) : Clerk.useX(...)` ternary
// inside each wrapper) also keeps eslint-plugin-react-hooks from
// flagging this as a conditional hook call — IS_LOCAL is fixed for the
// lifetime of a given build, so the same implementation is called in
// the same order on every render, which is what the rule actually
// cares about; it just can't prove that statically through a direct
// ternary.
function pick(localImpl, clerkImpl) {
  return IS_LOCAL ? localImpl : clerkImpl
}

export function useAuth(...args) {
  return pick(Local.useAuth, Clerk.useAuth)(...args)
}
export function useOrganization(...args) {
  return pick(Local.useOrganization, Clerk.useOrganization)(...args)
}
export function useClerk(...args) {
  return pick(Local.useClerk, Clerk.useClerk)(...args)
}
// JSX tags below use a direct ternary (not the `pick` indirection the
// hooks above need) — a literal `<Clerk.SignedIn>` tag only reads that
// property when its branch actually renders, same laziness as the
// hooks, but assigning the chosen component to a variable first
// (`const Impl = pick(...); <Impl/>`) trips eslint-plugin-react-hooks'
// static-components rule ("component created during render"), since it
// can't see that `pick` always returns the same reference for a given
// build. A literal tag per branch has no such ambiguity.
export function SignedIn(props) {
  return IS_LOCAL ? <Local.SignedIn {...props} /> : <Clerk.SignedIn {...props} />
}
export function SignedOut(props) {
  return IS_LOCAL ? <Local.SignedOut {...props} /> : <Clerk.SignedOut {...props} />
}
export function UserButton(props) {
  return IS_LOCAL ? <Local.UserButton {...props} /> : <Clerk.UserButton {...props} />
}
export function OrganizationSwitcher(props) {
  return IS_LOCAL ? (
    <Local.OrganizationSwitcher {...props} />
  ) : (
    <Clerk.OrganizationSwitcher {...props} />
  )
}
export function CreateOrganization(props) {
  return IS_LOCAL ? (
    <Local.CreateOrganization {...props} />
  ) : (
    <Clerk.CreateOrganization {...props} />
  )
}

// Local-mode-only: used by pages/SignInPage.jsx's local login form.
// Only ever called from a component that only renders when IS_LOCAL is
// true, so LocalAuthContext is guaranteed to be present.
export const useLocalLogin = Local.useLocalLogin

export const IS_LOCAL_AUTH = IS_LOCAL
