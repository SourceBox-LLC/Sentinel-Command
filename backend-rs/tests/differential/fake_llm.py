#!/usr/bin/env python3
"""A scripted LLM that speaks three wires and remembers what it was sent.

The agent's behaviour is a function of what the model returns, so
comparing two agents means giving both the same model. This is it: a
script of turns is loaded, and each completion request pops the next one
and gets it back in whichever wire it arrived on —

  POST /api/chat              Ollama's native chat
  POST /chat/completions      OpenAI Chat Completions (also under /v1)
  POST /v1/messages           Anthropic Messages

A turn is one of:

  {"text": "..."}                                   a final answer
  {"tool_calls": [{"name": ..., "arguments": {...}}]}
  {"tool_calls": [{"name": ..., "raw_arguments": "{not json"}]}
        arguments sent as a literal string — only meaningful on the
        OpenAI wire, where arguments ARE a string and can be malformed
  {"status": 500}                                   a provider failure
  {"sleep": 3.0, ...}                               stall, then answer

Every request is recorded whole. `agent_diff.py` reduces each to a
provider-neutral shape before comparing, because LiteLLM and rig are
different libraries and will not serialise identically — what has to
match is what the model is TOLD, not the JSON it is told in.

Control:
  POST /__script   {"turns": [...]}   load a script, clear the log
  GET  /__log                         every request since the last load

Usage: fake_llm.py [--port 18096]
"""
from __future__ import annotations

import argparse
import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

_lock = threading.Lock()
TURNS: list[dict] = []
LOG: list[dict] = []
COUNTER = [0]

EXHAUSTED = {"text": "SCRIPT EXHAUSTED"}


def next_turn() -> dict:
    with _lock:
        return TURNS.pop(0) if TURNS else dict(EXHAUSTED)


def call_id() -> str:
    with _lock:
        COUNTER[0] += 1
        return f"call_{COUNTER[0]:04d}"


def ollama(turn: dict, model: str) -> dict:
    message: dict = {"role": "assistant", "content": turn.get("text", "")}
    if turn.get("tool_calls"):
        message["tool_calls"] = [
            {"function": {"name": c["name"], "arguments": c.get("arguments", {})}}
            for c in turn["tool_calls"]
        ]
    return {
        "model": model, "created_at": "2026-10-01T00:00:00Z", "message": message,
        "done": True, "done_reason": "stop",
        "total_duration": 1, "load_duration": 1, "prompt_eval_count": 10,
        "prompt_eval_duration": 1, "eval_count": 5, "eval_duration": 1,
    }


def openai(turn: dict, model: str) -> dict:
    message: dict = {"role": "assistant", "content": turn.get("text")}
    if turn.get("tool_calls"):
        message["tool_calls"] = [
            {
                "id": call_id(), "type": "function",
                "function": {
                    "name": c["name"],
                    "arguments": c["raw_arguments"] if "raw_arguments" in c
                    else json.dumps(c.get("arguments", {})),
                },
            }
            for c in turn["tool_calls"]
        ]
    return {
        "id": "chatcmpl-fake", "object": "chat.completion", "created": 1790000000, "model": model,
        "choices": [{
            "index": 0, "message": message,
            "finish_reason": "tool_calls" if turn.get("tool_calls") else "stop",
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
    }


def anthropic(turn: dict, model: str) -> dict:
    content: list[dict] = []
    if turn.get("text"):
        content.append({"type": "text", "text": turn["text"]})
    for c in turn.get("tool_calls") or []:
        content.append({"type": "tool_use", "id": call_id().replace("call_", "toolu_"),
                        "name": c["name"], "input": c.get("arguments", {})})
    return {
        "id": "msg_fake", "type": "message", "role": "assistant", "model": model,
        "content": content,
        "stop_reason": "tool_use" if turn.get("tool_calls") else "end_turn",
        "stop_sequence": None,
        "usage": {"input_tokens": 10, "output_tokens": 5},
    }


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def _body(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length)
        try:
            return json.loads(raw) if raw else {}
        except json.JSONDecodeError:
            return {"__unparseable__": raw.decode("utf-8", "replace")}

    def _send(self, status: int, payload):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.startswith("/__log"):
            with _lock:
                self._send(200, list(LOG))
            return
        self._send(404, {"error": "no route"})

    def do_POST(self):
        body = self._body()
        path = self.path.split("?")[0]
        if path == "/__script":
            with _lock:
                TURNS[:] = body.get("turns") or []
                LOG.clear()
                COUNTER[0] = 0
            self._send(200, {"ok": True, "turns": len(TURNS)})
            return

        wire = None
        if path == "/api/chat":
            wire = "ollama"
        elif path.endswith("/chat/completions"):
            wire = "openai"
        elif path.endswith("/v1/messages"):
            wire = "anthropic"
        if wire is None:
            # Recorded as well, so a client reaching for an endpoint this
            # does not serve shows up in the log rather than as silence.
            with _lock:
                LOG.append({"wire": "UNKNOWN", "path": path, "body": body})
            self._send(404, {"error": f"fake_llm has no route {path}"})
            return

        with _lock:
            LOG.append({
                "wire": wire, "path": path, "body": body,
                # That a credential was presented is behaviour; its value
                # is a fixture constant.
                "authorized": bool(self.headers.get("Authorization") or self.headers.get("x-api-key")),
            })
        turn = next_turn()
        if turn.get("sleep"):
            time.sleep(float(turn["sleep"]))
        if turn.get("status"):
            self._send(int(turn["status"]), {"error": {"message": "scripted provider failure", "type": "server_error"}})
            return
        model = body.get("model", "fake")
        self._send(200, {"ollama": ollama, "openai": openai, "anthropic": anthropic}[wire](turn, model))


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=18096)
    args = ap.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"fake llm on http://127.0.0.1:{args.port}", flush=True)
    server.serve_forever()
    return 0


if __name__ == "__main__":
    sys.exit(main())
