// NodeStorageBar — the cap editor on Settings → Camera Nodes.
//
// Pinned: "Change" is offered only while the node is connected; saving
// sends the new cap, warns first when it will delete recordings, and
// shows the server's refusal (an older node, a cap larger than its disk).

import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'

vi.mock('@clerk/clerk-react', () => ({
  useAuth: () => ({ getToken: () => Promise.resolve('test-jwt'), isSignedIn: true }),
}))
const mockShowToast = vi.fn()
vi.mock('../../src/hooks/useToasts.jsx', () => ({
  useToasts: () => ({ showToast: mockShowToast }),
}))
const mockSetCap = vi.fn()
vi.mock('../../src/services/api', () => ({
  setNodeStorageCap: (...a) => mockSetCap(...a),
}))

import NodeStorageBar from '../../src/components/NodeStorageBar.jsx'

const GIB = 1024 ** 3
const storage = { used_bytes: 10 * GIB, max_bytes: 64 * GIB }

describe('NodeStorageBar cap editor', () => {
  beforeEach(() => {
    mockSetCap.mockReset()
    mockShowToast.mockReset()
  })

  it('offers no change while the node is offline', () => {
    render(<NodeStorageBar storage={storage} nodeId="n1" online={false} />)
    expect(screen.getByRole('button', { name: 'Change' })).toBeDisabled()
  })

  it('warns before deleting, saves, and refreshes', async () => {
    mockSetCap.mockResolvedValue({ max_size_gb: 4, freed_bytes: 6 * GIB })
    const onChanged = vi.fn()
    render(<NodeStorageBar storage={storage} nodeId="n1" online onChanged={onChanged} />)

    await userEvent.click(screen.getByRole('button', { name: 'Change' }))
    const input = screen.getByRole('spinbutton')
    await userEvent.clear(input)
    await userEvent.type(input, '4')
    expect(screen.getByText(/deletes about 6\.0 GB/)).toBeInTheDocument()

    await userEvent.click(screen.getByRole('button', { name: 'Save' }))
    await waitFor(() => expect(onChanged).toHaveBeenCalled())
    expect(mockSetCap.mock.calls[0].slice(1)).toEqual(['n1', 4])
    expect(mockShowToast.mock.calls[0][0]).toMatch(/Deleted 6\.0 GB/)
  })

  it("shows the server's refusal", async () => {
    mockSetCap.mockRejectedValue(new Error('This CameraNode is too old to change its storage cap remotely.'))
    render(<NodeStorageBar storage={storage} nodeId="n1" online />)
    await userEvent.click(screen.getByRole('button', { name: 'Change' }))
    const input = screen.getByRole('spinbutton')
    await userEvent.clear(input)
    await userEvent.type(input, '32')
    await userEvent.click(screen.getByRole('button', { name: 'Save' }))
    expect(await screen.findByRole('alert')).toHaveTextContent('too old')
  })
})
