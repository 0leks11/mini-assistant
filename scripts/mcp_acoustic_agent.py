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

    def list_profiles(self, include_archived: bool = False) -> Dict[str, Any]:
        qs = "?include_archived=true" if include_archived else ""
        return self._request("GET", f"/api/v1/acoustics/profiles{qs}")

    def get_profile(self, profile_id: str) -> Dict[str, Any]:
        return self._request("GET", f"/api/v1/acoustics/profiles/{profile_id}")

    def create_profile(
        self,
        name: str,
        settings: Dict[str, Any],
        profile_id: Optional[str] = None,
        notes: Optional[str] = None,
        conditions: Optional[str] = None,
        status: str = "draft",
        idempotency_key: Optional[str] = None,
    ) -> Dict[str, Any]:
        payload = {
            "name": name,
            "settings": settings,
            "status": status,
        }
        if profile_id:
            payload["id"] = profile_id
        if notes:
            payload["notes"] = notes
        if conditions:
            payload["conditions"] = conditions
        headers = {}
        if idempotency_key:
            headers["Idempotency-Key"] = idempotency_key
        return self._request(
            "POST", "/api/v1/acoustics/profiles", data=payload, headers=headers
        )

    def update_profile(
        self,
        profile_id: str,
        name: Optional[str] = None,
        settings: Optional[Dict[str, Any]] = None,
        notes: Optional[str] = None,
        conditions: Optional[str] = None,
        status: Optional[str] = None,
        expected_revision: Optional[int] = None,
    ) -> Dict[str, Any]:
        payload: Dict[str, Any] = {}
        if name is not None:
            payload["name"] = name
        if settings is not None:
            payload["settings"] = settings
        if notes is not None:
            payload["notes"] = notes
        if conditions is not None:
            payload["conditions"] = conditions
        if status is not None:
            payload["status"] = status
        headers = {}
        if expected_revision is not None:
            headers["If-Match"] = f'"{expected_revision}"'
        return self._request(
            "PUT",
            f"/api/v1/acoustics/profiles/{profile_id}",
            data=payload,
            headers=headers,
        )

    def activate_profile(self, profile_id: str) -> Dict[str, Any]:
        return self._request(
            "POST", f"/api/v1/acoustics/profiles/{profile_id}/activate"
        )

    def archive_profile(self, profile_id: str) -> Dict[str, Any]:
        return self._request(
            "POST", f"/api/v1/acoustics/profiles/{profile_id}/archive"
        )

    def preview_candidate_profile(
        self, settings: Dict[str, Any], ttl_secs: int = 30
    ) -> Dict[str, Any]:
        return self._request(
            "POST",
            "/api/v1/acoustics/preview",
            data={"settings": settings, "ttl_secs": ttl_secs},
        )

    def list_samples(
        self,
        include_archived: bool = False,
        split: Optional[str] = None,
        label: Optional[str] = None,
    ) -> Dict[str, Any]:
        params = []
        if include_archived:
            params.append("include_archived=true")
        if split:
            params.append(f"split={split}")
        if label:
            params.append(f"label={label}")
        query = ("?" + "&".join(params)) if params else ""
        return self._request("GET", f"/api/v1/acoustics/samples{query}")

    def get_sample(self, sample_id: str) -> Dict[str, Any]:
        return self._request("GET", f"/api/v1/acoustics/samples/{sample_id}")

    def upload_sample(
        self,
        name: str,
        label: str,
        split: str,
        wav_base64_or_path: str,
        interval: Optional[Dict[str, int]] = None,
        notes: Optional[str] = None,
    ) -> Dict[str, Any]:
        if os.path.isfile(wav_base64_or_path):
            with open(wav_base64_or_path, "rb") as f:
                wav_b64 = base64.b64encode(f.read()).decode("utf-8")
        else:
            wav_b64 = wav_base64_or_path

        payload: Dict[str, Any] = {
            "name": name,
            "label": label,
            "split": split,
            "wav_base64": wav_b64,
        }
        if interval:
            payload["interval"] = interval
        if notes:
            payload["notes"] = notes
        return self._request("POST", "/api/v1/acoustics/samples", data=payload)

    def update_sample(
        self,
        sample_id: str,
        name: Optional[str] = None,
        label: Optional[str] = None,
        split: Optional[str] = None,
        interval: Optional[Dict[str, int]] = None,
        notes: Optional[str] = None,
    ) -> Dict[str, Any]:
        payload: Dict[str, Any] = {}
        if name is not None:
            payload["name"] = name
        if label is not None:
            payload["label"] = label
        if split is not None:
            payload["split"] = split
        if interval is not None:
            payload["interval"] = interval
        if notes is not None:
            payload["notes"] = notes
        return self._request("PUT", f"/api/v1/acoustics/samples/{sample_id}", data=payload)

    def archive_sample(self, sample_id: str) -> Dict[str, Any]:
        return self._request("POST", f"/api/v1/acoustics/samples/{sample_id}/archive")

    def delete_sample(self, sample_id: str) -> Dict[str, Any]:
        return self._request("DELETE", f"/api/v1/acoustics/samples/{sample_id}")


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
    {
        "name": "list_room_profiles",
        "description": "List all saved room profiles on GPT Mini (e.g., 'Гостиная с эхом', 'Спальня с коврами').",
        "inputSchema": {
            "type": "object",
            "properties": {
                "include_archived": {
                    "type": "boolean",
                    "description": "Whether to include archived room profiles",
                }
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "get_room_profile",
        "description": "Retrieve full details and settings for a specific room profile by ID.",
        "inputSchema": {
            "type": "object",
            "required": ["profile_id"],
            "properties": {
                "profile_id": {
                    "type": "string",
                    "description": "The room profile ID",
                }
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "create_room_profile",
        "description": "Create a new named room profile with 33 acoustic parameters. Status can be 'draft' or 'automated_tested'.",
        "inputSchema": {
            "type": "object",
            "required": ["name", "settings"],
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Display name of the room (e.g., 'Гостиная с эхом')",
                },
                "settings": {
                    "type": "object",
                    "description": "Full dictionary of 33 acoustic settings",
                },
                "id": {
                    "type": "string",
                    "description": "Optional custom profile ID (e.g., 'prof_living_room')",
                },
                "notes": {
                    "type": "string",
                    "description": "Optional notes or room acoustic observations",
                },
                "conditions": {
                    "type": "string",
                    "description": "Optional acoustic conditions (e.g., 'echo', 'quiet', 'carpet')",
                },
                "status": {
                    "type": "string",
                    "enum": ["draft", "automated_tested"],
                    "description": "Initial status: 'draft' or 'automated_tested' ('owner_confirmed' is reserved for owner only)",
                },
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "update_room_profile",
        "description": "Update an existing room profile's metadata, settings, or automated test status.",
        "inputSchema": {
            "type": "object",
            "required": ["profile_id"],
            "properties": {
                "profile_id": {
                    "type": "string",
                    "description": "The profile ID to update",
                },
                "name": {"type": "string"},
                "settings": {"type": "object"},
                "notes": {"type": "string"},
                "conditions": {"type": "string"},
                "status": {
                    "type": "string",
                    "enum": ["draft", "automated_tested"],
                },
                "expected_revision": {
                    "type": "integer",
                    "description": "Optimistic concurrency revision check",
                },
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "preview_candidate_profile",
        "description": "Apply a candidate profile temporarily in RAM with a TTL (e.g. 30 seconds). Reverts automatically if not activated.",
        "inputSchema": {
            "type": "object",
            "required": ["settings"],
            "properties": {
                "settings": {
                    "type": "object",
                    "description": "Full dictionary of 33 acoustic settings to test in RAM",
                },
                "ttl_secs": {
                    "type": "integer",
                    "description": "Number of seconds before rolling back (default 30, max 300)",
                },
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "activate_room_profile",
        "description": "Activate a room profile on GPT Mini: applies its settings to audio loop and saves as active working config.",
        "inputSchema": {
            "type": "object",
            "required": ["profile_id"],
            "properties": {
                "profile_id": {
                    "type": "string",
                    "description": "The room profile ID to activate",
                }
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "list_samples",
        "description": "List control audio samples stored in GPT Mini library with optional split or label filtering.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "include_archived": {"type": "boolean", "description": "Include archived samples"},
                "split": {"type": "string", "enum": ["train", "calibration", "held_out"], "description": "Filter by dataset split"},
                "label": {"type": "string", "enum": ["positive", "negative", "own_output", "ambient"], "description": "Filter by sample label"}
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "get_sample",
        "description": "Get metadata and audio metrics for a specific sample ID.",
        "inputSchema": {
            "type": "object",
            "required": ["sample_id"],
            "properties": {
                "sample_id": {"type": "string", "description": "Unique sample ID (e.g. smp_...)"}
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "upload_sample",
        "description": "Upload a validated mono PCM16 16kHz WAV sample (duration <= 60s) to GPT Mini audio library.",
        "inputSchema": {
            "type": "object",
            "required": ["name", "label", "split", "wav_base64_or_path"],
            "properties": {
                "name": {"type": "string", "description": "Human-readable sample name"},
                "label": {"type": "string", "enum": ["positive", "negative", "own_output", "ambient"], "description": "Sample classification label"},
                "split": {"type": "string", "enum": ["train", "calibration", "held_out"], "description": "Target dataset split (held-out is strictly isolated from training)"},
                "wav_base64_or_path": {"type": "string", "description": "Base64-encoded WAV audio data or local file path to WAV file"},
                "interval": {
                    "type": "object",
                    "properties": {
                        "start_ms": {"type": "integer"},
                        "end_ms": {"type": "integer"}
                    },
                    "required": ["start_ms", "end_ms"],
                    "description": "Optional speech interval in milliseconds"
                },
                "notes": {"type": "string", "description": "Optional acoustic notes or recording conditions"}
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "update_sample",
        "description": "Update metadata, label, dataset split, or speech interval of an existing sample.",
        "inputSchema": {
            "type": "object",
            "required": ["sample_id"],
            "properties": {
                "sample_id": {"type": "string", "description": "Unique sample ID"},
                "name": {"type": "string"},
                "label": {"type": "string", "enum": ["positive", "negative", "own_output", "ambient"]},
                "split": {"type": "string", "enum": ["train", "calibration", "held_out"]},
                "interval": {
                    "type": "object",
                    "properties": {
                        "start_ms": {"type": "integer"},
                        "end_ms": {"type": "integer"}
                    },
                    "required": ["start_ms", "end_ms"]
                },
                "notes": {"type": "string"}
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "archive_sample",
        "description": "Mark a sample as archived without permanently deleting it from disk.",
        "inputSchema": {
            "type": "object",
            "required": ["sample_id"],
            "properties": {
                "sample_id": {"type": "string", "description": "Unique sample ID to archive"}
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "delete_sample",
        "description": "Permanently delete an audio sample and its metadata from GPT Mini.",
        "inputSchema": {
            "type": "object",
            "required": ["sample_id"],
            "properties": {
                "sample_id": {"type": "string", "description": "Unique sample ID to permanently remove"}
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
        elif tool_name == "list_room_profiles":
            result = client.list_profiles(
                include_archived=args.get("include_archived", False)
            )
        elif tool_name == "get_room_profile":
            result = client.get_profile(profile_id=args.get("profile_id", ""))
        elif tool_name == "create_room_profile":
            result = client.create_profile(
                name=args.get("name", ""),
                settings=args.get("settings", {}),
                profile_id=args.get("id"),
                notes=args.get("notes"),
                conditions=args.get("conditions"),
                status=args.get("status", "draft"),
                idempotency_key=args.get("idempotency_key"),
            )
        elif tool_name == "update_room_profile":
            result = client.update_profile(
                profile_id=args.get("profile_id", ""),
                name=args.get("name"),
                settings=args.get("settings"),
                notes=args.get("notes"),
                conditions=args.get("conditions"),
                status=args.get("status"),
                expected_revision=args.get("expected_revision"),
            )
        elif tool_name == "preview_candidate_profile":
            result = client.preview_candidate_profile(
                settings=args.get("settings", {}),
                ttl_secs=args.get("ttl_secs", 30),
            )
        elif tool_name == "activate_room_profile":
            result = client.activate_profile(
                profile_id=args.get("profile_id", "")
            )
        elif tool_name == "list_samples":
            result = client.list_samples(
                include_archived=args.get("include_archived", False),
                split=args.get("split"),
                label=args.get("label"),
            )
        elif tool_name == "get_sample":
            result = client.get_sample(sample_id=args.get("sample_id", ""))
        elif tool_name == "upload_sample":
            result = client.upload_sample(
                name=args.get("name", ""),
                label=args.get("label", "positive"),
                split=args.get("split", "train"),
                wav_base64_or_path=args.get("wav_base64_or_path", ""),
                interval=args.get("interval"),
                notes=args.get("notes"),
            )
        elif tool_name == "update_sample":
            result = client.update_sample(
                sample_id=args.get("sample_id", ""),
                name=args.get("name"),
                label=args.get("label"),
                split=args.get("split"),
                interval=args.get("interval"),
                notes=args.get("notes"),
            )
        elif tool_name == "archive_sample":
            result = client.archive_sample(sample_id=args.get("sample_id", ""))
        elif tool_name == "delete_sample":
            result = client.delete_sample(sample_id=args.get("sample_id", ""))
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
        if line.startswith("Content-Length:"):
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
