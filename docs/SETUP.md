# Mini-Assistant Setup & Deployment Guide

This guide walks you through compiling and running `mini-assistant` on your Yandex Station Mini 1.

---

## Prerequisites on Mac

1. **Rust & ARM32 Target:**
   ```bash
   rustup target add armv7-unknown-linux-musleabihf
   ```

2. **Docker:**
   Ensure Docker Desktop is running on your Mac (`docker --version`).

3. **Cross Compiler:**
   ```bash
   cargo install cross --git https://github.com/cross-rs/cross
   ```

---

## Step 1: Configuration & Secrets

1. Navigate to the project directory:
   ```bash
   cd /Users/oleksii/Documents/gpt-mini/mini-assistant
   ```

2. Create your `.env` file (this file is gitignored and will never be committed):
   ```bash
   cat << 'EOF' > .env
   GEMINI_API_KEY=your_gemini_api_key_here
   HA_URL=http://192.168.77.2:8123
   HA_TOKEN=your_ha_long_lived_token_here
   MINI_HOST=192.168.77.110
   MINI_PORT=2222
   MINI_SSH_KEY=~/.ssh/your_mini_key
   EOF
   ```

3. Copy the configuration template:
   ```bash
   cp config.example.toml config.toml
   ```

---

## Step 2: Build Binary for Mini (ARM32 Static)

Run the automated build script:
```bash
./scripts/build.sh
```

This uses `cross` inside Docker to compile a fully static musl binary (`armv7-unknown-linux-musleabihf`). It has zero external glibc dependencies and runs natively on the Mini's kernel (Linux 4.9.68).

---

## Step 3: Deploy to Mini

Deploy the binary and config over SSH:
```bash
./scripts/deploy.sh
```

---

## Step 4: Run & Test on Mini

SSH into the Mini:
```bash
ssh -p 2222 -i ~/.ssh/your_mini_key root@192.168.77.110
```

Run the assistant in the foreground to monitor live logs:
```bash
cd /data/opt/mini-assistant
./mini-assistant
```

### What You Will See in Logs:
1. `mini-assistant starting on Yandex Station Mini`
2. `Connecting to Home Assistant... Synchronized devices: [Midea, Lights, ...]`
3. `Connecting to Gemini Live API... Connected successfully`
4. `Starting VQE audio capture from yio_audio:9850... Subscribed to vqe_0`
5. `🚀 mini-assistant full audio + Home Assistant pipeline is active!`

---

## Step 5: Try Voice Commands!

Speak directly to the Mini:
- *"Привет, как дела?"* → Gemini speaks back through the Mini's speaker!
- *"Включи кондиционер на 23 градуса"* → Gemini triggers `control_home_device` → Home Assistant sets Midea AC temperature!
- *"Выключи свет в спальне"* → Gemini triggers `light/turn_off`!
- *"Сколько раз включался чайник?"* → Gemini queries history and answers!

---

## Step 6: Spotify Connect (Optional)

To enable Spotify Connect:
1. Build `librespot`:
   ```bash
   ./scripts/build_librespot.sh
   ```
2. Copy `bin/librespot` to Mini (`/data/opt/librespot/`).
3. Run `librespot` on Mini:
   ```bash
   /data/opt/librespot/librespot --name "Mini" --backend alsa --bitrate 320
   ```
4. Open Spotify on your phone or Mac — "Mini" appears as an available speaker!
