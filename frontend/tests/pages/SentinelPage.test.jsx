// Smoke tests for SentinelPage — the /sentinel route.
//
// The page was deleted in July alongside the marketing pages and is
// restored here; without it nothing in the product could turn Sentinel
// on, pause it, run it, or show what it did. What's pinned:
//   - An eligible admin gets live controls, and pausing PATCHes the config.
//   - A member sees the same page with the controls disabled — the
//     server refuses their edits, so the page does not offer them.
//   - The two gates read differently: a hosted org below Pro is sent to
//     plans; a self-hosted install without a licence is told about the
//     licence key, and offered no upgrade it cannot buy.

import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter } from 'react-router-dom'

let role = 'org:admin'
vi.mock('@clerk/clerk-react', () => ({
  useAuth: () => ({ getToken: () => Promise.resolve('test-jwt'), isSignedIn: true }),
  useOrganization: () => ({
    organization: { id: 'org_test', name: 'Test Org' },
    isLoaded: true,
    membership: { role },
  }),
}))

const mockGetConfig = vi.fn()
const mockUpdateConfig = vi.fn()
vi.mock('../../src/services/api', () => ({
  getSentinelConfig: (...a) => mockGetConfig(...a),
  updateSentinelConfig: (...a) => mockUpdateConfig(...a),
  getSentinelRuns: () => Promise.resolve({
    runs: [],
    stats: { runs_today: 0, runs_total: 0, incidents_filed: 0, runs_this_month: 0, monthly_cap: 100 },
  }),
  getSentinelRun: () => Promise.resolve(null),
  getCameras: () => Promise.resolve([{ camera_id: 'cam-1', name: 'Driveway' }]),
  dispatchSentinelManualRun: () => Promise.resolve({ id: 'r1' }),
}))

vi.mock('../../src/hooks/useToasts.jsx', () => ({
  useToasts: () => ({ showToast: vi.fn() }),
}))

import SentinelPage from '../../src/pages/SentinelPage.jsx'

const CONFIG = {
  enabled: true,
  motion_enabled: true,
  incident_opened_enabled: true,
  motion_cooldown_min: 5,
  schedule_mode: 'always',
  schedule_start: '22:00',
  schedule_end: '06:00',
  active_days: ['mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun'],
  camera_scope: {},
}

function configResponse(overrides = {}) {
  return {
    config: CONFIG,
    plan_gated: false,
    plan_gated_reason: null,
    plan_required: 'pro',
    plan_current: 'Pro',
    monthly_cap: 100,
    ...overrides,
  }
}

function renderPage() {
  return render(
    <MemoryRouter>
      <SentinelPage />
    </MemoryRouter>,
  )
}

describe('SentinelPage', () => {
  beforeEach(() => {
    role = 'org:admin'
    mockGetConfig.mockReset()
    mockUpdateConfig.mockReset()
    mockUpdateConfig.mockResolvedValue({ config: { ...CONFIG, enabled: false } })
  })

  it('gives an eligible admin live controls, and pausing saves', async () => {
    mockGetConfig.mockResolvedValue(configResponse())
    renderPage()
    expect((await screen.findAllByText('ARMED')).length).toBeGreaterThan(0)
    const runNow = screen.getByRole('button', { name: /Run now/ })
    expect(runNow).toBeEnabled()

    await userEvent.click(screen.getByRole('button', { name: 'Pause Sentinel' }))
    await waitFor(() => expect(mockUpdateConfig).toHaveBeenCalled())
    expect(mockUpdateConfig.mock.calls[0][1]).toEqual({ enabled: false })
  })

  it('shows a member the page with the controls disabled', async () => {
    role = 'org:member'
    mockGetConfig.mockResolvedValue(configResponse())
    renderPage()
    expect((await screen.findAllByText('ARMED')).length).toBeGreaterThan(0)
    const runNow = screen.getByRole('button', { name: /Run now/ })
    expect(runNow).toBeDisabled()
    expect(runNow).toHaveAttribute('title', 'Only an org admin can run or pause Sentinel')
  })

  it('sends a hosted org below Pro to the plans', async () => {
    mockGetConfig.mockResolvedValue(
      configResponse({ plan_gated: true, plan_gated_reason: 'plan_required', plan_current: 'Free', monthly_cap: 0 }),
    )
    renderPage()
    expect(await screen.findByText(/Sentinel is a paid feature/)).toBeInTheDocument()
    expect(screen.getByRole('link', { name: /See plans/ })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /Run now/ })).toBeDisabled()
  })

  it('tells an unlicensed self-hosted install about the licence key, not an upgrade', async () => {
    mockGetConfig.mockResolvedValue(
      configResponse({ plan_gated: true, plan_gated_reason: 'license_required', plan_current: 'Self-Hosted', monthly_cap: 0 }),
    )
    renderPage()
    expect(await screen.findByText(/needs a licence key/)).toBeInTheDocument()
    expect(screen.getByText('SENTINEL_LICENSE_KEY')).toBeInTheDocument()
    expect(screen.queryByText(/Sentinel is a paid feature/)).not.toBeInTheDocument()
    expect(screen.getByRole('button', { name: /Run now/ })).toHaveAttribute(
      'title',
      'Sentinel needs a licence key on a self-hosted install',
    )
    // Locked, not armed: the config says enabled, but nothing will run.
    expect(screen.queryByText('ARMED')).not.toBeInTheDocument()
    expect(screen.getAllByText('LOCKED').length).toBeGreaterThan(0)
  })
})
