// Routing smoke tests for App.jsx — pins the two behaviors that broke
// when the marketing pages moved to the standalone site (92d6f83):
//
//   - Old marketing paths the app used to host (/security, /docs,
//     /sentinel, /legal/*) forward to the same path on
//     sentinel-command.com instead of rendering a blank screen.
//     Hash included — security.txt and SECURITY.md point at
//     /security#vulnerability-disclosure.
//   - Any other unknown path renders the 404 page. React Router
//     renders NOTHING on an unmatched path, which is exactly how
//     app.sentinel-command.com/security shipped as a pitch-black
//     blank page.

import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import { MemoryRouter } from 'react-router-dom'

// ── Mocks (must come before importing App) ──────────────────────

vi.mock('@clerk/clerk-react', () => ({
  useAuth: () => ({ isSignedIn: false }),
  useClerk: () => ({ signOut: vi.fn() }),
  useOrganization: () => ({ organization: null, membership: null, isLoaded: true }),
  CreateOrganization: () => null,
}))

vi.mock('../src/services/api.js', () => ({
  setUnauthorizedHandler: vi.fn(),
}))

// Every authenticated route sits behind Layout; none are exercised
// here, so stub it out rather than mock its dependency graph.
vi.mock('../src/components/Layout.jsx', () => ({ default: () => null }))

import App from '../src/App.jsx'

// ── Helpers ─────────────────────────────────────────────────────

function renderAt(path) {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <App />
    </MemoryRouter>
  )
}

describe('App routing', () => {
  let replaceSpy

  beforeEach(() => {
    // happy-dom's location.replace would actually try to navigate;
    // stub it so the redirect target is observable instead.
    replaceSpy = vi.spyOn(window.location, 'replace').mockImplementation(() => {})
    replaceSpy.mockClear()
  })

  it.each([
    ['/', 'https://sentinel-command.com/'],
    ['/security', 'https://sentinel-command.com/security'],
    ['/security#vulnerability-disclosure', 'https://sentinel-command.com/security#vulnerability-disclosure'],
    ['/docs', 'https://sentinel-command.com/documentation/'],
    ['/sentinel', 'https://sentinel-command.com/sentinel'],
    ['/legal/privacy', 'https://sentinel-command.com/legal/privacy'],
    ['/legal/terms', 'https://sentinel-command.com/legal/terms'],
  ])('forwards %s to the standalone site', async (path, expected) => {
    renderAt(path)
    await waitFor(() => expect(replaceSpy).toHaveBeenCalledWith(expected))
  })

  it('renders the 404 page for unknown paths instead of a blank screen', () => {
    renderAt('/no-such-page')
    expect(screen.getByRole('heading', { name: /page not found/i })).toBeInTheDocument()
    expect(screen.getByRole('link', { name: /go to dashboard/i })).toHaveAttribute('href', '/dashboard')
    expect(screen.getByRole('link', { name: 'sentinel-command.com' })).toHaveAttribute(
      'href',
      'https://sentinel-command.com'
    )
    expect(replaceSpy).not.toHaveBeenCalled()
  })
})
