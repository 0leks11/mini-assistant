# mini-assistant

Custom voice assistant for **Yandex Station Mini 1** (A113X, ARM32).  
Replaces Yandex `maind` with a fully open, custom Rust stack.

**No Yandex services. No Yandex accounts. Full hardware reuse.**

## What it does

- 🎤 Wake word detection ("Эй Мини" or any custom word)
- 🔊 Real-time voice dialog via **Gemini Live API**
- 🎵 **Spotify Connect** via librespot
- 🏠 **Home Assistant** voice control
- 💡 LED + button control

## Hardware

| | |
|---|---|
| Device | Yandex Station Mini 1 (YNDX-0004) |
| SoC | Amlogic A113X, 4× Cortex-A53 |
| RAM | 236 MiB |
| Audio | DU561 DSP, 4 microphones |
| Build target | `armv7-unknown-linux-musleabihf` (static musl) |

## Quick Start

```bash
# 1. Install cross-compiler
cargo install cross --git https://github.com/cross-rs/cross

# 2. Add ARM32 target
rustup target add armv7-unknown-linux-musleabihf

# 3. Configure
cp config.example.toml config.toml  # edit with your values
cp .env.example .env                 # add your API keys

# 4. Build
./scripts/build.sh

# 5. Deploy to Mini
./scripts/deploy.sh
```

## Project Structure

```
src/
├── main.rs          # Entry point
├── config.rs        # Configuration
├── audio/           # ALSA capture + playback
├── gemini/          # Gemini Live API client
├── wakeword/        # Wake word detection (Porcupine)
├── ha/              # Home Assistant client
├── led/             # LED controller (sysfs)
└── buttons/         # Button handler (evdev)
scripts/
├── build.sh         # Build for ARM32
└── deploy.sh        # Deploy to Mini via SSH
docs/
├── ARCHITECTURE.md  # System architecture
├── SETUP.md         # Full setup guide
└── HARDWARE.md      # Mini hardware details
```

## Architecture

See [AGENTS.md](AGENTS.md) for full rules and [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for system design.

## Security

- API keys go in `.env` — never in git
- Config secrets via environment variables
- SSH key-only access to Mini

## License

MIT
