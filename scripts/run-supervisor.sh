#!/bin/sh
# run-supervisor.sh — Supervisor daemon for GPT Mini
# MINI_VOICE_PAUSE_GUARD
PAUSE_GUARD="${MINI_PAUSE_GUARD:-/bkupgrade/data/opt/mini-assistant/voice.paused}"
[ ! -e "$PAUSE_GUARD" ] || exit 0
DIR="${MINI_SUPERVISOR_DIR:-/data/opt/mini-assistant}"
DAEMON="$DIR/mini-assistant"
LOGFILE="$DIR/assistant.log"
IFACE="${MINI_WIFI_IFACE:-wlan0}"
CA_BUNDLE="${MINI_CA_BUNDLE:-/etc/ssl/certs/ca-certificates.crt}"
SPOTIFY_CHECK_URL="${MINI_SPOTIFY_CHECK_URL:-https://apresolve.spotify.com}"
cd "$DIR" || exit 1

echo "[$(date -Iseconds 2>/dev/null || date)] Supervisor started on GPT Mini" >> "$LOGFILE"

# 1. Enable ARMv8 hardware memory barriers on Cortex-A53 (0-2% CPU)
echo 2 > /proc/sys/abi/cp15_barrier 2>/dev/null || true

# 2. Enable VL53L0 proximity sensor
echo 1 > /sys/devices/virtual/input/input0/enable_ps_sensor 2>/dev/null || true

# 2b. Enable hardware audio amplifiers (internal speaker + AUX line-out)
echo 1 > /sys/class/tdm_state/amp_table 2>/dev/null || true
echo 1 > /sys/class/tdm_state/aux_amp 2>/dev/null || true

# 3. Start Bluetooth bcm-attach daemon if not running
if [ -x "$DIR/bcm-attach" ] && ! ps | grep -v grep | grep -q "$DIR/bcm-attach"; then
    echo "[$(date -Iseconds 2>/dev/null || date)] Starting bcm-attach daemon..." >> "$LOGFILE"
    setsid "$DIR/bcm-attach" > "$DIR/bcm-attach.log" 2>&1 &
fi

# Returns 0 when Wi-Fi interface has an IPv4 address and a default gateway (not bare fe80:: link-local).
network_ipv4_ready() {
    ip -4 addr show dev "$IFACE" 2>/dev/null | grep -q 'inet ' \
        && ip -4 route show default 2>/dev/null | grep -q '^default '
}

# Returns 0 when IPv4, NTP clock, DNS, and HTTPS reachability to Spotify endpoints are ready.
spotify_network_ready() {
    local year
    network_ipv4_ready || return 1
    year=$(date +%Y 2>/dev/null || echo 2015)
    [ "$year" -ge 2026 ] || return 1
    nslookup apresolve.spotify.com >/dev/null 2>&1 || return 1
    if [ -r "$CA_BUNDLE" ]; then
        wget --ca-certificate="$CA_BUNDLE" -T 4 -t 1 -q -O /dev/null "$SPOTIFY_CHECK_URL" >/dev/null 2>&1 || return 1
    else
        wget -T 4 -t 1 -q -O /dev/null "$SPOTIFY_CHECK_URL" >/dev/null 2>&1 || return 1
    fi
}

# 4. Wait for valid system time (year >= 2026) and initial Wi-Fi IPv4 readiness (up to 30s if year is already >= 2026)
wait_for_ntp() {
    echo "[$(date -Iseconds 2>/dev/null || date)] Waiting for NTP clock sync (year >= 2026) and $IFACE IPv4..." >> "$LOGFILE"
    local count=0
    while true; do
        YEAR=$(date +%Y 2>/dev/null || echo 2015)
        if [ "$YEAR" -ge 2026 ]; then
            if network_ipv4_ready || [ "$count" -ge 30 ]; then
                echo "[$(date -Iseconds 2>/dev/null || date)] Boot readiness OK: year=$YEAR net_wait=${count}s" >> "$LOGFILE"
                # Ensure waiting LED blink is off
                i2cset -y 0 0x3c 0x00 0x00 2>/dev/null || true
                break
            fi
        fi

        # Visual signal: blink upper LED (0.5s on, 0.5s off) to indicate waiting for NTP/network
        if [ "$YEAR" -lt 2026 ]; then
            i2cset -y 0 0x3c 0x00 0x01 2>/dev/null || true
            sleep 0.5
            i2cset -y 0 0x3c 0x00 0x00 2>/dev/null || true
            sleep 0.5
        else
            sleep 1
        fi

        count=$((count + 1))
        # Every 5 seconds, attempt to force chrony stepping
        if [ $((count % 5)) -eq 0 ]; then
            echo "[$(date -Iseconds 2>/dev/null || date)] Boot wait tick $count (year=$YEAR); kicking chrony..." >> "$LOGFILE"
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
    local soloist_wait_logged=0
    local soloist_token_synced=0
    while true; do
        sleep 4
        # Hardware memory barrier
        echo 2 > /proc/sys/abi/cp15_barrier 2>/dev/null || true
        # Proximity sensor active
        echo 1 > /sys/devices/virtual/input/input0/enable_ps_sensor 2>/dev/null || true
        # Hardware audio amplifiers active (speaker + AUX)
        echo 1 > /sys/class/tdm_state/amp_table 2>/dev/null || true
        echo 1 > /sys/class/tdm_state/aux_amp 2>/dev/null || true

        # Watchdog: ensure system year has not slipped backward
        YEAR=$(date +%Y 2>/dev/null || echo 2015)
        if [ "$YEAR" -lt 2026 ]; then
            echo "[$(date -Iseconds 2>/dev/null || date)] Watchdog: System time is invalid ($YEAR < 2026); stopping assistant..." >> "$LOGFILE"
            killall -TERM mini-assistant 2>/dev/null || true
        fi

        # Watchdog: Spotify Soloist engine (do not start until IPv4, DNS, and HTTPS reachability succeed,
        # otherwise Soloist fails with CURL error 6 and invalidates its persisted OAuth session on disk).
        if [ -x "$DIR/soloist-env/soloist-run.sh" ]; then
            if ! ps | grep -v grep | grep -q "soloist-env/soloist"; then
                soloist_token_synced=0
                if spotify_network_ready; then
                    echo "[$(date -Iseconds 2>/dev/null || date)] Watchdog: Network/DNS/HTTPS ready; starting Spotify Soloist..." >> "$LOGFILE"
                    soloist_wait_logged=0
                    nohup "$DIR/soloist-env/soloist-run.sh" > "$DIR/soloist.log" 2>&1 &
                elif [ "$soloist_wait_logged" -eq 0 ]; then
                    echo "[$(date -Iseconds 2>/dev/null || date)] Watchdog: Delaying Spotify Soloist startup until $IFACE IPv4, DNS, and HTTPS are ready" >> "$LOGFILE"
                    soloist_wait_logged=1
                fi
            elif [ "$soloist_token_synced" -eq 0 ] && [ -x "$DIR/soloist-ctl" ]; then
                if "$DIR/soloist-ctl" status 2>/dev/null | grep -q "logged in: yes"; then
                    sync 2>/dev/null || true
                    soloist_token_synced=1
                fi
            fi
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

    # Enable Amlogic hardware DAC/TDM loopback onto hw:0,2 Ch6..7 for M-AEC
    if [ -x /system/workdir/bin/amixer ]; then
        LD_LIBRARY_PATH=/system/vendor/quasar/libs /system/workdir/bin/amixer -q cset numid=7 1 >/dev/null 2>&1 || true
        LD_LIBRARY_PATH=/system/vendor/quasar/libs /system/workdir/bin/amixer -q cset numid=8 4 >/dev/null 2>&1 || true
        LD_LIBRARY_PATH=/system/vendor/quasar/libs /system/workdir/bin/amixer -q cset numid=9 4 >/dev/null 2>&1 || true
    fi

    if [ -f "$DIR/config.toml" ]; then
        "$DAEMON" -c "$DIR/config.toml" >> "$LOGFILE" 2>&1
    else
        "$DAEMON" >> "$LOGFILE" 2>&1
    fi
    EXIT_CODE=$?
    echo "[$(date -Iseconds 2>/dev/null || date)] Daemon exited with code $EXIT_CODE; restarting in 2s..." >> "$LOGFILE"
    sleep 2
done
