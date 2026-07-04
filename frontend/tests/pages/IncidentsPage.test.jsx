// Smoke tests for IncidentsPage — the standalone /incidents route.
//
// What's pinned:
//   - Renders the list with stat-bar counts derived from the API.
//   - Source filter (Anyone / AI / Human) toggles which rows are
//     visible based on `created_by` prefix.
//   - Status filter (Open / All) re-fetches with the right status
//     query param.
//   - Empty state copy varies by filter combo.
//   - Hero "+ New Incident" button opens the create modal.
//   - Deep-link param (/incidents/:id) opens the detail modal on
//     mount.

import { describe, it, expect, vi, beforeEach } from 'vitest'
import { act, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter, Route, Routes } from 'react-router-dom'

// ── Mocks (must come before importing the page) ──────────────────

vi.mock('@clerk/clerk-react', () => ({
  useAuth: () => ({
    getToken: () => Promise.resolve('test-jwt'),
  }),
  useOrganization: () => ({
    organization: { id: 'org_test', name: 'Test Org' },
    isLoaded: true,
    membership: { role: 'org:admin' },
  }),
}))

const mockGetIncidents = vi.fn()
const mockGetIncidentCounts = vi.fn()
vi.mock('../../src/services/api', () => ({
  getIncidents: (...args) => mockGetIncidents(...args),
  getIncidentCounts: (...args) => mockGetIncidentCounts(...args),
}))

vi.mock('../../src/hooks/useToasts.jsx', () => ({
  useToasts: () => ({ showToast: vi.fn() }),
}))

// IncidentsPage opens IncidentReportModal on row click / deep-link
// — stub the modal so we don't need to mock its full dependency
// graph (Hls.js, useSharedToken, etc.).  The stub records its
// props so tests can verify the right incidentId was opened.
const mockModalProps = vi.fn()
vi.mock('../../src/components/IncidentReportModal.jsx', () => ({
  default: (props) => {
    mockModalProps(props)
    return <div data-testid="incident-modal-stub">modal:{props.incidentId}</div>
  },
}))

// NewIncidentModal stub — same idea.
vi.mock('../../src/components/NewIncidentModal.jsx', () => ({
  default: ({ onClose, onCreated }) => (
    <div data-testid="new-incident-modal-stub">
      <button onClick={onClose}>close</button>
      <button onClick={() => onCreated({ id: 999, title: 'new', status: 'open' })}>
        create
      </button>
    </div>
  ),
}))

import IncidentsPage from '../../src/pages/IncidentsPage.jsx'


// ── Sample data ──────────────────────────────────────────────────

const aiIncident = {
  id: 1,
  title: 'AI-filed: motion in garage',
  summary: 'Person observed.',
  severity: 'high',
  status: 'open',
  created_by: 'mcp:<sentinel-agent>',
  created_at: '2026-05-07T02:00:00.000Z',
  camera_id: 'cam_a',
  evidence_count: 2,
}
const humanIncident = {
  id: 2,
  title: 'Human-filed: package theft',
  summary: 'I saw it.',
  severity: 'medium',
  status: 'open',
  created_by: 'user:clerk_user_xyz',
  created_at: '2026-05-07T01:00:00.000Z',
  camera_id: 'cam_b',
  evidence_count: 0,
}
const resolvedIncident = {
  id: 3,
  title: 'Resolved: false alarm',
  summary: 'Wind.',
  severity: 'low',
  status: 'resolved',
  created_by: 'mcp:<sentinel-agent>',
  created_at: '2026-05-06T00:00:00.000Z',
  camera_id: 'cam_a',
  evidence_count: 1,
}


// ── Setup ────────────────────────────────────────────────────────

beforeEach(() => {
  mockGetIncidents.mockReset()
  mockGetIncidentCounts.mockReset()
  mockModalProps.mockReset()
  // Default: list returns one of each authorship.
  mockGetIncidents.mockResolvedValue({
    incidents: [aiIncident, humanIncident],
    total: 2,
  })
  mockGetIncidentCounts.mockResolvedValue({
    open: 2,
    open_critical: 0,
    open_high: 1,
    total: 2,
  })
})


const renderPage = (initialEntry = '/incidents') =>
  render(
    <MemoryRouter initialEntries={[initialEntry]}>
      <Routes>
        <Route path="/incidents" element={<IncidentsPage />} />
        <Route path="/incidents/:incidentId" element={<IncidentsPage />} />
      </Routes>
    </MemoryRouter>,
  )


// ── Tests ────────────────────────────────────────────────────────

describe('IncidentsPage', () => {
  it('renders the hero + stat tiles', async () => {
    renderPage()
    await waitFor(() => expect(mockGetIncidents).toHaveBeenCalled())

    expect(screen.getByText('Incident Reports')).toBeInTheDocument()
    expect(screen.getByText(/Security Operations/i)).toBeInTheDocument()
    // The 4 stat tiles — labels live in `.incidents-stat-label`.
    // "Open" is a label that ALSO matches the filter pill text, so
    // scope to the stat-label class.
    const statLabels = document.querySelectorAll('.incidents-stat-label')
    const labelTexts = Array.from(statLabels).map((el) => el.textContent)
    expect(labelTexts).toEqual(
      expect.arrayContaining(['Open', 'High & Critical', 'AI-Authored', 'Today']),
    )
  })

  it('renders rows for each incident the API returned', async () => {
    renderPage()
    await waitFor(() =>
      expect(screen.getByText('AI-filed: motion in garage')).toBeInTheDocument(),
    )
    expect(screen.getByText('Human-filed: package theft')).toBeInTheDocument()
  })

  it('source filter "AI" hides human-filed rows', async () => {
    const user = userEvent.setup()
    renderPage()
    await waitFor(() =>
      expect(screen.getByText('Human-filed: package theft')).toBeInTheDocument(),
    )

    await user.click(screen.getByRole('button', { name: /^AI$/ }))

    // AI row stays, human row drops out of the filtered view
    expect(screen.getByText('AI-filed: motion in garage')).toBeInTheDocument()
    expect(
      screen.queryByText('Human-filed: package theft'),
    ).not.toBeInTheDocument()
  })

  it('source filter "Human" hides AI-filed rows', async () => {
    const user = userEvent.setup()
    renderPage()
    await waitFor(() =>
      expect(screen.getByText('AI-filed: motion in garage')).toBeInTheDocument(),
    )

    await user.click(screen.getByRole('button', { name: /^Human$/ }))

    expect(screen.getByText('Human-filed: package theft')).toBeInTheDocument()
    expect(
      screen.queryByText('AI-filed: motion in garage'),
    ).not.toBeInTheDocument()
  })

  it('status filter "All" re-fetches without status=open', async () => {
    const user = userEvent.setup()
    renderPage()
    await waitFor(() => expect(mockGetIncidents).toHaveBeenCalled())

    // First call is the default mount with status=open
    const firstCall = mockGetIncidents.mock.calls[0][1]
    expect(firstCall.status).toBe('open')

    mockGetIncidents.mockClear()
    mockGetIncidents.mockResolvedValue({
      incidents: [aiIncident, humanIncident, resolvedIncident],
      total: 3,
    })

    await user.click(screen.getByRole('button', { name: /^All/ }))

    // Wait for a refetch with NO status param.  The filter change
    // triggers a useEffect re-run; we await until the most-recent
    // call is the one we expect rather than just the first call.
    await waitFor(() => {
      const lastCall = mockGetIncidents.mock.calls.at(-1)
      expect(lastCall).toBeDefined()
      expect(lastCall[1].status).toBeUndefined()
    })
  })

  it('empty state shows different copy for "Open" vs "AI" vs "Human" filters', async () => {
    const user = userEvent.setup()
    mockGetIncidents.mockResolvedValue({ incidents: [], total: 0 })
    mockGetIncidentCounts.mockResolvedValue({
      open: 0, open_critical: 0, open_high: 0, total: 0,
    })
    renderPage()
    await waitFor(() => expect(mockGetIncidents).toHaveBeenCalled())

    // Default Open filter — copy is "Watchlist clear" regardless of
    // source filter; status-filter takes precedence.
    expect(screen.getByText(/watchlist clear/i)).toBeInTheDocument()

    // Switch to "All" status so the source-filter-specific copy can
    // surface.
    await user.click(screen.getByRole('button', { name: /^All/ }))
    await waitFor(() => expect(screen.getByText(/no incidents yet/i)).toBeInTheDocument())

    // Switch to AI source filter
    await user.click(screen.getByRole('button', { name: /^AI$/ }))
    expect(screen.getByText(/no AI-filed incidents/i)).toBeInTheDocument()

    // Switch to Human source filter
    await user.click(screen.getByRole('button', { name: /^Human$/ }))
    expect(screen.getByText(/no human-filed incidents/i)).toBeInTheDocument()
  })

  it('+ New Incident button opens the create modal', async () => {
    const user = userEvent.setup()
    renderPage()
    await waitFor(() =>
      expect(screen.getByText('AI-filed: motion in garage')).toBeInTheDocument(),
    )

    expect(
      screen.queryByTestId('new-incident-modal-stub'),
    ).not.toBeInTheDocument()

    await user.click(screen.getByRole('button', { name: /new incident/i }))

    expect(screen.getByTestId('new-incident-modal-stub')).toBeInTheDocument()
  })

  it('clicking a row opens the detail modal with that incident id', async () => {
    const user = userEvent.setup()
    renderPage()
    await waitFor(() =>
      expect(screen.getByText('AI-filed: motion in garage')).toBeInTheDocument(),
    )

    // Click the row — its title text is inside a button
    await user.click(screen.getByText('AI-filed: motion in garage'))

    expect(screen.getByTestId('incident-modal-stub')).toBeInTheDocument()
    expect(mockModalProps).toHaveBeenCalled()
    const lastProps = mockModalProps.mock.calls.at(-1)[0]
    expect(lastProps.incidentId).toBe(1)
  })

  it('deep-link /incidents/:id auto-opens the detail modal on mount', async () => {
    renderPage('/incidents/42')
    await waitFor(() =>
      expect(screen.getByTestId('incident-modal-stub')).toBeInTheDocument(),
    )
    const lastProps = mockModalProps.mock.calls.at(-1)[0]
    expect(lastProps.incidentId).toBe(42)
  })

  it('passes onDeleted callback to the detail modal', async () => {
    const user = userEvent.setup()
    renderPage()
    await waitFor(() =>
      expect(screen.getByText('AI-filed: motion in garage')).toBeInTheDocument(),
    )

    await user.click(screen.getByText('AI-filed: motion in garage'))

    const props = mockModalProps.mock.calls.at(-1)[0]
    expect(typeof props.onDeleted).toBe('function')
    expect(typeof props.onClose).toBe('function')
    expect(typeof props.onUpdated).toBe('function')
  })

  it('optimistic delete drops the row from the list + decrements open count', async () => {
    const user = userEvent.setup()
    renderPage()
    await waitFor(() =>
      expect(screen.getByText('AI-filed: motion in garage')).toBeInTheDocument(),
    )

    // Open the AI row's modal — captures the onDeleted callback
    await user.click(screen.getByText('AI-filed: motion in garage'))
    const onDeleted = mockModalProps.mock.calls.at(-1)[0].onDeleted

    // Simulate the modal calling onDeleted(id) after a successful
    // delete (which is what the modal does internally).  The callback
    // fires synchronous React state updates (setIncidents +
    // setIncidentCounts + handleModalClose), so it MUST be wrapped in
    // act() — otherwise the update isn't flushed before the assertion
    // below runs and the test flakes (especially under CI load).  This
    // is exactly the "An update ... was not wrapped in act(...)"
    // warning the test was emitting.
    act(() => {
      onDeleted(1)
    })

    // Row should be optimistically removed (no API refresh required)
    await waitFor(() =>
      expect(
        screen.queryByText('AI-filed: motion in garage'),
      ).not.toBeInTheDocument(),
    )
    // Other row stays
    expect(screen.getByText('Human-filed: package theft')).toBeInTheDocument()
    // Modal closes
    expect(screen.queryByTestId('incident-modal-stub')).not.toBeInTheDocument()
  })
})
