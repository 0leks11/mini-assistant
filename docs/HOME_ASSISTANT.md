# Home Assistant Integration in mini-assistant

`mini-assistant` connects directly to Home Assistant REST API and registers function declarations (tools) in the **Gemini 2.0 Live API**.

---

## How It Works

```
User says: "Включи кондиционер в спальне на 22 градуса"
     ↓
Mini 4-mic array + DU561 (echo cancellation)
     ↓
yio_audio IPC (16kHz PCM) → mini-assistant
     ↓
Gemini 2.0 Live WebSocket
     ↓
Gemini understands intent & calls tool:
  control_home_device(
    domain: "climate",
    service: "set_temperature",
    entity_id: "climate.midea_bedroom",
    parameters: {"temperature": 22}
  )
     ↓
mini-assistant executes POST http://PI_IP:8123/api/services/climate/set_temperature
     ↓
Home Assistant sets Midea AC to 22°C
     ↓
mini-assistant sends toolResponse to Gemini WebSocket
     ↓
Gemini speaks back: "Включила кондиционер в спальне на 22 градуса."
     ↓
ALSA playback on Mini speaker (24kHz S16_LE)
```

---

## Supported Commands

### 1. Direct Device Control (`control_home_device`)
- **Climate / Midea:**
  - *"Включи кондиционер"* → `climate/turn_on`
  - *"Поставь кондиционер на 24 градуса"* → `climate/set_temperature`
  - *"Переключи кондиционер на обогрев / охлаждение"* → `climate/set_hvac_mode`
- **Lights & Switches:**
  - *"Включи свет на кухне"* → `light/turn_on`
  - *"Сделай свет в зале потише"* → `light/turn_on` (brightness)
  - *"Выключи розетку / утюг"* → `switch/turn_off`
- **Delayed & Scheduled Actions:**
  - *"Выключи свет через 5 минут"* → handled by Gemini scheduling or HA timers.

### 2. Device History & Analytics (`get_device_history`)
- *"Сколько раз за день включался чайник?"*
- *"Какая температура была ночью в спальне?"*
- *"Когда в последний раз открывалась дверь?"*

---

## Dynamic Entity Refresh

Every time `mini-assistant` connects:
1. It queries `GET /api/states` from Home Assistant.
2. It filters actionable devices (climate, light, switch, vacuum, fan, cover).
3. It dynamically builds a device roster (friendly names, room IDs, current states).
4. It injects this roster into Gemini's system instructions.

If you add a new device or room in Home Assistant, **mini-assistant automatically knows about it upon restart or refresh!**

---

## Configuration

In `.env`:
```bash
HA_URL=http://192.168.77.2:8123
HA_TOKEN=eyJh... # Long-lived access token from HA Profile
```

In `config.toml`:
```toml
[ha]
url = "http://192.168.77.2:8123"
```
