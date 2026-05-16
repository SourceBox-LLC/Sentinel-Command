import { useDocs } from "./context"


function MotionDetection() {
  const { copyToClipboard } = useDocs()

  return (
    <section className="docs-section" id="motion-detection">
      <h2>Motion Detection<a href="#motion-detection" className="docs-anchor">#</a></h2>
      <p>
        Motion detection is built into CloudNode — no extra service, no external API
        calls. Every camera runs a second FFmpeg process in parallel that scores how
        much each frame differs from the previous one; above-threshold frames fire a
        <code>motion_detected</code> event.
      </p>
      <div className="docs-callout docs-callout-info">
        <p>
          <span className="docs-callout-icon">🛡️</span>
          <span>
            On Pro / Pro Plus orgs with <a href="#sentinel">Sentinel AI</a>{" "}
            configured, motion events also dispatch an autonomous AI
            investigation — the agent looks at the camera, decides whether
            the scene warrants attention, and (if so) files an incident with
            snapshot evidence. Sentinel AI has its own per-camera cooldown that's
            separate from the FFmpeg-level one below.
          </span>
        </p>
      </div>

      <h3>How it works</h3>
      <ol>
        <li>A lightweight FFmpeg probe runs alongside the HLS encoder for each camera</li>
        <li>It uses the <code>select='gt(scene,THRESHOLD)'</code> filter to emit a scene-change score per frame, between 0.0 (identical) and 1.0 (totally different)</li>
        <li>When a frame's score crosses your threshold, CloudNode raises a <code>MotionEvent</code></li>
        <li>The event is sent over the persistent WebSocket to Command Center. If the socket is down, it falls back to <code>POST /api/cameras/{"{id}"}/motion</code></li>
        <li>A per-camera cooldown timer prevents flapping (identical wind-blown tree, flickering light) from spamming events</li>
      </ol>
      <figure className="docs-diagram">
        <picture>
          <source srcSet="/images/motion-fsm.webp" type="image/webp" />
          <img
            src="/images/motion-fsm.jpg"
            alt="Motion detection state machine: Idle, Scoring, Fire event, Cooldown — looping clockwise. Side panel labelled Delivery shows three branches: primary (WebSocket — low latency), fallback (POST /cameras/{id}/motion), and consumed by (dashboard + MCP agents)."
            className="docs-diagram-image"
            width="1920"
            height="1080"
            loading="lazy"
          />
        </picture>
        <figcaption className="docs-diagram-caption">
          The state machine runs once per camera. The cooldown prevents a waving branch or flickering light from hammering the events channel — tune the threshold to control sensitivity, the cooldown to control chatter.
        </figcaption>
      </figure>

      <h3>Configuration</h3>
      <div className="docs-plans-table">
        <table>
          <thead>
            <tr><th>Field</th><th>Default</th><th>Meaning</th></tr>
          </thead>
          <tbody>
            <tr><td><code>motion.enabled</code></td><td><code>true</code></td><td>Toggle motion detection on/off</td></tr>
            <tr><td><code>motion.threshold</code></td><td><code>0.02</code></td><td>Scene-change score threshold. Lower = more sensitive.</td></tr>
            <tr><td><code>motion.cooldown_secs</code></td><td><code>30</code></td><td>Minimum seconds between events per camera</td></tr>
          </tbody>
        </table>
      </div>

      <h3>Tuning the threshold</h3>
      <ul>
        <li><strong>0.01–0.02 (default)</strong> — general-purpose indoor rooms and porches. Catches a person walking through frame.</li>
        <li><strong>0.03–0.05</strong> — outdoor scenes with wind or foliage. Ignores minor sway.</li>
        <li><strong>0.001–0.005</strong> — dim scenes with low contrast. Detects subtle changes — at the cost of noisier events.</li>
      </ul>
      <p>
        Watch the dashboard log — it prints <code>Motion detected on CAMERA (score N%)</code>
        every time an event fires. If you're getting too many, raise the threshold.
        Getting none when something clearly moved? Lower it.
      </p>

      <h3>Event payload</h3>
      <p>The event sent over WebSocket (or HTTP fallback) looks like:</p>
      <div className="docs-code-block">
        <code>{`{
  "command": "motion_detected",
  "camera_id": "cam_abc123",
  "score": 0.043,
  "timestamp": "2026-04-13T14:23:11Z"
}`}</code>
        <button className="docs-copy-btn" onClick={() => copyToClipboard(`{
  "command": "motion_detected",
  "camera_id": "cam_abc123",
  "score": 0.043,
  "timestamp": "2026-04-13T14:23:11Z"
}`)}>Copy</button>
      </div>

      <div className="docs-callout docs-callout-info">
        <p>
          <span className="docs-callout-icon">💡</span>
          <span>Motion events are the signal AI agents use to prioritize which camera
          to check first. Hook them into <code>create_incident</code> via MCP to auto-open
          incidents when motion fires in off-hours.</span>
        </p>
      </div>
    </section>
  )
}

export default MotionDetection
