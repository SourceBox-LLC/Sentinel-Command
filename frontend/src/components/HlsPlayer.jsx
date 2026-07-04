import { useEffect, useRef, useState } from "react"
import { useSharedToken } from "../hooks/useSharedToken.jsx"

// hls.js is ~500 KB / ~155 KB gzipped. Static import would bloat the main
// bundle on every route — including landing/pricing/security/docs which
// never play video. Dynamic import below keeps it out until a HlsPlayer
// actually mounts. The first video tile takes one extra round-trip to
// fetch the chunk (cached thereafter); every other route gets a faster
// first paint.

// Set to true to connect directly to CameraNode on localhost:8080
// Set to false to use backend proxy with authentication
const LOCAL_TEST_MODE = import.meta.env.VITE_LOCAL_HLS === "true"

function HlsPlayer({ cameraId, cameraName }) {
    const videoRef = useRef(null)
    const hlsRef = useRef(null)
    const stallRef = useRef(null)
    // Holds the pending backoff setTimeout id between a fatal NETWORK_ERROR
    // and its retry, so unmount can cancel an in-flight retry.
    const retryRef = useRef(null)
    // Teardown fn for the visibility/viewport load gate (listeners +
    // IntersectionObserver are created inside the async setup, so the
    // effect cleanup reaches them through this ref).
    const gateCleanupRef = useRef(null)
    const { getCurrentToken, refreshNow, ready } = useSharedToken()
    const [loading, setLoading] = useState(true)
    const [error, setError] = useState(null)
    const [isLive, setIsLive] = useState(false)
    // Bumped by the manual Retry button to force the effect to tear down
    // and rebuild the hls.js instance from scratch (resetting the recovery
    // budget).  Without this, Retry only cleared the error overlay — it
    // never re-ran setup, so a player that gave up stayed dead.
    const [retryNonce, setRetryNonce] = useState(0)
    // `stalled` is post-connect: MANIFEST_PARSED has fired (loading is
    // false) but the video element hasn't advanced in N seconds.
    // Common cause is a backend segment-cache hole that hls.js is
    // retry-skipping past (e.g. CameraNode's HLS uploader losing a
    // batch of segments to the muxer rotating files out from under
    // it).  Without surfacing this state the player goes dead black
    // with no error and no spinner — operator can't tell whether the
    // camera died or the dashboard broke.
    const [stalled, setStalled] = useState(false)

    useEffect(() => {
        const video = videoRef.current
        if (!video || !cameraId) {
                return
        }

        // Wait for the shared Clerk token before building the hls.js
        // instance.  Mounting earlier means the first playlist/segment
        // XHR fires with a null Authorization header → 401 → fatal
        // NETWORK_ERROR → forced recovery.  Usually survives but leaves
        // the player stuck on "Connecting…" for a beat and spams 401s
        // in the backend log on every page load.
        if (!LOCAL_TEST_MODE && !ready) {
            return
        }

        const API_URL = import.meta.env.VITE_API_URL || ""
        const ownOrigin = API_URL || window.location.origin

        // Cancellation flag for the async chunk load below — if the
        // component unmounts (or cameraId changes) while the dynamic
        // import is still in flight, we must NOT proceed to instantiate
        // an Hls that nobody will clean up. The cleanup function below
        // flips this to true.
        let cancelled = false

        const setupHls = async () => {
            try {
                // Lazy-load hls.js. Vite splits it into its own chunk
                // (see comment at top of file). After the first call in
                // a session the browser cache serves the chunk
                // instantly; the cost is one round-trip on the first
                // video the user opens.
                const { default: Hls } = await import("hls.js")
                if (cancelled) return

                const playlistUrl = LOCAL_TEST_MODE
                    ? `http://localhost:8080/hls/${cameraId}/stream.m3u8`
                    : `${API_URL}/api/cameras/${cameraId}/stream.m3u8`

                if (Hls.isSupported()) {
                    const hls = new Hls({
                        // Don't auto-start loading — we call startLoad(-1)
                        // in MANIFEST_PARSED to ensure playback always begins
                        // from the live edge, not from a stale buffer position.
                        autoStartLoad: false,

                        xhrSetup: (xhr, url) => {
                            const token = LOCAL_TEST_MODE ? null : getCurrentToken()
                            // hls.js may pass relative URLs (e.g. "segment/segment_00042.ts")
                            // or absolute URLs (e.g. "https://...stream.m3u8").  Always attach
                            // the token for our own API endpoints; skip third-party origins.
                            if (token && (url.startsWith(ownOrigin) || url.startsWith("/"))) {
                                xhr.setRequestHeader("Authorization", `Bearer ${token}`)
                                // Prevent browser from serving cached playlist/segment
                                xhr.setRequestHeader("Cache-Control", "no-cache")
                            }
                        },

                        // ── Latency tuning ────────────────────────────────────
                        // Pipeline latency (FFmpeg → push → backend cache → browser) is ~2-3s.
                        // With 1s segments, 4 segments behind live = enough
                        // headroom to absorb upload jitter without stalling.
                        liveSyncDurationCount: 4,        // stay 4 segments (4s) behind live
                        liveMaxLatencyDurationCount: 8,  // if >8 segs (8s) behind, jump to live
                        liveDurationInfinity: true,      // never treat the stream as ended
                        liveBackBufferLength: 10,        // keep 10s of back buffer in live mode

                        // Forward buffer: generous to ride out upload jitter
                        maxBufferLength: 10,
                        maxMaxBufferLength: 20,
                        maxBufferSize: 20 * 1024 * 1024, // 20 MB

                        // Back buffer: liveBackBufferLength (above) governs
                        // this in live mode.  backBufferLength is the
                        // VOD-mode equivalent — kept in sync so switching
                        // a live stream to a DVR-style replay wouldn't
                        // silently change retention.
                        backBufferLength: 10,

                        // Playlist reload — poll aggressively for new segments.
                        manifestLoadingMaxRetry: 15,
                        manifestLoadingRetryDelay: 400,
                        manifestLoadingTimeOut: 10000,
                        levelLoadingMaxRetry: 15,
                        levelLoadingRetryDelay: 400,
                        levelLoadingTimeOut: 10000,
                        fragLoadingMaxRetry: 15,
                        fragLoadingRetryDelay: 400,
                        fragLoadingTimeOut: 10000,

                        enableWorker: true,
                    })

                    hlsRef.current = hls

                    // Bounded fatal-error recovery budget (consumed in the
                    // ERROR handler below).  A genuinely dead stream fires
                    // fatal errors back-to-back; capping the attempts keeps a
                    // black tile from looping playlist fetches forever.  The
                    // budget resets to 0 whenever playback actually advances
                    // (stall-check below), so a stream that blips occasionally
                    // over hours never exhausts it — only a *sustained*
                    // failure with no recovery in between gives up.
                    const MAX_RECOVERY_ATTEMPTS = 5
                    let networkRetries = 0
                    let mediaRetries = 0

                    // Tear down all timers + the hls instance.  Used by the
                    // give-up branches and the non-recoverable default case.
                    const stopPlayback = () => {
                        if (gateCleanupRef.current) {
                            // Drop the visibility listener + observer too —
                            // they survive give-up otherwise (inert thanks
                            // to the hlsRef guard, but still leaked until
                            // unmount/Retry).
                            gateCleanupRef.current()
                            gateCleanupRef.current = null
                        }
                        if (stallRef.current) {
                            clearInterval(stallRef.current)
                            stallRef.current = null
                        }
                        if (retryRef.current) {
                            clearTimeout(retryRef.current)
                            retryRef.current = null
                        }
                        hls.destroy()
                        hlsRef.current = null
                    }

                    hls.loadSource(playlistUrl)
                    hls.attachMedia(video)

                    hls.on(Hls.Events.MANIFEST_PARSED, () => {
                        setLoading(false)
                        setIsLive(true)
                        // applyLoadGate is the single start/stop path:
                        // when visible it starts from the live edge and
                        // plays; when the tab is hidden (or the tile
                        // off-screen) before the manifest even parsed,
                        // it gates immediately — no viewer-hours burned
                        // warming a stream nobody can see, and no
                        // redundant double startLoad(-1).
                        applyLoadGate()
                    })

                    // If the player falls too far behind live, snap back.
                    hls.on(Hls.Events.LEVEL_UPDATED, (_, data) => {
                        if (data.live && video && !video.paused) {
                            const liveEdge = hls.liveSyncPosition
                            if (liveEdge && video.currentTime < liveEdge - 6) {
                                console.warn("[HlsPlayer] Fell behind live edge, snapping forward")
                                video.currentTime = liveEdge
                            }
                        }
                    })

                    // Stall detection + recovery.  Two roles:
                    //   1. Auto-recovery: if the player hasn't advanced for
                    //      ~2 seconds, snap to live edge.  Equivalent to a
                    //      manual page refresh, just automated.
                    //   2. Surface the state to the UI: if the recovery
                    //      attempt at 2s doesn't take and the player still
                    //      isn't advancing at 3s+, set `stalled` so the
                    //      branded camera-pulse overlay re-appears in amber
                    //      ("Reconnecting to stream...").  Without this the
                    //      view is dead black — operator can't tell the
                    //      camera went dark vs the dashboard broke.
                    //
                    // Bug worth knowing about: previous version reset
                    // stallCount to 0 every 2 seconds whether the recovery
                    // jump succeeded or not, so we never tracked stalls
                    // longer than 2s — meaning we could never distinguish
                    // "transient hiccup" from "stream is genuinely dead."
                    // New version only resets on actual playback advance.
                    let lastTime = 0
                    let stallCount = 0
                    const stallCheck = setInterval(() => {
                        if (!video || video.paused) return

                        if (Math.abs(video.currentTime - lastTime) < 0.1) {
                            stallCount++

                            // First recovery attempt at 2s — jump to live.
                            if (stallCount === 2) {
                                const liveEdge = hls.liveSyncPosition
                                if (liveEdge && liveEdge > video.currentTime + 1) {
                                    console.warn("[HlsPlayer] Stall detected, jumping to live edge")
                                    video.currentTime = liveEdge
                                    hls.startLoad(-1)
                                }
                            }

                            // After 3 consecutive ticks of no progress, the
                            // recovery jump (if any) clearly didn't help.
                            // Tell the UI so the operator sees the amber
                            // "Reconnecting" overlay instead of dead black.
                            if (stallCount >= 3) {
                                setStalled(true)
                            }
                        } else {
                            // Playback advanced — drop every stall flag and
                            // refill the recovery budget: a stream that's
                            // healthy now has "earned back" its retries, so a
                            // later blip starts from a full budget instead of
                            // inheriting exhaustion from an earlier hiccup.
                            if (stallCount > 0 && import.meta.env.DEV) {
                                console.log("[HlsPlayer] Stream resumed after stall")
                            }
                            stallCount = 0
                            networkRetries = 0
                            mediaRetries = 0
                            setLoading(false)
                            setStalled(false)
                        }
                        lastTime = video.currentTime
                    }, 1000)
                    stallRef.current = stallCheck

                    // ── Billing-aware load gating ─────────────────────
                    // Every segment fetch meters the org's PAID monthly
                    // viewer-hours.  Without gating, a dashboard left
                    // open in a hidden tab overnight kept all N cameras
                    // streaming to nobody (a Free org's whole 30 h/mo
                    // cap gone in one night), and a 25-camera grid kept
                    // fetching for tiles scrolled far below the fold.
                    // stopLoad() halts playlist+segment fetches but
                    // keeps the instance alive; startLoad(-1) resumes at
                    // the live edge instantly on return.
                    let pageHidden = document.hidden
                    let offScreen = false
                    const applyLoadGate = () => {
                        if (hlsRef.current !== hls) return  // destroyed/replaced
                        if (pageHidden || offScreen) {
                            hls.stopLoad()
                            video.pause()
                        } else {
                            hls.startLoad(-1)
                            video.play().catch(() => { })
                        }
                    }
                    const onVisibility = () => {
                        pageHidden = document.hidden
                        applyLoadGate()
                    }
                    document.addEventListener("visibilitychange", onVisibility)
                    let intersection = null
                    if (typeof IntersectionObserver !== "undefined") {
                        intersection = new IntersectionObserver((entries) => {
                            // ratio 0 = fully out of viewport.  Any sliver
                            // visible keeps the stream warm so scrolling
                            // back is instant.
                            offScreen = entries[0]
                                ? entries[0].intersectionRatio === 0
                                : false
                            applyLoadGate()
                        }, { threshold: 0 })
                        intersection.observe(video)
                    }
                    gateCleanupRef.current = () => {
                        document.removeEventListener("visibilitychange", onVisibility)
                        intersection?.disconnect()
                    }

                    hls.on(Hls.Events.ERROR, (event, data) => {
                        if (!data.fatal) return
                        switch (data.type) {
                            case Hls.ErrorTypes.NETWORK_ERROR: {
                                // A fatal NETWORK_ERROR on a live stream is
                                // often a transient 401 (expired token) or a
                                // momentary segment-cache hole — worth a retry.
                                // But on a *dead* stream it repeats forever, and
                                // the old code retried unconditionally: every
                                // failure triggered a token refresh + startLoad,
                                // re-fetching the playlist in a tight loop that
                                // burned the org's monthly viewer-hour cap,
                                // hammered Clerk, and flooded the backend log
                                // behind a black tile.  Cap the attempts and
                                // back off exponentially instead.
                                if (networkRetries >= MAX_RECOVERY_ATTEMPTS) {
                                    console.warn(
                                        `[HlsPlayer] Giving up after ${networkRetries} network retries:`,
                                        data.details,
                                    )
                                    setError("Stream unavailable — the camera may be offline.")
                                    stopPlayback()
                                    break
                                }
                                networkRetries++
                                // 1s, 2s, 4s, 8s, 8s… (capped) so a dead stream
                                // backs off rather than spinning at full tilt.
                                const delay = Math.min(1000 * 2 ** (networkRetries - 1), 8000)
                                console.warn(
                                    `[HlsPlayer] Network error (attempt ${networkRetries}/${MAX_RECOVERY_ATTEMPTS}), `
                                    + `retrying in ${delay}ms:`, data.details,
                                )
                                retryRef.current = setTimeout(() => {
                                    retryRef.current = null
                                    // Refresh the (possibly expired) token, then
                                    // resume — but only if we're still mounted.
                                    refreshNow().then(() => {
                                        if (hlsRef.current === hls) hls.startLoad()
                                    })
                                }, delay)
                                break
                            }
                            case Hls.ErrorTypes.MEDIA_ERROR: {
                                if (mediaRetries >= MAX_RECOVERY_ATTEMPTS) {
                                    console.warn(
                                        `[HlsPlayer] Giving up after ${mediaRetries} media retries:`,
                                        data.details,
                                    )
                                    setError("Stream playback error — please retry.")
                                    stopPlayback()
                                    break
                                }
                                mediaRetries++
                                console.warn(
                                    `[HlsPlayer] Media error (attempt ${mediaRetries}/${MAX_RECOVERY_ATTEMPTS}), recovering:`,
                                    data.details,
                                )
                                hls.recoverMediaError()
                                break
                            }
                            default:
                                setError(`Fatal error: ${data.type}`)
                                stopPlayback()
                                break
                        }
                    })
                } else {
                    setError("Your browser does not support HLS streaming. Please use a modern browser (Chrome, Firefox, Edge, or Safari 13+).")
                    setLoading(false)
                }
            } catch (err) {
                setError(err.message)
                setLoading(false)
            }
        }

        setupHls()

        return () => {
            // Block the late branch in setupHls (post-await) from running
            // its setup if the dynamic import hasn't resolved yet.
            cancelled = true
            if (gateCleanupRef.current) {
                gateCleanupRef.current()
                gateCleanupRef.current = null
            }
            if (stallRef.current) {
                clearInterval(stallRef.current)
                stallRef.current = null
            }
            if (retryRef.current) {
                clearTimeout(retryRef.current)
                retryRef.current = null
            }
            if (hlsRef.current) {
                hlsRef.current.destroy()
                hlsRef.current = null
            }
        }
    }, [cameraId, getCurrentToken, refreshNow, ready, retryNonce])

    if (error) {
        return (
            <div className="hls-player-error">
                <div className="error-icon">⚠️</div>
                <div className="error-text">{error}</div>
                <button
                    className="btn btn-small"
                    onClick={() => {
                        setError(null)
                        setLoading(true)
                        setStalled(false)
                        // Force the effect to re-run: tears down the old
                        // (destroyed) hls instance and builds a fresh one with
                        // a full recovery budget.
                        setRetryNonce((n) => n + 1)
                    }}
                >
                    Retry
                </button>
            </div>
        )
    }

    return (
        <div className="hls-player-container">
            <div className="hls-player-header">
                <h3>{cameraName || `Camera ${cameraId}`}</h3>
                {isLive && <span className="live-badge">LIVE</span>}
            </div>
            
            <div className="hls-player-video-wrapper">
                {(loading || stalled) && (
                    <div className={`hls-player-loading${stalled ? " stalled" : ""}`}>
                        {/*
                            Custom loading state — branded camera-pulse SVG instead
                            of the generic .loading-spinner (which is shared with
                            auth pages and empty states).  Keeps the connecting UI
                            on-brand and matches the SourceBox accent.

                            Two trigger paths:
                              - `loading` (initial connect, green pulse)
                              - `stalled` (post-connect interruption, amber pulse)
                            The `.stalled` modifier swaps the color so the
                            operator can tell at a glance which case they're in.
                        */}
                        <svg
                            className="camera-pulse"
                            viewBox="0 0 24 24"
                            fill="none"
                            stroke="currentColor"
                            strokeWidth="1.75"
                            strokeLinecap="round"
                            strokeLinejoin="round"
                            aria-hidden="true"
                        >
                            <path d="M23 7l-7 5 7 5V7z" />
                            <rect x="1" y="5" width="15" height="14" rx="2" ry="2" />
                        </svg>
                        <p>{stalled ? "Reconnecting to stream..." : "Connecting to stream..."}</p>
                    </div>
                )}

                {/*
                    No `controls` attribute on purpose.  The native HTML5 control
                    bar (play/pause, scrubber, time, mute, fullscreen) renders on
                    top of the video and is visible during the connect window AND
                    during normal playback — pure visual noise for our use case:
                      - every player is `muted` (line below) so volume is moot
                      - live streams aren't seekable
                      - autoplay starts on MANIFEST_PARSED so play/pause is moot
                      - the dedicated Fullscreen button below replaces the native
                        fullscreen icon
                    Removing `controls` cleans up both the loading state and the
                    steady-state playback view.  Right-click "Save video as" goes
                    away too, which is a nice security adjacent.
                */}
                <video
                    ref={videoRef}
                    className="hls-player-video"
                    playsInline
                    muted
                />
            </div>
            
            <div className="hls-player-controls">
                <button 
                    className="btn btn-small"
                    onClick={() => {
                        const video = videoRef.current
                        if (video) {
                            video.requestFullscreen?.() ||
                            video.webkitRequestFullscreen?.()
                        }
                    }}
                >
                    Fullscreen
                </button>
            </div>
        </div>
    )
}

export default HlsPlayer