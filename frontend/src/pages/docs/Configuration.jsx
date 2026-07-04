import { useDocs } from "./context"


function Configuration() {
  const { copyToClipboard } = useDocs()

  return (
    <section className="docs-section" id="configuration">
      <h2>Configuration<a href="#configuration" className="docs-anchor">#</a></h2>
      <p>
        CameraNode resolves configuration from multiple sources so you can run it
        however suits your deployment — interactive wizard for a single box,
        environment variables for Docker, CLI flags for one-off overrides.
      </p>

      <h3>Loading order</h3>
      <p>Higher priority overrides lower priority:</p>
      <ol>
        <li><strong>SQLite database</strong> (<code>data/node.db</code>) — primary source of truth, written by the setup wizard and encrypted at rest</li>
        <li><strong>YAML file</strong> (<code>config.yaml</code>) — legacy fallback, auto-migrated into the DB on first load</li>
        <li><strong>Environment variables</strong> — override any stored value at runtime</li>
        <li><strong>CLI flags</strong> — highest priority, typically used for debugging</li>
      </ol>
      <figure className="docs-diagram">
        <picture>
          <source srcSet="/images/config-precedence.webp" type="image/webp" />
          <img
            src="/images/config-precedence.jpg"
            alt="Configuration precedence stack: CLI flags (priority 4, top) override Environment (priority 3) override YAML config (priority 2) override SQLite database (priority 1, bottom). Vertical 'overrides' arrow on the left points up. Footnote: missing values fall through to the next band; present values at a higher band win."
            className="docs-diagram-image"
            width="1920"
            height="1080"
            loading="lazy"
          />
        </picture>
        <figcaption className="docs-diagram-caption">
          Higher bands override lower ones at runtime. The DB is the persistent source of truth — YAML, env vars, and CLI flags are ephemeral overrides layered on top for a single invocation.
        </figcaption>
      </figure>

      <h3>Environment variables</h3>
      <div className="docs-plans-table">
        <table>
          <thead>
            <tr><th>Variable</th><th>Purpose</th></tr>
          </thead>
          <tbody>
            <tr><td><code>SOURCEBOX_SENTRY_NODE_ID</code></td><td>Node ID assigned by Command Center</td></tr>
            <tr><td><code>SOURCEBOX_SENTRY_API_KEY</code></td><td>Node API key (encrypted at rest in the DB)</td></tr>
            <tr><td><code>SOURCEBOX_SENTRY_API_URL</code></td><td>Command Center URL (<code>https://sentinel-command.com</code>)</td></tr>
            <tr><td><code>SOURCEBOX_SENTRY_ENCODER</code></td><td>Force a specific encoder (e.g. <code>h264_nvenc</code>, <code>libx264</code>)</td></tr>
            <tr><td><code>RUST_LOG</code></td><td>Log verbosity: <code>trace</code>, <code>debug</code>, <code>info</code>, <code>warn</code>, <code>error</code></td></tr>
          </tbody>
        </table>
      </div>

      <h3>CLI flags</h3>
      <p>
        For one-off runs you can pass the three core values on the command line.
        They override anything in the database and env:
      </p>
      <div className="docs-code-block">
        <code>sourcebox-sentry-cameranode --node-id NODE --api-key KEY --api-url URL</code>
        <button className="docs-copy-btn" onClick={() => copyToClipboard('sourcebox-sentry-cameranode --node-id NODE --api-key KEY --api-url URL')}>Copy</button>
      </div>

      <h3>Example <code>config.yaml</code></h3>
      <p>
        The YAML file is optional — only needed if you want to seed values without the
        wizard, or tune motion detection. Placed next to the binary:
      </p>
      <div className="docs-code-block">
        <code>{`node_id: "node_abc123"
api_key: "nak_your_key_here"
api_url: "https://sentinel-command.com"

motion:
  enabled: true
  threshold: 0.02      # 0.0 = identical, 1.0 = totally different
  cooldown_secs: 30    # minimum seconds between motion events per camera

storage:
  max_size_gb: 64      # oldest recordings/snapshots evicted when over;
                       # the setup wizard suggests a disk-aware default
                       # (~80% of free disk, 5-64 GB clamp) — operator
                       # confirms or overrides at install time`}</code>
        <button className="docs-copy-btn" onClick={() => copyToClipboard(`node_id: "node_abc123"
api_key: "nak_your_key_here"
api_url: "https://sentinel-command.com"

motion:
  enabled: true
  threshold: 0.02
  cooldown_secs: 30

storage:
  max_size_gb: 64`)}>Copy</button>
      </div>

      <h3>Credential storage</h3>
      <p>
        The node API key is encrypted at rest in the SQLite DB using AES-256-GCM
        with a machine-derived key — SHA-256 of the OS-managed machine
        identifier (<code>/etc/machine-id</code> on Linux,
        <code>HKLM\\SOFTWARE\\Microsoft\\Cryptography\\MachineGuid</code> on
        Windows, <code>IOPlatformUUID</code> on macOS) plus a domain-separation
        tag. These are 128-bit values set once at OS install time, unique per
        host, and not user-modifiable. The database is <strong>not portable</strong>
        — copying <code>node.db</code> to a different host will make the stored
        key unreadable. Re-run <code>setup</code> after moving to a new machine.
      </p>

      <h3>Resetting a node</h3>
      <ul>
        <li><strong><code>/reauth confirm</code></strong> — from the dashboard's settings page, clears credentials and reopens the setup wizard. Preserves recordings.</li>
        <li><strong><code>/wipe confirm</code></strong> — erases all stored data (credentials, recordings, snapshots) and restarts setup from scratch.</li>
      </ul>
    </section>
  )
}

export default Configuration
