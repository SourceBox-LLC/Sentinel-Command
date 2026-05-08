function Faq() {
  return (
    <section className="docs-section" id="faq">
      <h2>FAQ<a href="#faq" className="docs-anchor">#</a></h2>

      <h3>Does SourceBox Sentry record audio?</h3>
      <p>
        If a camera's input has audio and its codec is supported (AAC, Opus, MP3),
        CloudNode passes it through the HLS pipeline and it's available during live
        playback. Recordings stored on the node include audio. There is no per-camera
        "mute" toggle yet — remove or mute the input source if you need silent-only.
      </p>

      <h3>How do I install CloudNode on Windows?</h3>
      <p>
        Download <code>sourcebox-sentry-cloudnode-windows-x86_64.msi</code> from the{" "}
        <a href="https://github.com/SourceBox-LLC/opensentry-cloud-node/releases/latest" target="_blank" rel="noopener noreferrer">latest GitHub release</a>{" "}
        and run it. The MSI is unsigned today, so SmartScreen will warn "Windows protected
        your PC" on first run — click <strong>More info → Run anyway</strong>. After install,
        click the <strong>SourceBox Sentry CloudNode</strong> shortcut from the Start menu —
        first launch runs the setup wizard, every launch after streams cameras directly. For
        24/7 unattended operation, the MSI also registers an optional Windows Service named{" "}
        <code>SourceBoxSentryCloudNode</code> that you can flip to auto-start; see the{" "}
        <a href="#cloudnode-setup">CloudNode Setup</a> section for details. There is no
        PowerShell one-liner installer — that path was retired in v0.1.31 because the MSI
        is the only Windows install that handles upgrades and Add/Remove Programs cleanly.
      </p>

      <h3>Can I use IP cameras (RTSP) instead of USB?</h3>
      <p>
        Not today. CloudNode currently only supports USB cameras via each platform's
        native API (Video4Linux2, DirectShow, AVFoundation). RTSP / ONVIF support is on
        the roadmap — if you have a strong need, open an issue on the CloudNode repo so
        we can prioritize.
      </p>

      <h3>How much bandwidth does a camera use?</h3>
      <p>
        Roughly 1–3 Mbps per 1080p camera at the default encoder settings. Multiply by
        camera count for your egress budget. Local recordings don't add to bandwidth —
        only live viewing via Command Center does.
      </p>

      <h3>Will I get email or SMS alerts when something happens?</h3>
      <p>
        <strong>Email: yes</strong> for the operator-critical events — camera
        offline + recovered, CloudNode offline + recovered, AI-agent-created
        incidents, MCP API key audit (created or revoked), CloudNode disk
        almost full, member audit (added / role changed / removed), and
        motion detection (with cooldown + digest).  Each kind is opt-in
        per-org via the{" "}
        <a href="/settings#settings-notifications">notification settings page</a>.
        Six default ON; <strong>motion defaults OFF</strong> and must be
        opted in (per-org volume variance is too wide for a safe default-on).
        Motion uses a per-camera cooldown window — one immediate "first motion"
        email plus at most one digest summary per ~15 minutes per camera, no
        matter how many events fire.
      </p>
      <p>
        <strong>Motion-event emails ship in v1.1 with cooldown + digest mode</strong>{" "}
        — per-camera cooldown (15 min default) caps volume to at most 2 emails per
        cycle per camera regardless of event count: one immediate "first motion"
        alert, plus an optional digest summary if more events landed during the
        window. Default OFF to protect sender reputation against high-volume
        outdoor cameras; opt in via the notification settings page if you want it.
      </p>
      <p>
        <strong>SMS and mobile push: not built in.</strong> Point an MCP agent
        (Claude, Cursor, your own) at the motion-events stream and route through
        Twilio, PagerDuty, or whatever you already use. Every plan has full MCP
        access. See the <a href="#notifications">Notifications</a> section below
        for the full list of triggers.
      </p>

      <h3>Does CloudNode need always-on internet?</h3>
      <p>
        For live streaming to Command Center, yes — segments are pushed as they're
        produced. If the internet drops, the node continues recording locally (if
        recording is enabled) and backfills motion events over the HTTP fallback once
        connectivity returns. Live playback resumes automatically on reconnect.
      </p>

      <h3>Is my video data secure?</h3>
      <ul>
        <li>All traffic between CloudNode, Command Center, and your browser is TLS-encrypted</li>
        <li>Node API keys are stored as SHA-256 hashes server-side and encrypted at rest on the node (AES-256-GCM, machine-derived key)</li>
        <li>Live segments are cached in Command Center RAM for a rolling ~60s window, then evicted — no long-term cloud storage</li>
        <li>Recordings and snapshots live only on your node, in an encrypted SQLite DB</li>
        <li>Every authenticated request is logged for audit</li>
      </ul>

      <h3>Why is the Command Center source public?</h3>
      <p>
        Command Center is a SaaS we host and operate — you sign up, we run it.
        We publish the source at <a href="https://github.com/SourceBox-LLC/OpenSentry-Command" target="_blank" rel="noopener noreferrer">github.com/SourceBox-LLC/OpenSentry-Command</a>{" "}
        under AGPL-3.0 for transparency: every claim on the security and
        privacy pages points at the file that implements it, and a customer
        or auditor can verify what we actually do — no analytics, no cloud ML
        on your video, recordings encrypted at rest on hardware you own.
        The piece designed to run on your premises is the CloudNode (GPL-3.0);
        the Command Center is operated by us.
      </p>

      <h3>What's Sentinel and how does the run cap work?</h3>
      <p>
        Sentinel is the optional AI agent that auto-investigates motion events
        and incident_opened notifications — it views the camera, decides whether
        what it sees warrants attention, files an incident report with snapshot
        evidence, and writes a summary. One "run" = one investigation,
        regardless of how many tool calls it took. Pro: 100 runs/month. Pro
        Plus: 500 runs/month. Caps reset on the 1st of each calendar month.
        When you hit the cap, dispatch pauses for the rest of the month — your
        recordings, motion alerts, and dashboard playback all keep working as
        normal. See the <a href="#sentinel">Sentinel section</a>.
      </p>

      <h3>Does Sentinel send my footage to a cloud LLM?</h3>
      <p>
        Yes — only when it fires, only the snapshots it actively investigates.
        Sentinel passes a JPEG from <code>view_camera</code> to the configured
        LLM endpoint (Ollama Cloud by default) so the model can see what
        triggered the run. The persistent video archive (your recordings) still
        stays on your CloudNode and never syncs to our cloud. If you don't want
        any footage leaving your hardware, leave Sentinel disabled — motion
        detection, recording, and notifications all work without it.
      </p>

      <h3>How is Sentinel scoped per camera and per time-of-day?</h3>
      <p>
        From the <a href="/sentinel">Sentinel page</a>: per-camera include /
        exclude (cameras default to in-scope so newly-added cameras aren't
        silently skipped), a per-camera motion cooldown (default 5 min), and a
        schedule mode — <em>always</em>, <em>scheduled</em> (HH:MM window on
        selected days, in your org's timezone, wrap-around supported), or{" "}
        <em>off</em>. The manual "Run now" button skips the schedule + scope
        checks but still counts against the monthly cap.
      </p>

      <h3>Which MCP clients does SourceBox Sentry work with?</h3>
      <p>
        Any MCP client that supports the streamable-HTTP transport. Tested with Claude
        Code, Cursor, and custom agents built on Anthropic's Agent SDK. ChatGPT and
        other clients that only support the stdio transport require a local proxy (we
        don't currently document one).
      </p>

      <h3>Can I move a node to a different machine?</h3>
      <p>
        The node database is bound to the host machine (the AES key is derived from
        hostname). To move a node: on the new machine, run <code>sourcebox-sentry-cloudnode setup</code>
        with the same <code>node_id</code> and API key — Command Center will re-associate
        the cameras to the new host. The old node should be stopped first to avoid a
        split-brain heartbeat.
      </p>

      <h3>How do I reset a node's credentials?</h3>
      <p>
        From the terminal dashboard, go to <code>/settings</code> and run
        <code>/reauth confirm</code>. This clears the stored API key and restarts the
        setup wizard. Use <code>/wipe confirm</code> to additionally delete all
        recordings and snapshots.
      </p>

      <h3>Do you offer an SLA?</h3>
      <p>
        Not on Free or Pro. Pro Plus plan customers get best-effort priority support.
        For enterprise agreements with an SLA, email us via the GitHub org page.
      </p>

      <h3>What license is SourceBox Sentry under?</h3>
      <p>
        Command Center is AGPL-3.0 and CloudNode is GPL-3.0 — both public so
        you can audit what we do with your data. CloudNode is the piece that
        runs on your hardware; Command Center is the cloud service we operate.
        For commercial licensing that avoids the copyleft obligations,
        contact <a href="https://github.com/SourceBox-LLC" target="_blank" rel="noopener noreferrer">SourceBox LLC</a>.
      </p>
    </section>
  )
}

export default Faq
