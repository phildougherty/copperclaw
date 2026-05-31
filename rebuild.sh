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
#   --skip-cli      don't bother with cclaw + copperclaw-setup, just the host.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_DIR="${COPPERCLAW_INSTALL_DIR:-$HOME/.local/bin}"
# Install root is the parent of the data dir; the setup wizard puts the
# data dir at <install_root>/data and chat.fifo at <install_root>/chat.fifo.
# Default install root is platform-specific (XDG on Linux, ~/Library on macOS).
if [ -n "${COPPERCLAW_DATA_DIR:-}" ]; then
    INSTALL_ROOT="$(dirname "$COPPERCLAW_DATA_DIR")"
elif [ "$(uname -s)" = "Darwin" ]; then
    INSTALL_ROOT="$HOME/Library/Application Support/copperclaw"
else
    INSTALL_ROOT="${XDG_DATA_HOME:-$HOME/.local/share}/copperclaw"
fi
DATA_DIR="$INSTALL_ROOT/data"

do_stop=1
do_start=1
do_clean=1
build_mode=release
crates=(copperclaw-host copperclaw-cclaw copperclaw-setup copperclaw-runner)

for arg in "$@"; do
    case "$arg" in
        --no-stop)  do_stop=0 ;;
        --no-start) do_start=0 ;;
        --no-clean) do_clean=0 ;;
        --release)  build_mode=release ;;
        --debug)    build_mode=debug ;;
        --skip-cli) crates=(copperclaw-host) ;;
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
    if command -v copperclaw >/dev/null 2>&1; then
        copperclaw stop 2>/dev/null || true
    fi
    # Belt-and-braces: kill any foreground 'copperclaw run' the daemon
    # didn't know about (e.g. started before the start/stop commands
    # were added).
    pkill -TERM -f 'copperclaw run' 2>/dev/null || true
    pkill -TERM -f 'cclaw chat' 2>/dev/null || true
    sleep 1
    pkill -KILL -f 'copperclaw run' 2>/dev/null || true
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

# ── Step 2b: link the install's skills/ + groups/ at the repo ────────
# Dev-loop ergonomic: skills are pure markdown, and we want edits in
# the repo to land in the running host without a rebuild. The host
# reads COPPERCLAW_SKILLS_DIR (defaulted by setup to
# <install_root>/data/skills) — symlink that at the repo's skills/ so
# every running session sees current-tree skills on its next spawn.
# A missing groups/ dir blocks per-agent-group overrides from being
# picked up, so we mkdir an empty one if needed.
if [ "$do_clean" = 1 ] && [ -d "$INSTALL_ROOT" ]; then
    install_skills="$DATA_DIR/skills"
    install_groups="$DATA_DIR/groups"
    repo_skills="$REPO_ROOT/skills"
    if [ -d "$repo_skills" ] && [ ! -e "$install_skills" ]; then
        say "symlinking $install_skills -> $repo_skills (dev skill loop)"
        mkdir -p "$(dirname "$install_skills")"
        ln -sfn "$repo_skills" "$install_skills"
    elif [ -d "$install_skills" ] && [ ! -L "$install_skills" ]; then
        warn "$install_skills is a real directory; skills edits in the repo won't reach the running host. Move it aside and re-run if you want the dev loop."
    fi
    if [ ! -d "$install_groups" ]; then
        mkdir -p "$install_groups"
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

# ── Step 3b: rebake the session container image with the new runner ──
# The host invokes /usr/local/bin/copperclaw-runner inside the container.
# If the host is upgraded but the container image is stale, the new
# code (apology-on-failure, new tools, etc.) never reaches the agent.
# Re-run the setup wizard's `image` step so the runner binary we just
# built lands in a freshly-tagged image. Also point the install at it
# via COPPERCLAW_DEFAULT_IMAGE_TAG so the next session spawn uses it.
#
# Skipped when --skip-cli is set (the runner binary isn't in the
# rebuild list under --skip-cli, so the existing image is still
# coherent with the on-disk host).
if [ "$do_clean" = 1 ] && [ -d "$INSTALL_ROOT" ] && [ "${crates[*]}" != "copperclaw-host" ]; then
    say "rebuilding session container image (so the new runner reaches the agent)"
    # The image step locates the runner binary next to copperclaw-setup
    # by default. Force-clear the `image` step from setup-state so it
    # re-runs even though it already completed once.
    state_file="$INSTALL_ROOT/setup-state.json"
    if [ -f "$state_file" ] && command -v python3 >/dev/null 2>&1; then
        python3 - "$state_file" <<'PY'
import json, sys
p = sys.argv[1]
with open(p) as f:
    d = json.load(f)
steps = d.get("completed_steps", [])
if "image" in steps:
    d["completed_steps"] = [s for s in steps if s != "image"]
    with open(p, "w") as f:
        json.dump(d, f, indent=2)
PY
    fi

    # Snapshot the .env BEFORE running setup. The headless wizard rewrites
    # .env from scratch and only repopulates keys it knows about
    # (ANTHROPIC_API_KEY, COPPERCLAW_DATA_DIR, COPPERCLAW_DEFAULT_IMAGE_TAG,
    # etc.) — anything channel- or feature-specific (TELEGRAM_BOT_TOKEN,
    # COPPERCLAW_CHANNELS, COPPERCLAW_CHANNELS_CONFIG, third-party API keys
    # like TAVILY_API_KEY) gets dropped. We snapshot every existing key
    # here and re-append the missing ones after setup runs, so the wizard
    # is effectively additive for the rebuild use case.
    env_file="$INSTALL_ROOT/.env"
    env_snapshot=""
    if [ -f "$env_file" ]; then
        env_snapshot="$(mktemp)"
        cp "$env_file" "$env_snapshot"
        api_key="$(grep '^ANTHROPIC_API_KEY=' "$env_file" | cut -d= -f2-)"
        export COPPERCLAW_SETUP_ANTHROPIC_API_KEY="${api_key:-rebuild-placeholder}"
    fi
    export COPPERCLAW_SETUP_QUICKSTART=no
    if "$INSTALL_DIR/copperclaw-setup" --headless 2>&1 | grep -E '^\[step\] image|reused image:|building locally' | tail -5; then
        :
    fi

    # Restore any keys the wizard dropped. Compare key names (left of
    # first `=`) — append originals back if not present after.
    if [ -n "$env_snapshot" ] && [ -f "$env_snapshot" ] && [ -f "$env_file" ]; then
        restored=0
        while IFS= read -r line; do
            case "$line" in
                ''|\#*) continue ;;
                *=*)
                    key="${line%%=*}"
                    if ! grep -q "^${key}=" "$env_file"; then
                        printf '%s\n' "$line" >> "$env_file"
                        restored=$((restored + 1))
                    fi
                    ;;
            esac
        done < "$env_snapshot"
        rm -f "$env_snapshot"
        if [ "$restored" -gt 0 ]; then
            say "restored $restored .env key(s) the wizard dropped"
        fi
    fi

    # Pin the install at the new tag so the manager picks it up.
    if [ -f "$state_file" ]; then
        new_tag="$(python3 -c "import json; print(json.load(open('$state_file')).get('config',{}).get('image_tag',''))" 2>/dev/null)"
        if [ -n "$new_tag" ] && [ -f "$env_file" ]; then
            if grep -q '^COPPERCLAW_DEFAULT_IMAGE_TAG=' "$env_file"; then
                sed -i.bak "s|^COPPERCLAW_DEFAULT_IMAGE_TAG=.*|COPPERCLAW_DEFAULT_IMAGE_TAG=$new_tag|" "$env_file"
                rm -f "$env_file.bak"
            else
                printf '\nCOPPERCLAW_DEFAULT_IMAGE_TAG=%s\n' "$new_tag" >> "$env_file"
            fi
            say "pinned COPPERCLAW_DEFAULT_IMAGE_TAG=$new_tag"
        fi

        # Repoint every existing agent group at the new image. The .env
        # default only applies to *new* groups; existing rows in
        # container_configs keep their previously-pinned tag and silently
        # run yesterday's runner binary forever otherwise. Caught live
        # when a fresh runner with new apology text shipped but the
        # running session kept emitting the old apology because its
        # container_configs row still pointed at the old image.
        #
        # Invariants enforced below:
        #   - $new_tag is interpolated directly into SQL, so we validate
        #     it against a conservative OCI-tag charset (no quotes, no
        #     spaces) before running UPDATE. Tags today are sha256 hex,
        #     but the regex documents and enforces the assumption.
        #   - updated_at uses strftime('%Y-%m-%dT%H:%M:%fZ','now') — must
        #     be RFC3339 because chrono's parse_from_rfc3339 rejects
        #     sqlite's default space-separated datetime('now') output
        #     ("2026-05-23 21:51:53") with "premature end of input".
        central_db="$DATA_DIR/copperclaw.db"
        if [ -n "$new_tag" ] && [ -f "$central_db" ]; then
            if ! command -v sqlite3 >/dev/null 2>&1; then
                warn "sqlite3 not installed; skipping container_configs repoint. Install sqlite3 or run 'cclaw groups config update <id> image_tag $new_tag' manually for each existing group."
            elif [[ ! "$new_tag" =~ ^[A-Za-z0-9._:/-]+$ ]]; then
                warn "refusing to repoint container_configs: image_tag '$new_tag' contains characters outside [A-Za-z0-9._:/-]; rerun setup or repoint manually with 'cclaw groups config update'."
            else
                stale_count="$(sqlite3 "$central_db" \
                    "select count(*) from container_configs where coalesce(image_tag,'') != '$new_tag';" 2>/dev/null || echo 0)"
                if [[ ! "$stale_count" =~ ^[0-9]+$ ]]; then
                    warn "sqlite3 returned non-numeric count ('$stale_count') for container_configs; skipping repoint."
                elif [ "$stale_count" -gt 0 ]; then
                    sqlite3 "$central_db" \
                        "update container_configs set image_tag='$new_tag', updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') where coalesce(image_tag,'') != '$new_tag';"
                    say "repointed $stale_count agent group(s) to $new_tag"
                fi
            fi
        fi
    fi
fi

# ── Step 4: start ─────────────────────────────────────────────────────
if [ "$do_start" = 1 ]; then
    say "starting host in the background"
    "$INSTALL_DIR/copperclaw" start
fi

# ── Step 5: tell the human what just happened ─────────────────────────
say "done. installed binaries:"
for b in copperclaw cclaw copperclaw-setup; do
    if [ -x "$INSTALL_DIR/$b" ]; then
        printf '    %s  (%s bytes)\n' "$INSTALL_DIR/$b" "$(stat -c '%s' "$INSTALL_DIR/$b" 2>/dev/null || stat -f '%z' "$INSTALL_DIR/$b")"
    fi
done

if [ "$do_start" = 1 ]; then
    cat <<EOF

  Try:    cclaw chat           # type a message, get a reply
  Status: cclaw                 # no-args dashboard
          copperclaw status       # PID, uptime, paths
          copperclaw logs -f      # tail the host log
  Stop:   copperclaw stop

EOF
fi
