"""Unit tests for the GitHub /releases/latest cache.

These exercise the resolution policy in isolation — cache hit vs cache
miss, leading-``v`` stripping, env-var fallback.  Network paths
(``get_latest_release`` / ``refresh_latest_release``) are not exercised
here; we don't want unit tests to hit GitHub.  The conftest fixture
``reset_release_cache`` keeps the module state clean between tests.
"""

import pytest

from app.core import release_cache
from app.core import versions as versions_mod
from app.core.release_cache import _strip_leading_v, latest_node_version


@pytest.mark.parametrize(
    "raw,expected",
    [
        ("v0.1.39", "0.1.39"),
        ("V0.1.39", "0.1.39"),  # uppercase tolerated
        ("0.1.39", "0.1.39"),   # already-bare version untouched
        ("vapor", "apor"),       # only strips a single leading char (intentional simplicity)
        ("", ""),
    ],
)
def test_strip_leading_v(raw, expected):
    assert _strip_leading_v(raw) == expected


def test_latest_node_version_falls_back_to_env_var(monkeypatch):
    """With an empty cache, the env var is the source of truth."""
    monkeypatch.setattr(versions_mod.settings, "LATEST_NODE_VERSION", "0.9.9")
    # Cache is reset by the autouse fixture in conftest, so this is the
    # cold-boot / GitHub-outage path.
    assert latest_node_version() == "0.9.9"


def test_latest_node_version_prefers_cached_release(monkeypatch):
    """Once a release is cached, the env var stops being read."""
    # Set the env var to something that would be wrong, to prove the
    # cache wins.
    monkeypatch.setattr(versions_mod.settings, "LATEST_NODE_VERSION", "0.0.1")

    # Simulate a successful GitHub fetch having populated the cache.
    release_cache._cached_release = {"tag_name": "v0.1.39"}
    release_cache._cached_at = 1.0  # any non-None float works here

    assert latest_node_version() == "0.1.39"


def test_latest_node_version_handles_tag_without_v(monkeypatch):
    """Tags without a leading ``v`` (rare but legal) pass through unchanged."""
    monkeypatch.setattr(versions_mod.settings, "LATEST_NODE_VERSION", "0.0.1")
    release_cache._cached_release = {"tag_name": "1.2.3"}
    release_cache._cached_at = 1.0

    assert latest_node_version() == "1.2.3"


def test_latest_node_version_handles_missing_tag_name(monkeypatch):
    """A cached release dict without ``tag_name`` falls back to env var.

    GitHub always sends ``tag_name`` for published releases, but we
    code defensively — a malformed cache entry should never be a hard
    failure on the heartbeat path.
    """
    monkeypatch.setattr(versions_mod.settings, "LATEST_NODE_VERSION", "0.5.0")
    release_cache._cached_release = {"name": "Some release"}  # no tag_name
    release_cache._cached_at = 1.0

    assert latest_node_version() == "0.5.0"


def test_check_node_version_uses_cached_release_when_present(monkeypatch):
    """End-to-end: a cached fresher release should override the env var
    in ``check_node_version`` output.

    This is the property that makes the whole refactor worthwhile —
    a CameraNode release on GitHub immediately surfaces as
    ``update_available`` to every connected node, without anyone
    bumping ``LATEST_NODE_VERSION``.
    """
    from app.core.versions import check_node_version

    monkeypatch.setattr(versions_mod.settings, "MIN_SUPPORTED_NODE_VERSION", "0.1.0")
    monkeypatch.setattr(versions_mod.settings, "LATEST_NODE_VERSION", "0.1.26")

    # GitHub has shipped 0.2.0 since the env var was last bumped.
    release_cache._cached_release = {"tag_name": "v0.2.0"}
    release_cache._cached_at = 1.0

    result = check_node_version("0.1.26")
    # Without the cache, this would say "you're at LATEST, no update".
    # With the cache, the node now sees 0.2.0 is available.
    assert result["latest"] == "0.2.0"
    assert result["update_available"] == "0.2.0"
