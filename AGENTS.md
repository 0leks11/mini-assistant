# Agent Rules — mini-assistant

This file defines rules and goals for AI agents (Antigravity, Gemini CLI, etc.)
working on this project.

## Project Goal

Replace Yandex `maind` on Yandex Station Mini 1 (YNDX-0004, A113X, ARM32)
with a fully custom Rust voice assistant stack. No Yandex services or accounts.

## Target Device

| Parameter | Value |
|---|---|
| SoC | Amlogic A113X — 4× Cortex-A53 |
| Userspace | ARM32 (ELF32), glibc 2.23 |
| RAM | ~236 MiB total |
| Build target | `armv7-unknown-linux-musleabihf` (static, musl) |
| Audio | ALSA, DU561 DSP/VQE, 4 microphones |
| SSH | Port 2222, key-only, root |

## Architecture

```
Mini (A113X):
  mini-assistant (Rust static binary)
    ├── wake-word   (Porcupine)
    ├── alsa-in     (mic capture, VQE tap at 16kHz)
    ├── gemini      (Gemini Live API, real-time voice)
    ├── alsa-out    (speaker playback)
    ├── ha-client   (Home Assistant REST API)
    ├── led         (sysfs GPIO)
    └── buttons     (evdev)
  librespot (Spotify Connect, static Rust)

Pi 4B:
  Home Assistant only
```

## Development Rules

### Security
- NEVER hardcode API keys, passwords, or tokens in source code
- NEVER commit `.env` or `config.toml` (they are gitignored)
- ALL secrets go in `.env` (local) — use `.env.example` as template
- Spotify credentials stored in keyring or env, never in git

### Code Style
- Rust edition 2021
- Use `tokio` for async runtime
- Error handling: `anyhow` for application errors, `thiserror` for library errors
- Logging: `tracing` crate
- All public functions must have doc comments
- Run `cargo clippy` before commit — zero warnings policy

### Cross-compilation
- Target: `armv7-unknown-linux-musleabihf` (musl = no glibc dependency)
- Tool: `cross` (Docker-based)
- Build: `cross build --target armv7-unknown-linux-musleabihf --release`
- Binary must be fully static — verify with: `file target/.../mini-assistant`

### Git Workflow
- Branch: `main` (stable), `dev` (development), `feature/xxx` (features)
- Commit format: `type(scope): description`
  - Types: `feat`, `fix`, `refactor`, `docs`, `test`, `chore`
  - Example: `feat(gemini): add real-time audio streaming`
- Every commit must build successfully for ARM32 target
- Tag releases: `v0.1.0`, `v0.2.0`, etc.

### Deployment
- Deploy script: `scripts/deploy.sh`
- Test on Mini before merging to main
- Never remove maind until our stack is fully tested in parallel

## Phase Plan

| Phase | Goal | Status |
|---|---|---|
| 0 | Setup cross-compilation toolchain | 🔄 In Progress |
| 1 | librespot (Spotify) on Mini | ⏳ Pending |
| 1 | Gemini Live voice bridge (test) | ⏳ Pending |
| 2 | mini-assistant v0.1 Rust binary on Mini | ⏳ Pending |
| 3 | Wake word + full voice cycle | ⏳ Pending |
| 4 | Replace maind completely | ⏳ Pending |
| 5 | Home Assistant voice integration | ⏳ Pending |

## What NOT to do

- Do NOT modify `/etc/init.d/S90maind` until Phase 4
- Do NOT kill maind manually during testing
- Do NOT write secrets to any file tracked by git
- Do NOT use Yandex APIs or accounts
- Do NOT assume glibc version > 2.23 on Mini
- Do NOT use Python for final Mini binaries (RAM constraint)

## Authentication Approach

For Gemini: OAuth2 with Google Account (no hardcoded API key)
- Preferred: device flow OAuth (user logs in once, token refreshed automatically)
- Reference: how SolidCraft (solidcraft.io) and Antigravity CLI handle Google auth
- Token stored in: `/data/opt/mini-assistant/.credentials/` (not in git)

For Spotify: OAuth2 PKCE flow via librespot
- Credentials stored in: `/data/opt/mini-assistant/.spotify-credentials`

For Home Assistant: Long-lived access token
- Stored in: `.env` → `HA_TOKEN`
