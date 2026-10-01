# Contributing to Sentinel

Thanks for your interest in Sentinel by SourceBox. This document explains how you can help, and what we accept.

## We do not currently accept external code contributions

Sentinel is **source-available under AGPL-3.0**. The source is public for trust and transparency — customers can audit the implementation behind every security and privacy claim on the live site — but we do not accept pull requests from outside the core team at this time. Command Center is a SaaS we host and operate; running your own copy is allowed by the license but is not the intended use case.

External pull requests opened against this repository will be automatically closed with a link back to this document. This is not personal — we keep the contribution surface narrow so we can move fast, retain clean copyright, and avoid the overhead of a Contributor License Agreement.

## What we *do* welcome

| Channel | What to use it for |
|---------|--------------------|
| [Issues](https://github.com/SourceBox-LLC/Sentinel-Command/issues) | Bug reports, reproducible problems, security disclosures |
| [Discussions](https://github.com/SourceBox-LLC/Sentinel-Command/discussions) | Feature ideas, questions about how the platform works |
| Forks | Audit, study, and private modifications under AGPL-3.0 (see the license for your §13 network-use obligations if you redistribute or run a modified network-accessible copy) |

A clear bug report with steps to reproduce is genuinely valuable — please open one if you hit something broken.

### Reporting bugs

Before filing, check [existing issues](https://github.com/SourceBox-LLC/Sentinel-Command/issues). Include:

- Steps to reproduce
- Expected vs. actual behavior
- Relevant logs (redact any secrets)
- Environment (OS, Python version, browser if UI-related)

### Reporting security issues

See [SECURITY.md](SECURITY.md). Do **not** file public issues for vulnerabilities.

## Local development setup

For engineers cloning the repo to read, audit, or contribute fixes locally. (Most end users sign up at the live SaaS rather than running Command Center themselves — but self-hosting via `AUTH_PROVIDER=local` is a supported mode, not just a dev-only setup; see [AGENTS.md › Build & Run](AGENTS.md#build--run).)

Sentinel has two main components:

| Component | Language | Repository |
|-----------|----------|------------|
| **Command Center** | Rust (axum) + React | [Sentinel-Command](https://github.com/SourceBox-LLC/Sentinel-Command) |
| **CameraNode** | Rust | [Sentinel-CameraNode](https://github.com/SourceBox-LLC/Sentinel-CameraNode) |

The backend was Python (FastAPI) and the AI agent a Python worker until
both were rewritten in Rust; they are two binaries from one crate,
`backend-rs/`, and there is no Python left. If you are looking for a file
that used to be under `backend/app/`, its counterpart is named in
[AGENTS.md › API Routes](AGENTS.md#api-routes); the agent's is
`backend-rs/src/agent/`.

### Command Center

To just *run* it, rather than develop on it, use the repo's
`docker-compose.yml` — see [AGENTS.md › Build & Run](AGENTS.md#build--run).
For development, with the frontend on its own dev server:

```bash
# Backend — needs PostgreSQL (SQLite is not supported; see AGENTS.md).
# `cp backend-rs/.env.example backend-rs/.env` for the full variable list.
docker run -d --name sentinel-pg -p 5432:5432 \
    -e POSTGRES_USER=sentinel -e POSTGRES_PASSWORD=sentinel \
    -e POSTGRES_DB=sentinel postgres:16-alpine
cd backend-rs
DATABASE_URL=postgresql://sentinel:sentinel@127.0.0.1:5432/sentinel \
    cargo run                 # http://localhost:8000

cargo test                    # DB-gated tests skip without TEST_DATABASE_URL
cargo clippy --all-targets    # kept at zero warnings

# Frontend
cd frontend
cp .env.example .env
npm install
npm run dev                   # http://localhost:5173
```

### CameraNode

```bash
cd Sentinel-CameraNode
cargo build --release
./target/release/sourcebox-sentry-cameranode setup
```

See the [CameraNode README](https://github.com/SourceBox-LLC/Sentinel-CameraNode) for full setup instructions.

## License

Sentinel Command Center is licensed under [AGPL-3.0](LICENSE). AGPL §13 obligates anyone who modifies the code and offers a network-accessible version of it to publish their changes. Read the license before redistributing or running a modified copy.

---

Thank you for your interest in Sentinel by SourceBox.
