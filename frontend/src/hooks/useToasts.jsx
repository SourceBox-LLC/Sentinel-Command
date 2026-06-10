import { createContext, useContext, useState, useCallback, useMemo, useRef } from 'react'

// Two contexts on purpose.  The API (showToast/removeToast) is stable
// for the provider's lifetime; the LIST changes twice per toast (add +
// timed remove).  When both lived in one context value, every toast
// re-rendered every consumer — including every CameraCard on the
// dashboard grid (each card calls useToasts for showToast), so a single
// motion toast forced two full-grid reconciliations over live <video>
// elements.  Now only ToastContainer subscribes to the list.
const ToastApiContext = createContext(null)
const ToastListContext = createContext(null)

export function ToastProvider({ children }) {
    const [toasts, setToasts] = useState([])
    // Monotonic id — Date.now() collides when an SSE chunk delivers
    // several events in the same millisecond (duplicate React keys, and
    // the first expiry timer's filter removed ALL colliding toasts at
    // once).
    const nextIdRef = useRef(1)

    const showToast = useCallback((message, type = 'success', durationMs = 3000) => {
        const id = nextIdRef.current++
        const toast = { id, message, type }

        setToasts(prev => [...prev, toast])

        setTimeout(() => {
            setToasts(prev => prev.filter(t => t.id !== id))
        }, durationMs)

        return id
    }, [])

    const removeToast = useCallback((id) => {
        setToasts(prev => prev.filter(t => t.id !== id))
    }, [])

    // Referentially stable — both callbacks are useCallback([]), so this
    // memo never invalidates and API consumers never re-render from here.
    const api = useMemo(() => ({ showToast, removeToast }), [showToast, removeToast])

    return (
        <ToastApiContext.Provider value={api}>
            <ToastListContext.Provider value={toasts}>
                {children}
            </ToastListContext.Provider>
        </ToastApiContext.Provider>
    )
}

export function useToasts() {
    const context = useContext(ToastApiContext)
    if (!context) {
        throw new Error('useToasts must be used within ToastProvider')
    }
    return context
}

// List subscription — ToastContainer only.  Everything else should use
// useToasts() so it doesn't re-render on every toast add/expire.
export function useToastList() {
    const list = useContext(ToastListContext)
    if (list === null) {
        throw new Error('useToastList must be used within ToastProvider')
    }
    return list
}

export default ToastApiContext
