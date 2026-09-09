"""Entry point for the Sentinel AI agent: ``python -m app.sentinel_agent``.

One command serves both places the agent runs:

  - Fly, as the ``agent`` process group of the ``sentinel-command`` app
    (see ``[processes]`` in fly.toml). The web process runs the same
    image with a different command.
  - A self-hosted operator's own box, alongside or apart from their
    Command Center install, typically with ``AGENT_MODE=poll`` so it
    needs no inbound connectivity.

The agent is an ASGI app rather than a bare worker because push mode
receives an HMAC-signed wakeup over HTTP. In poll mode the HTTP surface
is still useful — ``/health`` is what Fly's checks and any external
monitor hit.

PORT is read from the environment so the platform can place it; 8080 is
the historical default from when this ran as its own Fly app, and Fly's
service definition for the agent process group still targets it.
"""

import os

import uvicorn


def main() -> None:
    uvicorn.run(
        "app.sentinel_agent.main:app",
        host=os.getenv("AGENT_HOST", "0.0.0.0"),
        port=int(os.getenv("PORT", "8080")),
        # One worker, deliberately. A drain claims runs from a shared
        # queue via POST /start, so a second worker would race the first
        # for the same rows — and the whole point of the separate process
        # group is that this box does one thing at a time.
        workers=1,
        access_log=False,
    )


if __name__ == "__main__":
    main()
