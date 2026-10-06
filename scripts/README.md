# Operator scripts

Copied into the image at `/app/scripts` (`SCRIPTS_DIR`). The install
scripts are read off disk by routes the backend serves, so don't move
or rename them without changing `api/install.rs`.

| file | who reads it |
| --- | --- |
| `install.sh` | `GET /install.sh` — the CameraNode installer for Linux and macOS. Windows installs from the MSI in the CameraNode release. |
| `mcp-setup.sh`, `mcp-setup.ps1` | `GET /mcp-setup.sh` and `/mcp-setup.ps1` — configure Claude Code, Claude Desktop, Cursor or Windsurf against this Command Center. |
| `backup_db.sh` | the nightly backup workflow (`.github/workflows/backup.yml`), on the production machine; or an operator. PostgreSQL only. |
| `restore_db.sh` | an operator restoring a dump ([DISASTER_RECOVERY.md](../docs/runbooks/DISASTER_RECOVERY.md)). PostgreSQL only. |

Both use `pg_dump` / `pg_restore`, which is why the image installs `postgresql-client-18`. A SQLite install is backed up by copying its database file.

## Tools that are binaries, not scripts

Two operator tools used to be Python scripts here. They are now Rust
binaries in `backend-rs/src/bin/`:

| was | is | documented in |
| --- | --- | --- |
| `python scripts/hash_local_admin_password.py` | `sentinel-hash-password` | `AGENTS.md`, self-host setup |
| `python scripts/restore_from_cloud.py` | `sentinel-restore-from-cloud` | `docs/runbooks/DISASTER_RECOVERY.md` |

Both ship in the image at `/usr/local/bin`, so on a deployed machine
they are on `PATH`:

```bash
fly ssh console -a sentinel-command -C "sentinel-restore-from-cloud --list"
```

`sentinel-hash-password` also takes `--stdin`, for provisioning scripts.
