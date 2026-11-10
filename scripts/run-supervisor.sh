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
wait_for_ntp() {
    echo "[$(date -Iseconds 2>/dev/null || date)] Waiting for NTP clock sync (year >= 2026)..." >> "$LOGFILE"
    local count=0
    while true; do
        YEAR=$(date +%Y 2>/dev/null || echo 2015)
        if [ "$YEAR" -ge 2026 ]; then
            echo "[$(date -Iseconds 2>/dev/null || date)] NTP clock synchronized cleanly: year $YEAR" >> "$LOGFILE"
            # Ensure waiting LED blink is off
            i2cset -y 0 0x3c 0x00 0x00 2>/dev/null || true
            break
        fi

        # Visual signal: blink upper LED (0.5s on, 0.5s off) to indicate waiting for NTP/network
        i2cset -y 0 0x3c 0x00 0x01 2>/dev/null || true
        sleep 0.5
        i2cset -y 0 0x3c 0x00 0x00 2>/dev/null || true
        sleep 0.5

        count=$((count + 1))
        # Every 5 seconds, attempt to force chrony stepping
        if [ $((count % 5)) -eq 0 ]; then
            echo "[$(date -Iseconds 2>/dev/null || date)] NTP wait tick $count (current year: $YEAR); kicking chrony..." >> "$LOGFILE"
            chronyc -a 'burst 4/4' 2>/dev/null || true
            chronyc makestep 2>/dev/null || true
        fi

        # Every 15 seconds, attempt one-shot pool sync if chronyd has stalled
        if [ $((count % 15)) -eq 0 ]; then
            chronyd -q 'server time.google.com iburst' 2>/dev/null || chronyd -q 'server pool.ntp.org iburst' 2>/dev/null || true
        fi
    done
}

wait_for_ntp

# 5. Continuous background watchdog for dependent daemons & hardware settings
# Runs concurrently with the main voice daemon, checking health every 4 seconds.
watchdog_loop() {
    while true; do
        sleep 4
        # Hardware memory barrier
        echo 2 > /proc/sys/abi/cp15_barrier 2>/dev/null || true
        # Proximity sensor active
        echo 1 > /sys/devices/virtual/input/input0/enable_ps_sensor 2>/dev/null || true

        # Watchdog: ensure system year has not slipped backward
        YEAR=$(date +%Y 2>/dev/null || echo 2015)
        if [ "$YEAR" -lt 2026 ]; then
            echo "[$(date -Iseconds 2>/dev/null || date)] Watchdog: System time is invalid ($YEAR < 2026); stopping assistant..." >> "$LOGFILE"
            killall -TERM mini-assistant 2>/dev/null || true
        fi

        # Watchdog: Spotify Soloist engine
        if [ -x "$DIR/soloist-env/soloist-run.sh" ] && ! ps | grep -v grep | grep -q "soloist-env/soloist"; then
            echo "[$(date -Iseconds 2>/dev/null || date)] Watchdog: Spotify Soloist not running; starting..." >> "$LOGFILE"
            nohup "$DIR/soloist-env/soloist-run.sh" > "$DIR/soloist.log" 2>&1 &
        fi

        # Watchdog: Bluetooth attach
        if [ -x "$DIR/bcm-attach" ] && ! ps | grep -v grep | grep -q "$DIR/bcm-attach"; then
            echo "[$(date -Iseconds 2>/dev/null || date)] Watchdog: bcm-attach not running; starting..." >> "$LOGFILE"
            setsid "$DIR/bcm-attach" > "$DIR/bcm-attach.log" 2>&1 &
        fi
    done
}

watchdog_loop &
WATCHDOG_PID=$!
trap 'kill $WATCHDOG_PID 2>/dev/null' EXIT HUP INT TERM

# 6. Main restart loop for mini-assistant voice runtime
while true; do
    if [ -f "$LOGFILE" ] && [ "$(wc -c < "$LOGFILE" 2>/dev/null || echo 0)" -gt 2097152 ]; then
        mv -f "$LOGFILE" "$LOGFILE.old" 2>/dev/null || true
    fi

    # Before launching or respawning, guarantee clock is synchronized
    YEAR=$(date +%Y 2>/dev/null || echo 2015)
    if [ "$YEAR" -lt 2026 ]; then
        wait_for_ntp
    fi

    "$DAEMON" >> "$LOGFILE" 2>&1
    EXIT_CODE=$?
    echo "[$(date -Iseconds 2>/dev/null || date)] Daemon exited with code $EXIT_CODE; restarting in 2s..." >> "$LOGFILE"
    sleep 2
done
