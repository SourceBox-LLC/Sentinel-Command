"""LLM access for the agent, via LiteLLM.

One provider path for every model. LiteLLM speaks Ollama natively
(``ollama_chat/<model>``), so today's hosted model, a self-hoster's
bring-your-own-key provider, and eventually SourceBox's own model are all
selected by a single ``LLM_MODEL`` string rather than by swapping client
libraries.

This module owns ALL provider-shaped knowledge. The agent loop builds
messages exclusively through the helpers below, so if the message format
ever changes again it changes here and nowhere else. That mattered when
this moved off the Ollama SDK: Ollama and OpenAI-shaped APIs disagree
about more than field names.

  - Tool results: Ollama keys them by ``tool_name``, OpenAI by
    ``tool_call_id`` matched to the id it issued on the call.
  - Tool arguments: Ollama hands back a dict, OpenAI a JSON *string*.
  - Images: Ollama takes raw base64 in an ``images`` list on any message.
    OpenAI-shaped APIs cannot carry an image on a ``tool`` message at
    all — it has to be a ``user`` message whose content is a list of
    parts, with the image as a ``data:`` URI. That is a structural
    difference, not a formatting one, and it is why the pruning helper
    below has to rewrite a content list rather than pop a key.
"""

import asyncio
import json
import logging
from typing import Any

import litellm

from app.sentinel_agent.config import Settings

logger = logging.getLogger(__name__)

# LiteLLM chats to stdout about provider quirks on import and on first
# call; the agent's logs are read during incidents and this is noise.
litellm.suppress_debug_info = True

# Marker left on a message whose frames were pruned, so the pruning pass
# is idempotent and a re-prune doesn't stack notices.
_PRUNED_NOTE = "[frames pruned — superseded by newer visual output]"


class LLMProvider:
    def __init__(self, settings: Settings) -> None:
        self.model = settings.resolved_llm_model
        self.api_key = settings.resolved_llm_api_key
        self.api_base = settings.resolved_llm_api_base
        self.max_tokens = settings.max_tokens
        self.timeout_seconds = settings.llm_call_timeout_seconds

    async def chat(
        self,
        messages: list[dict],
        tools: list[dict] | None = None,
    ):
        """Send a chat request and return the assistant message.

        Still wrapped in ``asyncio.wait_for`` despite LiteLLM taking its
        own ``timeout``: that one bounds the HTTP request, not the whole
        call, so a provider that accepts the connection and then stalls
        mid-stream would otherwise hold the machine alive until the
        270 s wall clock in process_with_timeout fires — burning Fly
        minutes and stranding the run in `running` on Command Center.
        """
        kwargs: dict[str, Any] = {
            "model": self.model,
            "messages": messages,
            "max_tokens": self.max_tokens,
            "timeout": self.timeout_seconds,
        }
        if tools:
            kwargs["tools"] = tools
        if self.api_key:
            kwargs["api_key"] = self.api_key
        if self.api_base:
            kwargs["api_base"] = self.api_base

        response = await asyncio.wait_for(
            litellm.acompletion(**kwargs),
            timeout=self.timeout_seconds,
        )
        return response.choices[0].message


# ── Message construction (the only place the wire format is known) ────


def assistant_message(response_msg) -> dict:
    """Convert the provider's assistant message into a plain dict.

    Appended verbatim to the running `messages` list, so it has to be
    JSON-serialisable — a provider object round-trips inconsistently
    once it goes back over the wire.
    """
    if hasattr(response_msg, "model_dump"):
        msg = response_msg.model_dump()
    else:  # pragma: no cover - defensive
        msg = dict(response_msg)
    # Drop null-valued keys some providers include; a `tool_calls: null`
    # is rejected by others on the next request.
    return {k: v for k, v in msg.items() if v is not None}


def tool_call_arguments(tool_call) -> dict:
    """Arguments for a tool call, always as a dict.

    OpenAI-shaped APIs return a JSON *string* here. A model can emit
    malformed JSON, and that must not take the whole run down — an empty
    dict lets the tool fail on its own validation with a message the
    model can actually react to.
    """
    raw = tool_call.function.arguments
    if isinstance(raw, dict):
        return raw
    if not raw:
        return {}
    try:
        parsed = json.loads(raw)
    except (TypeError, ValueError):
        logger.warning(
            "llm: tool %s had unparseable arguments: %.200r",
            tool_call.function.name, raw,
        )
        return {}
    return parsed if isinstance(parsed, dict) else {}


def tool_result_message(tool_call, text: str) -> dict:
    """The tool's textual result, keyed back to the call that asked."""
    return {
        "role": "tool",
        "tool_call_id": tool_call.id,
        "name": tool_call.function.name,
        "content": text or "",
    }


def image_message(tool_name: str, images: list[str]) -> dict:
    """Frames from a tool, as a user message with image parts.

    A separate message because an OpenAI-shaped ``tool`` message cannot
    carry an image. ``images`` are raw base64 (no prefix) as returned by
    the MCP layer; the ``data:`` URI is applied here so the MCP client
    stays provider-neutral.
    """
    parts: list[dict] = [
        {"type": "text", "text": f"[Visual output from {tool_name}]"}
    ]
    for b64 in images:
        parts.append({
            "type": "image_url",
            "image_url": {"url": f"data:image/jpeg;base64,{b64}"},
        })
    return {"role": "user", "content": parts}


def has_images(message: Any) -> bool:
    if not isinstance(message, dict):
        return False
    content = message.get("content")
    if not isinstance(content, list):
        return False
    return any(
        isinstance(p, dict) and p.get("type") == "image_url" for p in content
    )


def prune_images(message: dict) -> None:
    """Strip image parts, collapsing the message back to its text.

    Mutates in place, matching how the caller walks the message list.
    Idempotent — a message already pruned is left alone.
    """
    content = message.get("content")
    if not isinstance(content, list):
        return
    texts = [
        p.get("text", "") for p in content
        if isinstance(p, dict) and p.get("type") == "text"
    ]
    joined = " ".join(t for t in texts if t).strip()
    if _PRUNED_NOTE in joined:
        message["content"] = joined
        return
    message["content"] = f"{joined} {_PRUNED_NOTE}".strip()
