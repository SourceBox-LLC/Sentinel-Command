// Smoke tests for OrgAuditLogPanel — the admin dashboard's
// Organization Audit Log section.
//
// What's pinned:
//
//   - Renders a row per log entry returned from the API
//   - Event filter dropdown sends the event= query param to the API
//   - Username filter sends the username= query param to the API
//   - Empty result shows the empty-state copy (not a confusing blank
//     table)
//   - Pretty event labels render for known events; raw event string
//     falls through for unknown events (forward-compat with backend
//     adding new audit events before the dropdown gets updated)
//   - Export CSV button calls the download helper with the active
//     filters

import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'

vi.mock('@clerk/clerk-react', () => ({
  useAuth: () => ({
    getToken: () => Promise.resolve('test-jwt'),
  }),
  // The panel is org-keyed now (cross-tab org switches must refresh
  // this compliance surface).
  useOrganization: () => ({ organization: { id: 'org_test' } }),
}))

const mockGetOrgAuditLogs = vi.fn()
const mockDownloadOrgAuditLogsCsv = vi.fn()
vi.mock('../../src/services/api', () => ({
  getOrgAuditLogs: (...args) => mockGetOrgAuditLogs(...args),
  downloadOrgAuditLogsCsv: (...args) => mockDownloadOrgAuditLogsCsv(...args),
}))

// Stub the toast hook so the component doesn't need a ToastsProvider
// wrapper.  Tests assert on rendered DOM, not toast state.
vi.mock('../../src/hooks/useToasts.jsx', () => ({
  useToasts: () => ({ showToast: vi.fn() }),
}))

import OrgAuditLogPanel from '../../src/components/OrgAuditLogPanel.jsx'

beforeEach(() => {
  mockGetOrgAuditLogs.mockReset()
  mockDownloadOrgAuditLogsCsv.mockReset()
})

const sampleLogs = [
  {
    id: 1,
    timestamp: '2026-05-04T12:00:00',
    event: 'mcp_key_created',
    username: 'alice@example.com',
    ip: '203.0.113.5',
    details: 'name=ci-robot scope=readonly',
  },
  {
    id: 2,
    timestamp: '2026-05-04T11:00:00',
    event: 'member_promotion_requested',
    username: 'bob@example.com',
    ip: '198.51.100.10',
    details: null,
  },
]

describe('OrgAuditLogPanel', () => {
  it('renders one row per audit log returned by the API', async () => {
    mockGetOrgAuditLogs.mockResolvedValueOnce({
      total: 2,
      limit: 50,
      offset: 0,
      logs: sampleLogs,
    })

    render(<OrgAuditLogPanel />)

    await waitFor(() =>
      expect(screen.getByText('alice@example.com')).toBeInTheDocument(),
    )
    expect(screen.getByText('bob@example.com')).toBeInTheDocument()
  })

  it('renders pretty labels for known events', async () => {
    mockGetOrgAuditLogs.mockResolvedValueOnce({
      total: 2, limit: 50, offset: 0, logs: sampleLogs,
    })

    render(<OrgAuditLogPanel />)

    // Pretty label for mcp_key_created should appear, not the raw
    // event string.  Catches "added a new event but forgot to add
    // a label" regressions.  The labels also appear in the dropdown
    // <option> elements, so we scope the assertion to the <code>
    // wrapper used in the table body to disambiguate.
    await waitFor(() =>
      expect(
        screen.getByText('MCP key created', { selector: 'code' }),
      ).toBeInTheDocument(),
    )
    expect(
      screen.getByText('Member requested promotion', { selector: 'code' }),
    ).toBeInTheDocument()
  })

  it('falls back to the raw event string for unknown events', async () => {
    mockGetOrgAuditLogs.mockResolvedValueOnce({
      total: 1, limit: 50, offset: 0,
      logs: [{
        id: 99,
        timestamp: '2026-05-04T12:00:00',
        event: 'experimental_event_v2',
        username: 'eve',
        ip: '1.1.1.1',
        details: null,
      }],
    })

    render(<OrgAuditLogPanel />)

    // Forward-compat: a backend that adds a new audit event before
    // this component's dropdown gets updated should still render
    // the row, just with the raw event string.
    await waitFor(() =>
      expect(screen.getByText('experimental_event_v2')).toBeInTheDocument(),
    )
  })

  it('shows empty-state copy when there are no results', async () => {
    mockGetOrgAuditLogs.mockResolvedValueOnce({
      total: 0, limit: 50, offset: 0, logs: [],
    })

    render(<OrgAuditLogPanel />)

    await waitFor(() =>
      expect(
        screen.getByText(/No audit log entries match your filters/i),
      ).toBeInTheDocument(),
    )
  })

  it('passes the event filter to the API when changed', async () => {
    // Initial load.
    mockGetOrgAuditLogs.mockResolvedValueOnce({
      total: 0, limit: 50, offset: 0, logs: [],
    })
    // Reload after filter change.
    mockGetOrgAuditLogs.mockResolvedValueOnce({
      total: 1, limit: 50, offset: 0,
      logs: [sampleLogs[0]],
    })

    const user = userEvent.setup()
    render(<OrgAuditLogPanel />)

    // Wait for initial load to complete.
    await waitFor(() => expect(mockGetOrgAuditLogs).toHaveBeenCalledTimes(1))

    // Pick an event from the dropdown.
    await user.selectOptions(
      screen.getByLabelText(/Event type/i),
      'mcp_key_created',
    )

    await waitFor(() => expect(mockGetOrgAuditLogs).toHaveBeenCalledTimes(2))

    // Last call should include the event filter.
    const lastCall = mockGetOrgAuditLogs.mock.calls.at(-1)
    expect(lastCall[1]).toMatchObject({
      event: 'mcp_key_created',
      offset: 0,  // filter change resets pagination
    })
  })

  it('passes the username filter to the API when typed', async () => {
    mockGetOrgAuditLogs.mockResolvedValue({
      total: 0, limit: 50, offset: 0, logs: [],
    })

    const user = userEvent.setup()
    render(<OrgAuditLogPanel />)

    await waitFor(() => expect(mockGetOrgAuditLogs).toHaveBeenCalledTimes(1))

    await user.type(
      screen.getByLabelText(/Username/i),
      'alice',
    )

    // userEvent.type fires per-keystroke; the LAST call should have
    // the full string.
    await waitFor(() => {
      const lastCall = mockGetOrgAuditLogs.mock.calls.at(-1)
      expect(lastCall[1].username).toBe('alice')
    })
  })

  it('Export CSV button calls the download helper with active filters', async () => {
    mockGetOrgAuditLogs.mockResolvedValueOnce({
      total: 1, limit: 50, offset: 0,
      logs: [sampleLogs[0]],
    })
    mockDownloadOrgAuditLogsCsv.mockResolvedValueOnce(undefined)

    const user = userEvent.setup()
    render(<OrgAuditLogPanel />)

    // Wait for initial load.
    await waitFor(() =>
      expect(screen.getByText('alice@example.com')).toBeInTheDocument(),
    )

    await user.click(screen.getByRole('button', { name: /Export CSV/i }))

    await waitFor(() =>
      expect(mockDownloadOrgAuditLogsCsv).toHaveBeenCalledTimes(1),
    )

    // No filters set → empty params object passed (the helper still
    // appends ?format=csv internally).
    const callArgs = mockDownloadOrgAuditLogsCsv.mock.calls[0]
    expect(callArgs[1]).toEqual({})
  })

  it('Export CSV forwards the event filter when one is active', async () => {
    mockGetOrgAuditLogs.mockResolvedValue({
      total: 0, limit: 50, offset: 0, logs: [],
    })
    mockDownloadOrgAuditLogsCsv.mockResolvedValueOnce(undefined)

    const user = userEvent.setup()
    render(<OrgAuditLogPanel />)

    await waitFor(() => expect(mockGetOrgAuditLogs).toHaveBeenCalledTimes(1))

    await user.selectOptions(
      screen.getByLabelText(/Event type/i),
      'full_reset',
    )
    await waitFor(() => expect(mockGetOrgAuditLogs).toHaveBeenCalledTimes(2))

    await user.click(screen.getByRole('button', { name: /Export CSV/i }))

    await waitFor(() =>
      expect(mockDownloadOrgAuditLogsCsv).toHaveBeenCalledTimes(1),
    )
    expect(mockDownloadOrgAuditLogsCsv.mock.calls[0][1]).toEqual({
      event: 'full_reset',
    })
  })
})
