function Architecture() {
  return (
    <section className="docs-section" id="architecture">
      <h2>Architecture<a href="#architecture" className="docs-anchor">#</a></h2>
      <p>SourceBox Sentry uses a cloud-first architecture designed for simplicity and security.</p>

      <h3>Data Flow</h3>
      <figure className="docs-diagram">
        <picture>
          <source srcSet="/images/system-architecture.webp" type="image/webp" />
          <img
            src="/images/system-architecture.jpg"
            alt="Three-zone system architecture: LOCAL (USB Camera and CloudNode), CLOUD (Command Center, Segment RAM Cache, Incident DB), CLIENT (Browser, AI Agent over MCP). HTTPS push from CloudNode to Command Center. Same-origin streaming to Browser. Outbound only — no inbound ports."
            className="docs-diagram-image"
            width="1920"
            height="1080"
            loading="lazy"
          />
        </picture>
        <figcaption className="docs-diagram-caption">
          The live-video path runs entirely inside the authenticated backend — CloudNode pushes outbound, the browser fetches same-origin. No third-party object storage in the hot path.
        </figcaption>
      </figure>

      <h3>How It Works</h3>
      <ol>
        <li><strong>CloudNode</strong> captures video from USB cameras using FFmpeg</li>
        <li>Video is encoded as HLS segments (1-second chunks by default) and pushed directly to the Command Center over authenticated HTTPS</li>
        <li>The <strong>Command Center</strong> caches the most recent segments in RAM and serves them to authorized viewers same-origin</li>
        <li>Viewers watch via HLS through the Command Center backend — no third-party storage in the live video path, no direct connection to your network</li>
      </ol>

      <h3>HLS Segment Pipeline</h3>
      <p>
        Zooming into the streaming path: each camera runs two FFmpeg processes
        in parallel — one producing playable HLS segments, a second probing
        scene changes for motion events. Playback is served same-origin from
        the backend's RAM cache, never through object storage.
      </p>
      <figure className="docs-diagram">
        <picture>
          <source srcSet="/images/hls-pipeline.webp" type="image/webp" />
          <img
            src="/images/hls-pipeline.jpg"
            alt="HLS segment pipeline. CloudNode lane: Camera → FFmpeg → HLS segments → Segment uploader. Parallel motion branch: Motion probe → scene-change score → WebSocket event. Cloud lane: Segment RAM cache → Same-origin proxy with a ~15 segments rolling-window pill. Client lane: hls.js player fetched via GET .ts."
            className="docs-diagram-image"
            width="1920"
            height="1080"
            loading="lazy"
          />
        </picture>
        <figcaption className="docs-diagram-caption">
          Each camera runs two FFmpeg processes: the encoder producing HLS segments, and a second probe scoring scene changes for motion events. Playback is served from the RAM cache same-origin, never through object storage.
        </figcaption>
      </figure>

      <h3>Sentinel agent (optional, Pro+)</h3>
      <p>
        On Pro and Pro Plus, an additional surface joins the architecture:{" "}
        <a href="#sentinel">Sentinel</a>, a webhook-driven serverless AI agent
        that auto-investigates motion and incident_opened events.
      </p>
      <ol>
        <li>A configured trigger fires (motion / incident_opened / manual).</li>
        <li>Command Center inserts a <code>sentinel_runs</code> row and POSTs an HMAC-signed wakeup webhook to the Sentinel agent on Fly.io.</li>
        <li>The agent boots if it was sleeping, drains all pending runs across all orgs, and processes each via an LLM ↔ MCP tool loop. Per-call org scoping happens server-side via the <code>X-Agent-Org-Override</code> header — one deployed agent serves every org with no cross-tenant state.</li>
        <li>Each run posts back via <code>/api/sentinel/runs/&#123;id&#125;/complete</code>. The agent returns 200 and Fly auto-stops the machine after the idle window — no idle billing between events.</li>
      </ol>
      <p>
        Per-run isolation comes from a fresh MCP client + fresh messages array
        per run. See <a href="#sentinel">the Sentinel section</a> for the full
        configuration surface, time bounds, and reliability layers.
      </p>

      <h3>Security Model</h3>
      <p>
        Every request crosses four layers of protection. TLS on the wire, an
        authenticated identity at the edge, hashing or encryption wherever
        data is stored, and tenant isolation all the way down to the database
        query.
      </p>
      <figure className="docs-diagram">
        <picture>
          <source srcSet="/images/security-model.webp" type="image/webp" />
          <img
            src="/images/security-model.jpg"
            alt="Security model — four concentric rings: TRANSPORT (TLS 1.2+, outbound-only CloudNode → cloud), AUTH (Clerk JWT for users, nak_* for CloudNode keys, osc_* for MCP agent keys), DATA (SHA-256 hashed API keys, AES-256-GCM creds on CloudNode, live video in RAM only), TENANT isolation (every row scoped to org_id, no cross-org reads, MCP scope filters per key)."
            className="docs-diagram-image"
            width="1920"
            height="1080"
            loading="lazy"
          />
        </picture>
        <figcaption className="docs-diagram-caption">
          Every call crosses every ring: TLS on the wire, a key or JWT at the edge, hashing or encryption wherever data lives, and org-scoped queries all the way down.
        </figcaption>
      </figure>
      <ul>
        <li><strong>Outbound Only</strong> — CloudNode pushes to cloud. No inbound ports, no router config.</li>
        <li><strong>Same-origin Streaming</strong> — Live segments are served through the authenticated backend, not a third-party object store.</li>
        <li><strong>API Key Auth</strong> — Node keys stored as SHA-256 hashes. Never stored in plaintext.</li>
        <li><strong>Clerk Organizations</strong> — Multi-tenant auth with admin and member roles.</li>
        <li><strong>HTTPS Everywhere</strong> — All traffic between CloudNode, Command Center, and viewers is encrypted.</li>
        <li><strong>MCP Keys</strong> — Separate API keys for MCP access, also SHA-256 hashed and org-scoped.</li>
      </ul>
    </section>
  )
}

export default Architecture
