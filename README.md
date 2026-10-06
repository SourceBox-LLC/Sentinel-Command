<p align="center">
  <h1 align="center">Sentinel Command Center</h1>
  <p align="center">
    One dashboard for every camera, on every site, live.
    <br />
    The cloud hub for <strong>Sentinel by SourceBox</strong>. Your footage stays yours.
    <br />
    <br />
    <a href="https://app.sentinel-command.com"><strong>► Open the app</strong></a>
    &nbsp;·&nbsp;
    <a href="https://sentinel-command.com">Website</a>
    &nbsp;·&nbsp;
    <a href="https://sentinel-command.com/documentation/">Documentation</a>
    &nbsp;·&nbsp;
    <a href="https://github.com/SourceBox-LLC/Sentinel-CameraNode">CameraNode</a>
  </p>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/status-live-22c55e.svg" alt="Live">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-AGPL_v3-blue.svg" alt="License: AGPL v3"></a>
  <img src="https://img.shields.io/badge/source-public_for_transparency-6366f1.svg" alt="Source available for transparency">
</p>

---

## What it does

📹 &nbsp;**Live video, private by design.** CameraNodes push video to your dashboard through an in-memory relay. Your recordings stay on your own device; Command Center holds only a short live buffer in memory. The only video it ever stores is what's attached to an incident: a snapshot or a short clip, saved by you or the Sentinel AI agent.

🔔 &nbsp;**Motion and alerts.** Real-time motion events, one notification inbox, and opt-in email for what matters: a camera going offline, a node low on disk, a new incident.

🤖 &nbsp;**AI that investigates.** The optional Sentinel AI agent looks into motion and incidents for you and files reports with snapshots and clips.

🔌 &nbsp;**Fits your setup.** A [Home Assistant](https://github.com/SourceBox-LLC/Sentinel-HomeAssistant) integration on every plan, and an MCP server so AI assistants such as Claude can view your cameras and review incidents.

👥 &nbsp;**Built for teams and multiple sites.** Organizations with roles, strict tenant isolation, and an audit trail on every sensitive action.

## How it fits together

```text
   Your network                        Our cloud                       You
 ┌──────────────────┐           ┌──────────────────────┐         ┌──────────────┐
 │    CameraNode    │  outbound │   Command Center     │         │    Browser   │
 │  camera + FFmpeg │══════════▶│   live relay +       │◀═══════▶│   dashboard  │
 │  records locally │   HTTPS   │   dashboard + API    │  HTTPS  │  (live video)│
 │  (your footage)  │           │  (short live buffer, │         │              │
 └──────────────────┘           │   not your archive)  │         └──────────────┘
                                └──────────────────────┘
```

CameraNode captures and encodes video on your network and pushes it **outbound** to Command Center over HTTPS: no inbound ports, no port forwarding, no VPN. Command Center keeps a short rolling buffer in memory and streams it to your browser. Your recordings never leave your CameraNode; only incident evidence (a snapshot or short clip) is saved in the cloud.

## Use it hosted, or run it yourself

**Most people should use the hosted app.** Sign up at **[app.sentinel-command.com](https://app.sentinel-command.com/sign-up)** and pair it with a [CameraNode](https://github.com/SourceBox-LLC/Sentinel-CameraNode). There are no servers or databases to look after.

**To run it yourself**, use Docker Compose:

```bash
cp backend-rs/.env.example .env      # set LOCAL_ADMIN_USERNAME and LOCAL_ADMIN_EMAIL
openssl rand -hex 32                 # → APP_SECRET_KEY in .env
docker compose run --rm --no-deps app sentinel-hash-password   # → LOCAL_ADMIN_PASSWORD_HASH
docker compose up -d                 # open http://localhost:8000
```

Put the password hash in **single quotes** in `.env`; it contains `$` characters that Compose would otherwise expand. `docker-compose.yml` runs PostgreSQL alongside the app. `docker-compose.sqlite.yml` is a single container with a SQLite file instead (`docker compose -f docker-compose.sqlite.yml …`).

A self-hosted install has one admin account and no Clerk account or billing, and every feature is unlocked except Sentinel AI. Sentinel AI has a real ongoing model cost, so it needs a licence key. The same licence can add **cloud data-sync**: a one-way backup of your database to a SourceBox-hosted mirror, restored with `sentinel-restore-from-cloud` ([how](docs/runbooks/DISASTER_RECOVERY.md#self-hosted-installs-restoring-from-the-cloud-mirror)). Your own database stays the source of truth, and the app works without internet access.

## Why is the source public?

**Trust.** Your security footage should be yours, and you shouldn't have to take our word for how it's handled. The code behind every privacy and security claim is here for anyone to audit. It is licensed [AGPL-3.0](LICENSE): anyone who modifies it and runs it as a network service must publish their changes.

## The Sentinel system

| Project | What it is |
| --- | --- |
| **Command Center** (this repo) | The dashboard, API, live-video relay, MCP server and the Sentinel AI agent |
| **[CameraNode](https://github.com/SourceBox-LLC/Sentinel-CameraNode)** | The camera software you install on your own hardware |
| **[Home Assistant integration](https://github.com/SourceBox-LLC/Sentinel-HomeAssistant)** | Your Sentinel cameras inside Home Assistant |
| **[License Service](https://github.com/SourceBox-LLC/Sentinel-License-Service)** | Validates self-hosted licence keys |
| **[Sync Service](https://github.com/SourceBox-LLC/Sentinel-Sync-Service)** | The cloud mirror for self-hosted installs |

## Documentation

| If you want to… | Read |
| --- | --- |
| **Use Sentinel**: cameras, recording, notifications, integrations | [sentinel-command.com/documentation](https://sentinel-command.com/documentation/) |
| **See how the whole system fits together** | [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) |
| **Work on this code**: configuration, API, data model, internals | [AGENTS.md](AGENTS.md) |
| **Understand the AI agent** | [docs/SENTINEL_AGENT.md](docs/SENTINEL_AGENT.md) |
| **Operate it**: runbooks, launch checklist, decision records | [docs/](docs/README.md) |
| **Report a vulnerability** | [SECURITY.md](SECURITY.md) |

## Tech stack

A Rust backend (axum, sqlx, rmcp) and a React 19 frontend, shipped as one Docker image on Fly.io. PostgreSQL in production, SQLite or PostgreSQL self-hosted. Sign-in through Clerk (hosted) or a local admin account (self-hosted). Developer setup is in [AGENTS.md › Build, run and test](AGENTS.md#build-run-and-test).

## License and contributions

[**AGPL-3.0**](LICENSE). SourceBox LLC operates Command Center as a hosted service, and self-hosting is a supported use too.

We don't accept external code contributions at the moment, but bug reports and ideas are welcome in [Issues](https://github.com/SourceBox-LLC/Sentinel-Command/issues) and [Discussions](https://github.com/SourceBox-LLC/Sentinel-Command/discussions). See [CONTRIBUTING.md](CONTRIBUTING.md).

---

<p align="center">
  Made by <a href="https://github.com/SourceBox-LLC">SourceBox LLC</a>
  &nbsp;·&nbsp;
  <a href="https://app.sentinel-command.com">App</a>
  &nbsp;·&nbsp;
  <a href="https://sentinel-command.com">Website</a>
</p>
