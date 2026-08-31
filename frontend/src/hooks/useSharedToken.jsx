import { createContext, useContext, useEffect, useRef, useCallback, useState } from "react"
import { useAuth } from "../auth/index.jsx"

/**
 * Shared auth token for HLS players.
 * Instead of N players each running their own 30s refresh interval,
 * one interval refreshes the token and all players read from it.
 */
const SharedTokenContext = createContext(null)

const REFRESH_INTERVAL = 30000 // 30 seconds

export function SharedTokenProvider({ children }) {
  const { getToken } = useAuth()
  const tokenRef = useRef(null)
  const [ready, setReady] = useState(false)

  const refresh = useCallback(async () => {
    try {
      tokenRef.current = await getToken()
      if (!ready) setReady(true)
    } catch (e) {
      console.warn("[SharedToken] Refresh failed:", e)
    }
  }, [getToken])

  useEffect(() => {
    refresh()
    const interval = setInterval(refresh, REFRESH_INTERVAL)
    return () => clearInterval(interval)
  }, [refresh])

  const getCurrentToken = useCallback(() => tokenRef.current, [])

  return (
    <SharedTokenContext.Provider value={{ getCurrentToken, refreshNow: refresh, ready }}>
      {children}
    </SharedTokenContext.Provider>
  )
}

export function useSharedToken() {
  const context = useContext(SharedTokenContext)
  if (!context) {
    throw new Error("useSharedToken must be used within SharedTokenProvider")
  }
  return context
}
