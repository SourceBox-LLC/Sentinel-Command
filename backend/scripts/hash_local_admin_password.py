#!/usr/bin/env python3
"""Hash a password for LOCAL_ADMIN_PASSWORD_HASH (self-hosted auth mode).

Usage (from backend/):
    uv run python scripts/hash_local_admin_password.py

Prompts for a password (input hidden), prints an argon2 hash to paste
into your .env as LOCAL_ADMIN_PASSWORD_HASH.
"""

import getpass
import sys

from argon2 import PasswordHasher


def main() -> int:
    password = getpass.getpass("New local admin password: ")
    if not password:
        print("Password must not be empty.", file=sys.stderr)
        return 1
    confirm = getpass.getpass("Confirm password: ")
    if password != confirm:
        print("Passwords did not match.", file=sys.stderr)
        return 1

    hashed = PasswordHasher().hash(password)
    print("\nLOCAL_ADMIN_PASSWORD_HASH=" + hashed)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
