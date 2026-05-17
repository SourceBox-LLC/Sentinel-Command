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

For engineers cloning the repo to read, audit, or contribute fixes locally. (End users do not run Command Center — they sign up at the live SaaS.)

Sentinel has two main components:

| Component | Language | Repository |
|-----------|----------|------------|
| **Command Center** | Python (FastAPI) + React | [Sentinel-Command](https://github.com/SourceBox-LLC/Sentinel-Command) |
| **CloudNode** | Rust | [OpenSentry-CloudNode](https://github.com/SourceBox-LLC/OpenSentry-CloudNode) |

### Command Center

```bash
# Backend
cd backend
cp .env.example .env
uv sync
uv run python start.py       # http://localhost:8000

# Frontend
cd frontend
cp .env.example .env
npm install
npm run dev                   # http://localhost:5173
```

### CloudNode

```bash
cd OpenSentry-CloudNode
cargo build --release
./target/release/sourcebox-sentry-cloudnode setup
```

See the [CloudNode README](https://github.com/SourceBox-LLC/OpenSentry-CloudNode) for full setup instructions.

## License

Sentinel Command Center is licensed under [AGPL-3.0](LICENSE). AGPL §13 obligates anyone who modifies the code and offers a network-accessible version of it to publish their changes. Read the license before redistributing or running a modified copy.

---

Thank you for your interest in Sentinel by SourceBox.
