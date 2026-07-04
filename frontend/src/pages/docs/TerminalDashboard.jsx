import { useDocs } from "./context"


function TerminalDashboard() {
  const { copyToClipboard } = useDocs()

  return (
    <section className="docs-section" id="terminal-dashboard">
      <h2>Terminal Dashboard<a href="#terminal-dashboard" className="docs-anchor">#</a></h2>
      <p>
        CameraNode runs a full-screen terminal dashboard while streaming. It shows
        camera status, upload progress, and live logs — and lets you drive the node
        with slash commands without restarting the process.
      </p>

      <h3>Main view</h3>
      <p>Type <code>/</code> and press <strong>Enter</strong> to open the command menu.</p>
      <div className="docs-plans-table">
        <table>
          <thead>
            <tr><th>Command</th><th>What it does</th></tr>
          </thead>
          <tbody>
            <tr><td><code>/settings</code></td><td>Open the settings page</td></tr>
            <tr><td><code>/status</code></td><td>Show a short status summary (cameras, uptime, last upload)</td></tr>
            <tr><td><code>/clear</code></td><td>Clear the log panel</td></tr>
            <tr><td><code>/quit</code></td><td>Stop the node and exit gracefully</td></tr>
          </tbody>
        </table>
      </div>

      <h3>Settings page</h3>
      <p>From the settings page, additional commands are available:</p>
      <div className="docs-plans-table">
        <table>
          <thead>
            <tr><th>Command</th><th>What it does</th></tr>
          </thead>
          <tbody>
            <tr><td><code>/export-logs</code></td><td>Save the full log buffer to a timestamped file</td></tr>
            <tr><td><code>/wipe confirm</code></td><td>Erase all stored data and restart setup</td></tr>
            <tr><td><code>/reauth confirm</code></td><td>Clear credentials and re-run the setup wizard</td></tr>
            <tr><td><code>/back</code></td><td>Return to the dashboard</td></tr>
          </tbody>
        </table>
      </div>
      <p>Press <strong>Esc</strong> at any time to go back. Destructive commands require the <code>confirm</code> argument to fire — typing <code>/wipe</code> alone does nothing.</p>

      <h3>Log levels</h3>
      <p>The dashboard respects <code>RUST_LOG</code>. Set it before starting to see more or less detail:</p>
      <div className="docs-code-block">
        <code>RUST_LOG=debug ./sourcebox-sentry-cameranode</code>
        <button className="docs-copy-btn" onClick={() => copyToClipboard('RUST_LOG=debug ./sourcebox-sentry-cameranode')}>Copy</button>
      </div>
    </section>
  )
}

export default TerminalDashboard
