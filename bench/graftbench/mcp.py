"""A minimal MCP client for the server's `/mcp` route (streamable HTTP,
JSON-RPC), standard library only. The bearer token is read from the token
file and goes only in the Authorization header: never in argv, never
printed."""

from __future__ import annotations

import json
import urllib.error
import urllib.request
from pathlib import Path

PROTOCOL = "2025-06-18"


class McpError(RuntimeError):
    """A transport or JSON-RPC error (never carries the token)."""


class Mcp:
    def __init__(self, url: str, token_file: Path, timeout: float = 60):
        self.url = url.rstrip("/") + "/mcp"
        self._token = Path(token_file).read_text().strip()
        self.timeout = timeout
        self.session: str | None = None
        self._id = 0
        self.tools: dict[str, dict] = {}

    def __repr__(self) -> str:
        return f"Mcp({self.url!r})"

    def _post(self, body: dict) -> tuple[int, dict | None]:
        headers = {
            "Authorization": f"Bearer {self._token}",
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
            "MCP-Protocol-Version": PROTOCOL,
        }
        if self.session:
            headers["Mcp-Session-Id"] = self.session
        req = urllib.request.Request(self.url, data=json.dumps(body).encode(), headers=headers, method="POST")
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as r:
                sid = r.headers.get("Mcp-Session-Id")
                if sid:
                    self.session = sid
                raw = r.read().decode(errors="replace")
                status = r.status
        except urllib.error.HTTPError as e:
            raise McpError(f"HTTP {e.code} from {self.url}") from None
        except OSError as e:
            raise McpError(f"{type(e).__name__} talking to {self.url}") from None
        return status, _decode(raw)

    def rpc(self, method: str, params: dict | None = None) -> dict:
        self._id += 1
        body = {"jsonrpc": "2.0", "id": self._id, "method": method}
        if params is not None:
            body["params"] = params
        _, reply = self._post(body)
        if reply is None:
            raise McpError(f"{method}: empty reply")
        if "error" in reply:
            raise McpError(f"{method}: {reply['error'].get('message', reply['error'])}")
        return reply.get("result", {})

    def initialize(self) -> dict:
        res = self.rpc(
            "initialize",
            {"protocolVersion": PROTOCOL, "capabilities": {}, "clientInfo": {"name": "graftbench", "version": "0"}},
        )
        self._post({"jsonrpc": "2.0", "method": "notifications/initialized"})
        self.tools = {t["name"]: t for t in self.rpc("tools/list").get("tools", [])}
        return res

    def call(self, tool: str, args: dict) -> tuple[bool, dict]:
        """Calls `tool`; returns (is_error, the JSON payload of its result)."""
        res = self.rpc("tools/call", {"name": tool, "arguments": args})
        payload = res.get("structuredContent")
        if payload is None:
            text = "".join(c.get("text", "") for c in res.get("content", []) if c.get("type") == "text")
            try:
                payload = json.loads(text) if text else {}
            except json.JSONDecodeError:
                payload = {"text": text}
        return bool(res.get("isError")), payload


def _decode(raw: str) -> dict | None:
    """A JSON body, or the last JSON-RPC message of an SSE stream."""
    raw = raw.strip()
    if not raw:
        return None
    if raw.startswith("{"):
        return json.loads(raw)
    last = None
    for line in raw.splitlines():
        if line.startswith("data:"):
            data = line[5:].strip()
            if data:
                try:
                    last = json.loads(data)
                except json.JSONDecodeError:
                    pass
    return last
