import asyncio
import logging

from ollama import AsyncClient

from app.config import Settings

logger = logging.getLogger(__name__)


class LLMProvider:
    def __init__(self, settings: Settings) -> None:
        self.client = AsyncClient(
            host=settings.ollama_host,
            headers={"Authorization": f"Bearer {settings.ollama_api_key}"},
        )
        self.model = settings.ollama_model
        self.max_tokens = settings.max_tokens
        self.timeout_seconds = settings.llm_call_timeout_seconds

    async def chat(
        self,
        messages: list[dict],
        tools: list[dict] | None = None,
    ):
        """Send a chat request and return the response message.

        Wrapped in asyncio.wait_for because the Ollama AsyncClient does
        not honour a per-call timeout and a hung connection would
        otherwise block until the wakeup's 270 s wall clock fires —
        wasting Fly minutes and stranding the in-flight run.
        """
        response = await asyncio.wait_for(
            self.client.chat(
                model=self.model,
                messages=messages,
                tools=tools or None,
                options={"num_predict": self.max_tokens},
            ),
            timeout=self.timeout_seconds,
        )
        return response.message
