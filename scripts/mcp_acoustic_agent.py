#!/usr/bin/env python3
"""MCP (Model Context Protocol) stdio adapter for GPT Mini acoustic calibration.

Exposes bounded acoustic calibration controls over standard I/O (JSON-RPC)
to AI agents without giving direct administrative access or exposing secrets.
"""

from __future__ import annotations

import base64
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
    def __init__(self, base_url: str, token: str, timeout: float = 60.0):
        self.base_url = base_url.rstrip("/")
        self.token = token.strip()
        self.timeout = timeout

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
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
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
                "status_code": e.code,
                "reason": e.reason,
                "details": err_json,
            }
        except Exception as e:
            return {"error": True, "message": str(e)}

    def get_status(self) -> Dict[str, Any]:
        return self._request("GET", "/api/v1/acoustics")

    def get_aec_telemetry(self) -> Dict[str, Any]:
        return self._request("GET", "/api/v1/acoustics/aec")

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
        wav_base64_or_path: Optional[str] = None,
        wav_bytes: Optional[bytes] = None,
        interval: Optional[Dict[str, int]] = None,
        notes: Optional[str] = None,
    ) -> Dict[str, Any]:
        if wav_bytes is not None:
            wav_b64 = base64.b64encode(wav_bytes).decode("utf-8")
        elif wav_base64_or_path and os.path.isfile(wav_base64_or_path):
            with open(wav_base64_or_path, "rb") as f:
                wav_b64 = base64.b64encode(f.read()).decode("utf-8")
        else:
            wav_b64 = wav_base64_or_path or ""

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

    def list_acoustic_jobs(self) -> Dict[str, Any]:
        return self._request("GET", "/api/v1/acoustics/jobs")

    def start_acoustic_job(
        self,
        stimulus_origin: str,
        output_route: str,
        stimulus: Dict[str, Any],
        pre_roll_ms: int = 500,
        post_roll_ms: int = 1000,
        volume_pct: int = 40,
        name: Optional[str] = None,
        idempotency_key: Optional[str] = None,
    ) -> Dict[str, Any]:
        payload: Dict[str, Any] = {
            "stimulus_origin": stimulus_origin,
            "output_route": output_route,
            "stimulus": stimulus,
            "pre_roll_ms": pre_roll_ms,
            "post_roll_ms": post_roll_ms,
            "volume_pct": volume_pct,
        }
        if name is not None:
            payload["name"] = name
        headers = {}
        if idempotency_key:
            headers["Idempotency-Key"] = idempotency_key
        return self._request(
            "POST", "/api/v1/acoustics/jobs", data=payload, headers=headers if headers else None
        )

    def get_acoustic_job(self, job_id: str) -> Dict[str, Any]:
        return self._request("GET", f"/api/v1/acoustics/jobs/{job_id}")

    def cancel_acoustic_job(
        self, job_id: str, reason: str = "Cancelled by agent"
    ) -> Dict[str, Any]:
        return self._request(
            "POST", f"/api/v1/acoustics/jobs/{job_id}/cancel", data={"reason": reason}
        )

    def get_acoustic_job_events(self, job_id: str) -> Dict[str, Any]:
        return self._request("GET", f"/api/v1/acoustics/jobs/{job_id}/events")

    def get_acoustic_job_report(self, job_id: str) -> Dict[str, Any]:
        return self._request("GET", f"/api/v1/acoustics/jobs/{job_id}/report")

    def evaluate_acoustic_candidate(
        self,
        settings: Optional[Dict[str, Any]] = None,
        split: str = "calibration",
    ) -> Dict[str, Any]:
        payload: Dict[str, Any] = {"split": split}
        if settings is not None:
            payload["settings"] = settings
        return self._request("POST", "/api/v1/acoustics/evaluate", data=payload)

    def search_acoustic_candidates(
        self,
        baseline: Optional[Dict[str, Any]] = None,
        split: str = "calibration",
        max_trials: int = 10,
    ) -> Dict[str, Any]:
        payload: Dict[str, Any] = {
            "split": split,
            "max_trials": max_trials,
        }
        if baseline is not None:
            payload["baseline"] = baseline
        return self._request("POST", "/api/v1/acoustics/search", data=payload)

    def get_openapi_schema(self) -> Dict[str, Any]:
        return self._request("GET", "/api/v1/acoustics/openapi.json")

    def get_acoustic_sample_audio(self, sample_id: str) -> Dict[str, Any]:
        url = f"{self.base_url}/api/v1/acoustics/samples/{sample_id}/audio"
        req = urllib.request.Request(url, method="GET")
        if self.token:
            req.add_header("Authorization", f"Bearer {self.token}")
        with urllib.request.urlopen(req, timeout=self.timeout) as resp:
            data = resp.read()
            return {
                "sample_id": sample_id,
                "bytes_len": len(data),
                "content_type": resp.headers.get("Content-Type", "audio/wav"),
                "base64": base64.b64encode(data).decode("ascii"),
            }

    def create_acoustic_session(
        self,
        name: Optional[str] = None,
        max_duration_secs: Optional[int] = None,
        max_jobs: Optional[int] = None,
        require_aux: Optional[bool] = None,
        notes: Optional[str] = None,
    ) -> Dict[str, Any]:
        payload: Dict[str, Any] = {}
        if name is not None:
            payload["name"] = name
        if max_duration_secs is not None:
            payload["max_duration_secs"] = max_duration_secs
        if max_jobs is not None:
            payload["max_jobs"] = max_jobs
        if require_aux is not None:
            payload["require_aux"] = require_aux
        if notes is not None:
            payload["notes"] = notes
        return self._request("POST", "/api/v1/acoustics/sessions", data=payload)

    def get_acoustic_session(self, session_id: Optional[str] = None) -> Dict[str, Any]:
        path = f"/api/v1/acoustics/sessions/{session_id}" if session_id else "/api/v1/acoustics/sessions"
        return self._request("GET", path)

    def apply_session_candidate(
        self, session_id: str, settings: Dict[str, Any]
    ) -> Dict[str, Any]:
        return self._request(
            "POST",
            f"/api/v1/acoustics/sessions/{session_id}/candidate",
            data={"settings": settings},
        )

    def restore_session_baseline(self, session_id: str) -> Dict[str, Any]:
        return self._request(
            "POST", f"/api/v1/acoustics/sessions/{session_id}/restore", data={}
        )

    def confirm_session_owner(
        self, session_id: str, notes: Optional[str] = None
    ) -> Dict[str, Any]:
        payload = {"notes": notes} if notes else {}
        return self._request(
            "POST",
            f"/api/v1/acoustics/sessions/{session_id}/confirm_owner",
            data=payload,
        )

    def finish_acoustic_session(
        self,
        session_id: str,
        owner_confirmed: bool = True,
        save_profile_name: Optional[str] = None,
        activate_profile: bool = False,
        notes: Optional[str] = None,
    ) -> Dict[str, Any]:
        payload: Dict[str, Any] = {
            "owner_confirmed": owner_confirmed,
            "activate_profile": activate_profile,
        }
        if save_profile_name is not None:
            payload["save_profile_name"] = save_profile_name
        if notes is not None:
            payload["notes"] = notes
        return self._request(
            "POST", f"/api/v1/acoustics/sessions/{session_id}/finish", data=payload
        )

    def cancel_acoustic_session(self, session_id: str) -> Dict[str, Any]:
        return self._request(
            "POST", f"/api/v1/acoustics/sessions/{session_id}/cancel", data={}
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
    {
        "name": "list_acoustic_jobs",
        "description": "List current active acoustic job (if running) and recent job history on GPT Mini.",
        "inputSchema": {
            "type": "object",
            "properties": {},
            "additionalProperties": False,
        },
    },
    {
        "name": "start_acoustic_job",
        "description": "Start a synchronized acoustic test job: captures pre-roll background, emits stimulus, drains playback, and captures reverberation tail. Only 1 job can be active at a time.",
        "inputSchema": {
            "type": "object",
            "required": ["stimulus_origin", "output_route", "stimulus"],
            "properties": {
                "stimulus_origin": {
                    "type": "string",
                    "enum": ["external_user_position", "own_output", "ambient"],
                    "description": "Origin of sound stimulus: external speaker (at 2m user position), own speaker (barge-in / self-interruption test), or ambient noise",
                },
                "output_route": {
                    "type": "string",
                    "enum": ["built_in", "aux_out", "none"],
                    "description": "Output route: built_in speaker, aux_out (3.5mm line jack), or none",
                },
                "stimulus": {
                    "type": "object",
                    "description": "Stimulus signal: sample ({type: 'sample', sample_id: '...'}), chirp_sweep ({type: 'chirp_sweep', start_hz: 200, end_hz: 2000, duration_ms: 1000, volume_dbfs: -12.0}), or silence ({type: 'silence', duration_ms: 1000})",
                },
                "pre_roll_ms": {
                    "type": "integer",
                    "description": "Duration of microphone capture before playback starts (100-2000 ms, default 500)",
                },
                "post_roll_ms": {
                    "type": "integer",
                    "description": "Duration of reverberation tail capture after playback finishes (100-5000 ms, default 1000)",
                },
                "volume_pct": {
                    "type": "integer",
                    "description": "Volume percentage for playback (0-80%, default 40)",
                },
                "name": {
                    "type": "string",
                    "description": "Optional friendly job name",
                },
                "idempotency_key": {
                    "type": "string",
                    "description": "Optional idempotency key to prevent duplicate playback runs",
                },
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "get_acoustic_job",
        "description": "Fetch status, timestamps, captured audio metrics, and KWS detection count for an acoustic job.",
        "inputSchema": {
            "type": "object",
            "required": ["job_id"],
            "properties": {
                "job_id": {"type": "string", "description": "Unique acoustic job ID"},
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "cancel_acoustic_job",
        "description": "Immediately cancel a running acoustic job, stopping audio playback and microphone capture.",
        "inputSchema": {
            "type": "object",
            "required": ["job_id"],
            "properties": {
                "job_id": {"type": "string", "description": "Unique acoustic job ID to cancel"},
                "reason": {"type": "string", "description": "Optional cancellation reason"},
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "get_acoustic_job_events",
        "description": "Get raw KWS observation and detection events occurred during a specific acoustic job.",
        "inputSchema": {
            "type": "object",
            "required": ["job_id"],
            "properties": {
                "job_id": {"type": "string", "description": "Unique acoustic job ID"},
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "get_acoustic_job_report",
        "description": "Fetch detailed measurement report for a completed acoustic job (linking stimulus, captured audio validity, recall, false alarms, sub-band frequency response, and timeline events).",
        "inputSchema": {
            "type": "object",
            "required": ["job_id"],
            "properties": {
                "job_id": {"type": "string", "description": "Unique acoustic job ID"},
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "evaluate_acoustic_candidate",
        "description": "Evaluate an acoustic configuration candidate on a fixed sample corpus (calibration/train/held_out), computing recall, miss rate, false alarms per hour, own-output leakage, duplicate triggers, and composite score.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "settings": {
                    "type": "object",
                    "description": "Candidate acoustic settings (threshold, gain_ref, score_mode, band_pass, etc.)",
                },
                "split": {
                    "type": "string",
                    "enum": ["calibration", "train", "held_out"],
                    "description": "Corpus split to evaluate against (default: calibration)",
                },
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "search_acoustic_candidates",
        "description": "Run bounded parameter grid/coordinate search (max 20 trials) on calibration corpus to find optimal threshold, gain_ref, score_mode, and band_pass settings.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "baseline": {
                    "type": "object",
                    "description": "Optional baseline settings to search around",
                },
                "split": {
                    "type": "string",
                    "enum": ["calibration", "train"],
                    "description": "Corpus split to optimize on (default: calibration)",
                },
                "max_trials": {
                    "type": "integer",
                    "description": "Maximum candidate evaluations to run (1-20, default 10)",
                },
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "get_openapi_schema",
        "description": "Fetch complete versioned OpenAPI 3.1 specification for GPT Mini Acoustic Calibration API.",
        "inputSchema": {
            "type": "object",
            "properties": {},
            "additionalProperties": False,
        },
    },
    {
        "name": "get_acoustic_sample_audio",
        "description": "Download or inspect WAV audio bytes (base64 encoded) for a given sample ID.",
        "inputSchema": {
            "type": "object",
            "required": ["sample_id"],
            "properties": {
                "sample_id": {"type": "string", "description": "Sample ID to fetch audio for"},
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "create_acoustic_session",
        "description": "Create a new bounded acoustic calibration session with baseline recovery record. Fails with 409 if active.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Optional friendly session name"},
                "max_duration_secs": {"type": "integer", "description": "Session timeout limit in seconds (default 1800)"},
                "max_jobs": {"type": "integer", "description": "Maximum jobs allowed in session (default 20)"},
                "require_aux": {"type": "boolean", "description": "Whether AUX output route is strictly required"},
                "notes": {"type": "string", "description": "Optional session notes"},
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "get_acoustic_session",
        "description": "Fetch active acoustic calibration session details and baseline info.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "session_id": {"type": "string", "description": "Optional session ID (defaults to active session)"},
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "apply_session_candidate",
        "description": "Apply a temporary candidate profile in RAM during an active session with ACK verification.",
        "inputSchema": {
            "type": "object",
            "required": ["session_id", "settings"],
            "properties": {
                "session_id": {"type": "string", "description": "Active session ID"},
                "settings": {"type": "object", "description": "Candidate acoustic settings dictionary"},
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "restore_session_baseline",
        "description": "Restore baseline settings in RAM during an active calibration session.",
        "inputSchema": {
            "type": "object",
            "required": ["session_id"],
            "properties": {
                "session_id": {"type": "string", "description": "Active session ID"},
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "confirm_session_owner",
        "description": "Confirm live owner test of the acoustic candidate.",
        "inputSchema": {
            "type": "object",
            "required": ["session_id"],
            "properties": {
                "session_id": {"type": "string", "description": "Active session ID"},
                "notes": {"type": "string", "description": "Optional notes from owner verification"},
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "finish_acoustic_session",
        "description": "Finish active session, with optional profile saving and activation.",
        "inputSchema": {
            "type": "object",
            "required": ["session_id"],
            "properties": {
                "session_id": {"type": "string", "description": "Active session ID"},
                "owner_confirmed": {"type": "boolean", "description": "Whether owner confirmed live test"},
                "save_profile_name": {"type": "string", "description": "Optional name to save candidate as named profile"},
                "activate_profile": {"type": "boolean", "description": "Whether to activate saved profile"},
                "notes": {"type": "string", "description": "Optional closing notes"},
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "cancel_acoustic_session",
        "description": "Cancel active session, restore baseline settings, and remove crash recovery record.",
        "inputSchema": {
            "type": "object",
            "required": ["session_id"],
            "properties": {
                "session_id": {"type": "string", "description": "Active session ID"},
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "get_aec_telemetry",
        "description": "Get live multichannel AEC telemetry (ERLE dB, DTD status, Ref/Mic/Err dBFS, active config).",
        "inputSchema": {
            "type": "object",
            "properties": {},
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

        if tool_name in ("get_acoustic_status", "get_acoustic_state"):
            result = client.get_status()
        elif tool_name == "get_aec_telemetry":
            result = client.get_aec_telemetry()
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
        elif tool_name in ("set_test_session", "set_acoustic_test_session"):
            result = client.set_test_session(active=args.get("active", False))
        elif tool_name == "revoke_calibration_lease":
            result = client.revoke_lease(lease_id=args.get("lease_id", ""))
        elif tool_name in ("list_room_profiles", "list_acoustic_profiles"):
            result = client.list_profiles(
                include_archived=args.get("include_archived", False)
            )
        elif tool_name in ("get_room_profile", "get_acoustic_profile"):
            result = client.get_profile(profile_id=args.get("profile_id", ""))
        elif tool_name in ("create_room_profile", "create_acoustic_profile"):
            result = client.create_profile(
                name=args.get("name", ""),
                settings=args.get("settings", {}),
                profile_id=args.get("id"),
                notes=args.get("notes"),
                conditions=args.get("conditions"),
                status=args.get("status", "draft"),
                idempotency_key=args.get("idempotency_key"),
            )
        elif tool_name in ("update_room_profile", "update_acoustic_profile"):
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
        elif tool_name in ("activate_room_profile", "activate_acoustic_profile"):
            result = client.activate_profile(
                profile_id=args.get("profile_id", "")
            )
        elif tool_name in ("list_samples", "list_acoustic_samples"):
            result = client.list_samples(
                include_archived=args.get("include_archived", False),
                split=args.get("split"),
                label=args.get("label"),
            )
        elif tool_name in ("get_sample", "get_acoustic_sample"):
            result = client.get_sample(sample_id=args.get("sample_id", ""))
        elif tool_name in ("upload_sample", "upload_acoustic_sample"):
            wav_arg = args.get("wav_base64") or args.get("wav_base64_or_path") or ""
            result = client.upload_sample(
                name=args.get("name", ""),
                label=args.get("label", "positive"),
                split=args.get("split", "train"),
                wav_base64_or_path=wav_arg,
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
        elif tool_name in ("delete_sample", "delete_acoustic_sample"):
            result = client.delete_sample(sample_id=args.get("sample_id", ""))
        elif tool_name == "list_acoustic_jobs":
            result = client.list_acoustic_jobs()
        elif tool_name == "start_acoustic_job":
            result = client.start_acoustic_job(
                stimulus_origin=args.get("stimulus_origin", ""),
                output_route=args.get("output_route", ""),
                stimulus=args.get("stimulus", {}),
                pre_roll_ms=args.get("pre_roll_ms", 500),
                post_roll_ms=args.get("post_roll_ms", 1000),
                volume_pct=args.get("volume_pct", 40),
                name=args.get("name"),
                idempotency_key=args.get("idempotency_key"),
            )
        elif tool_name == "get_acoustic_job":
            result = client.get_acoustic_job(job_id=args.get("job_id", ""))
        elif tool_name == "cancel_acoustic_job":
            result = client.cancel_acoustic_job(
                job_id=args.get("job_id", ""),
                reason=args.get("reason", "Cancelled by agent"),
            )
        elif tool_name == "get_acoustic_job_events":
            result = client.get_acoustic_job_events(job_id=args.get("job_id", ""))
        elif tool_name == "get_acoustic_job_report":
            result = client.get_acoustic_job_report(job_id=args.get("job_id", ""))
        elif tool_name == "evaluate_acoustic_candidate":
            result = client.evaluate_acoustic_candidate(
                settings=args.get("settings"),
                split=args.get("split", "calibration"),
            )
        elif tool_name == "search_acoustic_candidates":
            result = client.search_acoustic_candidates(
                baseline=args.get("baseline"),
                split=args.get("split", "calibration"),
                max_trials=args.get("max_trials", 10),
            )
        elif tool_name == "get_openapi_schema":
            result = client.get_openapi_schema()
        elif tool_name == "get_acoustic_sample_audio":
            result = client.get_acoustic_sample_audio(sample_id=args.get("sample_id", ""))
        elif tool_name == "create_acoustic_session":
            result = client.create_acoustic_session(
                name=args.get("name"),
                max_duration_secs=args.get("max_duration_secs"),
                max_jobs=args.get("max_jobs"),
                require_aux=args.get("require_aux"),
                notes=args.get("notes"),
            )
        elif tool_name == "get_acoustic_session":
            result = client.get_acoustic_session(session_id=args.get("session_id"))
        elif tool_name == "apply_session_candidate":
            result = client.apply_session_candidate(
                session_id=args.get("session_id", ""),
                settings=args.get("settings", {}),
            )
        elif tool_name == "restore_session_baseline":
            result = client.restore_session_baseline(session_id=args.get("session_id", ""))
        elif tool_name == "confirm_session_owner":
            result = client.confirm_session_owner(
                session_id=args.get("session_id", ""),
                notes=args.get("notes"),
            )
        elif tool_name == "finish_acoustic_session":
            result = client.finish_acoustic_session(
                session_id=args.get("session_id", ""),
                owner_confirmed=args.get("owner_confirmed", True),
                save_profile_name=args.get("save_profile_name"),
                activate_profile=args.get("activate_profile", False),
                notes=args.get("notes"),
            )
        elif tool_name == "cancel_acoustic_session":
            result = client.cancel_acoustic_session(session_id=args.get("session_id", ""))
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
    timeout_str = os.environ.get("GPT_MINI_TIMEOUT", "60.0")
    timeout = float(timeout_str) if timeout_str else 60.0
    client = GptMiniAcousticClient(base_url=base_url, token=token, timeout=timeout)
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
