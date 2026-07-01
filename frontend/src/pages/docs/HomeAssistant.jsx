import { useDocs } from "./context"


function HomeAssistant() {
  const { base } = useDocs()

  return (
    <section className="docs-section" id="home-assistant">
      <h2>Home Assistant<a href="#home-assistant" className="docs-anchor">#</a></h2>
      <div className="docs-callout docs-callout-info">
        <p>
          <span className="docs-callout-icon">🏠</span>
          <span>
            Available on <strong>every plan</strong>, including Free. Live video streams
            <strong> directly over your local network</strong>, so it never counts against
            your viewer-hour cap.
          </span>
        </p>
      </div>
      <p>
        Connect Sentinel to <strong>Home Assistant</strong> with a single key. Every camera
        across every CloudNode in your organization shows up at once — and the entity list
        re-syncs automatically as you add or move nodes. You configure it <em>once</em>
        against the Command Center, not per node.
      </p>

      <h3>What you get</h3>
      <ul>
        <li><strong>Cameras</strong> — live video (pulled LAN-direct from each node) plus on-demand snapshots.</li>
        <li><strong>Recording switch</strong> — turn continuous recording on/off per camera.</li>
        <li><strong>Motion sensor</strong> — a <code>binary_sensor</code> per camera, fired in real time, ready for automations.</li>
        <li><strong>Connectivity sensor</strong> — whether each camera is online.</li>
        <li><strong>Node diagnostics</strong> — storage used and CloudNode version, per node.</li>
      </ul>

      <h3>Setup</h3>
      <div className="docs-steps">
        <div className="docs-step">
          <div className="docs-step-number">1</div>
          <div className="docs-step-content">
            <h4>Generate an integration key</h4>
            <p>
              In the Command Center, open <strong>Integrations</strong> and click
              <strong> Generate Key</strong>. It starts with <code>osi_</code> and is shown
              once — copy it now.
            </p>
          </div>
        </div>
        <div className="docs-step">
          <div className="docs-step-number">2</div>
          <div className="docs-step-content">
            <h4>Install the integration in Home Assistant</h4>
            <p>
              Add the <strong>Sentinel</strong> integration via HACS (add its GitHub
              repository as a <em>custom repository</em>, category <em>Integration</em>), or
              copy <code>custom_components/sourcebox_sentry/</code> into your Home Assistant
              <code> config/custom_components/</code> folder. Restart Home Assistant.
            </p>
          </div>
        </div>
        <div className="docs-step">
          <div className="docs-step-number">3</div>
          <div className="docs-step-content">
            <h4>Add it and paste your key</h4>
            <p>
              In Home Assistant go to <strong>Settings → Devices &amp; Services → Add
              Integration → Sentinel by SourceBox</strong>. Enter your Command Center URL
              (<code>{base}</code>) and the key from step 1.
            </p>
          </div>
        </div>
      </div>
      <p className="docs-subtle">
        That's it — all your cameras appear as Home Assistant devices. Add a node or camera
        in the Command Center later and it shows up automatically on the next refresh, no
        reconfiguration needed.
      </p>

      <h3>Notes</h3>
      <ul>
        <li>
          <strong>Local network:</strong> live video requires Home Assistant to be on the
          same LAN as your camera nodes (the usual home setup). Snapshots, recording control,
          motion, and sensors work from anywhere.
        </li>
        <li>
          <strong>Enable LAN streaming on the node:</strong> Connected-mode CloudNodes bind
          to <code>localhost</code> by default, so their local video server isn&rsquo;t
          reachable from Home Assistant out of the box — the camera entities load but live
          video shows nothing. Re-run <code>sourcebox-sentry-cloudnode setup</code> with the{" "}
          <code>--lan-streaming</code> flag (or enable it in the setup wizard) on each node
          you want HA to stream from. Snapshots and all sensors work either way.
        </li>
        <li>
          <strong>Revoking access:</strong> revoke a key on the Integrations page and the
          connected Home Assistant instance stops receiving data immediately. Generate a new
          key to reconnect.
        </li>
        <li>
          <strong>MCP vs. Home Assistant:</strong> these are separate. MCP (Pro/Pro Plus) is
          for AI agents; the Home Assistant integration is a REST connection available on
          every plan. Their keys (<code>osc_</code> vs <code>osi_</code>) are not
          interchangeable.
        </li>
      </ul>
    </section>
  )
}

export default HomeAssistant
