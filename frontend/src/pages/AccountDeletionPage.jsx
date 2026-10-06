import { useEffect, useState } from "react"
import { Navigate, Link } from "react-router-dom"
import { useAuth, useClerk, IS_LOCAL_AUTH } from "../auth/index.jsx"
import { getAccountDeletionPreview, deleteAccount } from "../services/api"

// Deleting your own account. Reached from "Delete account" in the
// account menu; Clerk's own delete button is hidden so every deletion
// comes through here, where the backend can refuse to strand an
// organization without an admin and can erase what the person leaves
// behind. See backend-rs/src/api/account.rs.

function OrgList({ orgs }) {
  return (
    <ul className="account-delete-orgs">
      {orgs.map((org) => (
        <li key={org.id}>{org.name || org.id}</li>
      ))}
    </ul>
  )
}

function AccountDeletionPage() {
  const { isLoaded, isSignedIn, getToken } = useAuth()
  const { signOut } = useClerk()
  const [preview, setPreview] = useState(null)
  const [loadError, setLoadError] = useState(null)
  const [confirmText, setConfirmText] = useState("")
  const [deleting, setDeleting] = useState(false)
  const [deleteError, setDeleteError] = useState(null)

  useEffect(() => {
    if (!isSignedIn || IS_LOCAL_AUTH) return
    getAccountDeletionPreview(getToken)
      .then(setPreview)
      .catch((err) => setLoadError(err.message || "Could not load your account details."))
  }, [isSignedIn, getToken])

  if (IS_LOCAL_AUTH) {
    // A self-hosted install has one admin account, set in its
    // environment. There is nothing here to delete.
    return <Navigate to="/dashboard" replace />
  }
  if (isLoaded && !isSignedIn) {
    return <Navigate to="/sign-in" replace />
  }

  const phrase = preview?.confirm_phrase ?? "delete my account"
  const blocked = (preview?.blocked_by ?? []).length > 0
  const deletes = preview?.deletes_organizations ?? []
  const leaves = preview?.leaves_organizations ?? []

  const handleDelete = async () => {
    setDeleting(true)
    setDeleteError(null)
    try {
      await deleteAccount(getToken, confirmText)
      // The account no longer exists, so the session is already dead at
      // Clerk; signing out clears it here too.
      await signOut({ redirectUrl: "https://sentinel-command.com/" })
    } catch (err) {
      setDeleteError(err.message || "Your account could not be deleted.")
      setDeleting(false)
    }
  }

  return (
    <div className="settings-container account-delete">
      <h1 className="page-title">Delete your account</h1>

      {loadError && <p className="account-delete-error" role="alert">{loadError}</p>}
      {!preview && !loadError && (
        <div className="loading-container">
          <div className="loading-spinner"></div>
        </div>
      )}

      {preview && blocked && (
        <div className="settings-section">
          <h2>Make someone else an admin first</h2>
          <p>
            You are the only admin of these organizations, and they have other
            members. If you deleted your account now, nobody could manage them,
            pay for them, or delete them.
          </p>
          <OrgList orgs={preview.blocked_by} />
          <p>
            Switch to each one in the organization menu at the top of the page,
            choose <strong>Manage → Members</strong>, and make another member an
            admin. Or remove the other members, and the organization
            will be deleted with your account. Then come back here.
          </p>
          <Link to="/dashboard" className="btn btn-secondary">Back to the dashboard</Link>
        </div>
      )}

      {preview && !blocked && (
        <div className="settings-section danger-zone">
          <h2>What happens</h2>
          <ul className="account-delete-effects">
            <li>Your sign-in, name and email address are deleted. You cannot undo this.</li>
            <li>
              Your viewing history is deleted. In the audit logs of your
              organizations, your actions stay but are shown as "deleted user",
              without your IP address.
            </li>
            {leaves.length > 0 && (
              <li>
                You leave these organizations. They and their cameras are not
                affected:
                <OrgList orgs={leaves} />
              </li>
            )}
            {deletes.length > 0 && (
              <li>
                <strong>These organizations are deleted too, because you are their
                only member</strong>, with all their cameras, incidents, logs and
                settings:
                <OrgList orgs={deletes} />
                Your CameraNodes keep their local recordings. Uninstall them to
                remove those. If you want a copy of an organization&apos;s data first,
                download it from <strong>Settings → Privacy &amp; Data</strong>.
              </li>
            )}
            <li>
              If an organization you are leaving has a paid plan you pay for, cancel
              it first under <strong>Manage → Billing</strong> in the organization
              menu. Deleting your account does not issue a refund.
            </li>
          </ul>

          <div className="danger-confirm-input">
            <label htmlFor="account-delete-confirm">
              Type <strong>{phrase}</strong> to confirm:
            </label>
            <input
              id="account-delete-confirm"
              type="text"
              value={confirmText}
              onChange={(e) => setConfirmText(e.target.value)}
              placeholder={phrase}
              autoComplete="off"
              disabled={deleting}
            />
          </div>
          {deleteError && <p className="account-delete-error" role="alert">{deleteError}</p>}
          <div className="modal-actions">
            <Link to="/dashboard" className="btn btn-secondary">Cancel</Link>
            <button
              type="button"
              className="btn btn-danger"
              onClick={handleDelete}
              disabled={deleting || confirmText.trim().toLowerCase() !== phrase}
            >
              {deleting ? "Deleting…" : "Delete my account"}
            </button>
          </div>
        </div>
      )}
    </div>
  )
}

export default AccountDeletionPage
