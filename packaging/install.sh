#!/bin/sh
# Installs the bundled tablet binary on the reMarkable over SSH.
#   <AppImage> install [--ip HOST] [--remote PATH]
# HOST is an address or user@address (default root@10.11.99.1).
set -eu
here=$(dirname "$(readlink -f "$0")")
target=root@10.11.99.1
remote=/home/root/remdesktop

while [ $# -gt 0 ]; do
    case $1 in
        --ip | --ssh) target=${2:?$1 needs a value}; shift 2 ;;
        --remote) remote=${2:?--remote needs a value}; shift 2 ;;
        -h | --help) sed -n '2,4p' "$0"; exit 0 ;;
        *) echo "install: unknown argument $1" >&2; exit 1 ;;
    esac
done
case $target in *@*) ;; *) target=root@$target ;; esac

ssh_t() { ssh -o ConnectTimeout=5 -o ServerAliveInterval=1 -o ServerAliveCountMax=3 "$target" "$@"; }

arch=$(ssh_t uname -m)
case $arch in
    armv7l) bin=$here/tablet/remdesktop-armv7 ;;
    aarch64) bin=$here/tablet/remdesktop-aarch64 ;;
    *) echo "install: unsupported tablet CPU: $arch" >&2; exit 1 ;;
esac
[ -f "$bin" ] || { echo "install: this build does not bundle a binary for $arch" >&2; exit 1; }

# The running binary can't be overwritten, and stopping it re-enumerates USB
# (dropping SSH), so stop from a detached unit and wait for it.
if ssh_t systemctl is-active --quiet remdesktop 2>/dev/null; then
    echo "install: stopping the running tablet side"
    ssh_t systemd-run --quiet --collect --unit=remdesktop-stop "$remote" stop 2>/dev/null || true
    for _ in $(seq 30); do
        sleep 1
        ssh_t "! systemctl is-active --quiet remdesktop" 2>/dev/null && break
    done
    sleep 2
fi

echo "install: copying $(basename "$bin") to $target:$remote"
scp -q -O "$bin" "$target:$remote.new"
ssh_t "chmod +x $remote.new && mv $remote.new $remote"
echo "install: done; run the AppImage without arguments to start it"
