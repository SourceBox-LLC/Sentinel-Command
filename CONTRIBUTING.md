# Contributing to Sentinel

Thanks for your interest in Sentinel by SourceBox. This document explains how you can help, and what we accept.

## We do not currently accept external code contributions

Sentinel is **source-available under AGPL-3.0**. The source is public for trust and transparency: customers can audit the implementation behind every security and privacy claim. We do not accept pull requests from outside the core team at this time. You are welcome to run your own copy: self-hosting is a supported mode (see the [README](README.md#use-it-hosted-or-run-it-yourself)).

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
- Environment: hosted or self-hosted, the version from `GET /api/health`, OS, and browser if it's a UI problem

### Reporting security issues

See [SECURITY.md](SECURITY.md). Do **not** file public issues for vulnerabilities.

## Local development setup

For engineers cloning the repo to read, audit or fix it locally. To simply *run* Command Center, use Docker Compose instead (see the [README](README.md#use-it-hosted-or-run-it-yourself)).

Sentinel has two main components:

| Component | Language | Repository |
|-----------|----------|------------|
| **Command Center** | Rust (axum) + React | [Sentinel-Command](https://github.com/SourceBox-LLC/Sentinel-Command) |
| **CameraNode** | Rust | [Sentinel-CameraNode](https://github.com/SourceBox-LLC/Sentinel-CameraNode) |

The backend and the AI agent are two binaries from one Rust crate, `backend-rs/`. Both were Python until October 2026. The full developer reference is [AGENTS.md](AGENTS.md).

### Command Center

For development, with the frontend on its own dev server:

```bash
# Backend — PostgreSQL here; or skip the container and run
# `DATABASE_URL=sqlite:///./sentinel.db cargo run --features sqlite`.
# `cp backend-rs/.env.example backend-rs/.env` for the full variable list.
docker run -d --name sentinel-pg -p 5432:5432 \
    -e POSTGRES_USER=sentinel -e POSTGRES_PASSWORD=sentinel \
    -e POSTGRES_DB=sentinel postgres:16-alpine
cd backend-rs
DATABASE_URL=postgresql://sentinel:sentinel@127.0.0.1:5432/sentinel \
    cargo run                 # http://localhost:8000

cargo test                    # DB-gated tests skip without TEST_DATABASE_URL
cargo fmt                     # CI rejects unformatted code
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
