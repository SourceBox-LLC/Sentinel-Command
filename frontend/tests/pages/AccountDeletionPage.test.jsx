// AccountDeletionPage — deleting your own account (/account/delete).
//
// Pinned:
//   - The last admin of an organization with other members is told to
//     hand over first, and is offered no delete button at all.
//   - Otherwise the page says which organizations go with the account
//     and which are only left, and the button stays disabled until the
//     confirmation phrase is typed.
//   - A successful delete signs out to the marketing site.

import { describe, it, expect, vi, beforeEach } from 'vitest'
import { render, screen, waitFor } from '@testing-library/react'
import userEvent from '@testing-library/user-event'
import { MemoryRouter } from 'react-router-dom'

const mockSignOut = vi.fn(() => Promise.resolve())
vi.mock('@clerk/clerk-react', () => ({
  useAuth: () => ({ getToken: () => Promise.resolve('test-jwt'), isSignedIn: true, isLoaded: true }),
  useClerk: () => ({ signOut: mockSignOut }),
}))

const mockPreview = vi.fn()
const mockDelete = vi.fn()
vi.mock('../../src/services/api', () => ({
  getAccountDeletionPreview: (...a) => mockPreview(...a),
  deleteAccount: (...a) => mockDelete(...a),
}))

import AccountDeletionPage from '../../src/pages/AccountDeletionPage.jsx'

function renderPage() {
  return render(
    <MemoryRouter>
      <AccountDeletionPage />
    </MemoryRouter>,
  )
}

const base = {
  confirm_phrase: 'delete my account',
  deletes_organizations: [],
  leaves_organizations: [],
  blocked_by: [],
}

describe('AccountDeletionPage', () => {
  beforeEach(() => {
    mockPreview.mockReset()
    mockDelete.mockReset()
    mockSignOut.mockClear()
  })

  it('asks the last admin to hand over first, with no delete button', async () => {
    mockPreview.mockResolvedValue({ ...base, blocked_by: [{ id: 'org_1', name: 'Warehouse' }] })
    renderPage()
    expect(await screen.findByText('Make someone else an admin first')).toBeInTheDocument()
    expect(screen.getByText('Warehouse')).toBeInTheDocument()
    expect(screen.queryByRole('button', { name: 'Delete my account' })).toBeNull()
  })

  it('names what is deleted and what is left, and needs the phrase', async () => {
    mockPreview.mockResolvedValue({
      ...base,
      deletes_organizations: [{ id: 'org_solo', name: 'Home' }],
      leaves_organizations: [{ id: 'org_team', name: 'Office' }],
    })
    mockDelete.mockResolvedValue({ deleted: true, organizations_deleted: 1 })
    renderPage()

    expect(await screen.findByText('Home')).toBeInTheDocument()
    expect(screen.getByText('Office')).toBeInTheDocument()
    const button = screen.getByRole('button', { name: 'Delete my account' })
    expect(button).toBeDisabled()

    await userEvent.type(screen.getByLabelText(/to confirm/), 'delete my account')
    expect(button).toBeEnabled()
    await userEvent.click(button)

    await waitFor(() => expect(mockDelete).toHaveBeenCalled())
    expect(mockDelete.mock.calls[0][1]).toBe('delete my account')
    await waitFor(() =>
      expect(mockSignOut).toHaveBeenCalledWith({ redirectUrl: 'https://sentinel-command.com/' }),
    )
  })

  it('shows the server refusal and stays signed in', async () => {
    mockPreview.mockResolvedValue(base)
    mockDelete.mockRejectedValue(new Error('Could not reach the sign-in service.'))
    renderPage()
    await userEvent.type(await screen.findByLabelText(/to confirm/), 'delete my account')
    await userEvent.click(screen.getByRole('button', { name: 'Delete my account' }))
    expect(await screen.findByRole('alert')).toHaveTextContent('Could not reach the sign-in service.')
    expect(mockSignOut).not.toHaveBeenCalled()
  })
})
