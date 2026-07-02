function Notifications() {
  return (
    <section className="docs-section" id="notifications">
      <h2>Notifications<a href="#notifications" className="docs-anchor">#</a></h2>
      <p>
        Sentinel raises events for operational changes (nodes going offline, cameras
        dropping off) and motion activity. Each event flows through three channels:
        the in-app bell-icon panel, the email side-channel (opt-in per kind), and
        the MCP tool activity log.
      </p>

      <figure className="docs-diagram">
        <picture>
          <source srcSet="/images/notifications-fanout.webp" type="image/webp" />
          <img
            src="/images/notifications-fanout.jpg"
            alt="Notifications fan-out: a single platform event passes through the audience filter (admin-only vs all members), then fans out to five destinations — In-app inbox (SSE, real-time), Email (Resend, with per-org opt-in, bounce suppression, and 15-minute motion cooldown), the Incident Reports page at /incidents (when filed as incident), the MCP activity log (admin only), and the Sentinel AI (Pro/Pro Plus only, on motion + incident_created kinds). Motion digest behavior is surfaced near the email channel: 1 immediate + 1 summary, capped at 2 per cycle per camera. Legend in the top-right groups channels by role."
            className="docs-diagram-image"
            width="2752"
            height="1536"
            loading="lazy"
          />
        </picture>
        <figcaption className="docs-diagram-caption">
          One event, five destinations. The audience filter decides whether the event goes to admins only or to every member. The email gate adds per-org opt-in, bounce suppression, and motion-event cooldown — so the in-app inbox can be loud while email volume stays bounded. The Sentinel AI branch only fires for Pro/Pro Plus orgs that have configured the agent, on motion or incident_created kinds.
        </figcaption>
      </figure>

      <div className="docs-callout docs-callout-info">
        <p>
          <span className="docs-callout-icon">🛡️</span>
          <span>
            On Pro / Pro Plus orgs with the <a href="#sentinel">Sentinel AI</a>{" "}
            configured, two of the kinds below — <strong>motion</strong> and{" "}
            <strong>incident_created</strong> — also dispatch an autonomous
            agent run that investigates the event and may file an incident
            report. Sentinel AI reads the same per-camera scope, schedule, and
            cooldown you set on the Sentinel AI page, independent of these email
            toggles.
          </span>
        </p>
      </div>

      <h3>What triggers a notification</h3>
      <ul>
        <li><strong>Node offline</strong> — Command Center hasn't received a heartbeat from a node for 90 seconds.</li>
        <li><strong>Node recovered</strong> — A previously offline node has started heartbeating again.</li>
        <li><strong>Camera offline</strong> — A camera on an online node stopped reporting segments (cable unplugged, USB error, camera held open by another app).</li>
        <li><strong>Camera recovered</strong> — A previously offline camera started reporting segments again.</li>
        <li><strong>Motion detected</strong> — A camera's FFmpeg scene-change scorer crossed the configured threshold. See <a href="#motion-detection">Motion Detection</a>.</li>
        <li><strong>Incident opened</strong> — A human, an MCP-connected AI tool, or the <a href="#sentinel">Sentinel AI</a> filed a new incident report.</li>
        <li><strong>MCP API key created</strong> — An admin generated a new MCP API key (full programmatic access to cameras + nodes + incidents). Security-audit signal.</li>
        <li><strong>MCP API key revoked</strong> — An admin revoked an existing MCP key. Paired with the create event so the audit trail is symmetric.</li>
        <li><strong>CameraNode disk almost full</strong> — Your CameraNode hardware crossed 90% disk usage. Recordings will fail when the disk caps out. Customer-actionable (clean up files, expand storage).</li>
        <li><strong>Member added / role changed / removed</strong> — Org membership lifecycle. Catches "did someone just give themselves admin access to my cameras?" within seconds via the Clerk webhook.</li>
        <li><strong>Member promotion requested</strong> — A non-admin member clicked "Request admin access" from the dashboard. Fires to org admins so the request doesn't sit unread. Shares the Member-audit email toggle with the lifecycle events above.</li>
        <li><strong>Motion digest</strong> — When a camera with motion-email enabled accumulates additional events during the cooldown window (15 min default), a single summary fires at window close. Paired with the immediate "first motion" email so volume is bounded to 2 emails per cycle per camera.</li>
      </ul>

      <h3>Where they show up</h3>
      <ul>
        <li>
          <strong>In-app inbox</strong> — Bell-icon panel in the top nav. SSE-powered,
          updates in real time. Audience-filtered: admin-only events stay hidden from
          regular members.
        </li>
        <li>
          <strong>Email</strong> — Opt-in per setting key via{" "}
          <a href="/settings#settings-notifications">notification settings</a>. v1.1
          ships seven toggles. Six default ON for new orgs:{" "}
          <em>Camera offline / recovered</em> (gates both
          <code>camera_offline</code> and <code>camera_online</code>),{" "}
          <em>CameraNode offline / recovered</em> (gates both
          <code>node_offline</code> and <code>node_online</code>),{" "}
          <em>AI agent created an incident</em> (<code>incident_created</code>),{" "}
          <em>MCP API key audit</em> (gates both <code>mcp_key_created</code>{" "}
          and <code>mcp_key_revoked</code>),{" "}
          <em>CameraNode disk almost full</em> (<code>cameranode_disk_low</code>),
          and <em>Member audit</em> (gates <code>member_added</code>,{" "}
          <code>member_role_changed</code>, <code>member_removed</code>, and{" "}
          <code>member_promotion_requested</code>).
          The seventh — <em>Motion detection (with digest)</em> — defaults
          OFF and gates both <code>motion</code> (immediate first event per
          camera) and <code>motion_digest</code> (single summary at the end
          of the per-camera cooldown window). Default-OFF on motion is
          deliberate: per-org volume variance is too wide for a safe default
          opt-in, and aggressive day-one volume risks spam-marks that
          tank deliverability for every other email kind.
          Recipients are derived from the event's audience field — admin-only events
          email only org admins; everyone-else events email all members. Every email
          carries a one-click unsubscribe link that disables that setting for the org.
          Platform-level alerts (Command Center disk approaching full, Fly machine
          health) are operator-side concerns surfaced via{" "}
          <a href="#api-health">/api/health/detailed</a> and Sentry, not customer
          notifications.
        </li>
        <li><strong>Incident Reports page</strong> (<code>/incidents</code>) — Any notification filed as an incident appears there for triage, alongside human-filed reports.</li>
        <li><strong>MCP tool log</strong> — Admin dashboard shows every MCP call, including ones that fired on a motion event.</li>
      </ul>

      <h3>Email delivery details</h3>
      <p>
        Emails go through Resend (US-based transactional provider — see{" "}
        <a href="/legal/privacy">sub-processors</a> for the disclosure).
        The Command Center holds a small EmailOutbox per pending send; a background
        worker drains it every 5 seconds. Median time-to-inbox after the triggering
        event is under 10 seconds for the operator-critical kinds.
      </p>
      <p>
        <strong>Bounce + complaint handling:</strong> if a recipient address bounces
        or marks an email as spam, Resend webhooks tell us, we add the address to a
        local suppression list, and the worker stops sending to it. No further config
        needed.
      </p>

      <div className="docs-callout docs-callout-info">
        <p>
          <span className="docs-callout-icon">ℹ️</span>
          <span>
            <strong>Motion-event emails ship in v1.1 with cooldown + digest behavior.</strong>{" "}
            Per-camera cooldown (15 min default) caps volume to at most 2 emails per
            cycle per camera regardless of event count: one immediate "first motion"
            alert, plus an optional digest "X more motion events on Front Door" if
            additional events landed during the window. Default OFF to protect sender
            reputation against high-volume outdoor cameras; opt in via the{" "}
            <a href="/settings#settings-notifications">Email Alerts</a> section.
            For real-time external alerting (Twilio, PagerDuty, etc.), wire your
            preferred MCP agent to the motion event stream — MCP access requires a
            Pro or Pro Plus plan.
          </span>
        </p>
      </div>

      <div className="docs-callout docs-callout-info">
        <p>
          <span className="docs-callout-icon">ℹ️</span>
          <span>
            <strong>SMS and mobile push: not built in.</strong> Wire an MCP agent to
            Twilio, PagerDuty, or your existing webhook if you need them — MCP access
            requires a Pro or Pro Plus plan.
          </span>
        </p>
      </div>
    </section>
  )
}

export default Notifications
