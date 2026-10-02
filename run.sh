#!/usr/bin/env bash
# remdesktop launcher.
#
#   ./run.sh [client opts]       build, deploy, start the tablet side, run the client
#   ./run.sh tablet [opts]       one-shot: build, deploy and start the tablet side,
#                                then return (it keeps running on the tablet by itself)
#   ./run.sh client [opts]       run only the local client (attaches to the tablet side)
#   ./run.sh stop                stop the tablet side (hold X or press power on it also works)
#   ./run.sh status              is the tablet side running?
#
# Tablet options: --rotate 0|90|180|270  --touch touchpad|touchscreen  --pen tablet|mouse
#   --no-usb  --palm-major N  --pen-grace-ms N  --log-touch
#   --fast-mode M  --settle-mode M  --settle-ms N
# Client options: --stats  --no-display  --list-screens (the client passes tablet
#   options on when it starts the tablet side).
# Before the client starts, checks that /dev/uinput is writable (needed for
#   input over the network, without USB) and offers to install the udev rule.
# Environment: RM=root@10.11.99.1  REMOTE=/home/root/remdesktop
set -euo pipefail
cd "$(dirname "$0")"

RM=${RM:-root@10.11.99.1}
REMOTE=${REMOTE:-/home/root/remdesktop}
CLIENT=target/release/remdesktop-host
SSH=(ssh -o ConnectTimeout=5 -o ServerAliveInterval=1 -o ServerAliveCountMax=3 -o BatchMode=yes "$RM")

# The tablet binary's target, from the tablet's CPU: armv7l on the
# reMarkable 2, aarch64 on the Paper Pro / Move.
tablet_target() {
    case $("${SSH[@]}" uname -m) in
        aarch64) echo aarch64-unknown-linux-gnu ;;
        armv7l) echo armv7-unknown-linux-gnueabihf ;;
        *) echo "unsupported tablet CPU" >&2; exit 1 ;;
    esac
}

build() {
    wait_ssh
    TARGET=$(tablet_target)
    BIN=target/$TARGET/release/remdesktop
    cargo zigbuild -q -p remdesktop-tablet --release --target "$TARGET.2.31"
    cargo build -q -p remdesktop-host --release
}

wait_ssh() {
    for _ in $(seq 60); do
        "${SSH[@]}" true 2>/dev/null && return
        sleep 1
    done
    echo "tablet unreachable at $RM" >&2
    exit 1
}

running() {
    "${SSH[@]}" systemctl is-active --quiet remdesktop 2>/dev/null
}

# Stopping takes our USB HID function out, which re-enumerates USB and drops
# this SSH session, so stop from a detached unit and wait for it.
stop_tablet() {
    running || return 0
    "${SSH[@]}" systemd-run --quiet --collect --unit=remdesktop-stop "$REMOTE" stop 2>/dev/null || true
    for _ in $(seq 30); do
        sleep 1
        # Reconnect each time: the stop drops USB (and SSH) a few seconds in.
        if "${SSH[@]}" "! systemctl is-active --quiet remdesktop && ! ls /sys/kernel/config/usb_gadget/g_ether/configs/c.1 | grep -q ffs" 2>/dev/null; then
            # Stopped and USB restored; let the host re-enumerate.
            sleep 2
            wait_ssh
            return 0
        fi
    done
    echo "tablet side did not stop" >&2
    exit 1
}

# Copies the tablet binary if it changed (restarting a running instance,
# since it keeps the old binary loaded).
deploy() {
    local here there
    here=$(md5sum "$BIN" | cut -d' ' -f1)
    wait_ssh
    there=$("${SSH[@]}" "md5sum $REMOTE 2>/dev/null || true" | cut -d' ' -f1)
    [ "$here" = "$there" ] && return 0
    if running; then
        echo "remdesktop: new build, restarting the tablet side"
        stop_tablet
    fi
    scp -q -O "$BIN" "$RM:$REMOTE.new"
    "${SSH[@]}" "mv $REMOTE.new $REMOTE"
    echo "remdesktop: deployed $(du -h "$BIN" | cut -f1)"
}

RULE=host/99-remdesktop-uinput.rules

# Input over the network (no USB, e.g. after "go remote") is replayed through
# /dev/uinput, which needs a udev rule plus membership of the input group.
# USB HID input works without either, so this only asks, never insists.
ensure_uinput() {
    [ -w /dev/uinput ] && return 0
    local rule_present=no in_group=no
    grep -qs 'KERNEL=="uinput"' /etc/udev/rules.d/*.rules && rule_present=yes
    id -nG | tr ' ' '\n' | grep -qx input && in_group=yes
    echo "remdesktop: /dev/uinput is not writable, so input over the network (without USB) won't work."
    echo "            USB input is unaffected."
    local steps=()
    if [ "$rule_present" = no ]; then
        steps+=("sudo install -m644 $RULE /etc/udev/rules.d/")
        steps+=("sudo udevadm control --reload")
    fi
    steps+=("sudo chgrp input /dev/uinput && sudo chmod 660 /dev/uinput")
    if [ "$in_group" = no ]; then
        steps+=("sudo usermod -aG input $USER   # then log out and back in")
    fi
    if [ ! -t 0 ]; then
        echo "            To enable it, run:"
        printf '              %s\n' "${steps[@]}"
        return 0
    fi
    echo "            This would run:"
    printf '              %s\n' "${steps[@]}"
    read -r -p "remdesktop: enable it now? [y/N] " answer
    case $answer in
        y | Y | yes)
            if [ "$rule_present" = no ]; then
                sudo install -m644 "$RULE" /etc/udev/rules.d/
                sudo udevadm control --reload
            fi
            sudo chgrp input /dev/uinput && sudo chmod 660 /dev/uinput
            if [ "$in_group" = no ]; then
                sudo usermod -aG input "$USER"
                echo "remdesktop: added $USER to the input group; log out and back in for it to apply."
            fi
            ;;
        *) echo "remdesktop: skipped; continuing with USB input only." ;;
    esac
}

client() {
    [ -x "$CLIENT" ] || build
    ensure_uinput
    exec "$CLIENT" --ssh "$RM" --remote "$REMOTE" "$@"
}

cmd=${1:-}
case $cmd in
    tablet)
        shift
        build
        deploy
        wait_ssh
        "${SSH[@]}" "$REMOTE" start "$@"
        ;;
    client)
        shift
        client "$@"
        ;;
    stop)
        wait_ssh
        stop_tablet
        echo "remdesktop: stopped"
        ;;
    status)
        wait_ssh
        if running; then echo "remdesktop: running"; else echo "remdesktop: not running"; fi
        ;;
    -h | --help | help)
        sed -n '2,/^set /{/^#/p}' "$0"
        ;;
    "" | -*)
        build
        deploy
        client "$@"
        ;;
    *)
        sed -n '2,/^set /{/^#/p}' "$0" >&2
        exit 1
        ;;
esac
