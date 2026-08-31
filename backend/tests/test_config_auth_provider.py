"""Tests for Config's AUTH_PROVIDER predicates (app/core/config.py).

Regression coverage for a real bug a code review caught: is_clerk_auth()
and is_local_auth() were originally checked inline with opposite
polarity at different call sites (app/core/clerk.py + app/main.py
tested `== "clerk"`, everywhere else tested `== "local"`), which
silently disagreed for any AUTH_PROVIDER value other than the two exact
literals. They must always be exact complements — never both true,
never both false — for every input.

Also regression coverage for why these are instance methods (`self`,
not `@classmethod` `cls`): tests monkeypatch attributes directly on the
`settings` singleton instance, and a classmethod reading `cls.X` does
not see an instance-level override — see the comment above these
methods in config.py, and app/core/email.py's history with the same
pitfall.
"""

import pytest

from app.core.config import settings


@pytest.mark.parametrize(
    "value",
    ["local", "clerk", "", "Local", "LOCAL", " local", "local ", "banana", "clerk "],
)
def test_is_local_auth_and_is_clerk_auth_are_exact_complements(monkeypatch, value):
    monkeypatch.setattr(settings, "AUTH_PROVIDER", value)
    assert settings.is_local_auth() != settings.is_clerk_auth()


def test_only_exact_literal_local_selects_local_mode(monkeypatch):
    monkeypatch.setattr(settings, "AUTH_PROVIDER", "local")
    assert settings.is_local_auth() is True
    assert settings.is_clerk_auth() is False


@pytest.mark.parametrize("value", ["clerk", "", "Local", "banana", " local"])
def test_anything_other_than_exact_local_selects_clerk_mode(monkeypatch, value):
    monkeypatch.setattr(settings, "AUTH_PROVIDER", value)
    assert settings.is_clerk_auth() is True
    assert settings.is_local_auth() is False


def test_is_local_auth_configured_reflects_instance_monkeypatch(monkeypatch):
    """Regression: this must be an instance method, not @classmethod —
    see the module docstring. A @classmethod reading `cls.X` would
    still see the class-level default here and silently return True
    even though APP_SECRET_KEY was just cleared on the instance."""
    monkeypatch.setattr(settings, "APP_SECRET_KEY", "a" * 32)
    monkeypatch.setattr(settings, "LOCAL_ADMIN_USERNAME", "admin")
    monkeypatch.setattr(settings, "LOCAL_ADMIN_PASSWORD_HASH", "hash")
    assert settings.is_local_auth_configured() is True

    monkeypatch.setattr(settings, "APP_SECRET_KEY", "")
    assert settings.is_local_auth_configured() is False
