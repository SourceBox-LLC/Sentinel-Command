// Smoke tests for InstallCloudNodeCard — the in-app CloudNode install
// widget that replaced the "click out to GitHub README" flow on the
// empty-state dashboard.
//
// The single biggest moment of customer drop-off lives in this
// component: just signed up, empty grid, need to run a CLI command.
// A regression that broke the click → POST /api/nodes → display
// credentialed one-liner flow would silently lose every new sign-up.
//
// What's pinned:
//
//   - Initial state shows the "Get my install command →" CTA with no
//     credentials revealed (catches the orphan-node bug — auto-creating
//     on mount would spam the org with unused nodes).
//   - Click → calls createNode + transitions to the tabbed install view.
//   - The displayed Linux/macOS one-liner contains the returned
//     credentials (node_id + api_key).  This is the single most-
//     important assertion — without these in the command, the install
//     wouldn't work end-to-end.
//   - OS tabs render and the user can switch between them.
//   - API failure surfaces as an inline error (e.g. plan limit hit
//     would otherwise silently leave the button disabled).

import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'

// Mock Clerk's useAuth so the component doesn't need a ClerkProvider
// wrapper.  The widget's only Clerk usage is `getToken()`, which
// passes through to our createNode service helper — we mock that
// helper too.
vi.mock('@clerk/clerk-react', () => ({
  useAuth: () => ({
    getToken: () => Promise.resolve('test-jwt'),
  }),
}))

// Mock the API helper so tests don't actually fetch.  Override per
// test by reassigning the .mockResolvedValueOnce / .mockRejectedValueOnce
// stub.
const mockCreateNode = vi.fn()
vi.mock('../../src/services/api', () => ({
  createNode: (...args) => mockCreateNode(...args),
}))

import InstallCloudNodeCard from '../../src/components/InstallCloudNodeCard.jsx'


beforeEach(() => {
  mockCreateNode.mockReset()
})


describe('InstallCloudNodeCard', () => {
  it('renders the initial CTA without revealing credentials', () => {
    render(<InstallCloudNodeCard />)

    expect(
      screen.getByRole('button', { name: /Get my install command/i }),
    ).toBeInTheDocument()
    // No credential or command bytes leak before the user clicks.
    expect(screen.queryByText(/install\.sh/)).not.toBeInTheDocument()
    expect(screen.queryByText(/--node-id/)).not.toBeInTheDocument()
  })

  it('does NOT auto-create a node on mount (orphan-node guard)', () => {
    // The widget previously considered auto-creating but explicitly
    // doesn't — a hot reload during dev or a user reloading the
    // dashboard repeatedly while troubleshooting their network
    // would otherwise burn through the plan's max_nodes limit.
    // Pin "no API call without an explicit click".
    render(<InstallCloudNodeCard />)
    expect(mockCreateNode).not.toHaveBeenCalled()
  })

  it('calls createNode and reveals the credentialed one-liner on click', async () => {
    mockCreateNode.mockResolvedValueOnce({
      node_id: 'abc123',
      api_key: 'secret-key-xyz',
    })
    const user = userEvent.setup()
    render(<InstallCloudNodeCard />)

    await user.click(
      screen.getByRole('button', { name: /Get my install command/i }),
    )

    // createNode invoked exactly once with our auto-name.
    await waitFor(() => expect(mockCreateNode).toHaveBeenCalledTimes(1))
    expect(mockCreateNode).toHaveBeenCalledWith(
      expect.any(Function),  // getToken
      'First CloudNode',
    )

    // Wait for the post-creds state to render.
    await waitFor(() =>
      expect(screen.getByRole('tablist')).toBeInTheDocument(),
    )

    // The displayed command MUST contain the returned credentials —
    // without these, the install command is useless.  This is the
    // single highest-impact assertion in the file.
    const codeEl = document.querySelector('.install-command code')
    expect(codeEl).toBeInTheDocument()
    expect(codeEl.textContent).toContain('--node-id abc123')
    expect(codeEl.textContent).toContain('--key secret-key-xyz')
  })

  it('renders all three OS tabs after credentials are loaded', async () => {
    mockCreateNode.mockResolvedValueOnce({
      node_id: 'n1', api_key: 'k1',
    })
    const user = userEvent.setup()
    render(<InstallCloudNodeCard />)

    await user.click(
      screen.getByRole('button', { name: /Get my install command/i }),
    )
    await waitFor(() =>
      expect(screen.getByRole('tablist')).toBeInTheDocument(),
    )

    // All three tabs render — Linux, macOS, Windows.  Catches a
    // regression where someone simplified the tab loop and dropped one.
    expect(screen.getByRole('tab', { name: /Linux/i })).toBeInTheDocument()
    expect(screen.getByRole('tab', { name: /macOS/i })).toBeInTheDocument()
    expect(screen.getByRole('tab', { name: /Windows/i })).toBeInTheDocument()
  })

  it('switches to the Windows tab and shows the MSI download path', async () => {
    mockCreateNode.mockResolvedValueOnce({
      node_id: 'n1', api_key: 'k1',
    })
    const user = userEvent.setup()
    render(<InstallCloudNodeCard />)

    await user.click(
      screen.getByRole('button', { name: /Get my install command/i }),
    )
    await waitFor(() =>
      expect(screen.getByRole('tab', { name: /Windows/i })).toBeInTheDocument(),
    )

    await user.click(screen.getByRole('tab', { name: /Windows/i }))

    // Windows tab uses an MSI download (different from Linux/macOS
    // curl-pipe-bash).  The download anchor must point at our
    // backend's redirect endpoint.
    const downloadLink = screen.getByRole('link', {
      name: /Download CloudNode for Windows/i,
    })
    expect(downloadLink).toBeInTheDocument()
    expect(downloadLink.getAttribute('href')).toMatch(
      /\/downloads\/windows\/x86_64$/,
    )
  })

  it('surfaces API errors inline so the user knows what went wrong', async () => {
    // Simulate "Node limit reached on the Free plan" or similar.
    mockCreateNode.mockRejectedValueOnce(
      new Error('Node limit reached (3 on Free plan). Upgrade your plan.'),
    )
    const user = userEvent.setup()
    render(<InstallCloudNodeCard />)

    await user.click(
      screen.getByRole('button', { name: /Get my install command/i }),
    )

    await waitFor(() =>
      expect(screen.getByRole('alert')).toBeInTheDocument(),
    )
    expect(screen.getByRole('alert')).toHaveTextContent(/Node limit reached/)

    // Still in the initial state (CTA visible, tabs not rendered)
    // so the user can retry.
    expect(
      screen.getByRole('button', { name: /Get my install command/i }),
    ).toBeInTheDocument()
    expect(screen.queryByRole('tablist')).not.toBeInTheDocument()
  })
})
