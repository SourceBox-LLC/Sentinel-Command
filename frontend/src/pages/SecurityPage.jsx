import { Link } from "react-router-dom"

/**
 * /security — privacy & architecture page.
 *
 * Every claim on this page is backed by code you can read in our public
 * repositories. Links throughout point at the exact file/function that
 * implements the behaviour, so "you can verify" isn't a marketing slogan.
 *
 * Keep the copy honest. When you add a claim here, cite the implementing
 * commit or file. When we change behaviour, this page changes too.
 */
function SecurityPage() {
  return (
    <div className="security-page">
      <div className="security-hero">
        <div className="landing-container">
          <h1 className="security-title">
            Security & Privacy at <span className="landing-logo-text">Sentinel</span>
          </h1>
          <p className="security-subtitle">
            How Sentinel handles your video, what stays on your devices,
            and what you can independently verify in our source code.
          </p>
        </div>
      </div>

      <div className="landing-container">
        <div className="security-banner" aria-hidden="true">
          <picture>
            <source srcSet="/images/security-hero.webp" type="image/webp" />
            <img
              src="/images/security-hero.jpg"
              alt=""
              className="security-banner-image"
              width="2240"
              height="960"
              loading="lazy"
            />
          </picture>
          <div className="security-banner-caption">
            Your camera. Your CloudNode. Your loop. The cable is the longest journey your video takes.
          </div>
        </div>
      </div>

      <div className="landing-container security-body">

        {/* ── TL;DR ──────────────────────────────────────────────── */}
        <section className="security-section">
          <h2>At a glance</h2>
          <ul className="security-bullets">
            <li>
              <strong>Motion detection runs on your device.</strong> No video
              frames leave your network for cloud ML — detection uses FFmpeg's
              scene-change analysis locally on the CloudNode.
            </li>
            <li>
              <strong>Live video is cached in RAM, not stored.</strong> The
              Command Center holds roughly the last 60 seconds of HLS segments
              in memory per camera so your browser can play them back. Nothing
              is written to cloud disk or object storage.
            </li>
            <li>
              <strong>Recordings stay local to the CloudNode.</strong> If you
              enable recording, segments are stored in the node's local SQLite
              database. We never ingest your footage to the cloud.
            </li>
            <li>
              <strong>API keys and recordings are encrypted at rest on the node.</strong>
              {" "}Both are sealed with AES-256-GCM using a key derived from the
              host's OS machine ID. A stolen DB file alone can't be decrypted
              on another machine.
            </li>
            <li>
              <strong>No analytics, no ad networks, no data brokers.</strong> The
              only third parties that touch your account are Clerk (auth +
              billing), Stripe (payment, via Clerk), Fly.io (hosting), and
              optionally Sentry (error tracking, off by default).
            </li>
            <li>
              <strong>Source code is open.</strong> Command Center is AGPL-3.0;
              CloudNode is GPL-3.0. Every claim on this page points to the
              file that implements it.
            </li>
          </ul>
        </section>

        {/* ── Data flow ──────────────────────────────────────────── */}
        <section className="security-section">
          <h2>What actually travels to the cloud</h2>
          <p>
            When you watch a live camera, the CloudNode pushes 1-second HLS
            segments to the Command Center over authenticated HTTPS. The
            Command Center holds the most recent window in RAM and serves it
            same-origin to your browser. That's the whole live path.
          </p>

          <div className="security-dataflow">
            <table>
              <thead>
                <tr>
                  <th>Data</th>
                  <th>Where it lives</th>
                  <th>How long</th>
                </tr>
              </thead>
              <tbody>
                <tr>
                  <td>Live video segments (.ts files)</td>
                  <td>Command Center RAM cache</td>
                  <td>Rolling ~60-second buffer per camera. Never persisted to disk.</td>
                </tr>
                <tr>
                  <td>Recordings (if enabled)</td>
                  <td>CloudNode local SQLite, AES-256-GCM encrypted</td>
                  <td>Until you delete them or disk quota evicts the oldest.</td>
                </tr>
                <tr>
                  <td>Snapshots (if enabled)</td>
                  <td>CloudNode local SQLite, AES-256-GCM encrypted</td>
                  <td>Same as recordings — local only, retention is yours.</td>
                </tr>
                <tr>
                  <td>Motion events</td>
                  <td>Command Center database (metadata only: timestamp, score, camera)</td>
                  <td>Tiered: 30 days on Free, 90 days on Pro, 365 days on Pro Plus.</td>
                </tr>
                <tr>
                  <td>Stream access logs</td>
                  <td>Command Center database (user, IP, user agent, timestamp)</td>
                  <td>Tiered: 30 / 90 / 365 days.</td>
                </tr>
                <tr>
                  <td>Audit logs (admin actions)</td>
                  <td>Command Center database</td>
                  <td>Tiered: 30 / 90 / 365 days.</td>
                </tr>
                <tr>
                  <td>Monthly viewer-hour aggregate</td>
                  <td>Command Center database (one row per org per month: org_id, year_month, viewer_seconds)</td>
                  <td>Kept indefinitely for historical usage display; contains no personally identifiable information.</td>
                </tr>
                <tr>
                  <td>Cloud API key (held by your node)</td>
                  <td>CloudNode local SQLite, encrypted at rest</td>
                  <td>Until you rotate or decommission.</td>
                </tr>
              </tbody>
            </table>
          </div>

          <p>
            What's <em>not</em> in that list: no raw audio frames, no
            object-detection thumbnails, no ML-derived face or person embeddings,
            no location data, no geographic IP lookups. Cameras do not ship
            video to third parties under any condition we've designed for.
          </p>
        </section>

        {/* ── Motion on-device ───────────────────────────────────── */}
        <section className="security-section">
          <h2>Motion detection is local</h2>
          <p>
            Consumer cameras like Ring and Nest ship frames to cloud services
            for person, vehicle, and animal detection. We don't — motion
            detection is implemented with FFmpeg's <code>select='gte(scene,T)'</code>
            scene-change filter running as a local subprocess on the CloudNode.
            When a segment's scene score crosses your threshold, the node
            posts <em>only</em> the event metadata (camera ID, score, timestamp,
            segment sequence) to Command Center. The pixels stay on your device.
          </p>
          <p>
            You can read the implementation:{" "}
            <a
              href="https://github.com/SourceBox-LLC/opensentry-cloud-node/blob/master/src/streaming/motion_detector.rs"
              target="_blank"
              rel="noopener noreferrer"
            >
              <code>src/streaming/motion_detector.rs</code>
            </a>. There is no cloud ML dependency — no OpenAI, no Anthropic, no
            Google Vision, no AWS Rekognition. Verifiable with a <code>grep</code> of the
            source tree.
          </p>
        </section>

        {/* ── Encryption ─────────────────────────────────────────── */}
        <section className="security-section">
          <h2>Encryption</h2>

          <h3>In transit</h3>
          <p>
            TLS everywhere. CloudNode pushes and heartbeats use HTTPS; the
            browser fetches HLS over HTTPS same-origin; the MCP endpoint is
            HTTPS. Setup validation explicitly rejects invalid certificates —
            we don't have a "skip TLS" code path.
          </p>

          <h3>API keys (at rest, on the node)</h3>
          <p>
            The cloud API key your CloudNode holds is encrypted with AES-256-GCM.
            The 256-bit key is derived by hashing the host's OS-managed machine
            identifier (<code>/etc/machine-id</code> on Linux, <code>MachineGuid</code> on
            Windows, <code>IOPlatformUUID</code> on macOS) with a domain-separation tag.
            An attacker who steals just the DB file can't decrypt the key
            without also having code execution on the original host.
          </p>
          <p>
            Command Center stores node API keys and MCP keys as SHA-256 hashes —
            the plaintext values are shown once at creation and never again.
            Rotation invalidates the old hash immediately.
          </p>

          <h3>Recordings and snapshots (at rest, on the node)</h3>
          <p>
            Every recording segment and snapshot is encrypted with AES-256-GCM
            before it's written to the CloudNode's local SQLite database. The
            encryption key is the same machine-id-derived key used for your
            API key, which means:
          </p>
          <ul className="security-bullets">
            <li>An attacker who copies <code>node.db</code> off the device gets useless ciphertext — they also need code execution on the original host to re-derive the key.</li>
            <li>Every blob uses a fresh random nonce, so identical frames never produce identical ciphertexts.</li>
            <li>AES-GCM's authentication tag means tampering is detected — you can't silently flip bytes in a stored recording.</li>
          </ul>
          <p>
            For defense in depth we still recommend running your CloudNode on
            a disk protected with OS-level encryption (LUKS, BitLocker,
            FileVault) — that guards the machine-id file itself from offline
            attacks. But the application-level encryption means a casual
            theft or misplaced SD card doesn't expose your footage.
          </p>

          <h3>Command Center data</h3>
          <p>
            The Command Center database contains metadata only — camera names,
            motion event rows, audit and access logs, settings, hashed API
            keys. Our hosting provider (Fly.io) encrypts volumes at rest by
            default. No video content is stored on Command Center disks.
          </p>
        </section>

        {/* ── Access logs & transparency ──────────────────────── */}
        <section className="security-section">
          <h2>Who watched what, and when</h2>
          <p>
            Every authenticated stream view is logged: the viewer's user ID,
            email, IP address, truncated user agent, camera ID, and timestamp.
            Admins can see this from the in-app admin dashboard and export it.
            Logs are retained per the organization's plan (30 days on Free,
            90 days on Pro, 365 days on Pro Plus) and then automatically purged.
          </p>
          <p>
            This log trail belongs to your organization, not to us. We don't
            have a separate copy, we don't share it with partners, and we
            don't use it for analytics.
          </p>
        </section>

        {/* ── Third parties ─────────────────────────────────────── */}
        <section className="security-section">
          <h2>Third parties</h2>
          <p>
            The only third-party services that touch your account:
          </p>
          <ul className="security-bullets">
            <li><strong>Clerk</strong> — identity, organizations, session tokens, and subscription billing. Clerk never sees video.</li>
            <li><strong>Stripe</strong> — processes payments on Clerk's behalf. We never see your card details.</li>
            <li><strong>Fly.io</strong> — hosts the Command Center application and its database. Fly does not see video content (it's cached in application RAM, never written to persistent storage).</li>
            <li><strong>Sentry</strong> (optional) — error tracking. Disabled if <code>SENTRY_DSN</code> is unset. When enabled, captures exception stack traces, not video or user content. 10% trace sample rate.</li>
          </ul>
          <p>
            That's the complete list. No analytics providers, no ad networks,
            no data brokers, no tracking cookies, no third-party SDKs in the
            dashboard. You can verify by searching our source for the usual
            names (Mixpanel, Segment, GA, PostHog) — none are present.
          </p>
        </section>

        {/* ── LE & legal process ────────────────────────────────── */}
        <section className="security-section">
          <h2>Law enforcement & legal process</h2>
          <p>
            We have no pre-arranged data-sharing agreements with law
            enforcement. We do not participate in programs that grant warrantless
            access to customer accounts.
          </p>
          <p>
            If we receive a valid legal request, we respond only to the
            narrow scope it mandates. In practice, the only SourceBox-held
            data subject to such a request is what lives on Command Center: log
            metadata (access logs, audit logs) and account information.{" "}
            <strong>We do not have your video.</strong> Live segments are
            ephemeral in RAM; recordings live on your CloudNode hardware. If
            a request targets footage, the requester needs to approach you
            directly — we can't hand over something we don't store.
          </p>
          <p>
            We'll also notify the affected organization admin of any legal
            request unless legally prohibited from doing so.
          </p>
        </section>

        {/* ── How we compare ───────────────────────────────────── */}
        <section className="security-section">
          <h2>How we compare</h2>
          <p>
            Where Sentinel differs from consumer cameras people
            already have at home. Every row is sourced from the vendor's
            own policy pages or reputable journalism published in 2024 or
            later — citations are linked at the end of each cell.
            We'll keep this table current; if you find a stale claim,{" "}
            <a
              href="https://github.com/SourceBox-LLC/OpenSentry-Command/issues"
              target="_blank"
              rel="noopener noreferrer"
            >
              open an issue
            </a>.
          </p>

          <div className="security-compare">
            <table>
              <thead>
                <tr>
                  <th scope="col">Dimension</th>
                  <th scope="col">Sentinel</th>
                  <th scope="col">Ring</th>
                  <th scope="col">Google Nest</th>
                  <th scope="col">Wyze</th>
                </tr>
              </thead>
              <tbody>
                <tr>
                  <th scope="row">Video processed in cloud for AI features</th>
                  <td className="yes">On-device only (FFmpeg scene-change; no cloud ML)</td>
                  <td className="no">Cloud for Person/Package/Vehicle alerts, Video Descriptions, Familiar Faces (paid tier)</td>
                  <td className="no">Mixed; older cams process faces in cloud, newer on-device; paid-tier gated</td>
                  <td className="no">Edge + cloud; Descriptive Alerts and AI Search require Cam Unlimited Pro</td>
                </tr>
                <tr>
                  <th scope="row">Recording works without a subscription</th>
                  <td className="yes">Yes — all recording is local to your CloudNode, no cloud plan required</td>
                  <td className="no">No native local recording; cancel your plan and all cloud recordings are deleted</td>
                  <td className="partial">Cloud-first; only select Nest Cam models support microSD</td>
                  <td className="yes">microSD up to 512 GB supported natively, no subscription needed</td>
                </tr>
                <tr>
                  <th scope="row">Recordings encrypted at rest</th>
                  <td className="yes">AES-256-GCM with a machine-id-derived key</td>
                  <td className="partial">Amazon/AWS at rest; keys held by Amazon</td>
                  <td className="partial">Google at rest; keys held by Google; no user E2EE</td>
                  <td className="partial">AES-128 streams + TLS; Wyze holds the keys, not E2EE</td>
                </tr>
                <tr>
                  <th scope="row">Law-enforcement posture</th>
                  <td className="yes">No standing sharing agreements; our servers don't hold your video — requests for footage must go to you</td>
                  <td className="no">Ended Request-for-Assistance Jan 2024 — then restored warrantless request pathway in 2025 via Axon "Community Requests"</td>
                  <td className="partial">Reviews/narrows requests; data folded into Google's main Transparency Report</td>
                  <td className="partial">Complies with legal process incl. "national security or law enforcement requirements"; no public transparency report located</td>
                </tr>
                <tr>
                  <th scope="row">Sells or shares data for advertising</th>
                  <td className="yes">No — no analytics providers, ad networks, or data brokers</td>
                  <td className="partial">Amazon umbrella privacy policy applies; Ring-specific ad disclosures not detailed on Ring's own policy pages</td>
                  <td className="yes">No ad personalization from video, audio, or home-sensor data; third-party sharing requires explicit user permission</td>
                  <td className="no">Privacy policy explicitly states Wyze "sells" and "shares" personal information for advertising under state privacy laws</td>
                </tr>
                <tr>
                  <th scope="row">Source code you can audit</th>
                  <td className="yes">Yes — Command Center (AGPL-3.0) and CloudNode (GPL-3.0) are public on GitHub</td>
                  <td className="no">Proprietary firmware and app; OSS attribution only</td>
                  <td className="no">Proprietary firmware; some Nest dependencies public, not the device firmware or app</td>
                  <td className="no">Proprietary firmware and app; community firmware exists unofficially for some older models</td>
                </tr>
              </tbody>
            </table>
          </div>

          <h3>Notes and citations</h3>
          <ul className="security-bullets">
            <li>
              <strong>Ring's law-enforcement reversal:</strong> Ring ended the Neighbors Request-for-Assistance program in January 2024 (<a href="https://www.eff.org/deeplinks/2024/01/ring-announces-it-will-no-longer-facilitate-police-requests-footage-users" target="_blank" rel="noopener noreferrer">EFF, Jan 2024</a>). In 2025 it restored a warrantless-request pathway through the Axon-powered "Community Requests" feature (<a href="https://www.consumerreports.org/electronics/privacy/ring-community-requests-lets-police-ask-for-user-videos-a2437818485/" target="_blank" rel="noopener noreferrer">Consumer Reports, Sept 2025</a>; <a href="https://www.cnbc.com/2025/10/16/amazon-ring-cameras-surveillance-law-enforcement-crime-police-investigations.html" target="_blank" rel="noopener noreferrer">CNBC, Oct 2025</a>).
            </li>
            <li>
              <strong>Ring cloud-recording cancellation:</strong> After cancelling a Ring Protect / Ring Home subscription, cloud recordings are deleted and unrecoverable; as of November 6, 2025, pro-rata refunds were ended (<a href="https://ring.com/support/articles/0p7eu/Cancel-Ring-Protect-Subscription-Plan" target="_blank" rel="noopener noreferrer">Ring support, 2025</a>).
            </li>
            <li>
              <strong>Ring E2EE:</strong> Opt-in only; enabling it disables roughly twenty features including Shared User access, Event Timeline, Video Search, Person Detection, and Familiar Faces (<a href="https://ring.com/support/articles/7e3lk/using-video-end-to-end-encryption-e2ee" target="_blank" rel="noopener noreferrer">Ring support, 2025</a>).
            </li>
            <li>
              <strong>Google Nest:</strong> Google commits that video, audio, and home-sensor data are kept separate from advertising and not used for ad personalization (<a href="https://safety.google/nest/" target="_blank" rel="noopener noreferrer">safety.google/nest</a>). Government requests are folded into Google's main Transparency Report since 2020 (<a href="https://transparencyreport.google.com/user-data/overview" target="_blank" rel="noopener noreferrer">transparencyreport.google.com</a>). No E2EE is offered.
            </li>
            <li>
              <strong>Wyze data sale / share:</strong> Wyze's current Privacy Policy (April 2026) explicitly states Wyze "sells" and "shares" personal information under state privacy laws for advertising purposes, using cookies, SDKs, device identifiers, and web beacons (<a href="https://www.wyze.com/policies/privacy-policy" target="_blank" rel="noopener noreferrer">wyze.com privacy policy</a>). Biometric and facial data are excluded from that sharing.
            </li>
            <li>
              <strong>Wyze encryption:</strong> AES-128 for streams and TLS in transit. Wyze holds the keys — this is not end-to-end encryption, despite how it is sometimes described in secondary reporting (<a href="https://forums.wyze.com/t/end-to-end-encryption-e2ee/81562" target="_blank" rel="noopener noreferrer">Wyze forum, E2EE request</a>).
            </li>
            <li>
              <strong>Sentinel:</strong> Claims in the SourceBox column are implemented in our public source —{" "}
              <a href="https://github.com/SourceBox-LLC/opensentry-cloud-node/blob/master/src/storage/database.rs" target="_blank" rel="noopener noreferrer">encryption</a>,{" "}
              <a href="https://github.com/SourceBox-LLC/opensentry-cloud-node/blob/master/src/streaming/motion_detector.rs" target="_blank" rel="noopener noreferrer">motion detection</a>, and{" "}
              <a href="https://github.com/SourceBox-LLC/OpenSentry-Command" target="_blank" rel="noopener noreferrer">Command Center</a>.
            </li>
          </ul>

          <p className="security-disclaimer">
            <strong>A note on the limits of this comparison.</strong>{" "}
            Ring, Nest, and Wyze ship polished consumer hardware,
            mature mobile apps, and professional monitoring integrations.
            Sentinel does not — yet. We compete on <em>data posture</em>,
            not on feature parity. If 24/7 continuous cloud DVR with mobile
            motion-clip search is your top requirement, one of the incumbents
            will serve you better today.
          </p>
        </section>

        {/* ── Your data, your control ────────────────────────────── */}
        <section className="security-section">
          <h2>Your data: export and delete</h2>
          <p>
            Two GDPR rights, both implemented as in-app buttons that an
            org admin can use without contacting support:
          </p>
          <ul className="security-bullets">
            <li>
              <strong>Settings &rarr; Privacy &amp; Data &rarr; Download my data
              (ZIP)</strong> &mdash; GDPR <em>Article 20</em> (data
              portability).  Streams a ZIP containing one JSON file per
              data table in your organization &mdash; cameras, settings,
              audit log, motion events, notifications, MCP keys, email
              log, incidents, and the monthly usage counter &mdash; plus a
              machine-readable manifest.  Recordings live on your CloudNode
              devices, not Command Center, so they're not in the ZIP &mdash;
              use the CloudNode TUI to export local recordings.  Rate-
              limited to 3 exports/hour.
            </li>
            <li>
              <strong>Settings &rarr; Danger Zone &rarr; Reset Everything</strong>
              {" "}&mdash; GDPR <em>Article 17</em> (right to erasure).
              Triggers a full cascade across every org-scoped table:
              every node, camera, group, MCP key, audit log, stream
              access log, motion event, notification, incident, email
              log/outbox row, monthly-usage counter, and settings row
              is removed in a single transaction.  CloudNode devices
              are notified to wipe local data too.  No archival copy,
              no soft-delete grace window.
            </li>
            <li>
              <strong>Node &rarr; Decommission</strong> &mdash; removes a single
              node and all its cameras from your org, cleans up in-memory
              caches, and invalidates the node's API key.  The node
              itself wipes its local database on confirmation.  Use this
              when you're retiring one device but keeping the
              organization.
            </li>
          </ul>
          <p>
            The export and delete cascades both route through the same
            single source of truth (<code>app/core/gdpr.py</code>), so
            what you can download in the export is exactly what gets
            erased on delete &mdash; no partial coverage, no silent gaps.
            A separate <code>organization.deleted</code> webhook handler
            ensures that deleting your org via Clerk's UI runs the same
            cascade, even if you never click the in-app button.
          </p>
        </section>

        {/* ── What we don't ship yet ──────────────────────────────── */}
        <section className="security-section">
          <h2>Honest gaps — what we don't ship today</h2>
          <p>
            We'd rather you know what's missing before you sign up than
            discover it after a payment failure leaves you confused.
          </p>
          <ul className="security-bullets">
            <li>
              <strong>No SMS or mobile push alerts.</strong> Email alerts
              for operator-critical events <em>are</em> built in (camera
              offline + recovered, CloudNode offline + recovered,
              AI-created incident, MCP API key audit, CloudNode disk
              almost full, member audit, motion detection with
              cooldown + digest — opt-in per setting in your
              notification page).  Motion defaults OFF and uses a
              per-camera cooldown so a flappy outdoor camera caps
              out at ~2 emails per 15-minute window regardless of
              event volume.  SMS and mobile push remain MCP-only —
              wire your agent to Twilio or your existing PagerDuty
              webhook if you need them.
            </li>
            <li>
              <strong>No public status page yet.</strong> If Command Center
              is down, you'll see it from the dashboard but we don't yet
              publish historical uptime. Subscribe to{" "}
              <a
                href="https://github.com/SourceBox-LLC/OpenSentry-Command/issues"
                target="_blank"
                rel="noopener noreferrer"
              >
                the issues page
              </a>{" "}
              for incident write-ups in the meantime.
            </li>
            <li>
              <strong>No formal SLA on Free or Pro.</strong> Pro Plus is
              best-effort priority support; enterprise SLA agreements are
              available by request. We're not going to pretend we have
              99.99% uptime guaranteed when we run on a single Fly.io
              region — see "What happens if SourceBox goes down" below for
              the actual failure profile.
            </li>
          </ul>
        </section>

        {/* ── Resilience / data ownership ────────────────────────── */}
        <section className="security-section">
          <h2>What happens if SourceBox goes down</h2>
          <p>
            The CloudNode is designed to keep recording locally even when the
            Command Center is unreachable. Live streaming stops because the
            browser can't reach the cache, but segment capture, motion
            detection, and local recording all keep running. When connectivity
            returns, uploads resume automatically.
          </p>
          <p>
            Your recordings are yours either way — they live in a SQLite file
            on the hardware you own, encrypted at rest. We never ingest your
            footage to the cloud, so there's no data layer we can lock you
            into. The Command Center source being public (AGPL-3.0) is
            transparency, not a self-host product — verify what we run, audit
            what we touch, then sign up and let us operate it.
          </p>
        </section>

        {/* ── Vulnerability disclosure policy ───────────────────── */}
        {/*
          The id="vulnerability-disclosure" anchor is referenced from
          /.well-known/security.txt's Policy: field — DO NOT rename
          without updating app/api/well_known.py (and ideally publishing
          a redirect, since security scanners cache the policy URL).
        */}
        <section className="security-section" id="vulnerability-disclosure">
          <h2>Vulnerability Disclosure Policy</h2>
          <p>
            We take security seriously and welcome reports from researchers,
            customers, and operators.  This policy explains how to reach us,
            what we consider in-scope, what to expect after you report, and
            the safe-harbour terms we extend to good-faith research.
          </p>

          <h3>How to report</h3>
          <p>
            File a private{" "}
            <a
              href="https://github.com/SourceBox-LLC/OpenSentry-Command/security/advisories/new"
              target="_blank"
              rel="noopener noreferrer"
            >
              GitHub Security Advisory
            </a>{" "}
            on the Command Center repository.  This gives us a private
            channel for triage and the standard CVE workflow if one is
            warranted.  A free GitHub account is enough &mdash; you don't
            need to be a contributor to file one.
          </p>
          <p>
            We don't yet operate a security@ mailbox &mdash; the
            sourceboxsentry.com domain isn't provisioned for incoming
            mail.  A bounced report is worse than no email channel at
            all, so we publish only the GitHub path until MX records
            are live.
          </p>
          <p>
            Machine-readable contact info also lives at{" "}
            <a
              href="/.well-known/security.txt"
              target="_blank"
              rel="noopener noreferrer"
            >
              <code>/.well-known/security.txt</code>
            </a>{" "}
            (RFC 9116).
          </p>

          <h3>What to include</h3>
          <ul className="security-bullets">
            <li>A short description of the issue and its impact.</li>
            <li>Step-by-step reproduction (URLs, payloads, screenshots, etc).</li>
            <li>The version / commit SHA you tested against — surfaced by{" "}
              <a href="/api/health" target="_blank" rel="noopener noreferrer"><code>GET /api/health</code></a>.
            </li>
            <li>Optionally: a suggested fix or mitigation.</li>
          </ul>

          <h3>Scope</h3>
          <p>In scope:</p>
          <ul className="security-bullets">
            <li>The deployed Command Center (this site) and its API.</li>
            <li>The CloudNode binary + repository (
              <a
                href="https://github.com/SourceBox-LLC/opensentry-cloud-node"
                target="_blank"
                rel="noopener noreferrer"
              >SourceBox-LLC/opensentry-cloud-node</a>
              ).</li>
            <li>Auth / authorization, including IDOR, privilege escalation, and tenant-isolation breaks.</li>
            <li>RCE, SSRF, XSS, CSRF, SQL injection, deserialization attacks.</li>
            <li>Information disclosure that crosses tenant boundaries.</li>
            <li>Cryptographic weaknesses in the at-rest encryption story.</li>
            <li>MCP key scope-bypass — anything that lets a read-only key call a write tool.</li>
          </ul>
          <p>Out of scope (please don't report these as vulnerabilities — go to the upstream provider):</p>
          <ul className="security-bullets">
            <li>Issues in third-party services we use (Clerk, Stripe, Fly.io, Resend, Sentry).  Report those to the vendor directly; we'll coordinate if needed.</li>
            <li>Social engineering, physical attacks, or attacks requiring local access to a CloudNode you don't own.</li>
            <li>Volumetric DoS or bandwidth-exhaustion attacks on the deployed service.  Application-layer rate-limit bypasses ARE in scope; pure flood attacks are not.</li>
            <li>Missing security headers we've consciously chosen not to set, missing rate limits on read-only authenticated GETs (covered in our design — see commit history of <code>backend/app/core/limiter.py</code>).</li>
            <li>Self-XSS that requires the victim to paste attacker-controlled content into their own console.</li>
            <li>Email spoofing of domains we don't own.</li>
            <li>Reports generated solely by automated scanners with no proof-of-impact.</li>
          </ul>

          <h3>What you can expect from us</h3>
          <ul className="security-bullets">
            <li><strong>Acknowledgement</strong> within 72 hours of receiving the report.</li>
            <li><strong>Initial assessment</strong> within 7 days — confirmed / can't reproduce / declined-with-reason.</li>
            <li><strong>Fix coordination</strong> for confirmed reports — we'll keep you in the loop on timing and let you know when we've shipped.</li>
            <li><strong>Recognition</strong> in the release notes for the fix (and on a future hall-of-fame page) if you'd like — we'll ask before naming you.</li>
            <li><strong>No bug bounty</strong> today — we're pre-PMF.  We're upfront about that so you can decide whether to invest the time.  If we ever launch one, you'll be at the front of the line.</li>
          </ul>

          <h3>Safe harbour</h3>
          <p>
            If you make a good-faith effort to comply with this policy
            during your security research, we will:
          </p>
          <ul className="security-bullets">
            <li>Consider your research authorised under the Computer Fraud and Abuse Act (and equivalent state laws).</li>
            <li>Not pursue or support legal action related to your research.</li>
            <li>Work with you to understand and resolve the issue quickly.</li>
            <li>Recognise your contribution publicly if you wish.</li>
          </ul>
          <p>
            "Good faith" means: you avoid privacy violations and service
            disruptions, you only access accounts you own (or have explicit
            permission to test), you don't exfiltrate data beyond the
            minimum needed to demonstrate the issue, you give us reasonable
            time to fix before public disclosure, and you stop and tell us
            the moment you realise you've encountered customer data or
            personal information.
          </p>
          <p>
            If legal action is brought against you by a third party for
            activities that complied with this policy, we'll make it known
            that your actions were authorised.
          </p>
        </section>

        {/* ── Related links ─────────────────────────────────────── */}
        <section className="security-section">
          <h2>Related</h2>
          <ul className="security-bullets">
            <li><Link to="/legal/privacy">Privacy Policy</Link> — the legal version of the policies on this page.</li>
            <li><Link to="/legal/terms">Terms of Service</Link> — grace periods, plan enforcement, acceptable use.</li>
            <li><Link to="/docs#api-rate-limits">API Rate Limits</Link> — per-route limits and bucketing.</li>
            <li><Link to="/docs#security-procedures">Security Procedures</Link> — what to do if a key is compromised.</li>
          </ul>
        </section>

      </div>
    </div>
  )
}

export default SecurityPage
