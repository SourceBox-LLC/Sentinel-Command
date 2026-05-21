import { Link } from "react-router-dom"


function Plans() {
  return (
    <section className="docs-section" id="plans">
      <h2>Plans & Limits<a href="#plans" className="docs-anchor">#</a></h2>
      <p>
        Sentinel is priced on <strong>usage</strong> — how much live
        video you watch per month — not on how many cameras you connect.
        Hardware counts (cameras, nodes) are generous abuse-rails rather than
        product differentiators; the real tier axis is viewer-hours and
        MCP integration depth.
      </p>

      <div className="docs-plans-table">
        <table>
          <thead>
            <tr>
              <th>Feature</th>
              <th>Free</th>
              <th>Pro</th>
              <th>Pro Plus</th>
            </tr>
          </thead>
          <tbody>
            <tr><td><strong>Viewer-hours / month</strong></td><td><strong>30</strong></td><td><strong>300</strong></td><td><strong>1,500</strong></td></tr>
            <tr><td>Cameras (abuse rail)</td><td>5</td><td>25</td><td>200</td></tr>
            <tr><td>Nodes (abuse rail)</td><td>2</td><td>10</td><td>Unlimited</td></tr>
            <tr><td>Team seats</td><td>2</td><td>10</td><td>20</td></tr>
            <tr><td>Live dashboard connections (SSE)</td><td>10</td><td>30</td><td>100</td></tr>
            <tr><td>Log retention</td><td>30 days</td><td>90 days</td><td>365 days</td></tr>
            <tr><td>Live streaming + recording</td><td>Yes</td><td>Yes</td><td>Yes</td></tr>
            <tr><td>Local recording to CloudNode (unmetered)</td><td>Yes</td><td>Yes</td><td>Yes</td></tr>
            <tr><td>Snapshots</td><td>Yes</td><td>Yes</td><td>Yes</td></tr>
            <tr><td>Camera groups</td><td>Yes</td><td>Yes</td><td>Yes</td></tr>
            <tr><td>Admin dashboard + stream analytics</td><td>—</td><td>Yes</td><td>Yes</td></tr>
            <tr><td>Full organization reset (GDPR Article 17 right-to-erasure)</td><td>Yes</td><td>Yes</td><td>Yes</td></tr>
            <tr><td>Selective log wipe (stream + MCP activity, keep org running)</td><td>—</td><td>Yes</td><td>Yes</td></tr>
            <tr><td>MCP integration</td><td>—</td><td>Yes</td><td>Yes</td></tr>
            <tr><td>MCP rate limit (per key)</td><td>—</td><td>30 / min · 5,000 / day</td><td>120 / min · 30,000 / day</td></tr>
            <tr><td><a href="#sentinel">Sentinel AI agent</a></td><td>—</td><td><strong>100 runs / month</strong></td><td><strong>500 runs / month</strong></td></tr>
            <tr><td>Priority support</td><td>—</td><td>—</td><td>Best-effort priority</td></tr>
          </tbody>
        </table>
      </div>

      <h3>What counts as a viewer-hour</h3>
      <p>
        One viewer-hour = one hour of live HLS video served to an
        authenticated browser session. The counter increments by
        1 for every segment our backend serves (segments are ~1 second
        each), so a minute of live playback costs ~1/60th of an hour.
      </p>
      <ul>
        <li><strong>Counts:</strong> live playback from <code>GET /stream.m3u8</code>, including background dashboard tabs that keep polling segments.</li>
        <li><strong>Does not count:</strong> recordings stored locally on your CloudNode (they never touch the cloud), motion event metadata, incident snapshots shown in the dashboard, MCP tool calls.</li>
        <li><strong>When you hit the cap:</strong> segment requests return HTTP <code>429</code> with an upgrade prompt. Your cameras keep recording locally, your motion events still fire, your MCP integrations still work — only live playback to the dashboard pauses until the 1st of next month.</li>
      </ul>

      <h3>Enforcement</h3>
      <ul>
        <li><strong>Viewer-hour cap</strong> — enforced on each HLS segment serve. The dashboard shows a live usage gauge with warn/full states at 80% / 100%.</li>
        <li><strong>Camera / node limits</strong> — when a node registers and you're at your camera cap, additional cameras are skipped with HTTP 402 and a <code>plan_limit_hit</code> detail. They are preserved in the database (soft-disable, not deletion) so upgrading restores them instantly.</li>
        <li><strong>SSE connection cap</strong> — per-org concurrent live-dashboard connections are capped per tier. Hitting the cap returns HTTP 429; close unused tabs or upgrade.</li>
        <li><strong>Feature gates</strong> — admin dashboard, danger zone, MCP, and Sentinel AI all require Pro or Pro Plus.</li>
        <li><strong>MCP rate limits</strong> — enforced per API key as a sliding window: a per-minute cap for burst control and a 24-hour cap that catches runaway automation loops.</li>
        <li><strong>Sentinel AI run cap</strong> — enforced on dispatch. Hits the cap → motion + incident_opened triggers stop firing new runs and the manual "Run now" button returns 429 until the 1st of next month. One run = one investigation regardless of how many tool calls it took. See <a href="#sentinel">Sentinel AI</a>.</li>
        <li><strong>Log retention</strong> — stream access logs, MCP activity, audit logs, motion events, and notifications are automatically deleted after the per-tier retention window (a nightly cleanup task iterates orgs and applies each org's tier).</li>
      </ul>

      <h3>Past-due grace period</h3>
      <p>
        When a payment fails, your account enters a 7-day grace period
        during which retries happen automatically and your service
        keeps running at full tier. After 7 days without a successful
        payment, cameras beyond the Free-tier cap are suspended and
        you're rebased to Free-tier viewer-hours. Updating your card
        resumes everything immediately. The dashboard shows a live
        countdown while you're in the grace window.
      </p>

      <p>Manage your subscription from <strong>Settings &gt; Subscription</strong> or the <Link to="/pricing">Pricing</Link> page.</p>
    </section>
  )
}

export default Plans
