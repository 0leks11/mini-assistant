//! OpenAPI 3.1 specification generator for GPT Mini Acoustic Calibration API.

use serde_json::{json, Value};

/// Generates a complete, versioned OpenAPI 3.1 schema for GPT Mini Acoustics.
pub fn generate_openapi_spec() -> Value {
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "GPT Mini Acoustic Calibration API",
            "version": "1.0.0",
            "description": "Autonomous acoustic calibration, measurement jobs, profile management and KWS tuning for GPT Mini."
        },
        "servers": [
            {
                "url": "/api/v1/acoustics",
                "description": "GPT Mini Acoustics V1 Base URL"
            }
        ],
        "paths": {
            "/state": {
                "get": {
                    "summary": "Get acoustic runtime state, snapshot and capabilities",
                    "operationId": "get_acoustic_state",
                    "responses": {
                        "200": { "description": "Current snapshot and capabilities" }
                    }
                }
            },
            "/schema": {
                "get": {
                    "summary": "Get OpenAPI and JSON Schema specification",
                    "operationId": "get_openapi_schema",
                    "responses": {
                        "200": { "description": "OpenAPI 3.1 specification" }
                    }
                }
            },
            "/aec": {
                "get": {
                    "summary": "Get live multichannel AEC telemetry (ERLE, DTD state, levels)",
                    "operationId": "get_aec_telemetry",
                    "responses": {
                        "200": { "description": "Live AEC telemetry snapshot" },
                        "503": { "description": "Audio capture loop unavailable" }
                    }
                },
                "post": {
                    "summary": "Enable or disable M-AEC at runtime (with optional owner persistence)",
                    "operationId": "set_aec_state",
                    "responses": {
                        "200": { "description": "Updated AEC telemetry snapshot" }
                    }
                }
            },
            "/profiles": {
                "get": {
                    "summary": "List saved acoustic profiles",
                    "operationId": "list_acoustic_profiles",
                    "responses": {
                        "200": { "description": "List of saved acoustic profiles" }
                    }
                },
                "post": {
                    "summary": "Create a new named acoustic profile",
                    "operationId": "create_acoustic_profile",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": { "$ref": "#/components/schemas/CreateProfileDto" }
                            }
                        }
                    },
                    "responses": {
                        "201": { "description": "Profile created" },
                        "409": { "description": "Profile name already exists or conflict" }
                    }
                }
            },
            "/profiles/{id}": {
                "get": {
                    "summary": "Get profile by ID",
                    "operationId": "get_acoustic_profile",
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": { "200": { "description": "Profile detail" }, "404": { "description": "Not found" } }
                },
                "put": {
                    "summary": "Update profile",
                    "operationId": "update_acoustic_profile",
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": { "200": { "description": "Profile updated" } }
                },
                "delete": {
                    "summary": "Delete profile",
                    "operationId": "delete_acoustic_profile",
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": { "200": { "description": "Profile deleted" } }
                }
            },
            "/profiles/{id}/activate": {
                "post": {
                    "summary": "Activate profile on GPT Mini",
                    "operationId": "activate_acoustic_profile",
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": { "200": { "description": "Profile activated" } }
                }
            },
            "/samples": {
                "get": {
                    "summary": "List stored acoustic audio samples",
                    "operationId": "list_acoustic_samples",
                    "parameters": [
                        { "name": "split", "in": "query", "required": false, "schema": { "type": "string", "enum": ["calibration", "train", "held_out"] } },
                        { "name": "label", "in": "query", "required": false, "schema": { "type": "string", "enum": ["positive", "negative", "own_output", "ambient"] } }
                    ],
                    "responses": { "200": { "description": "List of samples" } }
                },
                "post": {
                    "summary": "Upload a WAV sample",
                    "operationId": "upload_acoustic_sample",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": { "$ref": "#/components/schemas/CreateSampleDto" }
                            }
                        }
                    },
                    "responses": { "201": { "description": "Sample uploaded" } }
                }
            },
            "/samples/{id}": {
                "get": {
                    "summary": "Get sample metadata",
                    "operationId": "get_acoustic_sample",
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": { "200": { "description": "Sample metadata" } }
                },
                "delete": {
                    "summary": "Delete sample",
                    "operationId": "delete_acoustic_sample",
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": { "200": { "description": "Sample deleted" } }
                }
            },
            "/samples/{id}/audio": {
                "get": {
                    "summary": "Download or stream sample WAV audio",
                    "operationId": "get_acoustic_sample_audio",
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": {
                        "200": {
                            "description": "PCM16 mono 16kHz WAV audio stream",
                            "content": { "audio/wav": {} }
                        }
                    }
                }
            },
            "/samples/{id}/spectrogram": {
                "get": {
                    "summary": "Compute 32-band STFT spectrogram and 6-band energy summary for a sample",
                    "operationId": "get_sample_spectrogram",
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": {
                        "200": {
                            "description": "Sample spectrogram (32 bins 0..8000 Hz, 6-band dBFS summary)",
                            "content": { "application/json": {} }
                        }
                    }
                }
            },
            "/sessions": {
                "get": {
                    "summary": "Get active calibration session",
                    "operationId": "get_acoustic_session",
                    "responses": { "200": { "description": "Active session or null" } }
                },
                "post": {
                    "summary": "Create a new bounded acoustic calibration session",
                    "operationId": "create_acoustic_session",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": { "$ref": "#/components/schemas/CreateSessionDto" }
                            }
                        }
                    },
                    "responses": { "201": { "description": "Session created" }, "409": { "description": "Conflict" } }
                }
            },
            "/sessions/{id}/candidate": {
                "post": {
                    "summary": "Apply a candidate profile in RAM during active session",
                    "operationId": "apply_session_candidate",
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": { "$ref": "#/components/schemas/AcousticSettingsDto" }
                            }
                        }
                    },
                    "responses": { "200": { "description": "Candidate applied in RAM" } }
                }
            },
            "/sessions/{id}/restore": {
                "post": {
                    "summary": "Restore baseline settings in active session",
                    "operationId": "restore_session_baseline",
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": { "200": { "description": "Baseline restored" } }
                }
            },
            "/sessions/{id}/confirm_owner": {
                "post": {
                    "summary": "Confirm live owner test for session",
                    "operationId": "confirm_session_owner",
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": { "200": { "description": "Owner confirmed" } }
                }
            },
            "/sessions/{id}/finish": {
                "post": {
                    "summary": "Finish session with optional profile save and activation",
                    "operationId": "finish_acoustic_session",
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": { "200": { "description": "Session completed" } }
                }
            },
            "/sessions/{id}/cancel": {
                "post": {
                    "summary": "Cancel session and restore baseline",
                    "operationId": "cancel_acoustic_session",
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": { "200": { "description": "Session cancelled" } }
                }
            },
            "/jobs": {
                "get": {
                    "summary": "List recent measurement jobs and active job",
                    "operationId": "list_acoustic_jobs",
                    "responses": { "200": { "description": "Jobs list" } }
                },
                "post": {
                    "summary": "Start an acoustic measurement job",
                    "operationId": "start_acoustic_job",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": { "$ref": "#/components/schemas/CreateJobDto" }
                            }
                        }
                    },
                    "responses": { "201": { "description": "Job started" }, "409": { "description": "Conflict" } }
                }
            },
            "/jobs/{id}": {
                "get": {
                    "summary": "Get job detail",
                    "operationId": "get_acoustic_job",
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": { "200": { "description": "Job details" } }
                }
            },
            "/jobs/{id}/report": {
                "get": {
                    "summary": "Get structured job report with frequency response and validity",
                    "operationId": "get_acoustic_job_report",
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": { "200": { "description": "Job report" } }
                }
            },
            "/jobs/{id}/cancel": {
                "post": {
                    "summary": "Cancel running job",
                    "operationId": "cancel_acoustic_job",
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": { "200": { "description": "Job cancelled" } }
                }
            },
            "/evaluate": {
                "post": {
                    "summary": "Evaluate candidate settings against audio corpus",
                    "operationId": "evaluate_acoustic_candidate",
                    "responses": { "200": { "description": "Corpus evaluation report" } }
                }
            },
            "/search": {
                "post": {
                    "summary": "Bounded coordinate search for optimal parameters (up to 20 trials)",
                    "operationId": "search_acoustic_candidates",
                    "responses": { "200": { "description": "Ranked candidates report" } }
                }
            },
            "/cue": {
                "post": {
                    "summary": "Play an audible ready/ack/custom tone cue on GPT Mini built-in speaker",
                    "operationId": "play_acoustic_cue",
                    "responses": { "200": { "description": "Cue played" }, "409": { "description": "Active job running" } }
                }
            },
            "/volume": {
                "get": {
                    "summary": "Get current master speaker volume level (0-10) and Soloist percent",
                    "operationId": "get_speaker_volume",
                    "responses": { "200": { "description": "Current volume state" } }
                },
                "post": {
                    "summary": "Set master speaker volume level (0-10) or Soloist percent (bounded by calibration lease)",
                    "operationId": "set_speaker_volume",
                    "responses": { "200": { "description": "Updated volume state" } }
                }
            },
            "/reference_playback": {
                "get": {
                    "summary": "Get Spotify Soloist reference playback status",
                    "operationId": "get_reference_playback",
                    "responses": { "200": { "description": "Soloist playback status" } }
                },
                "post": {
                    "summary": "Control Spotify Soloist reference playback (play/pause/next/prev/volume)",
                    "operationId": "control_reference_playback",
                    "responses": { "200": { "description": "Updated playback status" } }
                }
            }
        },
        "components": {
            "schemas": {
                "AcousticSettingsDto": {
                    "type": "object",
                    "required": ["threshold", "avg_threshold", "min_scores", "score_mode", "gain_normalizer", "band_pass"],
                    "properties": {
                        "threshold": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
                        "avg_threshold": { "type": "number", "minimum": 0.0, "maximum": 1.0 },
                        "min_scores": { "type": "integer", "minimum": 1, "maximum": 50 },
                        "score_mode": { "type": "string", "enum": ["max", "avg", "median", "p25", "p75", "p80", "p90", "p95"] },
                        "gain_normalizer": { "type": "boolean" },
                        "band_pass": { "type": "boolean" }
                    }
                },
                "CreateProfileDto": {
                    "type": "object",
                    "required": ["name", "settings"],
                    "properties": {
                        "name": { "type": "string" },
                        "description": { "type": "string" },
                        "settings": { "$ref": "#/components/schemas/AcousticSettingsDto" }
                    }
                },
                "CreateSampleDto": {
                    "type": "object",
                    "required": ["name", "label", "split", "wav_base64"],
                    "properties": {
                        "name": { "type": "string" },
                        "label": { "type": "string", "enum": ["positive", "negative", "own_output", "ambient"] },
                        "split": { "type": "string", "enum": ["calibration", "train", "held_out"] },
                        "wav_base64": { "type": "string" },
                        "auto_convert": { "type": "boolean" }
                    }
                },
                "CreateJobDto": {
                    "type": "object",
                    "required": ["stimulus_origin", "output_route", "stimulus"],
                    "properties": {
                        "name": { "type": "string" },
                        "stimulus_origin": { "type": "string", "enum": ["external_user_position", "own_output", "ambient"] },
                        "output_route": { "type": "string", "enum": ["built_in", "aux_out", "none"] },
                        "stimulus": { "type": "object" },
                        "volume_pct": { "type": "integer", "minimum": 0, "maximum": 100 },
                        "cue_before": { "type": "boolean" },
                        "cue_after": { "type": "boolean" }
                    }
                },
                "CreateSessionDto": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "max_duration_secs": { "type": "integer" },
                        "max_jobs": { "type": "integer" },
                        "require_aux": { "type": "boolean" },
                        "notes": { "type": "string" }
                    }
                }
            }
        }
    })
}
