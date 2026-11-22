#!/usr/bin/env python3
"""MCP (Model Context Protocol) stdio adapter for GPT Mini acoustic calibration.

Exposes bounded acoustic calibration controls over standard I/O (JSON-RPC)
to AI agents without giving direct administrative access or exposing secrets.
"""

from __future__ import annotations

import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Dict, Optional


def log(msg: str) -> None:
    sys.stderr.write(f"[mcp_acoustic_agent] {msg}\n")
    sys.stderr.flush()


class GptMiniAcousticClient:
    def __init__(self, base_url: str, token: str):
        self.base_url = base_url.rstrip("/")
        self.token = token.strip()

    def _request(
        self,
        method: str,
        path: str,
        data: Optional[Dict[str, Any]] = None,
        headers: Optional[Dict[str, str]] = None,
    ) -> Dict[str, Any]:
        url = f"{self.base_url}{path}"
        req_headers = {
            "User-Agent": "gpt-mini-mcp-agent/0.1.0",
            "Accept": "application/json",
        }
        if self.token:
            req_headers["Authorization"] = f"Bearer {self.token}"
        if headers:
            req_headers.update(headers)

        body_bytes = None
        if data is not None:
            body_bytes = json.dumps(data).encode("utf-8")
            req_headers["Content-Type"] = "application/json"

        req = urllib.request.Request(
            url, data=body_bytes, headers=req_headers, method=method
        )
        try:
            with urllib.request.urlopen(req, timeout=10.0) as resp:
                resp_bytes = resp.read()
                if not resp_bytes:
                    return {"status": "ok"}
                return json.loads(resp_bytes.decode("utf-8"))
        except urllib.error.HTTPError as e:
            err_body = e.read().decode("utf-8", errors="replace")
            try:
                err_json = json.loads(err_body)
            except Exception:
                err_json = {"raw_error": err_body}
            return {
                "error": True,
                "http_status": e.code,
                "reason": e.reason,
                "details": err_json,
            }
        except Exception as e:
            return {"error": True, "message": str(e)}

    def get_status(self) -> Dict[str, Any]:
        return self._request("GET", "/api/v1/acoustics")

    def get_events(
        self, since_sample: Optional[int] = None, limit: Optional[int] = None
    ) -> Dict[str, Any]:
        params = {}
        if since_sample is not None:
            params["since_sample"] = str(since_sample)
        if limit is not None:
            params["limit"] = str(limit)
        qs = f"?{urllib.parse.urlencode(params)}" if params else ""
        return self._request("GET", f"/api/v1/acoustics/events{qs}")

    def apply_profile(
        self, settings: Dict[str, Any], expected_revision: int
    ) -> Dict[str, Any]:
        payload = {"expected_revision": expected_revision, "settings": settings}
        headers = {"If-Match": f'"{expected_revision}"'}
        return self._request(
            "POST", "/api/v1/acoustics", data=payload, headers=headers
        )

    def restore_working_profile(self) -> Dict[str, Any]:
        return self._request("POST", "/api/v1/acoustics/restore")

    def set_test_session(self, active: bool) -> Dict[str, Any]:
        return self._request(
            "POST", "/api/v1/acoustics/test_session", data={"active": active}
        )

    def revoke_lease(self, lease_id: str) -> Dict[str, Any]:
        return self._request(
            "POST", "/api/v1/acoustics/lease/revoke", data={"lease_id": lease_id}
        )


TOOLS = [
    {
        "name": "get_acoustic_status",
        "description": "Fetch acoustic calibration schema, current applied/saved settings, revision, and runtime audio telemetry from GPT Mini.",
        "inputSchema": {
            "type": "object",
            "properties": {},
            "additionalProperties": False,
        },
    },
    {
        "name": "get_acoustic_events",
        "description": "Fetch recent acoustic detection, observation, and suppression events from GPT Mini ring buffer.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "since_sample": {
                    "type": "integer",
                    "description": "Optional sample offset cursor to fetch events occurred after this sample",
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of events to return (1-256)",
                },
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "apply_acoustic_profile",
        "description": "Validate and apply complete 33-parameter acoustic calibration profile to GPT Mini. Requires expected_revision matching current revision.",
        "inputSchema": {
            "type": "object",
            "required": ["expected_revision", "settings"],
            "properties": {
                "expected_revision": {
                    "type": "integer",
                    "description": "The exact revision from get_acoustic_status to protect against concurrent modifications",
                },
                "settings": {
                    "type": "object",
                    "description": "The complete dictionary of 33 acoustic configuration parameters",
                },
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "restore_working_profile",
        "description": "Restore applied runtime acoustic settings from saved disk profile on GPT Mini.",
        "inputSchema": {
            "type": "object",
            "properties": {},
            "additionalProperties": False,
        },
    },
    {
        "name": "set_test_session",
        "description": "Toggle acoustic test session mode (shadow detection, no cloud assistant turn, no chime/ducking audio feedback).",
        "inputSchema": {
            "type": "object",
            "required": ["active"],
            "properties": {
                "active": {
                    "type": "boolean",
                    "description": "true to activate test session, false to return to normal assistant mode",
                }
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "revoke_calibration_lease",
        "description": "Revoke the calibration lease immediately, invalidating the calibration identity and stopping active test session.",
        "inputSchema": {
            "type": "object",
            "required": ["lease_id"],
            "properties": {
                "lease_id": {
                    "type": "string",
                    "description": "The calibration lease ID to revoke",
                }
            },
            "additionalProperties": False,
        },
    },
]


def send_response(response: Dict[str, Any]) -> None:
    body = json.dumps(response)
    sys.stdout.write(f"{body}\n")
    sys.stdout.flush()


def handle_request(client: GptMiniAcousticClient, req: Dict[str, Any]) -> None:
    req_id = req.get("id")
    method = req.get("method")
    params = req.get("params", {})

    if method == "initialize":
        send_response(
            {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {
                        "name": "gpt-mini-acoustic-agent",
                        "version": "0.1.0",
                    },
                },
            }
        )
    elif method == "notifications/initialized":
        # No reply for notification
        pass
    elif method == "tools/list":
        send_response(
            {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {"tools": TOOLS},
            }
        )
    elif method == "tools/call":
        tool_name = params.get("name")
        args = params.get("arguments", {})
        result: Dict[str, Any] = {}

        if tool_name == "get_acoustic_status":
            result = client.get_status()
        elif tool_name == "get_acoustic_events":
            result = client.get_events(
                since_sample=args.get("since_sample"),
                limit=args.get("limit"),
            )
        elif tool_name == "apply_acoustic_profile":
            result = client.apply_profile(
                settings=args.get("settings", {}),
                expected_revision=args.get("expected_revision", 0),
            )
        elif tool_name == "restore_working_profile":
            result = client.restore_working_profile()
        elif tool_name == "set_test_session":
            result = client.set_test_session(active=args.get("active", False))
        elif tool_name == "revoke_calibration_lease":
            result = client.revoke_lease(lease_id=args.get("lease_id", ""))
        else:
            send_response(
                {
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {
                        "code": -32601,
                        "message": f"Method not found: {tool_name}",
                    },
                }
            )
            return

        is_error = isinstance(result, dict) and result.get("error", False)
        send_response(
            {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "content": [
                        {
                            "type": "text",
                            "text": json.dumps(result, indent=2, ensure_ascii=False),
                        }
                    ],
                    "isError": is_error,
                },
            }
        )
    else:
        if req_id is not None:
            send_response(
                {
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {
                        "code": -32601,
                        "message": f"Unknown method: {method}",
                    },
                }
            )


def main() -> None:
    base_url = os.environ.get("GPT_MINI_URL", "http://127.0.0.1:8080")
    token = os.environ.get("GPT_MINI_CALIBRATION_TOKEN", "")
    client = GptMiniAcousticClient(base_url=base_url, token=token)
    log(f"Starting MCP stdio server targeting {base_url} (token present: {bool(token)})")

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        # Support Content-Length prefix if client uses header framing
        if line.startswith("Content-Length:"):
            # Skip empty lines until body
            for next_line in sys.stdin:
                if not next_line.strip():
                    break
            line = sys.stdin.readline().strip()
        try:
            req = json.loads(line)
            handle_request(client, req)
        except Exception as e:
            log(f"Error handling request: {e}")


if __name__ == "__main__":
    main()
