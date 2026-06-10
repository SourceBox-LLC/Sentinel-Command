import { createContext, useContext, useState, useEffect, useCallback, useMemo, useRef } from "react"
import { useAuth, useOrganization } from "@clerk/clerk-react"
import { getPlanInfo } from "../services/api"

const PlanInfoContext = createContext(null)

const REFRESH_INTERVAL = 60000 // 60 seconds

const EMPTY_PLAN_INFO = { planInfo: null, loading: false, refreshPlanInfo: () => {} }

// Inner provider that actually calls useOrganization(). Only mounted when the
// user has an active Clerk session — otherwise Clerk logs a warning on every
// public-page render.
function ActivePlanInfoProvider({ children }) {
  const { getToken } = useAuth()
  const { organization } = useOrganization()
  const [planInfo, setPlanInfo] = useState(null)
  const [loading, setLoading] = useState(false)
  const lastOrgRef = useRef(null)
  // True once the current org's first fetch has landed — background
  // refreshes must not flip `loading` (it re-rendered every consumer
  // 3x/minute with no data change).
  const hasLoadedRef = useRef(false)

  const loadPlanInfo = useCallback(async () => {
    if (!organization) return
    const orgAtCall = organization.id
    try {
      // Only flag `loading` for the INITIAL fetch — flipping it on
      // every 60s background tick re-rendered every consumer (Layout,
      // sidebar, dashboard…) three times a minute with no data change.
      if (!hasLoadedRef.current) setLoading(true)
      const token = await getToken()
      const data = await getPlanInfo(() => Promise.resolve(token))
      // An in-flight request started under the PREVIOUS org (e.g. the
      // 60s tick racing an org switch) must not write that org's plan
      // and limits into the new org's context — it drives plan badges
      // and upgrade gates for up to a full refresh interval.
      if (lastOrgRef.current === orgAtCall) {
        hasLoadedRef.current = true
        // Content compare: the 60s refresh almost always returns an
        // identical payload; storing a fresh object identity anyway
        // forced a context update through the whole authed UI.
        setPlanInfo(prev =>
          prev && JSON.stringify(prev) === JSON.stringify(data) ? prev : data
        )
      }
    } catch (err) {
      console.error("[PlanInfo] Failed to load:", err)
    } finally {
      setLoading(false)
    }
  }, [organization, getToken])

  // Load on mount and when org changes
  useEffect(() => {
    if (!organization) {
      setPlanInfo(null)
      return
    }
    // Reset if org changed
    if (lastOrgRef.current !== organization.id) {
      lastOrgRef.current = organization.id
      hasLoadedRef.current = false
      setPlanInfo(null)
    }
    loadPlanInfo()
    const interval = setInterval(() => {
      // Plan data is invisible in a hidden tab; refresh on return.
      if (!document.hidden) loadPlanInfo()
    }, REFRESH_INTERVAL)
    const onVisible = () => {
      if (!document.hidden) loadPlanInfo()
    }
    document.addEventListener("visibilitychange", onVisible)
    return () => {
      clearInterval(interval)
      document.removeEventListener("visibilitychange", onVisible)
    }
  }, [organization, loadPlanInfo])

  // Memoized — rebuilding this object every provider render forced a
  // context update on all consumers even when nothing changed.
  const value = useMemo(
    () => ({ planInfo, loading, refreshPlanInfo: loadPlanInfo }),
    [planInfo, loading, loadPlanInfo],
  )

  return (
    <PlanInfoContext.Provider value={value}>
      {children}
    </PlanInfoContext.Provider>
  )
}

export function PlanInfoProvider({ children }) {
  const { isLoaded, isSignedIn } = useAuth()

  // Public pages: provide an empty context so consumers don't crash, but don't
  // call useOrganization() (which would warn about missing session).
  if (!isLoaded || !isSignedIn) {
    return (
      <PlanInfoContext.Provider value={EMPTY_PLAN_INFO}>
        {children}
      </PlanInfoContext.Provider>
    )
  }

  return <ActivePlanInfoProvider>{children}</ActivePlanInfoProvider>
}

export function usePlanInfo() {
  const context = useContext(PlanInfoContext)
  if (!context) {
    throw new Error("usePlanInfo must be used within PlanInfoProvider")
  }
  return context
}
