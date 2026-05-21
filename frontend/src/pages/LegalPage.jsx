import { useParams, Link } from "react-router-dom"

const LAST_UPDATED = "April 24, 2026"

// Contact channel for Terms / Privacy / DSAR / CCPA questions.
//
// Until ``sourceboxsentry.com`` is provisioned for incoming mail
// (no MX records yet), we route every contact path through the
// public GitHub repo's issue tracker.  The dashboard's in-app
// Settings → Privacy & Data section already covers the two big
// GDPR rights self-serve (Article 20 export + Article 17 delete),
// so the issue tracker is only the path for "questions about
// what data you hold beyond what the export already shows" —
// which is uncommon enough that the public-issue concern is
// acceptable as long as users know not to paste PII into the
// issue body.  When a real ``legal@`` mailbox lands, swap the
// helpers below for ``mailto:`` links.
const CONTACT_GITHUB_ISSUES_URL =
  "https://github.com/SourceBox-LLC/Sentinel-Command/issues"

function TermsContent() {
  return (
    <>
      <h1>Terms of Service</h1>
      <p className="legal-updated">Last updated: {LAST_UPDATED}</p>

      <h2>1. Acceptance of Terms</h2>
      <p>
        By accessing or using the Sentinel Command Center service ("Service"),
        operated by SourceBox LLC ("Company," "we," "us," or "our"),
        you ("User," "you," or "your") agree to be bound by these Terms of
        Service ("Terms"). If you are using the Service on behalf of an
        organization, you represent and warrant that you have authority to
        bind that organization to these Terms, and "you" refers to both you
        individually and the organization.
      </p>
      <p>
        If you do not agree to these Terms, you must not access or use the
        Service.
      </p>

      <h2>2. Description of Service</h2>
      <p>
        Sentinel provides a cloud-hosted security camera management platform
        that enables users to connect local camera nodes, view live video
        streams, and manage camera configurations. The Service includes web
        dashboard access, API access, and MCP (Model Context Protocol)
        integration for AI-powered camera interaction.
      </p>

      <h2>3. Important Security Camera Disclaimer</h2>
      <p>
        <strong>
          THE SERVICE IS A CAMERA MANAGEMENT AND VIEWING TOOL ONLY. IT IS
          NOT A PROFESSIONAL SECURITY, SURVEILLANCE, OR ALARM MONITORING
          SYSTEM AND SHOULD NOT BE RELIED UPON AS SUCH.
        </strong>
      </p>
      <p>
        The Service does not provide emergency response, law enforcement
        notification, or 24/7 monitoring. We do not guarantee that cameras
        will remain online, that video will be captured or retained during
        any specific event, or that the Service will detect, prevent, or
        record any incident including but not limited to theft, vandalism,
        trespass, or personal injury.
      </p>
      <p>
        You acknowledge that camera connectivity depends on your local
        network, hardware, power supply, and internet connection, none of
        which are under our control. You are solely responsible for your
        own physical security arrangements.
      </p>

      <h2>4. Compliance with Surveillance and Recording Laws</h2>
      <p>
        You are solely responsible for ensuring that your use of the Service
        complies with all applicable federal, state, local, and international
        laws and regulations regarding video surveillance, audio recording,
        data collection, and privacy, including but not limited to:
      </p>
      <ul>
        <li>Consent requirements for audio and video recording in your jurisdiction</li>
        <li>Signage or notice requirements for surveillance in your area</li>
        <li>Restrictions on recording in private spaces or workplaces</li>
        <li>Data protection regulations (such as GDPR, CCPA, or equivalent local laws)</li>
      </ul>
      <p>
        We do not provide legal advice regarding surveillance compliance.
        You should consult with a qualified attorney to understand your
        obligations. We are not liable for any claims, fines, or damages
        arising from your failure to comply with applicable surveillance
        or recording laws.
      </p>

      <h2>5. Accounts and Organizations</h2>
      <p>
        You must create an account and organization to use the Service.
        You are responsible for maintaining the confidentiality of your
        account credentials and all API keys (including CloudNode keys and
        MCP keys). You must notify us immediately of any unauthorized use
        of your account. You are responsible for all activity that occurs
        under your account.
      </p>

      <h2>6. Subscription Plans and Payment</h2>
      <p>
        The Service offers Free, Pro, and Pro Plus subscription tiers.
        Paid plans are billed monthly or annually at your election through
        our payment processor (Stripe, via Clerk). Upgrades take effect
        immediately; downgrades take effect at the end of the current
        billing period.
      </p>
      <p>
        <strong>Usage-based tier structure.</strong> The binding constraint
        for each tier is a monthly <strong>viewer-hour cap</strong>, which
        meters the number of hours of live HLS video we serve to
        authenticated viewers in your organization (30 hours on Free, 300
        on Pro, 1,500 on Pro Plus). Recordings stored locally on your
        CloudNode device and motion-event metadata do not count against
        this cap. When the cap is reached, live-video playback pauses
        with an upgrade prompt until the 1st of the next calendar month;
        cameras continue recording locally, motion detection continues
        to fire, and MCP integrations continue to function. Hardware
        counts (cameras, nodes, seats) are enforced as generous safety
        limits rather than primary tier differentiators.
      </p>
      <p>
        <strong>No overage billing.</strong> Your monthly bill is the plan
        price you selected. When a metered limit is reached (viewer-hours
        or MCP calls) we pause the metered feature rather than apply
        automatic overage charges.
      </p>
      <p>
        <strong>Camera-cap enforcement on downgrade or cancellation.</strong>{" "}
        If, after a downgrade, your organization has more cameras registered
        than your new plan allows, we retain the oldest cameras (by creation
        date) up to your new cap and mark the rest as <em>suspended</em>.
        Suspended cameras stop streaming to the Command Center — the
        CloudNode surfaces a <q>Suspended — Plan Limit</q> badge and an
        upgrade prompt on each affected camera tile. The camera records
        themselves are preserved, so upgrading back to a higher tier
        immediately restores streaming with no reconfiguration.
      </p>
      <p>
        <strong>Failed payments and the grace period.</strong> If a charge
        fails, your account enters a <strong>7-day grace period</strong>{" "}
        during which the charge is retried automatically. During grace you
        retain full access to your paid plan, except that creation of new
        MCP API keys is blocked. If the card is not recovered within 7 days
        of the first failure, we apply Free-tier camera caps using the
        oldest-first rule above until payment succeeds — at which point
        suspended cameras resume streaming within one heartbeat cycle
        (~30 seconds). Nothing is deleted during this process.
      </p>
      <p>
        <strong>Re-subscription.</strong> Resuming a cancelled subscription
        (or upgrading back from Free) re-enables all previously-suspended
        cameras and nodes automatically. Local recordings, snapshots, and
        camera metadata persist on your CloudNode device throughout this
        cycle — we never delete your local data as part of billing
        enforcement.
      </p>

      <h2>7. Acceptable Use</h2>
      <p>You agree not to:</p>
      <ul>
        <li>Use the Service for any unlawful purpose or in violation of any applicable laws</li>
        <li>Use the Service to conduct surveillance in violation of any person's reasonable expectation of privacy</li>
        <li>Attempt to gain unauthorized access to the Service or its related systems</li>
        <li>Interfere with or disrupt the Service or its infrastructure</li>
        <li>Use the Service to store or transmit malicious code</li>
        <li>Reverse engineer, decompile, or disassemble the Service</li>
        <li>Resell or redistribute access to the Service without authorization</li>
        <li>Exceed published rate limits or abuse API access</li>
      </ul>

      <h2>8. API Keys and Access Credentials</h2>
      <p>
        API keys (including CloudNode keys and MCP API keys) are sensitive
        credentials. You are solely responsible for securing your keys and
        for any actions taken using them. We store keys using SHA-256 hashing
        and never retain plaintext copies. If you believe a key has been
        compromised, you must revoke it immediately through the dashboard.
      </p>

      <h2>9. Data and Video Content</h2>
      <p>
        You retain ownership of all video content captured by your cameras and
        served through the Service. We do not access, view, or share your
        video content except as necessary to provide the Service or as required
        by law. Live video segments are buffered briefly in your organization's
        isolated in-memory cache for playback; recordings and snapshots are
        stored locally on your CloudNode.
      </p>
      <p>
        You are solely responsible for the legality of content captured,
        stored, and shared through your use of the Service.
      </p>

      <h2>10. Third-Party Services</h2>
      <p>
        The Service relies on third-party providers including Clerk
        (authentication) and Fly.io (hosting). We are not responsible for
        the availability, performance, or policies of these third-party
        services. Outages or changes by these providers may affect the
        Service, and we shall not be liable for any resulting disruption
        or data loss.
      </p>

      <h2>11. Disclaimer of Warranties</h2>
      <p>
        <strong>
          THE SERVICE IS PROVIDED "AS IS" AND "AS AVAILABLE" WITHOUT
          WARRANTIES OF ANY KIND, WHETHER EXPRESS, IMPLIED, OR STATUTORY.
          TO THE MAXIMUM EXTENT PERMITTED BY APPLICABLE LAW, WE DISCLAIM
          ALL WARRANTIES, INCLUDING BUT NOT LIMITED TO IMPLIED WARRANTIES
          OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE,
          NON-INFRINGEMENT, AND ANY WARRANTIES ARISING FROM COURSE OF
          DEALING OR USAGE OF TRADE.
        </strong>
      </p>
      <p>
        <strong>
          WE DO NOT WARRANT THAT THE SERVICE WILL BE UNINTERRUPTED,
          ERROR-FREE, SECURE, OR FREE OF VIRUSES OR OTHER HARMFUL
          COMPONENTS. WE DO NOT WARRANT THAT CAMERAS WILL REMAIN
          CONNECTED, THAT VIDEO WILL BE CAPTURED OR STORED SUCCESSFULLY,
          OR THAT THE SERVICE WILL MEET YOUR SECURITY REQUIREMENTS.
        </strong>
      </p>

      <h2>12. Limitation of Liability</h2>
      <p>
        <strong>
          TO THE MAXIMUM EXTENT PERMITTED BY APPLICABLE LAW, IN NO EVENT
          SHALL THE COMPANY, ITS OFFICERS, DIRECTORS, EMPLOYEES, AGENTS,
          OR AFFILIATES BE LIABLE FOR ANY INDIRECT, INCIDENTAL, SPECIAL,
          CONSEQUENTIAL, OR PUNITIVE DAMAGES, OR ANY LOSS OF PROFITS,
          REVENUE, DATA, GOODWILL, OR OTHER INTANGIBLE LOSSES, ARISING
          FROM OR RELATED TO YOUR USE OF OR INABILITY TO USE THE SERVICE,
          WHETHER BASED ON WARRANTY, CONTRACT, TORT (INCLUDING NEGLIGENCE),
          STRICT LIABILITY, OR ANY OTHER LEGAL THEORY.
        </strong>
      </p>
      <p>
        <strong>
          WITHOUT LIMITING THE FOREGOING, THE COMPANY SHALL NOT BE LIABLE
          FOR ANY DAMAGES ARISING FROM: (A) THE FAILURE OF CAMERAS OR NODES
          TO CAPTURE, TRANSMIT, OR STORE VIDEO; (B) SECURITY INCIDENTS,
          THEFT, PROPERTY DAMAGE, OR PERSONAL INJURY THAT CAMERAS FAILED
          TO RECORD OR PREVENT; (C) SERVICE INTERRUPTIONS OR DOWNTIME;
          (D) UNAUTHORIZED ACCESS TO YOUR ACCOUNT OR DATA; OR (E) ACTIONS
          OF THIRD-PARTY SERVICE PROVIDERS.
        </strong>
      </p>
      <p>
        <strong>
          TO THE EXTENT PERMITTED BY LAW, OUR TOTAL AGGREGATE LIABILITY
          FOR ALL CLAIMS ARISING FROM OR RELATED TO THE SERVICE SHALL NOT
          EXCEED THE TOTAL AMOUNT YOU PAID TO US IN THE TWELVE (12) MONTHS
          IMMEDIATELY PRECEDING THE EVENT GIVING RISE TO THE CLAIM, OR
          FIFTY DOLLARS ($50.00), WHICHEVER IS GREATER.
        </strong>
      </p>

      <h2>13. Indemnification</h2>
      <p>
        You agree to indemnify, defend, and hold harmless the Company, its
        officers, directors, employees, agents, and affiliates from and
        against any and all claims, damages, losses, liabilities, costs,
        and expenses (including reasonable attorneys' fees) arising from
        or related to:
      </p>
      <ul>
        <li>Your use of the Service</li>
        <li>Your violation of these Terms</li>
        <li>Your violation of any applicable law, including surveillance and recording laws</li>
        <li>Any content captured, stored, or shared through your use of the Service</li>
        <li>Any claim by a third party related to your camera deployment or video content</li>
        <li>Your failure to secure your account credentials or API keys</li>
      </ul>

      <h2>14. Termination</h2>
      <p>
        Either party may terminate the agreement at any time. You may cancel
        your subscription through the billing settings. We may suspend or
        terminate your access for violation of these Terms, non-payment,
        or any other reason at our sole discretion.
      </p>
      <p>
        Upon termination, your access to the Service will cease. Your data
        may be deleted after a reasonable retention period (typically 30
        days). You may use the Full Reset feature in Settings to delete all
        your data before canceling. Sections 3, 4, 9, 11, 12, 13, 16, and
        17 survive termination.
      </p>

      <h2>15. Changes to Terms</h2>
      <p>
        We may update these Terms from time to time. Material changes will
        be communicated through the Service dashboard or via email. Your
        continued use of the Service after changes take effect constitutes
        acceptance of the updated Terms. If you do not agree with the
        updated Terms, you must stop using the Service.
      </p>

      <h2>16. Governing Law and Dispute Resolution</h2>
      <p>
        These Terms shall be governed by and construed in accordance with
        the laws of the State of Washington, without regard to its conflict
        of law provisions. Any disputes arising under these Terms shall be
        resolved in the state or federal courts located in Washington State, and
        you consent to personal jurisdiction in such courts.
      </p>

      <h2>17. General Provisions</h2>
      <p>
        <strong>Severability:</strong> If any provision of these Terms is
        found to be unenforceable, the remaining provisions shall continue
        in full force and effect.
      </p>
      <p>
        <strong>Entire Agreement:</strong> These Terms, together with the
        Privacy Policy, constitute the entire agreement between you and
        the Company regarding the Service and supersede all prior agreements.
      </p>
      <p>
        <strong>Waiver:</strong> The failure to enforce any provision of
        these Terms shall not constitute a waiver of that provision.
      </p>
      <p>
        <strong>Force Majeure:</strong> We shall not be liable for any delay
        or failure to perform resulting from causes outside our reasonable
        control, including but not limited to natural disasters, power
        outages, internet disruptions, government actions, or pandemics.
      </p>
      <p>
        <strong>Assignment:</strong> You may not assign your rights under
        these Terms without our prior written consent. We may assign our
        rights at any time.
      </p>

      <h2>18. Contact</h2>
      <p>
        For questions about these Terms, open an issue on our{" "}
        <a
          href={CONTACT_GITHUB_ISSUES_URL}
          target="_blank"
          rel="noopener noreferrer"
        >
          GitHub repository
        </a>. A dedicated <code>legal@</code> mailbox will land alongside
        our custom domain; until then GitHub Issues is the working
        channel. Please don't include personally identifying details in
        the public issue body — open the issue first and we'll arrange
        a private follow-up.
      </p>
    </>
  )
}

function PrivacyContent() {
  return (
    <>
      <h1>Privacy Policy</h1>
      <p className="legal-updated">Last updated: {LAST_UPDATED}</p>

      <p>
        This Privacy Policy describes how SourceBox LLC ("Company," "we,"
        "us," or "our") collects, uses, and protects your information when
        you use the Sentinel Command Center service ("Service").
      </p>
      <p>
        A plain-language, engineer-facing walkthrough of the same data flows
        (with links to the exact files in our public source code that
        implement each behavior) is available at{" "}
        <Link to="/security">/security</Link>. This Policy is the legally
        binding version; where a detail differs, the language here governs.
      </p>

      <h2>1. Information We Collect</h2>

      <h3>Account Information</h3>
      <p>
        When you create an account, we collect your name, email address, and
        organization details through our authentication provider (Clerk).
        We do not store passwords directly. Payment information is processed
        by Stripe through Clerk and is never stored on our servers.
      </p>

      <h3>Camera and Video Data</h3>
      <p>
        Live video segments captured by your CloudNode cameras are pushed
        directly to the Command Center backend, where they are held in an
        in-memory cache (approximately the most recent 60 seconds per
        camera) only long enough to be served to authorized viewers in
        your organization. Live segments are not written to persistent
        disk, object storage, or any long-term storage on our servers.
      </p>
      <p>
        Recordings and snapshots (if you enable them) are stored on your
        local CloudNode device in an encrypted SQLite database. Recording
        and snapshot blobs are sealed with AES-256-GCM at rest using a key
        derived from the host device's operating-system machine identifier,
        and the Service never ingests or retains copies of these files.
      </p>
      <p>
        Motion detection runs entirely on the CloudNode device using local
        video analysis. No raw video frames, object-detection thumbnails,
        or machine-learning embeddings are transmitted to the Service or
        to any third-party machine-learning provider. Only motion event
        metadata (camera identifier, timestamp, scene-change score) is
        transmitted to the Service.
      </p>
      <p>
        We do not access, analyze, view, or share your video content except
        as strictly necessary to provide the Service (e.g., serving HLS
        streams to authenticated users in your organization).
      </p>

      <h3>Usage and Log Data</h3>
      <p>We collect operational data to provide and secure the Service:</p>
      <ul>
        <li>Stream access logs (who viewed which camera, when, and IP address)</li>
        <li>Per-organization monthly viewer-second aggregates used to enforce the viewer-hour cap for your plan</li>
        <li>MCP tool call activity (tool name, API key used, timestamps, and duration)</li>
        <li>Node registration and heartbeat data (hostname, local IP, camera status)</li>
        <li>Audit logs for administrative actions</li>
      </ul>
      <p>
        Log retention is tiered by plan: 30 days on the Free tier, 90 days
        on Pro, and 365 days on Pro Plus. Expired records are permanently
        deleted by a scheduled daily cleanup task.
      </p>

      <h3>Codec and Device Information</h3>
      <p>
        Your CloudNode reports video/audio codec information (e.g.,
        H.264 profile, AAC format) to ensure proper HLS stream playback.
        No other device telemetry is collected.
      </p>

      <h2>2. How We Use Your Information</h2>
      <p>We use collected information solely to:</p>
      <ul>
        <li>Provide, maintain, and improve the Service</li>
        <li>Authenticate users and enforce organization-based access control</li>
        <li>Serve HLS video streams with correct codec parameters</li>
        <li>Enforce plan limits (cameras, nodes, MCP rate limits)</li>
        <li>Generate usage statistics visible in your admin dashboard</li>
        <li>Detect and prevent abuse or unauthorized access</li>
        <li>Process payments and manage subscriptions</li>
        <li>Communicate important service updates</li>
      </ul>
      <p>
        We do not use your information for advertising, profiling, or
        any purpose unrelated to providing the Service.
      </p>

      <h2>3. Data Storage and Security</h2>
      <p>We implement the following security measures:</p>
      <ul>
        <li>Service-side API keys (CloudNode keys and MCP integration keys) are stored as SHA-256 hashes; plaintext keys are never retained after issuance</li>
        <li>Live video segments are kept in an isolated in-memory cache per organization and never written to a third-party object store</li>
        <li>On the CloudNode device, the cloud-facing API key, recording segment blobs, and snapshot image blobs are encrypted at rest with AES-256-GCM using a key derived from the host's operating-system machine identifier</li>
        <li>All connections between your browser, CloudNode devices, and the Service use HTTPS with HSTS enforcement; there is no disable-TLS code path</li>
        <li>Authentication is handled by Clerk with industry-standard JWT verification</li>
        <li>Organization data is isolated at the database level using org_id scoping on every query</li>
        <li>Rate limits apply per tenant (CloudNode key, organization, or client IP) to mitigate abuse</li>
        <li>Security headers (X-Content-Type-Options, X-Frame-Options, Referrer-Policy, Strict-Transport-Security) are applied to all responses</li>
      </ul>
      <p>
        While we take reasonable measures to protect your data, no method
        of electronic transmission or storage is 100% secure. We cannot
        guarantee absolute security.
      </p>

      <h2>4. Data Sharing</h2>
      <p>
        We do not sell, rent, or trade your personal information or video
        data. We do not use analytics providers, advertising networks,
        retargeting services, or data brokers. We share information only
        with the following categories of third parties, solely as necessary
        to provide the Service:
      </p>
      <ul>
        <li><strong>Authentication and billing:</strong> Clerk (account management, organization membership, subscription management, session handling)</li>
        <li><strong>Payment processing:</strong> Stripe via Clerk (subscription billing and tax calculation). Payment card data is handled by Stripe and is never transmitted to or stored on our servers</li>
        <li><strong>Hosting:</strong> Fly.io (application hosting for the Command Center). Live video segments held in application memory are never persisted to Fly.io's disk-backed volumes</li>
        <li><strong>Transactional email:</strong> Resend (US-based transactional email provider). Receives the recipient email address, subject line, and rendered body of each operational notification we send (camera offline, motion alerts, MCP key audit, etc.). Resend does not receive video, snapshots, recordings, or any data beyond what is necessary to deliver the email. Each email contains a one-click unsubscribe link.</li>
        <li><strong>Error monitoring (optional):</strong> Sentry, only when the operator has configured the <code>SENTRY_DSN</code> environment variable. When enabled, Sentry receives application exception traces and a 10% sample of performance traces. Sentry does not receive video content, recording metadata, or end-user identifiers beyond what is present in exception stack traces. When <code>SENTRY_DSN</code> is unset, no data is sent to Sentry</li>
        <li><strong>Legal requirements:</strong> When required by law, regulation, subpoena, or other legal process. See Section 5 for our posture on legal process</li>
        <li><strong>Safety:</strong> To protect the rights, property, or safety of our users or the public, where disclosure is narrowly necessary</li>
      </ul>
      <p>
        Each third-party provider operates under their own privacy policy
        and data processing terms. We encourage you to review their policies.
      </p>

      <h3>Law enforcement and legal process</h3>
      <p>
        We maintain no standing data-sharing arrangements with law
        enforcement agencies and do not participate in programs that grant
        warrantless access to customer accounts. When we receive a valid
        legal request, we respond only to the scope it mandates.
      </p>
      <p>
        The only video-related data that resides on our servers is the
        ephemeral in-memory live cache described in Section 1; we do not
        retain copies of your recordings or snapshots. Requests for such
        recordings or snapshots must be directed to the controller of the
        CloudNode device on which that footage is stored. Where not
        prohibited by law, we will notify the affected organization
        administrator of legal requests so the organization may contest
        them directly.
      </p>

      <h2>5. Data Retention</h2>
      <ul>
        <li>Live video segments are held in application memory only as long as needed for playback (approximately the most recent 60 one-second segments per camera) and are evicted automatically; segments are not persisted to disk on our servers</li>
        <li>Recordings and snapshots are stored in encrypted form locally on your CloudNode device, not on our servers, and are subject to the retention and disk-quota settings you configure on that device</li>
        <li>Stream access logs, MCP activity logs, motion event metadata, notification records, and audit logs are retained according to the organization's plan: 30 days on the Free tier, 90 days on Pro, and 365 days on Pro Plus. Expired records are permanently deleted by a scheduled daily cleanup task</li>
        <li>Per-organization monthly viewer-second aggregates are retained indefinitely in a single aggregate row per calendar month so historical usage can be displayed in the dashboard; this row contains no personally identifiable information</li>
        <li>Account data is retained as long as your account is active</li>
        <li>Upon organization deletion, all associated nodes, cameras, camera groups, MCP keys, stream access logs, MCP activity logs, motion events, notifications, audit logs, monthly usage rows, and settings are permanently deleted in a single database transaction</li>
        <li>You can delete all organization data at any time using the Full Reset feature in Settings, or by deleting your organization through Clerk</li>
      </ul>

      <h2>6. Your Rights</h2>
      <p>Depending on your jurisdiction, you may have the right to:</p>
      <ul>
        <li><strong>Access:</strong> View your data through the dashboard and admin panel</li>
        <li><strong>Deletion:</strong> Delete all your organization data via Full Reset in Settings, or by deleting your organization</li>
        <li><strong>Portability:</strong> Stream access logs and MCP activity logs are viewable and exportable from the admin dashboard</li>
        <li><strong>Correction:</strong> Update your account information through Clerk's account management</li>
        <li><strong>Objection:</strong> Cancel your account at any time</li>
        <li><strong>Withdraw consent:</strong> Stop using the Service at any time</li>
      </ul>
      <p>
        Most of these rights are self-serve from the dashboard&rsquo;s{" "}
        <strong>Settings &rarr; Privacy &amp; Data</strong> section &mdash;
        the <strong>Download my data (ZIP)</strong> button covers Article
        20 portability with one click, and the{" "}
        <strong>Reset Everything</strong> action under Danger Zone
        covers Article 17 erasure (every node, camera, log, notification,
        and config row removed in a single transaction). <strong>Reset
        Everything is available on every plan, including Free</strong> &mdash;
        the right to erasure is a legal obligation we cannot gate behind a
        paid subscription. For anything the in-app actions don&rsquo;t
        cover, open an issue on our{" "}
        <a
          href={CONTACT_GITHUB_ISSUES_URL}
          target="_blank"
          rel="noopener noreferrer"
        >
          GitHub repository
        </a>{" "}
        and we&rsquo;ll arrange a private follow-up. We respond within
        30 days.
      </p>

      <h2>7. International Data Transfers</h2>
      <p>
        The Service is hosted in the United States. If you access the Service
        from outside the United States, your information may be transferred
        to and processed in the United States, where data protection laws
        may differ from those in your jurisdiction. By using the Service,
        you consent to this transfer. If you are located in the European
        Economic Area (EEA), United Kingdom, or other region with data
        transfer regulations, please be aware that we rely on your consent
        and the necessity of processing to provide the Service as the legal
        basis for data transfers.
      </p>

      <h2>8. Cookies</h2>
      <p>
        We use cookies only for authentication session management through
        Clerk. We do not use advertising, analytics, or tracking cookies.
        These cookies are strictly necessary for the Service to function
        and cannot be disabled while using the Service.
      </p>

      <h2>9. Children's Privacy</h2>
      <p>
        The Service is not intended for use by individuals under the age
        of 18. We do not knowingly collect personal information from
        children under 18. If we become aware that a child under 18 has
        provided us with personal information, we will take steps to
        delete such information promptly.
      </p>

      <h2>10. California Privacy Rights (CCPA)</h2>
      <p>
        If you are a California resident, you have additional rights under
        the California Consumer Privacy Act (CCPA), including the right to
        know what personal information we collect, the right to delete your
        information, and the right to opt out of the sale of your
        information. We do not sell personal information. CCPA delete
        and access requests are self-serve from{" "}
        <strong>Settings &rarr; Privacy &amp; Data</strong> in the
        dashboard. For anything else, open an issue on our{" "}
        <a
          href={CONTACT_GITHUB_ISSUES_URL}
          target="_blank"
          rel="noopener noreferrer"
        >
          GitHub repository
        </a>.
      </p>

      <h2>11. Changes to This Policy</h2>
      <p>
        We may update this Privacy Policy from time to time. Material changes
        will be communicated through the Service dashboard or via email.
        Your continued use of the Service after changes take effect
        constitutes acceptance of the updated policy. If you do not agree
        with the updated policy, you must stop using the Service.
      </p>

      <h2>12. Contact</h2>
      <p>
        For privacy-related questions, open an issue on our{" "}
        <a
          href={CONTACT_GITHUB_ISSUES_URL}
          target="_blank"
          rel="noopener noreferrer"
        >
          GitHub repository
        </a>. To exercise your data rights, use the in-app{" "}
        <strong>Settings &rarr; Privacy &amp; Data</strong> section &mdash;
        Download my data (Article 20) and the Danger Zone&rsquo;s
        Reset Everything (Article 17) are both self-serve. A
        dedicated <code>privacy@</code> mailbox will land alongside our
        custom domain.
      </p>
    </>
  )
}

function LegalPage() {
  const { page } = useParams()

  return (
    <div className="legal-container">
      <div className="legal-nav">
        <Link to="/legal/terms" className={page === "terms" ? "active" : ""}>Terms of Service</Link>
        <Link to="/legal/privacy" className={page === "privacy" ? "active" : ""}>Privacy Policy</Link>
      </div>
      <div className="legal-content">
        {page === "privacy" ? <PrivacyContent /> : <TermsContent />}
      </div>
    </div>
  )
}

export default LegalPage
