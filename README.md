# mini-assistant

Lightweight, fully autonomous Rust daemon running directly on the Yandex Station Mini 1 (Amlogic A113X, ARM32 / armhf).

It completely replaces the original proprietary userspace software: **0% Yandex code, 100% open-source custom Rust stack**.

---

## What It Does

| Capability | Implementation | Details |
|---|---|---|
| **Audio Capture** | Direct ALSA (`hw:0,2`) | 8-channel PDM microphone capture decimated to 16 kHz mono PCM |
| **Audio Playback** | ALSA `default` / `softvol` | Clean audio playback with dynamic ducking and barge-in |
| **Wake Word** | Rustpotter KWS + SimpleVad | Local wake-word detection with browser-based interactive training wizard |
| **Voice AI** | Gemini Live API & OpenAI Realtime | Full-duplex WebSocket streaming (24 kHz PCM) with instant interruption |
| **Music Playback** | Spotify Connect / Soloist | Direct hardware playback with acoustic ducking during conversation |
| **Smart Home** | Home Assistant REST API | Voice tool-calling for lights, climate, switches, and sensors |
| **Control Center** | On-device Web UI & Telegram Mini App | Port 8080 zero-dependency control panel for voice, model, volume, and LED |
| **Gesture Sensor** | ST VL53L0X ToF radar | Physical hand distance measurement for volume and gesture control |
| **Visual Feedback**| SN3236 RGB LED matrix | Dynamic status indication (listening, speaking, music, alarm) |

---

## Project Structure

```text
mini-assistant/
├── src/
│   ├── main.rs              # Application entry point and supervisor loops
│   ├── audio/               # ALSA capture, playback, volume, VL53 gesture reader
│   ├── gemini/              # Google Gemini Live API bidirectional WebSocket client
│   ├── openai/              # OpenAI Realtime API bidirectional client
│   ├── voice/               # Multi-provider voice abstraction and tool dispatch
│   ├── wakeword/            # KWS engine, VAD, and web-based enrollment wizard
│   ├── spotify/             # Spotify Web API and Soloist playback supervisor
│   ├── ha/                  # Home Assistant REST client and tool definitions
│   ├── alarms/              # Local alarm clock and persistent scheduler
│   ├── weather/             # Open-Meteo weather integration
│   ├── web/                 # Built-in Web UI & Telegram Mini App HTTP server
│   └── wifi/                # SoftAP captive portal and Wi-Fi provisioning
├── scripts/
│   ├── build.sh             # Cross-compilation using Docker messense/rust-musl-cross
│   ├── deploy.sh            # Network deployment to the speaker over SSH
│   └── verify_deploy.sh     # 22-point hardware and software acceptance suite
├── tests/                   # 210+ comprehensive unit and contract tests
├── Cargo.toml               # Rust dependencies and ARM32 optimization profile
└── config.example.toml      # Documented template for runtime configuration
```

---

## Building and Testing

### 1. Run Local Tests (macOS / Linux)
```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

### 2. Cross-Compile for Amlogic A113X (ARM32 armhf)
```sh
./scripts/build.sh
```
This builds a static binary `target/armv7-unknown-linux-musleabihf/release/mini-assistant` using Docker.

### 3. Deploy to Speaker
```sh
MINI_HOST=192.168.1.110 MINI_SSH_KEY=~/.ssh/id_ed25519 ./scripts/deploy.sh
```

### 4. Verify on Hardware
```sh
MINI_HOST=192.168.1.110 MINI_SSH_KEY=~/.ssh/id_ed25519 ./scripts/verify_deploy.sh
```

---

## Configuration

Copy `.env.example` to `.env` and `config.example.toml` to `config.toml`:

```sh
cp .env.example .env
cp config.example.toml config.toml
```

- **`.env`**: Store your `GEMINI_API_KEY` or `OPENAI_API_KEY`, and optional `HA_TOKEN`. Never commit `.env`.
- **`config.toml`**: Configure wake word, model name (`models/gemini-3.8-live` or `gpt-4o-realtime-preview`), assistant voice, and network addresses. Supports live hot-reloading from the Web UI.

---

## License

MIT / Apache-2.0
