// Smoke tests for the IncidentReportModal — the dashboard's primary
// incident triage surface.
//
// Today's incident-lifecycle work added meaningful UI surface area
// (edit mode toggle, severity dropdown, summary/report textareas,
// save-with-diff, permanent delete with confirmation, onDeleted
// callback wiring).  None of it had test coverage.  This file is
// the regression net.
//
// What's pinned:
//
//   - Modal renders the loaded incident (title, severity, summary,
//     status meta).
//   - Status-change buttons (Dismiss / Acknowledge / Mark Resolved /
//     Reopen) call patchIncident with the right body.
//   - Edit button toggles edit mode: severity badge → dropdown,
//     summary <p> → textarea, full-report div → textarea.
//   - Lifecycle status buttons HIDE in edit mode.
//   - Save in edit mode sends only changed fields (diff PATCH).
//   - No-op save (no changes) exits edit mode without an API call.
//   - Cancel exits edit mode without saving.
//   - Delete button shows window.confirm; on confirm, calls
//     deleteIncident + onDeleted callback (NOT onClose).
//   - Delete cancellation does nothing.

import { describe, it, expect, vi, beforeEach } from 'vitest'
import { fireEvent, render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'

// ── Mocks (must come before importing the modal) ─────────────────

vi.mock('@clerk/clerk-react', () => ({
  useAuth: () => ({
    getToken: () => Promise.resolve('test-jwt'),
  }),
}))

const mockGetIncident = vi.fn()
const mockPatchIncident = vi.fn()
const mockDeleteIncident = vi.fn()
vi.mock('../../src/services/api', () => ({
  getIncident: (...args) => mockGetIncident(...args),
  patchIncident: (...args) => mockPatchIncident(...args),
  deleteIncident: (...args) => mockDeleteIncident(...args),
  // Evidence helpers — never invoked in these tests because the
  // sample incident has empty `evidence: []`, but the modal imports
  // them at module load so we have to provide stubs.
  fetchIncidentEvidenceBlobUrl: vi.fn(),
  incidentEvidencePlaylistUrl: vi.fn(),
}))

// Toast hook stubbed to a spy so we don't need a ToastProvider.
const mockShowToast = vi.fn()
vi.mock('../../src/hooks/useToasts.jsx', () => ({
  useToasts: () => ({ showToast: mockShowToast }),
}))

// Shared-token hook is used by the evidence loader.  Stub returns a
// resolved token immediately so the modal doesn't sit in "waiting for
// shared token" state.
vi.mock('../../src/hooks/useSharedToken.jsx', () => ({
  useSharedToken: () => ({
    getCurrentToken: () => Promise.resolve('test-jwt'),
    ready: true,
  }),
}))

import IncidentReportModal from '../../src/components/IncidentReportModal.jsx'


// ── Sample data ──────────────────────────────────────────────────

const sampleIncident = {
  id: 42,
  org_id: 'org_test',
  title: 'Possible intruder — garage USB cam',
  summary: 'Person observed entering the storage area at 02:14.',
  report: '## What happened\n\nMotion fired at 02:14.  View_camera showed...',
  severity: 'high',
  status: 'open',
  created_by: 'mcp:<sentinel-agent>',
  created_at: '2026-05-07T02:14:33.000Z',
  updated_at: '2026-05-07T02:15:00.000Z',
  resolved_at: null,
  resolved_by: null,
  camera_id: 'b1b6c256_USB_Webcam',
  evidence: [],
  evidence_count: 0,
}


// ── Setup / teardown ─────────────────────────────────────────────

beforeEach(() => {
  mockGetIncident.mockReset()
  mockPatchIncident.mockReset()
  mockDeleteIncident.mockReset()
  mockShowToast.mockReset()
  // Default — every render starts with the sample incident loaded.
  mockGetIncident.mockResolvedValue(sampleIncident)
  mockPatchIncident.mockResolvedValue(sampleIncident)
  mockDeleteIncident.mockResolvedValue({ deleted: 42 })
  // jsdom doesn't ship a `window.confirm` implementation, so vi.spyOn
  // would fail at the spy site.  Install a fresh vi.fn() each test;
  // tests that need a specific return value call
  // `window.confirm.mockReturnValue(true|false)` directly.  Default
  // is true (matches the optimistic "user clicked OK") so any test
  // that forgets to set it doesn't hang waiting for confirmation.
  window.confirm = vi.fn(() => true)
})

const renderModal = (overrides = {}) => {
  const props = {
    incidentId: 42,
    onClose: vi.fn(),
    onUpdated: vi.fn(),
    onDeleted: vi.fn(),
    ...overrides,
  }
  const result = render(<IncidentReportModal {...props} />)
  return { ...result, props }
}


// ── Tests ────────────────────────────────────────────────────────

describe('IncidentReportModal', () => {
  it('shows a loading state then renders the loaded incident', async () => {
    renderModal()
    expect(screen.getByText(/loading incident/i)).toBeInTheDocument()
    // Title shows up after getIncident resolves
    await waitFor(() =>
      expect(
        screen.getByText('Possible intruder — garage USB cam'),
      ).toBeInTheDocument(),
    )
    expect(screen.getByText('High')).toBeInTheDocument()      // severity badge
    expect(screen.getByText(sampleIncident.summary)).toBeInTheDocument()
  })

  it('Dismiss button calls patchIncident with status=dismissed', async () => {
    const user = userEvent.setup()
    const { props } = renderModal()
    await waitFor(() => screen.getByText('Possible intruder — garage USB cam'))

    await user.click(screen.getByRole('button', { name: /dismiss/i }))
    await waitFor(() =>
      expect(mockPatchIncident).toHaveBeenCalledWith(
        expect.any(Function),
        42,
        { status: 'dismissed' },
      ),
    )
    expect(props.onUpdated).toHaveBeenCalled()
  })

  it('Mark Resolved button calls patchIncident with status=resolved', async () => {
    const user = userEvent.setup()
    renderModal()
    await waitFor(() => screen.getByText('Possible intruder — garage USB cam'))

    await user.click(screen.getByRole('button', { name: /mark resolved/i }))
    await waitFor(() =>
      expect(mockPatchIncident).toHaveBeenCalledWith(
        expect.any(Function),
        42,
        { status: 'resolved' },
      ),
    )
  })

  it('Edit button toggles into edit mode — dropdown + textareas appear', async () => {
    const user = userEvent.setup()
    renderModal()
    await waitFor(() => screen.getByText('Possible intruder — garage USB cam'))

    // View mode: severity is a span badge, summary is in a <p>.  No
    // <select>, no <textarea>.
    expect(screen.queryByRole('combobox', { name: /severity/i })).not.toBeInTheDocument()
    expect(document.querySelector('textarea')).toBeNull()

    await user.click(screen.getByRole('button', { name: /^edit$/i }))

    // Edit mode: severity dropdown, summary textarea, report textarea.
    expect(screen.getByRole('combobox', { name: /severity/i })).toBeInTheDocument()
    const textareas = document.querySelectorAll('textarea')
    expect(textareas.length).toBeGreaterThanOrEqual(2)
    expect(screen.getByRole('button', { name: /save changes/i })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /^cancel$/i })).toBeInTheDocument()
  })

  it('hides lifecycle status buttons in edit mode', async () => {
    const user = userEvent.setup()
    renderModal()
    await waitFor(() => screen.getByText('Possible intruder — garage USB cam'))

    // View mode — Dismiss/Acknowledge/Mark Resolved all visible
    expect(screen.getByRole('button', { name: /dismiss/i })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /acknowledge/i })).toBeInTheDocument()
    expect(screen.getByRole('button', { name: /mark resolved/i })).toBeInTheDocument()

    await user.click(screen.getByRole('button', { name: /^edit$/i }))

    // Edit mode — none of those should still be in the action row.
    expect(screen.queryByRole('button', { name: /dismiss/i })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: /acknowledge/i })).not.toBeInTheDocument()
    expect(screen.queryByRole('button', { name: /mark resolved/i })).not.toBeInTheDocument()
  })

  it('Cancel exits edit mode without an API call', async () => {
    const user = userEvent.setup()
    renderModal()
    await waitFor(() => screen.getByText('Possible intruder — garage USB cam'))

    await user.click(screen.getByRole('button', { name: /^edit$/i }))
    await user.click(screen.getByRole('button', { name: /^cancel$/i }))

    // Back in view mode — Edit visible again, no PATCH call.
    expect(screen.getByRole('button', { name: /^edit$/i })).toBeInTheDocument()
    expect(mockPatchIncident).not.toHaveBeenCalled()
  })

  it('Save with no changes is a no-op (no API call) and exits edit mode', async () => {
    const user = userEvent.setup()
    renderModal()
    await waitFor(() => screen.getByText('Possible intruder — garage USB cam'))

    await user.click(screen.getByRole('button', { name: /^edit$/i }))
    await user.click(screen.getByRole('button', { name: /save changes/i }))

    // No PATCH (nothing changed); modal returns to view mode.
    expect(mockPatchIncident).not.toHaveBeenCalled()
    await waitFor(() =>
      expect(screen.getByRole('button', { name: /^edit$/i })).toBeInTheDocument(),
    )
  })

  it('Save sends only the diff when severity changes', async () => {
    const user = userEvent.setup()
    renderModal()
    await waitFor(() => screen.getByText('Possible intruder — garage USB cam'))

    await user.click(screen.getByRole('button', { name: /^edit$/i }))
    await waitFor(() =>
      expect(screen.getByRole('combobox', { name: /severity/i })).toBeInTheDocument(),
    )

    // Change severity from high → medium
    await user.selectOptions(
      screen.getByRole('combobox', { name: /severity/i }),
      'medium',
    )

    await user.click(screen.getByRole('button', { name: /save changes/i }))

    await waitFor(() =>
      expect(mockPatchIncident).toHaveBeenCalledWith(
        expect.any(Function),
        42,
        // Only severity in the body — summary + report stayed the same
        { severity: 'medium' },
      ),
    )
  })

  it('Save sends multiple fields when severity AND summary both change', async () => {
    const user = userEvent.setup()
    renderModal()
    await waitFor(() => screen.getByText('Possible intruder — garage USB cam'))

    await user.click(screen.getByRole('button', { name: /^edit$/i }))

    // userEvent.selectOptions handles controlled <select> cleanly.
    await user.selectOptions(
      screen.getByRole('combobox', { name: /severity/i }),
      'low',
    )
    // Summary textarea — userEvent.type loses characters when typing
    // into a controlled <textarea> (each keystroke causes a re-render
    // that races against the next type call, leaving most chars
    // dropped).  fireEvent.change sets the entire value in one shot
    // via the React-internal event path.
    const summaryTa = screen.getByPlaceholderText(/one or two sentences/i)
    fireEvent.change(summaryTa, { target: { value: 'updated summary' } })
    // Wait for the controlled re-render to flush before querying for
    // the Save button (the textarea value-prop change can briefly
    // unmount+remount nearby DOM in test mode).
    await waitFor(() => expect(summaryTa.value).toBe('updated summary'))

    await user.click(
      await screen.findByRole('button', { name: /save changes/i }),
    )

    await waitFor(() => expect(mockPatchIncident).toHaveBeenCalled())
    const callArgs = mockPatchIncident.mock.calls[0]
    expect(callArgs[1]).toBe(42)
    expect(callArgs[2]).toEqual({
      severity: 'low',
      summary: 'updated summary',
    })
    // Crucially — no `report` field, since it didn't change.
    expect(callArgs[2]).not.toHaveProperty('report')
  })

  it('Delete prompts for confirmation; on confirm calls deleteIncident + onDeleted', async () => {
    const user = userEvent.setup()
    const { props } = renderModal()
    await waitFor(() => screen.getByText('Possible intruder — garage USB cam'))

    // Stub window.confirm to return true (user clicks OK)
    window.confirm.mockReturnValue(true)

    await user.click(screen.getByRole('button', { name: /^delete$/i }))

    expect(window.confirm).toHaveBeenCalled()
    await waitFor(() =>
      expect(mockDeleteIncident).toHaveBeenCalledWith(expect.any(Function), 42),
    )
    expect(props.onDeleted).toHaveBeenCalledWith(42)
    // onClose must NOT fire on the delete path — onDeleted handles
    // the parent's modal teardown so callers can do extra work
    // (decrement counts, drop the row from a list) before the modal
    // unmounts.
    expect(props.onClose).not.toHaveBeenCalled()
  })

  it('Delete cancellation does nothing', async () => {
    const user = userEvent.setup()
    const { props } = renderModal()
    await waitFor(() => screen.getByText('Possible intruder — garage USB cam'))

    window.confirm.mockReturnValue(false)
    await user.click(screen.getByRole('button', { name: /^delete$/i }))

    expect(window.confirm).toHaveBeenCalled()
    expect(mockDeleteIncident).not.toHaveBeenCalled()
    expect(props.onDeleted).not.toHaveBeenCalled()
    expect(props.onClose).not.toHaveBeenCalled()
  })

  it('falls back to onClose if the parent did not pass onDeleted', async () => {
    const user = userEvent.setup()
    const onClose = vi.fn()
    render(
      <IncidentReportModal
        incidentId={42}
        onClose={onClose}
        onUpdated={vi.fn()}
      />,
    )
    await waitFor(() => screen.getByText('Possible intruder — garage USB cam'))

    window.confirm.mockReturnValue(true)
    await user.click(screen.getByRole('button', { name: /^delete$/i }))

    await waitFor(() => expect(mockDeleteIncident).toHaveBeenCalled())
    // No onDeleted → modal closes via onClose so the user isn't
    // stuck staring at a modal pointing at a deleted record.
    await waitFor(() => expect(onClose).toHaveBeenCalled())
  })

  it('shows a toast when the patch API call fails', async () => {
    const user = userEvent.setup()
    mockPatchIncident.mockRejectedValueOnce(new Error('server boom'))
    renderModal()
    await waitFor(() => screen.getByText('Possible intruder — garage USB cam'))

    await user.click(screen.getByRole('button', { name: /dismiss/i }))

    await waitFor(() =>
      expect(mockShowToast).toHaveBeenCalledWith(
        expect.stringContaining('boom'),
        'error',
      ),
    )
  })

  it('shows a toast when the delete API call fails', async () => {
    const user = userEvent.setup()
    const { props } = renderModal()
    await waitFor(() => screen.getByText('Possible intruder — garage USB cam'))
    mockDeleteIncident.mockRejectedValueOnce(new Error('delete blocked'))
    window.confirm.mockReturnValue(true)

    await user.click(screen.getByRole('button', { name: /^delete$/i }))

    await waitFor(() =>
      expect(mockShowToast).toHaveBeenCalledWith(
        expect.stringContaining('blocked'),
        'error',
      ),
    )
    // Failure path should NOT fire onDeleted — list shouldn't drop a
    // row that's still on the server.
    expect(props.onDeleted).not.toHaveBeenCalled()
  })
})
