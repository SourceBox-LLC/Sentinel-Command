## What and why

<!-- What does this change, and why? Link the issue if there is one: Fixes #… -->

## Areas touched

- [ ] Backend (Rust, `backend-rs/`)
- [ ] Sentinel AI agent (`backend-rs/src/agent/`)
- [ ] Frontend (React, `frontend/`)
- [ ] Database schema (a migration in **both** `migrations/` and `migrations-sqlite/`)
- [ ] Auth, permissions or tenant isolation
- [ ] Live video / segment cache
- [ ] Docker, `fly.toml` or CI
- [ ] Documentation only

## How it was tested

<!-- Commands run, manual steps, anything a reviewer should repeat. -->

- [ ] `cargo fmt` and `cargo clippy --all-targets -- -D warnings` (both builds) are clean
- [ ] `cargo test` passes, with `TEST_DATABASE_URL` and with `--features sqlite` if it touches the database
- [ ] `npx vitest run` and `npm run build` pass, for frontend changes
- [ ] Checked by hand, with a CameraNode if it touches video or nodes

## Checklist

- [ ] AGENTS.md / docs updated if behaviour, routes or configuration changed
- [ ] No secrets, keys or customer data in the diff

## Screenshots

<!-- For UI changes. -->
