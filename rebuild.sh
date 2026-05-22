#!/usr/bin/env bash
#
# Local dev rebuild: stop the running host, recompile + reinstall the
# three binaries to ~/.local/bin, start the host back up, print where
# things landed.
#
# Use from a clean working tree (or after `git pull`). For fresh installs
# on a new box use `install.sh` instead — that one handles tarball
# downloads and platform detection.
#
# Flags:
#   --no-stop       leave whatever's running running (rebuild + reinstall only).
#   --no-start      stop + rebuild + install but don't start the host.
#   --no-clean      keep the chat.fifo + dangling sleep, just rebuild.
#   --release       cargo install --release (default; here for symmetry).
#   --debug         cargo install --debug — faster compile, slower runtime.
#   --skip-cli      don't bother with iclaw + ironclaw-setup, just the host.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_DIR="${IRONCLAW_INSTALL_DIR:-$HOME/.local/bin}"
# Install root is the parent of the data dir; the setup wizard puts the
# data dir at <install_root>/data and chat.fifo at <install_root>/chat.fifo.
# Default install root is platform-specific (XDG on Linux, ~/Library on macOS).
if [ -n "${IRONCLAW_DATA_DIR:-}" ]; then
    INSTALL_ROOT="$(dirname "$IRONCLAW_DATA_DIR")"
elif [ "$(uname -s)" = "Darwin" ]; then
    INSTALL_ROOT="$HOME/Library/Application Support/ironclaw"
else
    INSTALL_ROOT="${XDG_DATA_HOME:-$HOME/.local/share}/ironclaw"
fi
DATA_DIR="$INSTALL_ROOT/data"

do_stop=1
do_start=1
do_clean=1
build_mode=release
crates=(ironclaw-host ironclaw-iclaw ironclaw-setup)

for arg in "$@"; do
    case "$arg" in
        --no-stop)  do_stop=0 ;;
        --no-start) do_start=0 ;;
        --no-clean) do_clean=0 ;;
        --release)  build_mode=release ;;
        --debug)    build_mode=debug ;;
        --skip-cli) crates=(ironclaw-host) ;;
        -h|--help)
            head -n 22 "${BASH_SOURCE[0]}" | tail -n 19
            exit 0
            ;;
        *) echo "rebuild.sh: unknown flag '$arg'" >&2; exit 2 ;;
    esac
done

say()  { printf '\033[1;36m▸\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!\033[0m %s\n' "$*" >&2; }
fail() { printf '\033[1;31m✗\033[0m %s\n' "$*" >&2; exit 1; }

cd "$REPO_ROOT"

# ── Step 1: stop ──────────────────────────────────────────────────────
if [ "$do_stop" = 1 ]; then
    say "stopping any running host"
    # Prefer the daemon's own stop command if the binary exists.
    if command -v ironclaw >/dev/null 2>&1; then
        ironclaw stop 2>/dev/null || true
    fi
    # Belt-and-braces: kill any foreground 'ironclaw run' the daemon
    # didn't know about (e.g. started before the start/stop commands
    # were added).
    pkill -TERM -f 'ironclaw run' 2>/dev/null || true
    pkill -TERM -f 'iclaw chat' 2>/dev/null || true
    sleep 1
    pkill -KILL -f 'ironclaw run' 2>/dev/null || true
fi

# ── Step 2: clean up stale FIFO writers from old runs ─────────────────
if [ "$do_clean" = 1 ]; then
    # The pre-bridge era used `sleep infinity > chat.fifo` to hold the
    # writer side open. The new cli channel adapter opens the FIFO with
    # O_RDWR so the sleep is dead weight. Kill it.
    pkill -KILL -f 'sleep 100000000' 2>/dev/null || true
    # If chat.fifo exists with stale data and the host is stopped, drain
    # it by removing + recreating. Setup's quickstart_group step will
    # re-mkfifo on next boot, but we'll just zero it here for safety.
    fifo="$INSTALL_ROOT/chat.fifo"
    if [ -p "$fifo" ]; then
        say "draining stale chat.fifo at $fifo"
        rm -f "$fifo"
        mkfifo -m 0600 "$fifo"
    fi
fi

# ── Step 3: build + install ───────────────────────────────────────────
say "building + installing binaries ($build_mode) to $INSTALL_DIR"
mkdir -p "$INSTALL_DIR"

install_flags=(--locked --force --root "$(dirname "$INSTALL_DIR")")
if [ "$build_mode" = "debug" ]; then
    install_flags+=(--debug)
fi

# `cargo install` with --root puts binaries under <root>/bin, so the
# --root parent dir must be such that <parent>/bin == INSTALL_DIR. Most
# people use ~/.local/bin, so root=~/.local.
for crate in "${crates[@]}"; do
    say "  cargo install -p $crate"
    cargo install "${install_flags[@]}" --path "crates/$crate"
done

# ── Step 4: start ─────────────────────────────────────────────────────
if [ "$do_start" = 1 ]; then
    say "starting host in the background"
    "$INSTALL_DIR/ironclaw" start
fi

# ── Step 5: tell the human what just happened ─────────────────────────
say "done. installed binaries:"
for b in ironclaw iclaw ironclaw-setup; do
    if [ -x "$INSTALL_DIR/$b" ]; then
        printf '    %s  (%s bytes)\n' "$INSTALL_DIR/$b" "$(stat -c '%s' "$INSTALL_DIR/$b" 2>/dev/null || stat -f '%z' "$INSTALL_DIR/$b")"
    fi
done

if [ "$do_start" = 1 ]; then
    cat <<EOF

  Try:    iclaw chat           # type a message, get a reply
  Status: iclaw                 # no-args dashboard
          ironclaw status       # PID, uptime, paths
          ironclaw logs -f      # tail the host log
  Stop:   ironclaw stop

EOF
fi
