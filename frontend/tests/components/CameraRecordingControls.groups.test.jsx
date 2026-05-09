// Camera Groups Phase 2: per-camera Group <select> inside the
// CameraRecordingControls panel.  Pinned behaviour:
//   - Selector renders only when groups exist AND canManageGroups
//   - Initial value mirrors camera.group_id; "(no group)" maps to null
//   - Changing selection calls assignCameraGroup with the new id
//   - Picking "(no group)" calls assignCameraGroup with null
//   - Optimistic UI: select flips immediately; failure rolls back

import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'

vi.mock('@clerk/clerk-react', () => ({
  useAuth: () => ({ getToken: () => Promise.resolve('test-jwt') }),
}))

const mockUpdateRecordingPolicy = vi.fn()
const mockAssignCameraGroup = vi.fn()
vi.mock('../../src/services/api', () => ({
  updateCameraRecordingPolicy: (...args) => mockUpdateRecordingPolicy(...args),
  assignCameraGroup: (...args) => mockAssignCameraGroup(...args),
}))

vi.mock('../../src/components/HelpTooltip.jsx', () => ({
  default: ({ children }) => <span>{children}</span>,
}))

import CameraRecordingControls from '../../src/components/CameraRecordingControls.jsx'


// ── Sample data ──────────────────────────────────────────────────

const baseCamera = {
  camera_id: 'cam_a',
  name: 'Garage cam',
  group_id: null,
  recording_policy: {
    continuous_24_7: false,
    scheduled_recording: false,
    scheduled_start: null,
    scheduled_end: null,
  },
}

const groups = [
  { id: 1, name: 'Front yard', color: '#22c55e', icon: '🏡', camera_count: 0 },
  { id: 2, name: 'Workshop', color: '#a855f7', icon: '🔧', camera_count: 0 },
]


beforeEach(() => {
  mockUpdateRecordingPolicy.mockReset()
  mockAssignCameraGroup.mockReset()
})


describe('CameraRecordingControls / Group selector (Phase 2)', () => {
  it('does not render the selector when groups list is empty', () => {
    render(<CameraRecordingControls camera={baseCamera} groups={[]} />)
    expect(screen.queryByLabelText(/camera group for/i)).not.toBeInTheDocument()
  })

  it('does not render the selector when canManageGroups is false', () => {
    render(
      <CameraRecordingControls
        camera={baseCamera}
        groups={groups}
        canManageGroups={false}
      />,
    )
    expect(screen.queryByLabelText(/camera group for/i)).not.toBeInTheDocument()
  })

  it('renders the selector with each group as an option + (no group)', () => {
    render(<CameraRecordingControls camera={baseCamera} groups={groups} />)
    const select = screen.getByLabelText(/camera group for garage cam/i)
    expect(select).toBeInTheDocument()
    // Options: "(no group)" + 2 groups
    const options = select.querySelectorAll('option')
    expect(options).toHaveLength(3)
    expect(options[0]).toHaveTextContent('(no group)')
    expect(options[1]).toHaveTextContent('Front yard')
    expect(options[2]).toHaveTextContent('Workshop')
  })

  it('initial selected value mirrors camera.group_id', () => {
    render(
      <CameraRecordingControls
        camera={{ ...baseCamera, group_id: 2 }}
        groups={groups}
      />,
    )
    expect(screen.getByLabelText(/camera group for/i)).toHaveValue('2')
  })

  it('changing the selection calls assignCameraGroup with the new id', async () => {
    const user = userEvent.setup()
    mockAssignCameraGroup.mockResolvedValue({ success: true })
    const onGroupChanged = vi.fn()
    render(
      <CameraRecordingControls
        camera={baseCamera}
        groups={groups}
        onGroupChanged={onGroupChanged}
      />,
    )

    const select = screen.getByLabelText(/camera group for/i)
    await user.selectOptions(select, '1')

    await waitFor(() =>
      expect(mockAssignCameraGroup).toHaveBeenCalledWith(
        expect.any(Function),
        'cam_a',
        1,
      ),
    )
    // Optimistic: the value updates without waiting for the parent prop
    expect(select).toHaveValue('1')
    // Parent callback fires with the new id (so SettingsPage can mirror)
    await waitFor(() => expect(onGroupChanged).toHaveBeenCalledWith(1))
  })

  it('selecting "(no group)" sends null to the API', async () => {
    const user = userEvent.setup()
    mockAssignCameraGroup.mockResolvedValue({ success: true })
    render(
      <CameraRecordingControls
        camera={{ ...baseCamera, group_id: 2 }}
        groups={groups}
      />,
    )

    const select = screen.getByLabelText(/camera group for/i)
    await user.selectOptions(select, '')

    await waitFor(() =>
      expect(mockAssignCameraGroup).toHaveBeenCalledWith(
        expect.any(Function),
        'cam_a',
        null,
      ),
    )
    expect(select).toHaveValue('')
  })

  it('rolls back the optimistic selection on API failure', async () => {
    const user = userEvent.setup()
    mockAssignCameraGroup.mockRejectedValue(new Error('boom'))
    const onGroupChanged = vi.fn()
    render(
      <CameraRecordingControls
        camera={baseCamera}
        groups={groups}
        onGroupChanged={onGroupChanged}
      />,
    )

    const select = screen.getByLabelText(/camera group for/i)
    await user.selectOptions(select, '1')

    // Settles back to the original value (null → empty-string select value)
    await waitFor(() => expect(select).toHaveValue(''))
    expect(mockAssignCameraGroup).toHaveBeenCalled()
    // Parent callback never fires on failure
    expect(onGroupChanged).not.toHaveBeenCalled()
  })
})
