"""Unit tests for the CameraNode version-compatibility helper.

The HTTP-level behavior is covered in test_nodes.py; this file exercises
the parser and policy logic in isolation so a regression in
``parse_version`` shows up with a sharp local failure instead of a
confusing 426 in an integration test.
"""

import pytest

from app.core import versions as versions_mod
from app.core.versions import (
    check_node_version,
    format_version,
    parse_version,
)


@pytest.mark.parametrize(
    "raw,expected",
    [
        ("0.0.0", (0, 0, 0)),
        ("1.2.3", (1, 2, 3)),
        ("v1.2.3", (1, 2, 3)),  # leading "v" tolerated for git-tag style
        ("  1.2.3  ", (1, 2, 3)),  # surrounding whitespace
        ("10.20.30", (10, 20, 30)),
        ("1.2.3-rc1", (1, 2, 3)),  # pre-release suffix ignored
        ("1.2.3+build.5", (1, 2, 3)),  # build metadata ignored
        ("", (0, 0, 0)),
        (None, (0, 0, 0)),
        ("not.a.version", (0, 0, 0)),
        ("1.2", (0, 0, 0)),  # no patch → can't compare safely
    ],
)
def test_parse_version(raw, expected):
    assert parse_version(raw) == expected


def test_format_version_round_trips():
    assert format_version((1, 2, 3)) == "1.2.3"
    assert format_version((0, 0, 0)) == "0.0.0"


def test_check_node_version_unknown_is_supported(monkeypatch):
    """A None / empty version is tolerated (legacy CameraNode) and flagged
    for update so the dashboard can nudge the operator."""
    monkeypatch.setattr(versions_mod.settings, "MIN_SUPPORTED_NODE_VERSION", "0.1.0")
    monkeypatch.setattr(versions_mod.settings, "LATEST_NODE_VERSION", "0.2.0")

    result = check_node_version(None)
    assert result["supported"] is True
    assert result["update_available"] == "0.2.0"

    result = check_node_version("")
    assert result["supported"] is True
    assert result["update_available"] == "0.2.0"


def test_check_node_version_too_old_is_unsupported(monkeypatch):
    monkeypatch.setattr(versions_mod.settings, "MIN_SUPPORTED_NODE_VERSION", "0.5.0")
    monkeypatch.setattr(versions_mod.settings, "LATEST_NODE_VERSION", "0.5.0")

    result = check_node_version("0.4.9")
    assert result["supported"] is False
    assert result["min_supported"] == "0.5.0"
    assert result["latest"] == "0.5.0"
    assert result["update_available"] == "0.5.0"
    assert result["parsed"] == "0.4.9"
    assert result["reported"] == "0.4.9"


def test_check_node_version_at_min_is_supported(monkeypatch):
    """Boundary case: reported == MIN must be accepted."""
    monkeypatch.setattr(versions_mod.settings, "MIN_SUPPORTED_NODE_VERSION", "0.1.0")
    monkeypatch.setattr(versions_mod.settings, "LATEST_NODE_VERSION", "0.1.0")

    result = check_node_version("0.1.0")
    assert result["supported"] is True
    assert result["update_available"] is None


def test_check_node_version_outdated_supported(monkeypatch):
    """The common case: above MIN, below LATEST → green light + nudge."""
    monkeypatch.setattr(versions_mod.settings, "MIN_SUPPORTED_NODE_VERSION", "0.1.0")
    monkeypatch.setattr(versions_mod.settings, "LATEST_NODE_VERSION", "0.3.0")

    result = check_node_version("0.2.0")
    assert result["supported"] is True
    assert result["update_available"] == "0.3.0"


def test_check_node_version_at_latest_no_update(monkeypatch):
    monkeypatch.setattr(versions_mod.settings, "MIN_SUPPORTED_NODE_VERSION", "0.1.0")
    monkeypatch.setattr(versions_mod.settings, "LATEST_NODE_VERSION", "0.3.0")

    result = check_node_version("0.3.0")
    assert result["supported"] is True
    assert result["update_available"] is None


def test_check_node_version_ahead_of_latest(monkeypatch):
    """Pre-release dev builds running ahead of LATEST mustn't get a
    confusing 'update to an older version' nudge."""
    monkeypatch.setattr(versions_mod.settings, "MIN_SUPPORTED_NODE_VERSION", "0.1.0")
    monkeypatch.setattr(versions_mod.settings, "LATEST_NODE_VERSION", "0.3.0")

    result = check_node_version("0.4.0")
    assert result["supported"] is True
    assert result["update_available"] is None


def test_check_node_version_garbage_treated_as_zero(monkeypatch):
    """Unparseable strings parse as 0.0.0; we still treat the request as
    'reported but malformed' and apply the version gate (not the unknown
    tolerance) so a bug in the node doesn't bypass the floor."""
    monkeypatch.setattr(versions_mod.settings, "MIN_SUPPORTED_NODE_VERSION", "0.1.0")
    monkeypatch.setattr(versions_mod.settings, "LATEST_NODE_VERSION", "0.1.0")

    result = check_node_version("not.a.version")
    assert result["supported"] is False
    assert result["parsed"] == "0.0.0"
    assert result["reported"] == "not.a.version"
