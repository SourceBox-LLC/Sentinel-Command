// Phase 1 of camera-groups: Settings page CRUD section.
//
// Pinned behaviour:
//   - Empty state copy + "Create Your First Group" CTA visible to admins.
//   - Non-admin members see the list (so they can confirm groups exist
//     for the agent) but no create / delete buttons.
//   - Clicking the create CTA opens the inline form; submitting
//     optimistically appends the new row.
//   - Existing groups render with name, camera_count plural, and a
//     swatch of their color.
//   - Delete confirms via window.confirm, optimistically drops the row,
//     and posts a toast.

import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter } from 'react-router-dom'

// ── Mocks (must come before importing the page) ──────────────────

// Stable refs for `organization` and the auth hook — Clerk's real hook
// caches these, so the production useEffect([organization]) only fires
// once.  A naive mock that returns a fresh object literal each call
// would cause infinite re-renders the moment any state inside the page
// mutates (the optimistic create-group flow specifically loops with a
// non-stable org dep).  Membership is fine to construct fresh each
// call since no useEffect depends on it — the page just reads
// `membership?.role` in render.
const STABLE_ORG = { id: 'org_test', name: 'Test Org', membersCount: 1, createdAt: '2026-01-01' }
const STABLE_AUTH = { getToken: () => Promise.resolve('test-jwt') }
let mockMembershipRole = 'org:admin'
vi.mock('@clerk/clerk-react', () => ({
  useAuth: () => STABLE_AUTH,
  useOrganization: () => ({
    organization: STABLE_ORG,
    isLoaded: true,
    membership: { role: mockMembershipRole },
  }),
}))

// All API helpers used by SettingsPage's mount path.  Defaulted to
// resolved-empty so the page renders without spinners; per-test we
// override the camera-groups ones that we actually exercise.
const mockGetCameraGroups = vi.fn()
const mockCreateCameraGroup = vi.fn()
const mockDeleteCameraGroup = vi.fn()
vi.mock('../../src/services/api', () => ({
  getNodes: vi.fn(() => Promise.resolve([])),
  createNode: vi.fn(),
  rotateNodeKey: vi.fn(),
  deleteNode: vi.fn(),
  wipeStreamLogs: vi.fn(),
  fullReset: vi.fn(),
  getSettings: vi.fn(() => Promise.resolve({ notifications: {}, timezone: 'UTC' })),
  updateNotificationSettings: vi.fn(),
  updateOrgTimezone: vi.fn(),
  getCameras: vi.fn(() => Promise.resolve([])),
  getEmailPreferences: vi.fn(() => Promise.resolve({ email_globally_enabled: false, preferences: {} })),
  updateEmailPreferences: vi.fn(),
  downloadGdprExport: vi.fn(),
  getCameraGroups: (...args) => mockGetCameraGroups(...args),
  createCameraGroup: (...args) => mockCreateCameraGroup(...args),
  deleteCameraGroup: (...args) => mockDeleteCameraGroup(...args),
}))

vi.mock('../../src/hooks/useToasts.jsx', () => ({
  useToasts: () => ({ showToast: vi.fn() }),
}))

vi.mock('../../src/hooks/usePlanInfo.jsx', () => ({
  usePlanInfo: () => ({
    planInfo: {
      plan: 'pro',
      plan_name: 'Pro',
      usage: { cameras: 0, nodes: 0 },
      limits: { max_cameras: 10, max_nodes: 5 },
      features: ['admin'],
    },
    refreshPlanInfo: vi.fn(),
  }),
}))

// Heavy components stubbed — they're not what this test cares about.
vi.mock('../../src/components/AddNodeModal.jsx', () => ({
  default: () => null,
}))
vi.mock('../../src/components/KeyRotationModal.jsx', () => ({
  default: () => null,
}))
vi.mock('../../src/components/UpgradeModal.jsx', () => ({
  default: () => null,
}))
vi.mock('../../src/components/NodeStorageBar.jsx', () => ({
  default: () => null,
}))
vi.mock('../../src/components/CameraRecordingControls.jsx', () => ({
  default: () => null,
}))
vi.mock('../../src/components/HelpTooltip.jsx', () => ({
  default: () => null,
}))

import SettingsPage from '../../src/pages/SettingsPage.jsx'


const renderPage = () =>
  render(
    <MemoryRouter>
      <SettingsPage />
    </MemoryRouter>,
  )


// ── Tests ────────────────────────────────────────────────────────

describe('SettingsPage / Camera Groups', () => {
  beforeEach(() => {
    mockGetCameraGroups.mockReset()
    mockCreateCameraGroup.mockReset()
    mockDeleteCameraGroup.mockReset()
    mockMembershipRole = 'org:admin'
    // jsdom doesn't ship window.confirm — install a stub each test.
    window.confirm = vi.fn(() => true)
  })

  it('shows the empty state with a Create CTA when no groups exist (admin)', async () => {
    mockGetCameraGroups.mockResolvedValue([])
    renderPage()

    expect(await screen.findByText('No camera groups yet.')).toBeInTheDocument()
    expect(
      screen.getByRole('button', { name: /create your first group/i }),
    ).toBeInTheDocument()
  })

  it('hides the create CTA from non-admin members', async () => {
    mockMembershipRole = 'org:basic_member'
    mockGetCameraGroups.mockResolvedValue([])
    renderPage()

    expect(await screen.findByText('No camera groups yet.')).toBeInTheDocument()
    expect(
      screen.queryByRole('button', { name: /create your first group/i }),
    ).not.toBeInTheDocument()
  })

  it('renders existing groups with name + camera-count plural', async () => {
    mockGetCameraGroups.mockResolvedValue([
      { id: 1, name: 'Front yard', color: '#22c55e', icon: '🏡', camera_count: 3 },
      { id: 2, name: 'Workshop', color: '#a855f7', icon: '🔧', camera_count: 1 },
    ])
    renderPage()

    expect(await screen.findByText('Front yard')).toBeInTheDocument()
    expect(screen.getByText('Workshop')).toBeInTheDocument()
    expect(screen.getByText('3 cameras')).toBeInTheDocument()
    expect(screen.getByText('1 camera')).toBeInTheDocument()
  })

  it('clicking the create CTA opens the inline form', async () => {
    const user = userEvent.setup()
    mockGetCameraGroups.mockResolvedValue([])
    renderPage()

    await user.click(
      await screen.findByRole('button', { name: /create your first group/i }),
    )

    expect(screen.getByLabelText(/^name$/i)).toBeInTheDocument()
    expect(screen.getByLabelText(/group color/i)).toBeInTheDocument()
    expect(screen.getByLabelText(/^icon$/i)).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /create group/i })).toBeInTheDocument()
  })

  it('submitting the form optimistically appends a new row + closes form', async () => {
    const user = userEvent.setup()
    mockGetCameraGroups.mockResolvedValue([])
    mockCreateCameraGroup.mockResolvedValue({ success: true, id: 42, name: 'Garage' })
    renderPage()

    await user.click(
      await screen.findByRole('button', { name: /create your first group/i }),
    )

    const nameInput = screen.getByLabelText(/^name$/i)
    await user.type(nameInput, 'Garage')
    expect(nameInput).toHaveValue('Garage')

    const createBtn = screen.getByRole('button', { name: /create group/i })
    expect(createBtn).not.toBeDisabled()
    await user.click(createBtn)

    // First verify the API was called (catches "click never fired"
    // separately from "state didn't update after").
    await waitFor(() => expect(mockCreateCameraGroup).toHaveBeenCalled())

    // Optimistic — row appears without re-fetch
    await waitFor(() =>
      expect(screen.getByText('Garage')).toBeInTheDocument(),
    )
    // camera_count starts at 0 (assignment is Phase 2)
    expect(screen.getByText('0 cameras')).toBeInTheDocument()
    // Form closes after success
    expect(screen.queryByLabelText(/^name$/i)).not.toBeInTheDocument()
    expect(mockCreateCameraGroup).toHaveBeenCalledWith(
      expect.any(Function),
      'Garage',
      '#22c55e',
      '📁',
    )
  })

  it('delete button removes the row optimistically (admin)', async () => {
    const user = userEvent.setup()
    mockGetCameraGroups.mockResolvedValue([
      { id: 7, name: 'Side gate', color: '#ef4444', icon: '🚪', camera_count: 0 },
    ])
    mockDeleteCameraGroup.mockResolvedValue({ success: true, deleted: 'Side gate' })
    renderPage()

    await user.click(await screen.findByRole('button', { name: /delete side gate/i }))

    await waitFor(() =>
      expect(screen.queryByText('Side gate')).not.toBeInTheDocument(),
    )
    expect(window.confirm).toHaveBeenCalled()
    expect(mockDeleteCameraGroup).toHaveBeenCalledWith(expect.any(Function), 7)
  })

  it('delete cancelled at confirm leaves the row in place', async () => {
    const user = userEvent.setup()
    mockGetCameraGroups.mockResolvedValue([
      { id: 7, name: 'Side gate', color: '#ef4444', icon: '🚪', camera_count: 0 },
    ])
    window.confirm.mockReturnValue(false)
    renderPage()

    await user.click(await screen.findByRole('button', { name: /delete side gate/i }))

    // Row still there, no API call fired
    expect(screen.getByText('Side gate')).toBeInTheDocument()
    expect(mockDeleteCameraGroup).not.toHaveBeenCalled()
  })

  it('non-admins do not see delete buttons on existing rows', async () => {
    mockMembershipRole = 'org:basic_member'
    mockGetCameraGroups.mockResolvedValue([
      { id: 1, name: 'Front yard', color: '#22c55e', icon: '🏡', camera_count: 3 },
    ])
    renderPage()

    expect(await screen.findByText('Front yard')).toBeInTheDocument()
    // Member sees the row but not the action button
    expect(
      screen.queryByRole('button', { name: /delete front yard/i }),
    ).not.toBeInTheDocument()
  })
})
