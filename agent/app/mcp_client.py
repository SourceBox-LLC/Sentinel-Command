import asyncio
import json
import logging
from contextlib import AsyncExitStack

from mcp import ClientSession
from mcp.client.sse import sse_client
# The MCP SDK renamed this symbol (streamablehttp_client ->
# streamable_http_client). requirements.txt pins `mcp>=1.6.0` with no
# upper bound, so a rebuild silently picks up whichever the current
# release uses — and this import is at module scope, so getting it wrong
# is a hard crash on boot, not a degraded feature. Accept both names so a
# rebuild is not a coin flip. Verified 2026-09-07 against the version a
# fresh build resolves to.
try:
    from mcp.client.streamable_http import streamablehttp_client
except ImportError:  # newer SDK
    from mcp.client.streamable_http import (
        streamable_http_client as streamablehttp_client,
    )

logger = logging.getLogger(__name__)


class MCPClientManager:
    def __init__(self, tool_timeout_seconds: float = 60.0) -> None:
        self._exit_stack = AsyncExitStack()
        self.sessions: dict[str, ClientSession] = {}
        self._tools: dict[str, tuple[str, object]] = {}
        self.tool_timeout_seconds = tool_timeout_seconds

    async def connect(self, servers_config: dict) -> None:
        errors: list[str] = []
        for name, config in servers_config.items():
            try:
                await self._connect_server(name, config)
            except Exception as exc:
                msg = f"MCP server '{name}' at {config.get('url', '?')}: {exc}"
                errors.append(msg)
                logger.error(msg, exc_info=True)
        if errors:
            # Defensive teardown: if some servers connected before another
            # one failed, their transports are already on the exit_stack
            # and `self.sessions`/`self._tools` are partially populated.
            # The caller (_process_one_run) currently always calls
            # disconnect() on raise — but make connect() self-contained
            # so future callers can't leak streamable-HTTP connections
            # by handling the error path differently.
            try:
                await self._exit_stack.aclose()
            except Exception:  # noqa: BLE001
                logger.exception("connect: partial-cleanup aclose failed")
            self.sessions.clear()
            self._tools.clear()
            raise ConnectionError(
                "Failed to connect to MCP servers: " + "; ".join(errors)
            )

    async def _connect_server(self, name: str, config: dict) -> None:
        transport = config.get("transport", "sse")
        url = config["url"]
        headers = config.get("headers", {})

        # Each transport's async context yields a different tuple:
        #   sse_client          → (read_stream, write_stream)
        #   streamablehttp_client → (read_stream, write_stream, get_session_id)
        # Only the first two map to ClientSession's positional args
        # (the third positional is read_timeout_seconds: timedelta).
        # Unpack carefully so the get_session_id callable doesn't get
        # mistaken for a timeout — that misalignment trips
        # ``'function' object has no attribute 'total_seconds'``
        # deep inside the SDK.
        if transport == "sse":
            read_stream, write_stream = await self._exit_stack.enter_async_context(
                sse_client(url=url, headers=headers)
            )
        elif transport == "streamable_http":
            read_stream, write_stream, _get_session_id = (
                await self._exit_stack.enter_async_context(
                    streamablehttp_client(url=url, headers=headers)
                )
            )
        else:
            raise ValueError(f"Unsupported transport: {transport}")

        session = await self._exit_stack.enter_async_context(
            ClientSession(read_stream, write_stream)
        )
        await session.initialize()
        self.sessions[name] = session

        tools_result = await session.list_tools()
        for tool in tools_result.tools:
            self._tools[tool.name] = (name, tool)

        logger.info(
            "Connected to MCP server '%s' — %d tools", name, len(tools_result.tools)
        )

    async def disconnect(self) -> None:
        await self._exit_stack.aclose()
        self.sessions.clear()
        self._tools.clear()

    def get_tools_for_llm(self) -> list[dict]:
        tools = []
        for _tool_name, (_server, tool) in self._tools.items():
            tools.append(
                {
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description or "",
                        "parameters": tool.inputSchema
                        or {"type": "object", "properties": {}},
                    },
                }
            )
        return tools

    async def call_tool(self, tool_name: str, arguments: dict) -> dict:
        """Execute a tool and return {text: str, images: list[str]}.

        images are raw base64 strings (no data URI prefix) for Ollama.
        """
        if tool_name not in self._tools:
            return {
                "text": json.dumps({"error": f"Unknown tool: {tool_name}"}),
                "images": [],
            }

        server_name, _ = self._tools[tool_name]
        session = self.sessions[server_name]

        try:
            # Per-call timeout — without it a stuck MCP tool (e.g. a
            # watch_camera against a node that's mid-restart) hangs the
            # whole agent loop, holding the Fly machine alive until the
            # 270 s wall clock fires.  Errors here surface to the LLM as
            # tool output so the model can decide whether to retry,
            # switch tools, or give up.
            result = await asyncio.wait_for(
                session.call_tool(tool_name, arguments=arguments),
                timeout=self.tool_timeout_seconds,
            )
            text_parts: list[str] = []
            image_parts: list[str] = []
            for content in result.content:
                if hasattr(content, "text"):
                    text_parts.append(content.text)
                elif hasattr(content, "data"):
                    image_parts.append(content.data)
            text = "\n".join(text_parts) if text_parts else "OK (no output)"
            return {"text": text, "images": image_parts}
        except asyncio.TimeoutError:
            logger.warning(
                "Tool call timed out after %.0fs: %s",
                self.tool_timeout_seconds, tool_name,
            )
            return {
                "text": json.dumps({
                    "error": (
                        f"Tool '{tool_name}' timed out after "
                        f"{self.tool_timeout_seconds:.0f}s"
                    ),
                }),
                "images": [],
            }
        except Exception as exc:
            logger.exception("Tool call failed: %s", tool_name)
            return {"text": json.dumps({"error": str(exc)}), "images": []}
