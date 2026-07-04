import { useState } from "react"

function KeyRotationModal({ isOpen, onClose, node, onRotate }) {
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState(null)
  const [credentials, setCredentials] = useState(null)

  const handleRotate = async () => {
    if (!node) return

    setLoading(true)
    setError(null)

    try {
      const result = await onRotate(node.node_id)
      setCredentials(result)
    } catch (err) {
      setError(err.message || "Failed to rotate key")
    } finally {
      setLoading(false)
    }
  }

  const handleCopy = (text) => {
    navigator.clipboard.writeText(text)
  }

  const handleClose = () => {
    // Never close while the rotation is in flight: the request still
    // completes server-side (old key already invalidated — the node
    // drops offline while the user believes they cancelled), and the
    // late setCredentials would leak THIS node's new key into the next
    // open of the still-mounted modal.
    if (loading) return
    setCredentials(null)
    setError(null)
    onClose()
  }

  // Overlay/× clicks must not silently discard a one-time key — the
  // only recovery is rotating again.  Require the explicit Done button
  // once credentials are showing.
  const handleDismissAttempt = () => {
    if (credentials) return
    handleClose()
  }

  if (!isOpen) return null

  return (
    <div className="modal-overlay" onClick={handleDismissAttempt}>
      <div className="modal-content" onClick={(e) => e.stopPropagation()}>
        <div className="modal-header">
          <h2>{credentials ? "API Key Rotated" : "Rotate API Key"}</h2>
          <button className="modal-close" onClick={handleDismissAttempt}>&times;</button>
        </div>

        {!credentials ? (
          <div className="modal-body">
            <p className="modal-description">
              Rotating the API key will immediately invalidate the old key.
              You'll need to update your CameraNode configuration with the new key.
            </p>
            
            <div className="node-info-box">
              <div className="info-row">
                <span className="info-label">Node:</span>
                <span className="info-value">{node?.name || `Node ${node?.node_id}`}</span>
              </div>
              <div className="info-row">
                <span className="info-label">Node ID:</span>
                <span className="info-value">{node?.node_id}</span>
              </div>
              {node?.key_rotated_at && (
                <div className="info-row">
                  <span className="info-label">Last rotated:</span>
                  <span className="info-value">{new Date(node.key_rotated_at).toLocaleString()}</span>
                </div>
              )}
            </div>

            {error && (
              <div className="error-message">{error}</div>
            )}

            <div className="modal-actions">
              <button
                type="button"
                className="btn btn-secondary"
                onClick={handleClose}
                disabled={loading}
              >
                Cancel
              </button>
              <button
                type="button"
                className="btn btn-danger"
                onClick={handleRotate}
                disabled={loading}
              >
                {loading ? "Rotating..." : "Rotate Key"}
              </button>
            </div>
          </div>
        ) : (
          <div className="modal-body">
            <div className="warning-banner">
              <span className="warning-icon">⚠️</span>
              <div>
                <strong>Old key invalidated!</strong>
                <p>Update your CameraNode configuration immediately.</p>
              </div>
            </div>

            <div className="credentials-box">
              <div className="credential-item">
                <label>Node ID</label>
                <div className="credential-value">
                  <code>{credentials.node_id}</code>
                  <button
                    className="btn btn-small"
                    onClick={() => handleCopy(credentials.node_id)}
                  >
                    Copy
                  </button>
                </div>
              </div>

              <div className="credential-item">
                <label>New API Key</label>
                <div className="credential-value">
                  <code>{credentials.api_key}</code>
                  <button
                    className="btn btn-small"
                    onClick={() => handleCopy(credentials.api_key)}
                  >
                    Copy
                  </button>
                </div>
              </div>
            </div>

            <div className="command-section">
              <h4>Update your CameraNode:</h4>
              <p className="deployment-description">
                Run the setup wizard on your device to update the API key:
              </p>
              <div className="command-box">
                <code>sourcebox-sentry-cameranode setup</code>
                <button
                  className="btn btn-small copy-command-btn"
                  onClick={() => handleCopy("sourcebox-sentry-cameranode setup")}
                >
                  Copy
                </button>
              </div>
              <div className="command-note">
                <strong>Tip:</strong> The wizard will prompt you for the new API key, then restart CameraNode.
              </div>
            </div>

            <div className="modal-actions">
              <button
                type="button"
                className="btn btn-primary"
                onClick={handleClose}
              >
                Done
              </button>
            </div>
          </div>
        )}
      </div>
    </div>
  )
}

export default KeyRotationModal