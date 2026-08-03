// Smoke test #4 — UpgradeModal renders the right plan and tier values.
//
// This component is the in-app paywall surface. The PLAN_COMPARISON table
// inside it has historically drifted from backend/app/core/plans.py
// PLAN_LIMITS — that's the bug we just fixed in commit 9ac7e8d. These
// tests pin the current numbers so the next drift fails CI loudly.
//
// UpgradeModal uses <Link> from react-router, so we wrap renders in
// <MemoryRouter>. It does NOT use Clerk hooks directly — it just receives
// `currentPlan` as a prop — so no Clerk mocking is needed.

import { describe, it, expect, vi } from 'vitest'
import { render, screen } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter } from 'react-router'

import UpgradeModal from '../../src/components/UpgradeModal.jsx'

const renderModal = (overrides = {}) =>
  render(
    <MemoryRouter>
      <UpgradeModal
        isOpen
        feature="cameras"
        currentPlan="free_org"
        onClose={() => {}}
        {...overrides}
      />
    </MemoryRouter>,
  )

describe('UpgradeModal', () => {
  it('renders nothing when isOpen is false', () => {
    const { container } = render(
      <MemoryRouter>
        <UpgradeModal isOpen={false} feature="cameras" currentPlan="free_org" onClose={() => {}} />
      </MemoryRouter>,
    )
    expect(container.firstChild).toBeNull()
  })

  it('renders the comparison table with current PLAN_LIMITS values', () => {
    renderModal()

    // Viewer-hours is the lead row — these numbers must match
    // backend/app/core/plans.py::PLAN_LIMITS exactly.
    expect(screen.getByText('Viewer-hours / month')).toBeInTheDocument()
    expect(screen.getByText('30')).toBeInTheDocument()        // Free
    expect(screen.getByText('300')).toBeInTheDocument()       // Pro
    expect(screen.getByText('1,500')).toBeInTheDocument()     // Pro Plus

    // Hardware caps (the abuse rails) — fixed in commit 9ac7e8d.
    expect(screen.getByText('Cameras')).toBeInTheDocument()
    expect(screen.getByText('5')).toBeInTheDocument()         // Free cameras
    expect(screen.getByText('25')).toBeInTheDocument()        // Pro cameras
    expect(screen.getByText('200')).toBeInTheDocument()       // Pro Plus cameras

    // Nodes row — Free 2, Pro 10, Pro Plus Unlimited.
    expect(screen.getByText('Nodes')).toBeInTheDocument()
    expect(screen.getByText('Unlimited')).toBeInTheDocument()
  })

  it('shows the current plan name in the body copy', () => {
    renderModal({ currentPlan: 'pro' })
    // "Currently on the Pro plan"
    expect(screen.getByText(/Currently on the/)).toHaveTextContent(/\bPro\b/)
  })

  it('renders Pro Plus when currentPlan is pro_plus', () => {
    renderModal({ currentPlan: 'pro_plus' })
    expect(screen.getByText(/Currently on the/)).toHaveTextContent('Pro Plus')
  })

  it('calls onClose when "Maybe Later" is clicked', async () => {
    const user = userEvent.setup()
    const onClose = vi.fn()
    renderModal({ onClose })

    await user.click(screen.getByRole('button', { name: /maybe later/i }))
    expect(onClose).toHaveBeenCalledTimes(1)
  })

  it('exposes a "View Plans" link to /pricing', () => {
    renderModal()
    const link = screen.getByRole('link', { name: /view plans/i })
    expect(link).toHaveAttribute('href', '/pricing')
  })
})
