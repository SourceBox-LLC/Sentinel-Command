import { PricingTable } from "@clerk/clerk-react"

function PricingPage() {
  return (
    <div className="pricing-page">
      <div className="pricing-glow pricing-glow-1"></div>
      <div className="pricing-glow pricing-glow-2"></div>

      <div className="pricing-hero">
        <div className="pricing-badge">PRICING</div>
        <h1 className="pricing-title">
          Connect as many cameras as you need.<br />
          <span className="pricing-title-accent">Pay for how much you actually watch.</span>
        </h1>
        <p className="pricing-subtitle">
          Your monthly viewer-hours are the real tier differentiator — not
          how many cameras you plug in. Recording to your CloudNode is
          always local and never counts against your cap.
        </p>
      </div>

      <div className="pricing-table-wrapper">
        <PricingTable for="organization" />
      </div>

      <p className="pricing-detail-footnote">
        Need higher caps? If you legitimately need more than 200 cameras or
        1,500 viewer-hours per month, email{" "}
        <a href="https://github.com/SourceBox-LLC" target="_blank" rel="noopener noreferrer">
          SourceBox LLC
        </a>{" "}
        — we'd rather raise your bucket than lose a real customer to an
        arbitrary ceiling.
      </p>

      <div className="pricing-features">
        <h2 className="pricing-features-title">How usage-based pricing works</h2>
        <div className="pricing-features-grid">
          <div className="pricing-feature-item">
            <div className="pricing-feature-icon">⏱️</div>
            <h3>Viewer-hours, not cameras</h3>
            <p>Every second of live video played to an authenticated viewer counts. A camera you never watch costs nothing against your cap.</p>
          </div>
          <div className="pricing-feature-item">
            <div className="pricing-feature-icon">💾</div>
            <h3>Recordings don't count</h3>
            <p>Local recording to your CloudNode's encrypted SQLite is unlimited and free. Only cloud-served live playback is metered.</p>
          </div>
          <div className="pricing-feature-item">
            <div className="pricing-feature-icon">📊</div>
            <h3>Live usage display</h3>
            <p>See exactly how many hours you've used this month on the dashboard. No surprises, no overage billing — we cap, we don't charge extra.</p>
          </div>
          <div className="pricing-feature-item">
            <div className="pricing-feature-icon">🔐</div>
            <h3>Encrypted end to end to disk</h3>
            <p>TLS in flight, AES-256-GCM on the CloudNode at rest. A stolen drive is unreadable elsewhere.</p>
          </div>
          <div className="pricing-feature-item">
            <div className="pricing-feature-icon">🚫</div>
            <h3>No analytics, no trackers</h3>
            <p>No Mixpanel, no Segment, no ad networks, no data brokers. Verifiable with a grep of our open source.</p>
          </div>
          <div className="pricing-feature-item">
            <div className="pricing-feature-icon">🖥️</div>
            <h3>Runs on your hardware</h3>
            <p>CloudNode (GPL-3) installs on any Linux, macOS, or Windows machine. Use a Pi, a NUC, or an old laptop.</p>
          </div>
          <div className="pricing-feature-item">
            <div className="pricing-feature-icon">🛡️</div>
            <h3>Sentinel AI agent</h3>
            <p>Vision-capable AI investigates motion events and incidents, files reports with snapshot evidence, and updates status. Pro: 100 runs/month. Pro Plus: 500 runs/month.</p>
          </div>
        </div>
      </div>

      <div className="pricing-faq">
        <h2 className="pricing-faq-title">Common questions</h2>
        <div className="pricing-faq-grid">
          <div className="pricing-faq-item">
            <h3>What counts as a "viewer-hour"?</h3>
            <p>One viewer-hour = one hour of live video played to an authenticated browser session. Background tabs that keep pulling segments count; idle cameras with no one watching don't. Recordings stored on your CloudNode are unlimited and free — they never count against your cap.</p>
          </div>
          <div className="pricing-faq-item">
            <h3>What happens when I hit my viewer-hour cap?</h3>
            <p>Live playback pauses with an upgrade prompt until the next calendar month begins. Your cameras keep recording locally, your motion events still fire, and your CloudNode keeps running. You just can't stream video live to the dashboard until your cap resets or you upgrade.</p>
          </div>
          <div className="pricing-faq-item">
            <h3>Can I upgrade or downgrade anytime?</h3>
            <p>Yes. Upgrades take effect immediately. Downgrades apply at the end of the current billing period. Annual plans save roughly 17% versus the monthly price.</p>
          </div>
          <div className="pricing-faq-item">
            <h3>Do you bill for overage?</h3>
            <p>No. Your monthly bill is exactly the plan price, always. When you hit a cap we pause the metered feature (live playback or MCP calls) rather than surprise-charge you.</p>
          </div>
          <div className="pricing-faq-item">
            <h3>What if my payment fails?</h3>
            <p>Your account enters a 7-day grace period during which the charge is retried automatically. Your cameras keep streaming throughout. After 7 days without a successful payment, cameras beyond the Free-tier limit are suspended and you're rebased to Free-tier viewer-hours. Updating your card resumes everything immediately.</p>
          </div>
          <div className="pricing-faq-item">
            <h3>Can I get a refund?</h3>
            <p>You can cancel anytime and keep your paid features through the end of the period you've already paid for. Fees are billed in advance and are otherwise non-refundable — we don't pro-rate unused time on a voluntary cancellation. We always honor refunds required by law, including the EU/UK consumer right of withdrawal, and we'll make it right at our discretion for things like a confirmed extended outage. Full details are in the <a href="/legal/terms">Terms</a> (Section 6). Reach out before starting a chargeback and we'll sort it directly.</p>
          </div>
          <div className="pricing-faq-item">
            <h3>Why are camera counts still capped?</h3>
            <p>They're abuse rails, not product tiers. Every connected camera continuously pushes segments to our cache even when idle, which drives our backend load. The caps (5 / 25 / 200) are well above what any realistic customer needs. If you legitimately need more, email us and we'll raise yours.</p>
          </div>
          <div className="pricing-faq-item">
            <h3>Is the CloudNode software free?</h3>
            <p>Yes, always. CloudNode is open source (GPL-3) and runs on your own hardware — the only thing you pay for is the Command Center cloud service we operate. The Command Center source is also public (AGPL-3) so you can read and audit exactly what runs on the cloud side; you don't need to run it yourself.</p>
          </div>
          <div className="pricing-faq-item">
            <h3>What's Sentinel and how does the run cap work?</h3>
            <p>Sentinel is the optional AI agent that investigates motion events and incidents on your behalf — it views the camera, decides whether what it sees warrants attention, files an incident report with snapshot evidence, and writes a long-form summary. One "run" = one investigation, regardless of how many tool calls it took. Pro includes 100 runs/month, Pro Plus includes 500 runs/month, and the cap resets on the 1st of each calendar month. There's no overage billing — when you hit the cap, dispatch pauses for the rest of the month and your existing recordings, motion alerts, and dashboard keep working as normal. Note: Sentinel is the one feature that uses cloud AI — when it investigates, it sends camera snapshots to our LLM provider (Ollama Cloud) to analyze the scene. It's opt-in per organization; if you don't use it, no imagery leaves your devices. See the <a href="/security">Security page</a> for details.</p>
          </div>
          <div className="pricing-faq-item">
            <h3>Do you send email or SMS alerts?</h3>
            <p>Yes for email — every plan gets opt-in email alerts for the operator-critical events: camera offline + recovered, CloudNode offline + recovered, AI-agent-created incidents, MCP API key audit, CloudNode disk almost full, member audit (added / role-changed / removed), and motion detection with cooldown + digest. Toggle each kind on or off per-org in your notification settings. Motion defaults OFF (per-org volume varies wildly); when enabled, the first motion event per camera fires immediately and any additional events in the next 15 minutes are summarised in a single digest. SMS and mobile push remain MCP-only — wire your agent to Twilio, PagerDuty, or whatever you already use.</p>
          </div>
        </div>
      </div>
    </div>
  )
}

export default PricingPage
