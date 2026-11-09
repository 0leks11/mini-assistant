#!/bin/sh
# run-supervisor.sh — Supervisor daemon for GPT Mini
# MINI_VOICE_PAUSE_GUARD
[ ! -e /bkupgrade/data/opt/mini-assistant/voice.paused ] || exit 0
DIR="/data/opt/mini-assistant"
DAEMON="$DIR/mini-assistant"
LOGFILE="$DIR/assistant.log"
cd "$DIR" || exit 1

echo "[$(date -Iseconds 2>/dev/null || date)] Supervisor started on GPT Mini" >> "$LOGFILE"

# 1. Enable ARMv8 hardware memory barriers on Cortex-A53 (0-2% CPU)
echo 2 > /proc/sys/abi/cp15_barrier 2>/dev/null || true

# 2. Enable VL53L0 proximity sensor
echo 1 > /sys/devices/virtual/input/input0/enable_ps_sensor 2>/dev/null || true

# 3. Start Bluetooth bcm-attach daemon if not running
if [ -x "$DIR/bcm-attach" ] && ! ps | grep -v grep | grep -q "$DIR/bcm-attach"; then
    echo "[$(date -Iseconds 2>/dev/null || date)] Starting bcm-attach daemon..." >> "$LOGFILE"
    setsid "$DIR/bcm-attach" > "$DIR/bcm-attach.log" 2>&1 &
fi

# 4. Wait for valid system time (year >= 2026) so TLS certificates validate cleanly
for i in $(seq 1 30); do
    YEAR=$(date +%Y 2>/dev/null || echo 2015)
    if [ "$YEAR" -ge 2026 ]; then
        break
    fi
    echo "[$(date -Iseconds 2>/dev/null || date)] Waiting for NTP clock sync (current year: $YEAR)..." >> "$LOGFILE"
    sleep 1
done

# 5. Main loop for mini-assistant voice runtime & companions
while true; do
    # Ensure VL53 sensor is enabled
    echo 1 > /sys/devices/virtual/input/input0/enable_ps_sensor 2>/dev/null || true

    # Keep Spotify Soloist alive
    if [ -x "$DIR/soloist-env/soloist-run.sh" ] && ! ps | grep -v grep | grep -q "soloist-env/soloist"; then
        echo "[$(date -Iseconds 2>/dev/null || date)] Starting Spotify Soloist engine..." >> "$LOGFILE"
        nohup "$DIR/soloist-env/soloist-run.sh" > "$DIR/soloist.log" 2>&1 &
    fi

    if [ -f "$LOGFILE" ] && [ "$(wc -c < "$LOGFILE" 2>/dev/null || echo 0)" -gt 2097152 ]; then
        mv -f "$LOGFILE" "$LOGFILE.old" 2>/dev/null || true
    fi

    "$DAEMON" >> "$LOGFILE" 2>&1
    EXIT_CODE=$?
    echo "[$(date -Iseconds 2>/dev/null || date)] Daemon exited with code $EXIT_CODE; restarting in 2s..." >> "$LOGFILE"
    sleep 2
done
