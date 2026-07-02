import { useDocs } from "./context"


function ApiRateLimits() {
  const { copyToClipboard } = useDocs()

  return (
    <section className="docs-section" id="api-rate-limits">
      <h2>API Rate Limits<a href="#api-rate-limits" className="docs-anchor">#</a></h2>
      <p>
        Every mutating route is rate limited to protect the service from
        runaway scripts and abuse. Limits are bucketed per tenant: a
        CameraNode API key gets its own bucket, an authenticated user's
        Clerk JWT shares a bucket with the rest of their org, and
        unauthenticated callers bucket by IP. This means one noisy integrator
        can't starve other orgs.
      </p>
      <p>
        When you exceed a limit the response is HTTP <code>429</code> with the
        body shape below and a <code>Retry-After: 60</code> header:
      </p>
      <div className="docs-code-block">
        <code>{`{
  "error": "rate_limit_exceeded",
  "message": "Too many requests. Back off and retry after the Retry-After window.",
  "limit": "60 per 1 minute",
  "retry_after_seconds": 60
}`}</code>
        <button className="docs-copy-btn" onClick={() => copyToClipboard(`{
  "error": "rate_limit_exceeded",
  "message": "Too many requests. Back off and retry after the Retry-After window.",
  "limit": "60 per 1 minute",
  "retry_after_seconds": 60
}`)}>Copy</button>
      </div>

      <h3>REST API Limits</h3>
      <div className="docs-plans-table">
        <table>
          <thead>
            <tr>
              <th>Endpoint</th>
              <th>Limit</th>
              <th>Bucket</th>
            </tr>
          </thead>
          <tbody>
            <tr><td>POST <code>/api/nodes/validate</code></td><td>10 / min</td><td>Per IP / org</td></tr>
            <tr><td>POST <code>/api/nodes/register</code></td><td>10 / min</td><td>Per IP / org</td></tr>
            <tr><td>POST <code>/api/nodes/heartbeat</code></td><td>60 / min</td><td>Per node key</td></tr>
            <tr><td>POST <code>/api/nodes/{"{id}"}/rotate-key</code></td><td>5 / min</td><td>Per org</td></tr>
            <tr><td>POST <code>/api/nodes</code></td><td>20 / hour</td><td>Per org</td></tr>
            <tr><td>POST <code>/api/nodes/self/decommission</code></td><td>10 / hour</td><td>Per node key</td></tr>
            <tr><td>POST <code>/api/cameras/{"{id}"}/snapshot</code></td><td>30 / min</td><td>Per org</td></tr>
            <tr><td>POST <code>/api/cameras/{"{id}"}/recording</code></td><td>30 / min</td><td>Per org</td></tr>
            <tr><td>POST <code>/api/cameras/{"{id}"}/push-segment</code></td><td>1200 / min</td><td>Per node key</td></tr>
            <tr><td>POST <code>/api/cameras/{"{id}"}/playlist</code></td><td>600 / min</td><td>Per node key</td></tr>
            <tr><td>POST <code>/api/cameras/{"{id}"}/motion</code></td><td>120 / min</td><td>Per node key</td></tr>
            <tr><td>POST <code>/api/cameras/{"{id}"}/codec</code></td><td>30 / min</td><td>Per node key</td></tr>
            <tr><td>GET <code>/api/incidents</code></td><td>120 / min</td><td>Per org</td></tr>
            <tr><td>PATCH <code>/api/incidents/{"{id}"}</code></td><td>120 / min</td><td>Per org</td></tr>
            <tr><td>DELETE <code>/api/incidents/{"{id}"}</code></td><td>60 / min</td><td>Per org</td></tr>
            <tr><td>POST <code>/api/mcp/keys</code></td><td>10 / hour</td><td>Per org</td></tr>
            <tr><td>DELETE <code>/api/mcp/keys/{"{id}"}</code></td><td>30 / hour</td><td>Per org</td></tr>
            <tr><td>DELETE <code>/api/camera-groups/{"{id}"}</code></td><td>60 / min</td><td>Per org</td></tr>
            <tr><td>POST <code>/api/webhooks/clerk</code></td><td>120 / min</td><td>Per IP (Svix-signed)</td></tr>
            <tr><td>GET live stream and segment proxies</td><td>Unlimited (read-path)</td><td>Capped by viewer-hours</td></tr>
          </tbody>
        </table>
      </div>

      <h3>Viewer-hour cap (HLS live playback)</h3>
      <p>
        Live-segment reads are not per-request rate limited, but every
        successful segment delivery counts against your monthly
        viewer-hour cap. When you hit the cap, subsequent segment
        requests return HTTP <code>429</code> with <code>Retry-After: 3600</code>
        and a body explaining when the cap resets. Caps:
      </p>
      <div className="docs-plans-table">
        <table>
          <thead>
            <tr><th>Plan</th><th>Viewer-hours / month</th></tr>
          </thead>
          <tbody>
            <tr><td>Free</td><td>30</td></tr>
            <tr><td>Pro</td><td>300</td></tr>
            <tr><td>Pro Plus</td><td>1,500</td></tr>
          </tbody>
        </table>
      </div>

      <h3>MCP tool-call limits</h3>
      <p>
        MCP keys have a separate budget on top of the REST limits.
        Buckets are per-key, so revoking and re-issuing a key starts
        fresh. Two windows run in parallel: a per-minute cap for burst
        protection and a 24-hour cap that catches runaway automations.
        The 429 response body's <code>breach</code> field tells you which
        window you tripped.
      </p>
      <div className="docs-plans-table">
        <table>
          <thead>
            <tr><th>Plan</th><th>Per minute</th><th>Per 24 hours</th></tr>
          </thead>
          <tbody>
            <tr><td>Free</td><td colSpan="2">— (MCP not available)</td></tr>
            <tr><td>Pro</td><td>30</td><td>5,000</td></tr>
            <tr><td>Pro Plus</td><td>120</td><td>30,000</td></tr>
          </tbody>
        </table>
      </div>

      <h3>SSE subscriber caps (per org)</h3>
      <p>
        Concurrent live-dashboard connections are capped per tier per
        channel (motion, notifications, MCP activity). Hitting the cap
        returns 429 with a detail that includes your current cap;
        close unused browser tabs to free a slot.
      </p>
      <div className="docs-plans-table">
        <table>
          <thead>
            <tr><th>Plan</th><th>Concurrent SSE / org</th></tr>
          </thead>
          <tbody>
            <tr><td>Free</td><td>10</td></tr>
            <tr><td>Pro</td><td>30</td></tr>
            <tr><td>Pro Plus</td><td>100</td></tr>
          </tbody>
        </table>
      </div>

      <h3>Other caps</h3>
      <ul>
        <li><strong>Segment payload size</strong> — <code>push-segment</code> uploads are capped at 2 MiB per segment. Oversized segments return HTTP 400.</li>
        <li><strong>Pagination</strong> — <code>limit</code> is capped per endpoint (200 for incidents and notifications, 500 for motion events, 500 for audit logs). <code>offset</code> is capped at 1,000,000 on every paginated endpoint because large offsets force a full table scan.</li>
        <li><strong>Plan resolution</strong> — live Clerk lookups are throttled to one every 60 seconds per org to bound our billing-API spend; cached plan values are always served in between.</li>
      </ul>

      <h3>Scaling past the defaults</h3>
      <p>
        If a legitimate workload is bumping into these limits (e.g. an
        integration that needs a higher MCP budget or burst headroom on
        heartbeat), email SourceBox LLC via the <a href="https://github.com/SourceBox-LLC" target="_blank" rel="noopener noreferrer">GitHub org page</a>.
        We'd rather raise your bucket than have you build retry jitter.
      </p>
    </section>
  )
}

export default ApiRateLimits
