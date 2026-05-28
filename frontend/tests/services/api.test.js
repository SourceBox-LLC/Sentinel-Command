// Smoke test #2 — fetchWithAuth in services/api.js.
//
// `fetchWithAuth` is the central HTTP helper every API call routes through.
// If its error-handling shape drifts, every consumer breaks silently. These
// tests pin the four contract paths:
//
//   1. happy path: 2xx → returns parsed JSON
//   2. 204: returns null (no body parse)
//   3. non-2xx: throws Error with detail message from response body
//   4. network failure: throws original fetch error
//
// We mock the global `fetch` directly — no need to spin up a server, no
// need to import from a wrapper. ``getToken`` is just a function we hand in.

import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import { fetchWithAuth, setUnauthorizedHandler } from '../../src/services/api.js'

describe('fetchWithAuth', () => {
  let originalFetch

  beforeEach(() => {
    originalFetch = globalThis.fetch
  })

  afterEach(() => {
    globalThis.fetch = originalFetch
    vi.restoreAllMocks()
  })

  it('returns parsed JSON on 2xx', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      json: async () => ({ cameras: [{ id: 'cam_1' }] }),
    })

    const out = await fetchWithAuth('/api/cameras', async () => 'tok_x')

    expect(out).toEqual({ cameras: [{ id: 'cam_1' }] })
    // Authorization header carried the token from getToken().
    const callArgs = globalThis.fetch.mock.calls[0][1]
    expect(callArgs.headers.Authorization).toBe('Bearer tok_x')
  })

  it('returns null on 204 No Content', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: true,
      status: 204,
      json: async () => {
        throw new Error('should not be called for 204')
      },
    })

    const out = await fetchWithAuth('/api/widget', async () => 'tok_x')

    expect(out).toBeNull()
  })

  it('throws Error with string detail on non-2xx (legacy HTTPException shape)', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: false,
      status: 402,
      json: async () => ({ detail: 'plan_limit_hit: camera over Free-tier cap' }),
    })

    await expect(
      fetchWithAuth('/api/cameras/cam_1/push-segment', async () => 'tok_x'),
    ).rejects.toThrow(/plan_limit_hit/)
  })

  it('parses ApiError envelope (Shape 1) — sets message + code + detail + status', async () => {
    // Matches what backend/app/core/errors.py::ApiError produces, also the
    // existing 402 plan-limit-hit body, also the new 422 validation handler.
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: false,
      status: 402,
      json: async () => ({
        detail: {
          error: 'plan_limit_hit',
          message: 'Camera over the Free plan limit',
          plan: 'Free',
          max_cameras: 5,
          camera_name: 'Front Door',
        },
      }),
    })

    let caught
    try {
      await fetchWithAuth('/api/cameras/cam_1/push-segment', async () => 'tok_x')
    } catch (e) {
      caught = e
    }

    expect(caught).toBeInstanceOf(Error)
    expect(caught.message).toBe('Camera over the Free plan limit')
    expect(caught.code).toBe('plan_limit_hit')   // for branching
    expect(caught.status).toBe(402)
    expect(caught.detail.max_cameras).toBe(5)    // structured fields preserved
    expect(caught.detail.plan).toBe('Free')
  })

  it('parses Pydantic 422 array detail (Shape 4) — defensive fallback', async () => {
    // Backend's main.py rewrites 422s through the validation handler now,
    // but in-flight deploys / dev servers can still surface the raw shape.
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: false,
      status: 422,
      json: async () => ({
        detail: [
          { loc: ['body', 'name'], msg: 'field required', type: 'value_error.missing' },
        ],
      }),
    })

    let caught
    try {
      await fetchWithAuth('/api/nodes', async () => 'tok_x', {
        method: 'POST',
        body: JSON.stringify({}),
      })
    } catch (e) {
      caught = e
    }

    expect(caught.message).toMatch(/field required/)
    expect(caught.message).toMatch(/name/)        // location surfaced
    expect(caught.code).toBe('validation_failed')
    expect(caught.status).toBe(422)
  })

  it('parses top-level envelope without .detail (Shape 3 — rate-limit handler)', async () => {
    // rate_limit_exceeded_handler in main.py builds a JSONResponse with a
    // top-level shape, not wrapped under .detail like HTTPException would be.
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: false,
      status: 429,
      json: async () => ({
        error: 'rate_limit_exceeded',
        message: 'Too many requests. Back off and retry after the Retry-After window.',
        limit: '60/minute',
        retry_after_seconds: 60,
      }),
    })

    let caught
    try {
      await fetchWithAuth('/api/nodes/heartbeat', async () => 'tok_x')
    } catch (e) {
      caught = e
    }

    expect(caught.message).toMatch(/Too many requests/)
    expect(caught.code).toBe('rate_limit_exceeded')
    expect(caught.status).toBe(429)
  })

  it('falls back gracefully when error body is empty/non-JSON', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: false,
      status: 500,
      json: async () => {
        throw new SyntaxError('unexpected end of JSON input')
      },
    })

    let caught
    try {
      await fetchWithAuth('/api/anything', async () => 'tok_x')
    } catch (e) {
      caught = e
    }

    expect(caught.message).toBe('Request failed with status 500')
    expect(caught.code).toBeNull()
    expect(caught.status).toBe(500)
  })

  it('throws fetch error when network fails', async () => {
    const networkErr = new TypeError('Failed to fetch')
    globalThis.fetch = vi.fn().mockRejectedValue(networkErr)

    await expect(
      fetchWithAuth('/api/cameras', async () => 'tok_x'),
    ).rejects.toBe(networkErr) // exact same Error instance bubbles up
  })

  it('omits Authorization header when getToken returns null', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      json: async () => ({}),
    })

    await fetchWithAuth('/api/health', async () => null)

    const headers = globalThis.fetch.mock.calls[0][1].headers
    expect(headers.Authorization).toBeUndefined()
  })
})

// ── Central 401 (dead-session) handling ──────────────────────────────
//
// A 401 means the credential itself was rejected — the only sane recovery
// is to end the session and bounce to sign-in, handled once centrally
// instead of by every call site.  A 403 (authenticated-but-forbidden) must
// NOT trigger that — it's a legitimate permission denial the component
// surfaces.  These pin that boundary plus the fire-once latch.
describe('fetchWithAuth — central 401 handling', () => {
  let originalFetch

  beforeEach(async () => {
    originalFetch = globalThis.fetch
    // Reset the module-level "already handled" latch by forcing one
    // successful request through (a success clears it).  Keeps these
    // tests independent of execution order.
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: true, status: 200, json: async () => ({}),
    })
    setUnauthorizedHandler(null)
    await fetchWithAuth('/__reset_latch__', async () => 't')
  })

  afterEach(() => {
    setUnauthorizedHandler(null)
    globalThis.fetch = originalFetch
    vi.restoreAllMocks()
  })

  it('invokes the registered handler on 401 and still throws the API error', async () => {
    const handler = vi.fn()
    setUnauthorizedHandler(handler)
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: false, status: 401, json: async () => ({ detail: 'invalid token' }),
    })

    await expect(
      fetchWithAuth('/api/cameras', async () => 'stale'),
    ).rejects.toThrow(/invalid token/)
    expect(handler).toHaveBeenCalledTimes(1)
  })

  it('does NOT invoke the handler on 403 (permission denial, not a dead session)', async () => {
    const handler = vi.fn()
    setUnauthorizedHandler(handler)
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: false, status: 403, json: async () => ({ detail: 'forbidden' }),
    })

    await expect(
      fetchWithAuth('/api/admin/thing', async () => 'tok'),
    ).rejects.toThrow(/forbidden/)
    expect(handler).not.toHaveBeenCalled()
  })

  it('fires once across a burst of 401s, then re-arms after a success', async () => {
    const handler = vi.fn()
    setUnauthorizedHandler(handler)

    // Three back-to-back 401s — the latch means the handler fires once.
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: false, status: 401, json: async () => ({ detail: 'nope' }),
    })
    for (let i = 0; i < 3; i++) {
      await fetchWithAuth('/api/x', async () => 't').catch(() => {})
    }
    expect(handler).toHaveBeenCalledTimes(1)

    // A success re-arms the latch…
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: true, status: 200, json: async () => ({}),
    })
    await fetchWithAuth('/api/ok', async () => 't')

    // …so a later genuine expiry fires the handler again.
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: false, status: 401, json: async () => ({ detail: 'again' }),
    })
    await fetchWithAuth('/api/x', async () => 't').catch(() => {})
    expect(handler).toHaveBeenCalledTimes(2)
  })

  it('a 401 with no handler registered is a safe no-op (still throws)', async () => {
    setUnauthorizedHandler(null)
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: false, status: 401, json: async () => ({ detail: 'unauthorized' }),
    })

    await expect(
      fetchWithAuth('/api/cameras', async () => 'stale'),
    ).rejects.toThrow(/unauthorized/)
  })
})
