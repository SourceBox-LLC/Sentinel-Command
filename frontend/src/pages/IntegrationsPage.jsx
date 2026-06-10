import { useState, useEffect } from "react"
import { useAuth, useOrganization } from "@clerk/clerk-react"
import {
  getIntegrationKeys,
  createIntegrationKey,
  revokeIntegrationKey,
} from "../services/api"
import { useToasts } from "../hooks/useToasts.jsx"

// The user pastes this into the Home Assistant integration's config flow,
// alongside a key generated below.
const COMMAND_CENTER_URL = window.location.origin

function IntegrationsPage() {
  const { getToken } = useAuth()
  const { organization } = useOrganization()
  const { showToast } = useToasts()

  const [keys, setKeys] = useState([])
  const [keysLoading, setKeysLoading] = useState(false)
  const [newKeyName, setNewKeyName] = useState("")
  const [createdKey, setCreatedKey] = useState(null)
  const [creating, setCreating] = useState(false)
  const [revoking, setRevoking] = useState(null)

  useEffect(() => {
    if (organization) loadKeys()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [organization])

  const loadKeys = async () => {
    setKeysLoading(true)
    try {
      const token = await getToken()
      const data = await getIntegrationKeys(() => Promise.resolve(token))
      setKeys(data)
    } catch (err) {
      console.error("Failed to load integration keys:", err)
    } finally {
      setKeysLoading(false)
    }
  }

  const handleCreate = async () => {
    // Re-entrancy guard — Enter on the name input calls this directly
    // and only the button is disabled while a create is in flight; a
    // double Enter would mint a second key and overwrite the one-time
    // secret display of the first.
    if (creating) return
    if (!newKeyName.trim()) return
    setCreating(true)
    try {
      const token = await getToken()
      const data = await createIntegrationKey(
        () => Promise.resolve(token),
        { name: newKeyName.trim() },
      )
      setCreatedKey(data.key)
      setNewKeyName("")
      await loadKeys()
      showToast("Integration key created", "success")
    } catch (err) {
      showToast(err.message || "Failed to create key", "error")
    } finally {
      setCreating(false)
    }
  }

  const handleRevoke = async (key) => {
    const confirmed = window.confirm(
      `Revoke integration key "${key.name}"?\n\n` +
      `Any Home Assistant instance using this key will immediately stop ` +
      `receiving camera, snapshot, and motion data. This cannot be undone — ` +
      `generate a new key to reconnect.`
    )
    if (!confirmed) return
    setRevoking(key.id)
    try {
      const token = await getToken()
      await revokeIntegrationKey(() => Promise.resolve(token), key.id)
      await loadKeys()
      showToast(`Key "${key.name}" revoked`, "success")
    } catch (err) {
      showToast(err.message || "Failed to revoke key", "error")
    } finally {
      setRevoking(null)
    }
  }

  const copy = async (text) => {
    try {
      await navigator.clipboard.writeText(text)
      showToast("Copied to clipboard", "success")
    } catch {
      showToast("Copy failed — select and copy manually", "error")
    }
  }

  if (!organization) {
    return (
      <div className="mcp-container">
        <h1 className="page-title">Integrations</h1>
        <p className="text-muted">Please select an organization.</p>
      </div>
    )
  }

  return (
    <div className="mcp-container">
      <h1 className="page-title">Integrations</h1>
      <p className="text-muted">
        Connect Sentinel to <strong>Home Assistant</strong> with a single key —
        every camera across every node appears at once, with live video,
        snapshots, recording control, and motion triggers. Available on every
        plan. Live video streams directly over your local network, so it never
        counts against your viewer-hour cap.
      </p>

      {createdKey && (
        <div className="mcp-key-created">
          <div className="mcp-key-created-header">
            <span className="mcp-key-created-icon">🔑</span>
            <strong>Your new integration key</strong>
          </div>
          <p className="mcp-key-warning">
            This is the only time you&rsquo;ll see this key. Copy it now.
          </p>
          <div className="mcp-key-display">
            <code>{createdKey}</code>
            <button className="btn btn-small btn-secondary" onClick={() => copy(createdKey)}>
              Copy
            </button>
          </div>
          <button
            className="btn btn-small btn-secondary mcp-key-dismiss"
            onClick={() => setCreatedKey(null)}
          >
            Done
          </button>
        </div>
      )}

      <div className="mcp-key-create">
        <input
          type="text"
          placeholder="Key name (e.g. 'Home Assistant')"
          value={newKeyName}
          onChange={(e) => setNewKeyName(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && handleCreate()}
          className="mcp-key-input"
        />
        <button
          className="btn btn-primary"
          onClick={handleCreate}
          disabled={creating || !newKeyName.trim()}
        >
          {creating ? "Creating..." : "Generate Key"}
        </button>
      </div>

      {keysLoading ? (
        <div className="loading-spinner" />
      ) : keys.length > 0 ? (
        <div className="mcp-keys-list">
          {keys.map((k) => (
            <div key={k.id} className="mcp-key-item">
              <div className="mcp-key-info">
                <div className="mcp-key-name-row">
                  <span className="mcp-key-name">{k.name}</span>
                </div>
                <span className="mcp-key-meta">
                  Created {new Date(k.created_at).toLocaleDateString()}
                  {k.last_used_at && (
                    <> — Last used {new Date(k.last_used_at).toLocaleDateString()}</>
                  )}
                </span>
              </div>
              <button
                className="btn btn-small btn-danger"
                onClick={() => handleRevoke(k)}
                disabled={revoking === k.id}
                title="Revoke this key — connected Home Assistant instances stop working"
              >
                {revoking === k.id ? "Revoking..." : "Revoke"}
              </button>
            </div>
          ))}
        </div>
      ) : (
        <p className="text-muted">
          No integration keys yet. Generate one above to connect Home Assistant.
        </p>
      )}

      <div className="mcp-key-created" style={{ marginTop: "2rem" }}>
        <div className="mcp-key-created-header">
          <span className="mcp-key-created-icon">🏠</span>
          <strong>Connecting Home Assistant</strong>
        </div>
        <p className="mcp-key-warning" style={{ color: "var(--text-muted)" }}>
          In the Sentinel Home Assistant integration, enter your Command Center
          URL and paste a key from above. Live video is pulled directly from
          each camera node on your LAN.
        </p>
        <div className="mcp-key-display">
          <code>{COMMAND_CENTER_URL}</code>
          <button className="btn btn-small btn-secondary" onClick={() => copy(COMMAND_CENTER_URL)}>
            Copy
          </button>
        </div>
      </div>
    </div>
  )
}

export default IntegrationsPage
