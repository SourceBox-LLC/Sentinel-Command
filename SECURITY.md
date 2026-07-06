# Security Policy

SourceBox Sentry is a security-focused application and we take vulnerabilities seriously.

The full policy — scope, response timelines, safe-harbour terms, and the standard machine-readable [`security.txt`](https://app.sentinel-command.com/.well-known/security.txt) — lives at:

**https://sentinel-command.com/security#vulnerability-disclosure**

This file is the GitHub-standard summary; the deployed page above is canonical when the two disagree.

## Reporting a vulnerability

Two channels, use whichever you prefer:

- **Email:** [`security@sentinel-command.com`](mailto:security@sentinel-command.com) — a monitored mailbox for security reports.
- **GitHub:** file a private [Security Advisory on this repository](https://github.com/SourceBox-LLC/Sentinel-Command/security/advisories/new) for structured private triage and the standard CVE workflow if one is warranted. A free GitHub account is enough — you don't need to be a contributor to file one.

**Please do NOT:**

- Open a public GitHub issue for security vulnerabilities.
- Disclose details publicly before we've shipped a fix.

### What to include

- Description of the issue and its impact
- Steps to reproduce (URLs, payloads, screenshots)
- Version / commit you tested against — surfaced by `GET /api/health`
- Optional suggested fix or mitigation

### Response timeline

- Acknowledgement within 72 hours
- Initial assessment within 7 days
- Fix coordinated with the reporter, with credit in the release notes if you'd like

## Scope

**In scope:**

- The deployed Command Center API and web application (https://app.sentinel-command.com)
- The CameraNode binary + repository ([`SourceBox-LLC/Sentinel-CameraNode`](https://github.com/SourceBox-LLC/Sentinel-CameraNode))
- Auth / authorization, including IDOR, privilege escalation, and tenant-isolation breaks
- RCE, SSRF, XSS, CSRF, SQL injection, deserialization
- Cryptographic weaknesses in the at-rest encryption story
- MCP key scope-bypass — anything that lets a read-only key call a write tool

**Out of scope:**

- Issues in third-party services (Clerk, Stripe, Fly.io, Resend, Sentry) — report upstream
- Social engineering, physical attacks, attacks needing local access to a CameraNode you don't own
- Volumetric DoS / bandwidth flood attacks (application-layer rate-limit bypasses ARE in scope)
- Missing security headers / rate limits we've consciously chosen not to set
- Self-XSS requiring the victim to paste attacker-controlled content
- Email spoofing of domains we don't own
- Reports generated solely by automated scanners with no proof-of-impact

## Safe harbour

If you make a good-faith effort to comply with this policy:

- We consider your research authorised under the Computer Fraud and Abuse Act (and equivalent state laws)
- We will not pursue or support legal action related to your research
- We will recognise your contribution publicly if you wish
- We will work with you to understand and resolve the issue quickly

"Good faith" means: avoid privacy violations and service disruptions, only test accounts you own (or have explicit permission to test), don't exfiltrate data beyond what's needed to demonstrate the issue, give us reasonable time to fix before public disclosure, stop and tell us the moment you realise you've encountered customer data.

## Bug bounty

There is no monetary bug bounty today — SourceBox Sentry is pre-PMF. We're upfront about that so you can decide whether to invest the time. If we ever launch one, prior reporters will be at the front of the line.

## Security updates

Monitor:

- [GitHub Releases](https://github.com/SourceBox-LLC/Sentinel-Command/releases)
- [GitHub Security Advisories](https://github.com/SourceBox-LLC/Sentinel-Command/security/advisories)
