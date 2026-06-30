<p align="center">
  <h1 align="center">Sentinel Command Center</h1>
  <p align="center">
    One dashboard for every camera, on every site — live, in real time.
    <br />
    The cloud hub for <strong>Sentinel by SourceBox</strong>. Your footage stays yours.
    <br />
    <br />
    <a href="https://opensentry-command.fly.dev"><strong>► Try the live app</strong></a>
    &nbsp;·&nbsp;
    <a href="https://opensentry-command.fly.dev/docs">Documentation</a>
    &nbsp;·&nbsp;
    <a href="https://github.com/SourceBox-LLC/Sentinel-CameraNode">CloudNode</a>
  </p>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/status-live-22c55e.svg" alt="Live">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-AGPL_v3-blue.svg" alt="License: AGPL v3"></a>
  <img src="https://img.shields.io/badge/source-public_for_transparency-6366f1.svg" alt="Source available for transparency">
</p>

<p align="center">
  <a href="https://github.com/SourceBox-LLC/Sentinel-Command">
    <img src="https://img.shields.io/github/stars/SourceBox-LLC/Sentinel-Command?style=social&label=Star" alt="Star this repo">
  </a>
  &nbsp; If Sentinel is useful to you, please ⭐ <a href="https://github.com/SourceBox-LLC/Sentinel-Command">star the repo</a> — it helps others find it.
</p>

---

## You don't run this — we host it for you

Sentinel Command Center is a **product we operate as a service**, not something you download and deploy. To use it, just **[sign up on the live app](https://opensentry-command.fly.dev)** — no servers to provision, no Docker, no database to babysit. Pair a [CloudNode](https://github.com/SourceBox-LLC/Sentinel-CameraNode) with a camera and it appears in your dashboard automatically.

> **Looking for the part you actually install?** That's **[CloudNode](https://github.com/SourceBox-LLC/Sentinel-CameraNode)** — a small daemon that turns any USB or IP camera into a private, cloud-connected feed. It runs on your hardware and has its own setup guide.

### So why is this repo public?

**Trust.** Sentinel is built on the idea that your security footage should be *yours*, and you shouldn't have to take our word for how it's handled. So the code behind every privacy and security claim on the site is right here, in the open — auditable by anyone. It's licensed [AGPL-3.0](LICENSE): source-available, with the obligation that anyone who modifies it and runs it as a network service publishes their changes too.

---

## What it does

📹 &nbsp;**Live video, private by design** — CloudNodes push video straight to your dashboard through an in-memory proxy. Your recordings stay on your own device; Command Center keeps only a short live buffer in memory, never a copy of your footage at rest.

🔔 &nbsp;**Motion & alerts** — real-time motion events, a unified notification inbox, and opt-in email for the things that matter: a camera going offline, a node low on disk, a new incident.

🤖 &nbsp;**AI that investigates, not just alerts** — the optional Sentinel agent watches for motion and incidents and looks into them for you, filing reports with snapshots and short clips.

🔌 &nbsp;**Fits your setup** — a one-key [Home Assistant](https://github.com/SourceBox-LLC/Sentinel-HomeAssistant) integration (on every plan), plus an MCP server so AI assistants like Claude can view your cameras and review past incidents.

👥 &nbsp;**Built for teams and multiple sites** — organizations with role-based access, multi-tenant isolation, and an audit trail on every sensitive action.

---

## How it fits together

```
   Your network                        Our cloud                       You
 ┌──────────────────┐           ┌──────────────────────┐         ┌──────────────┐
 │     CloudNode    │  outbound │   Command Center     │         │    Browser   │
 │  camera + FFmpeg │══════════▶│   live proxy +       │◀═══════▶│   dashboard  │
 │  records locally │   HTTPS   │   dashboard + API    │  HTTPS  │  (live video)│
 │  (your footage)  │           │  (short live buffer, │         │              │
 └──────────────────┘           │   not your archive)  │         └──────────────┘
                                └──────────────────────┘
```

CloudNode captures and encodes video on your network, then pushes it **outbound** to Command Center over HTTPS — no inbound ports, no port-forwarding, no VPN. Command Center holds a short rolling buffer in memory and streams it to your browser. Your actual recordings never leave your CloudNode.

→ Full architecture, API routes, and data models: **[AGENTS.md](AGENTS.md)**.

---

## The Sentinel ecosystem

| Project | What it is | |
|---------|------------|---|
| **Command Center** *(this repo)* | The hosted dashboard, API, and live-video hub | [Live app ›](https://opensentry-command.fly.dev) |
| **CloudNode** | The camera daemon you install on your own hardware | [Repo ›](https://github.com/SourceBox-LLC/Sentinel-CameraNode) |
| **Home Assistant integration** | Your Sentinel cameras inside Home Assistant | [Repo ›](https://github.com/SourceBox-LLC/Sentinel-HomeAssistant) |
| **Sentinel AI agent** | Serverless agent that investigates motion & incidents | [Repo ›](https://github.com/SourceBox-LLC/SourceBox-Sentinel) |

---

## Documentation

| If you want to… | Go to |
|-----------------|-------|
| **Use Sentinel** — set up cameras, recording, notifications, integrations | The in-app [Documentation](https://opensentry-command.fly.dev/docs) |
| **Understand the code** — architecture, API, data models, configuration | [AGENTS.md](AGENTS.md) |
| **Operate it** — decision records, runbooks, legal templates | [docs/](docs/) |
| **Audit or run the source locally** for review | [AGENTS.md › Build & Run](AGENTS.md) |

---

## License & contributions

[**AGPL-3.0**](LICENSE) — source-available. Command Center is operated by **SourceBox LLC** as a SaaS; the source is public so customers can verify the implementation behind the product's privacy and security claims. Running your own copy is permitted under the license, but it isn't the intended use — and AGPL §13 requires anyone who modifies it and offers it over a network to publish their changes.

This project is **not currently accepting external code contributions**, but bug reports and feature ideas are very welcome via [Issues](https://github.com/SourceBox-LLC/Sentinel-Command/issues) and [Discussions](https://github.com/SourceBox-LLC/Sentinel-Command/discussions). See [CONTRIBUTING.md](CONTRIBUTING.md).

---

<p align="center">
  Made by <a href="https://github.com/SourceBox-LLC">SourceBox LLC</a>
  &nbsp;·&nbsp;
  <a href="https://opensentry-command.fly.dev">Live app</a>
  &nbsp;·&nbsp;
  <a href="https://github.com/SourceBox-LLC/Sentinel-CameraNode">CloudNode</a>
</p>
