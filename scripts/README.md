# Operator scripts

Served or run from the deployed image at `/app/scripts`
(`SCRIPTS_DIR`). Moved here from `backend/scripts/` when the Python web
tier was deleted — the two install scripts are read off disk by routes
the Rust tier serves, so their location is load-bearing rather than
conventional.

| file | who reads it |
| --- | --- |
| `install.sh` | `GET /install.sh` — the CameraNode installer for Linux and macOS. Windows installs from the MSI in the CameraNode release. |
| `mcp-setup.sh`, `mcp-setup.ps1` | `GET /mcp-setup.sh` and `/mcp-setup.ps1` — configure Claude Code, Claude Desktop, Cursor or Windsurf against this Command Center. |
| `backup_db.sh`, `restore_db.sh` | an operator, by hand. Both need `pg_dump`/`pg_restore`, which is why the image installs `postgresql-client-18`. |

## The two that became binaries

`hash_local_admin_password.py` and `restore_from_cloud.py` were Python
and went with the rest of it. Both are documented, so neither could
simply disappear:

| was | is | documented in |
| --- | --- | --- |
| `python scripts/hash_local_admin_password.py` | `sentinel-hash-password` | `AGENTS.md`, self-host setup |
| `python scripts/restore_from_cloud.py` | `sentinel-restore-from-cloud` | `docs/runbooks/DISASTER_RECOVERY.md` |

Both ship in the image at `/usr/local/bin`, so on a deployed machine
they are on `PATH`:

```
fly ssh console -a sentinel-command -C sentinel-restore-from-cloud --list
```

`sentinel-hash-password` also takes `--stdin`, which the Python version
had no way to offer — useful from a provisioning script.
