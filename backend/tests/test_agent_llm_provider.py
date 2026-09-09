"""Tests for the agent's provider layer (app/sentinel_agent/llm.py).

These pin the message translation that moving off the Ollama SDK
introduced. It is worth pinning because every failure mode here is
quiet: a wrongly-shaped message does not raise, it just makes the model
answer as if it never saw the camera frame, or never learns what a tool
returned. The run completes, reports `no_action`, and looks fine.

The three differences that actually bite:

  - Tool results key off ``tool_call_id`` (OpenAI) rather than
    ``tool_name`` (Ollama). Get this wrong and the model cannot match a
    result to the call it made.
  - Tool arguments arrive as a JSON *string*, not a dict.
  - Images cannot ride on a ``tool`` message and must be ``data:`` URIs
    inside a content-parts list — so pruning has to rewrite that list,
    where it used to just pop an ``images`` key.
"""

from types import SimpleNamespace

import pytest

from app.sentinel_agent.llm import (
    assistant_message,
    has_images,
    image_message,
    prune_images,
    tool_call_arguments,
    tool_result_message,
)


def _tool_call(name="view_camera", args='{"camera_id": 3}', call_id="call_abc"):
    return SimpleNamespace(
        id=call_id,
        function=SimpleNamespace(name=name, arguments=args),
    )


# ── arguments ────────────────────────────────────────────────────────


def test_json_string_arguments_are_parsed():
    """OpenAI-shaped providers send a JSON string, not a dict."""
    assert tool_call_arguments(_tool_call()) == {"camera_id": 3}


def test_dict_arguments_pass_through():
    """Ollama-style dicts still work, so the helper is provider-agnostic."""
    assert tool_call_arguments(_tool_call(args={"camera_id": 7})) == {"camera_id": 7}


@pytest.mark.parametrize("bad", ['{"camera_id": ', "", None, "[1,2,3]", '"str"'])
def test_malformed_arguments_degrade_to_empty_dict(bad):
    """A model emitting bad JSON must not kill the run.

    An empty dict lets the tool fail its own validation and hand the
    model an error it can react to; raising here would abort a run that
    had already been paid for.
    """
    assert tool_call_arguments(_tool_call(args=bad)) == {}


# ── tool results ─────────────────────────────────────────────────────


def test_tool_result_is_keyed_by_call_id():
    msg = tool_result_message(_tool_call(call_id="call_xyz"), "2 cameras online")
    assert msg["role"] == "tool"
    assert msg["tool_call_id"] == "call_xyz"
    assert msg["content"] == "2 cameras online"
    # The Ollama-era key would be silently ignored by an OpenAI-shaped
    # provider, leaving the result unattributable to its call.
    assert "tool_name" not in msg


def test_tool_result_handles_empty_output():
    assert tool_result_message(_tool_call(), "")["content"] == ""


# ── images ───────────────────────────────────────────────────────────


def test_images_become_data_uri_parts_on_a_user_message():
    msg = image_message("view_camera", ["QUJD", "REVG"])
    # Must be `user`: an OpenAI-shaped `tool` message cannot carry an image.
    assert msg["role"] == "user"
    parts = msg["content"]
    assert parts[0]["type"] == "text"
    assert "view_camera" in parts[0]["text"]
    urls = [p["image_url"]["url"] for p in parts if p["type"] == "image_url"]
    assert urls == [
        "data:image/jpeg;base64,QUJD",
        "data:image/jpeg;base64,REVG",
    ]


def test_has_images_only_matches_real_image_parts():
    assert has_images(image_message("view_camera", ["QUJD"]))
    assert not has_images({"role": "user", "content": "plain text"})
    assert not has_images({"role": "tool", "content": [{"type": "text", "text": "x"}]})
    assert not has_images("not a dict")


# ── pruning ──────────────────────────────────────────────────────────


def test_pruning_strips_frames_but_keeps_the_text():
    """Frames are re-sent on every subsequent call otherwise — O(N²)
    retransmission that blows the context window mid-run."""
    msg = image_message("view_camera", ["QUJD", "REVG"])
    prune_images(msg)
    assert not has_images(msg)
    assert isinstance(msg["content"], str)
    assert "view_camera" in msg["content"]
    assert "pruned" in msg["content"]


def test_pruning_is_idempotent():
    """The pass walks every prior message each iteration, so a message
    can be visited repeatedly; notices must not stack."""
    msg = image_message("view_camera", ["QUJD"])
    prune_images(msg)
    once = msg["content"]
    prune_images(msg)
    assert msg["content"] == once


def test_pruning_leaves_plain_messages_alone():
    msg = {"role": "user", "content": "no images here"}
    prune_images(msg)
    assert msg["content"] == "no images here"


# ── assistant message ────────────────────────────────────────────────


def test_assistant_message_drops_none_values():
    """Some providers reject `tool_calls: null` on the NEXT request, so a
    null-valued key round-tripping back over the wire is a real failure."""
    class _Msg:
        def model_dump(self):
            return {"role": "assistant", "content": "hi", "tool_calls": None}

    out = assistant_message(_Msg())
    assert out == {"role": "assistant", "content": "hi"}


def test_assistant_message_is_json_serialisable():
    import json

    class _Msg:
        def model_dump(self):
            return {"role": "assistant", "content": "x", "tool_calls": [
                {"id": "c1", "function": {"name": "t", "arguments": "{}"}}
            ]}

    json.dumps(assistant_message(_Msg()))  # must not raise
