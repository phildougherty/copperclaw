#!/usr/bin/env bash
# install.sh — one-command installer for copperclaw.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/phildougherty/copperclaw/main/install.sh | bash
#   ./install.sh                                   # from inside a checkout
#   COPPERCLAW_REPO=owner/fork ./install.sh          # override release source
#   COPPERCLAW_INSTALL_DIR=$HOME/bin ./install.sh    # override install prefix
#   COPPERCLAW_SKIP_SETUP=1 ./install.sh             # install binaries only
#
# Picks the first of these that works, in order:
#   (a) prebuilt tarball from GitHub Releases
#   (b) `cargo install --git` from the upstream repo
#   (c) `cargo install --path` if run from a workspace checkout
#
# Out of scope: installing Docker or Rust. The script detects them and
# prints the appropriate platform-specific install hint if missing.
#
# Test-only escape hatches (default-off; preserve normal behaviour):
#   INSTALL_SH_SKIP_DOCKER_CHECK=1   # skip the container-runtime check
#   COPPERCLAW_INSTALL_DRY_RUN=1       # print the release-tarball URL and exit 0
#   COPPERCLAW_FORCE_TARGET=<triple>   # override platform detection (e.g.
#                                    # aarch64-unknown-linux-gnu) — pairs with
#                                    # the dry-run mode for tarball-URL tests.

set -euo pipefail

# ----- configuration ---------------------------------------------------------

COPPERCLAW_REPO="${COPPERCLAW_REPO:-phildougherty/copperclaw}"
COPPERCLAW_INSTALL_DIR="${COPPERCLAW_INSTALL_DIR:-$HOME/.local/bin}"
COPPERCLAW_SKIP_SETUP="${COPPERCLAW_SKIP_SETUP:-0}"
COPPERCLAW_FORCE_REINSTALL="${COPPERCLAW_FORCE_REINSTALL:-0}"
COPPERCLAW_RELEASE_TAG="${COPPERCLAW_RELEASE_TAG:-latest}"
# TODO(team-j): test-only knobs. Default off; do not document beyond the
# header comment. Used by tests/install/test_install_sh.sh.
INSTALL_SH_SKIP_DOCKER_CHECK="${INSTALL_SH_SKIP_DOCKER_CHECK:-0}"
COPPERCLAW_INSTALL_DRY_RUN="${COPPERCLAW_INSTALL_DRY_RUN:-0}"
COPPERCLAW_FORCE_TARGET="${COPPERCLAW_FORCE_TARGET:-}"

BINARIES=(copperclaw cclaw copperclaw-setup)
# crate path per binary, used by the --path fallback.
crate_for_bin() {
    case "$1" in
        copperclaw)       echo "crates/copperclaw-host" ;;
        cclaw)          echo "crates/copperclaw-cclaw" ;;
        copperclaw-setup) echo "crates/copperclaw-setup" ;;
        *)              return 1 ;;
    esac
}
# crate name per binary, used by the --git fallback (cargo install <name>).
crate_name_for_bin() {
    case "$1" in
        copperclaw)       echo "copperclaw-host" ;;
        cclaw)          echo "copperclaw-cclaw" ;;
        copperclaw-setup) echo "copperclaw-setup" ;;
        *)              return 1 ;;
    esac
}

# Resolve our own directory so relative paths work no matter where we are
# invoked from. This stays unset if piped through bash without a real file
# on disk, in which case (c) is unavailable and we fall through.
SCRIPT_DIR=""
if [ "${BASH_SOURCE[0]:-}" != "" ] && [ -f "${BASH_SOURCE[0]}" ]; then
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
fi

# ----- colour / output -------------------------------------------------------

if [ -n "${NO_COLOR:-}" ] || [ ! -t 1 ]; then
    C_RESET=""; C_BOLD=""; C_DIM=""; C_RED=""; C_YELLOW=""; C_GREEN=""; C_BLUE=""
else
    C_RESET=$'\033[0m'; C_BOLD=$'\033[1m'; C_DIM=$'\033[2m'
    C_RED=$'\033[31m'; C_YELLOW=$'\033[33m'; C_GREEN=$'\033[32m'; C_BLUE=$'\033[34m'
fi

step()  { printf '%s==>%s %s\n'   "${C_BLUE}${C_BOLD}" "${C_RESET}" "$*"; }
ok()    { printf '%s ok%s %s\n'   "${C_GREEN}"        "${C_RESET}" "$*"; }
warn()  { printf '%swarn%s %s\n'  "${C_YELLOW}"       "${C_RESET}" "$*" >&2; }
err()   { printf '%serr%s %s\n'   "${C_RED}${C_BOLD}" "${C_RESET}" "$*" >&2; }
dim()   { printf '%s%s%s\n'       "${C_DIM}"          "$*" "${C_RESET}"; }

# Verbose log buffer — kept silent on success, dumped on failure for triage.
LOG_FILE="$(mktemp -t copperclaw-install.XXXXXX.log)"
trap '_on_exit $?' EXIT

_on_exit() {
    local code="$1"
    if [ "$code" -ne 0 ]; then
        err "install failed (exit $code). Verbose log:"
        if [ -s "$LOG_FILE" ]; then
            sed 's/^/  /' "$LOG_FILE" >&2 || true
        fi
        err "log preserved at $LOG_FILE"
    else
        rm -f "$LOG_FILE" 2>/dev/null || true
    fi
}

run_quiet() {
    # Run a command; show output only on failure (via the global EXIT trap or
    # via an explicit caller check). Always tee into LOG_FILE.
    if "$@" >>"$LOG_FILE" 2>&1; then
        return 0
    else
        return 1
    fi
}

# ----- platform detection ----------------------------------------------------

detect_platform() {
    local uname_s uname_m os arch triple
    # TODO(team-j): COPPERCLAW_FORCE_TARGET lets the install-sh integration test
    # exercise the per-triple tarball URL without faking uname. No effect when
    # unset.
    if [ -n "$COPPERCLAW_FORCE_TARGET" ]; then
        case "$COPPERCLAW_FORCE_TARGET" in
            x86_64-unknown-linux-gnu)   os="linux";  arch="x86_64" ;;
            aarch64-unknown-linux-gnu)  os="linux";  arch="aarch64" ;;
            x86_64-apple-darwin)        os="macos";  arch="x86_64" ;;
            aarch64-apple-darwin)       os="macos";  arch="aarch64" ;;
            *)
                err "COPPERCLAW_FORCE_TARGET=$COPPERCLAW_FORCE_TARGET is not a recognised triple"
                exit 1 ;;
        esac
        PLATFORM_OS="$os"
        # shellcheck disable=SC2034  # exposed for completeness / future use
        PLATFORM_ARCH="$arch"
        PLATFORM_TRIPLE="$COPPERCLAW_FORCE_TARGET"
        ok "platform: $os/$arch ($COPPERCLAW_FORCE_TARGET) [forced]"
        return 0
    fi
    uname_s="$(uname -s 2>/dev/null || echo unknown)"
    uname_m="$(uname -m 2>/dev/null || echo unknown)"

    case "$uname_s" in
        Linux)   os="linux" ;;
        Darwin)  os="macos" ;;
        MINGW*|MSYS*|CYGWIN*)
            err "native Windows is not supported. Use WSL2 and re-run inside the WSL shell."
            exit 1 ;;
        *)
            err "unsupported OS: $uname_s"
            exit 1 ;;
    esac

    case "$uname_m" in
        x86_64|amd64)  arch="x86_64" ;;
        aarch64|arm64) arch="aarch64" ;;
        *)
            err "unsupported architecture: $uname_m"
            exit 1 ;;
    esac

    # Map to Rust target triple for the release tarball name.
    if [ "$os" = "linux" ]; then
        if [ "$arch" = "x86_64" ]; then
            triple="x86_64-unknown-linux-gnu"
        else
            triple="aarch64-unknown-linux-gnu"
        fi
    else
        if [ "$arch" = "x86_64" ]; then
            triple="x86_64-apple-darwin"
        else
            triple="aarch64-apple-darwin"
        fi
    fi

    PLATFORM_OS="$os"
    # shellcheck disable=SC2034  # exposed for completeness / future use
    PLATFORM_ARCH="$arch"
    PLATFORM_TRIPLE="$triple"
    ok "platform: $os/$arch ($triple)"
}

# ----- container runtime check ----------------------------------------------

check_container_runtime() {
    # TODO(team-j): the install-sh integration test runs in containers that
    # don't have Docker/Podman available — opt out cleanly.
    if [ "$INSTALL_SH_SKIP_DOCKER_CHECK" = "1" ]; then
        dim "container-runtime check skipped (INSTALL_SH_SKIP_DOCKER_CHECK=1)"
        return 0
    fi
    local found=""
    if command -v docker >/dev/null 2>&1; then
        if docker info >/dev/null 2>&1; then
            found="docker"
        else
            warn "docker binary found but the daemon is unreachable"
            warn "  start it (Linux: 'sudo systemctl start docker'; macOS: open Docker Desktop) and re-run"
            exit 1
        fi
    elif command -v podman >/dev/null 2>&1; then
        if podman info >/dev/null 2>&1; then
            found="podman"
        else
            warn "podman binary found but the daemon is unreachable"
            warn "  run 'podman machine start' (macOS) or check 'systemctl --user status podman' (Linux)"
            exit 1
        fi
    elif [ "$PLATFORM_OS" = "macos" ] && command -v container >/dev/null 2>&1; then
        # Apple Container (the new macOS-native container runtime). Detected
        # by copperclaw-setup's env_check step as well; included here so a
        # fresh mac user with only Apple Container installed doesn't get a
        # misleading "install Docker" prompt.
        found="apple-container"
    fi

    if [ -z "$found" ]; then
        err "no container runtime found (looked for docker, podman, container)."
        case "$PLATFORM_OS" in
            linux)
                err "  install Docker: https://docs.docker.com/engine/install/"
                err "  or Podman:      sudo apt-get install podman   # or your distro's package manager" ;;
            macos)
                err "  install Docker Desktop: https://docs.docker.com/desktop/install/mac-install/"
                err "  or Podman:              brew install podman && podman machine init && podman machine start"
                err "  or Apple Container:     https://github.com/apple/container (macOS 15+)" ;;
        esac
        exit 1
    fi
    ok "container runtime: $found"
}

# ----- download helpers ------------------------------------------------------

have_curl() { command -v curl >/dev/null 2>&1; }
have_wget() { command -v wget >/dev/null 2>&1; }

download() {
    # download <url> <dest>
    local url="$1" dest="$2"
    if have_curl; then
        curl -fsSL --retry 2 -o "$dest" "$url"
    elif have_wget; then
        wget -q -O "$dest" "$url"
    else
        err "need curl or wget to download artifacts"
        exit 1
    fi
}

remote_exists() {
    # 200 → 0, 404 → 1, anything else → 2 (so callers can distinguish).
    local url="$1"
    if have_curl; then
        local code
        code="$(curl -fsSL -o /dev/null -w '%{http_code}' --retry 1 -I "$url" 2>/dev/null || echo 000)"
        case "$code" in
            2*) return 0 ;;
            404) return 1 ;;
            *) return 2 ;;
        esac
    elif have_wget; then
        if wget -q --spider "$url" 2>/dev/null; then return 0; else return 1; fi
    else
        return 2
    fi
}

# ----- existing-install detection -------------------------------------------

already_installed() {
    local missing=0
    for b in "${BINARIES[@]}"; do
        if [ ! -x "$COPPERCLAW_INSTALL_DIR/$b" ] && ! command -v "$b" >/dev/null 2>&1; then
            missing=1
            break
        fi
    done
    [ "$missing" -eq 0 ]
}

prompt_upgrade_or_skip() {
    # Prompt only when we have a real TTY; otherwise pick "upgrade".
    if [ ! -t 0 ] || [ ! -t 1 ]; then
        warn "existing install detected; re-installing (non-interactive)"
        return 0
    fi
    printf '%scopperclaw is already installed.%s\n' "${C_BOLD}" "${C_RESET}"
    printf '  [u] upgrade / reinstall (default)\n'
    printf '  [s] skip binary install, just rerun setup\n'
    printf '  [q] quit\n'
    printf 'choice: '
    local ans=""
    read -r ans || ans=""
    case "${ans:-u}" in
        q|Q) ok "nothing to do"; exit 0 ;;
        s|S) INSTALL_SKIP_BINS=1; return 0 ;;
        *)   return 0 ;;
    esac
}

# ----- install strategies ----------------------------------------------------

install_via_release() {
    local tag="$COPPERCLAW_RELEASE_TAG" base
    if [ "$tag" = "latest" ]; then
        base="https://github.com/${COPPERCLAW_REPO}/releases/latest/download"
    else
        base="https://github.com/${COPPERCLAW_REPO}/releases/download/${tag}"
    fi
    local tarball="copperclaw-${PLATFORM_TRIPLE}.tar.gz"
    local url="$base/$tarball"

    step "checking for prebuilt release at $url"
    if ! remote_exists "$url"; then
        dim "  no prebuilt release for $PLATFORM_TRIPLE — falling back to source build"
        return 1
    fi

    local tmpdir
    tmpdir="$(mktemp -d -t copperclaw-dl.XXXXXX)"
    # shellcheck disable=SC2064
    trap "rm -rf '$tmpdir'" RETURN

    step "downloading $tarball"
    if ! run_quiet download "$url" "$tmpdir/$tarball"; then
        warn "download failed — falling back to source build"
        return 1
    fi

    step "extracting"
    if ! run_quiet tar -xzf "$tmpdir/$tarball" -C "$tmpdir"; then
        warn "extract failed — falling back to source build"
        return 1
    fi

    mkdir -p "$COPPERCLAW_INSTALL_DIR"
    local bin found_any=0
    for bin in "${BINARIES[@]}"; do
        # Tarballs may put binaries at the top level or inside a versioned
        # subdirectory. Accept either.
        local src
        src="$(find "$tmpdir" -type f -name "$bin" -print -quit 2>/dev/null || true)"
        if [ -z "$src" ]; then
            warn "tarball did not contain $bin — falling back to source build"
            return 1
        fi
        install -m 0755 "$src" "$COPPERCLAW_INSTALL_DIR/$bin"
        found_any=1
    done
    [ "$found_any" -eq 1 ] || return 1
    ok "installed ${BINARIES[*]} -> $COPPERCLAW_INSTALL_DIR"
    return 0
}

check_cargo() {
    if command -v cargo >/dev/null 2>&1; then return 0; fi
    err "cargo (Rust) is required to build from source but is not installed."
    err "  install rustup with:"
    err "    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    err "  then re-open your shell and re-run this installer."
    exit 1
}

cargo_root() {
    # cargo --root places binaries in <root>/bin. If COPPERCLAW_INSTALL_DIR
    # already ends in /bin, strip it so cargo's implicit /bin lines up with
    # what the user asked for. Otherwise warn — the user's directory will
    # gain a /bin subdirectory containing the actual binaries.
    case "$COPPERCLAW_INSTALL_DIR" in
        */bin) echo "${COPPERCLAW_INSTALL_DIR%/bin}" ;;
        *)
            warn "COPPERCLAW_INSTALL_DIR ($COPPERCLAW_INSTALL_DIR) does not end in /bin;"
            warn "  cargo will install into $COPPERCLAW_INSTALL_DIR/bin instead."
            # TODO(team-a): consider symlinking after install for paths that
            # don't end in /bin, or rejecting non-/bin dirs outright.
            echo "$COPPERCLAW_INSTALL_DIR" ;;
    esac
}

install_via_cargo_git() {
    check_cargo
    step "building from source via 'cargo install --git'"
    local crate_name
    local args=(install --locked --root "$(cargo_root)" \
                --git "https://github.com/${COPPERCLAW_REPO}.git")
    for bin in "${BINARIES[@]}"; do
        crate_name="$(crate_name_for_bin "$bin")"
        args+=("$crate_name")
    done
    if ! run_quiet cargo "${args[@]}"; then
        warn "'cargo install --git' failed"
        return 1
    fi
    ok "installed ${BINARIES[*]} -> $COPPERCLAW_INSTALL_DIR"
    return 0
}

install_via_cargo_path() {
    [ -n "$SCRIPT_DIR" ] || return 1
    [ -f "$SCRIPT_DIR/Cargo.toml" ] || return 1
    check_cargo
    step "building from source checkout at $SCRIPT_DIR"
    for bin in "${BINARIES[@]}"; do
        local crate_path
        crate_path="$SCRIPT_DIR/$(crate_for_bin "$bin")"
        if [ ! -f "$crate_path/Cargo.toml" ]; then
            warn "expected crate at $crate_path but it is missing"
            return 1
        fi
        if ! run_quiet cargo install --locked --path "$crate_path" \
                --root "$(cargo_root)" --force; then
            warn "'cargo install --path $crate_path' failed"
            return 1
        fi
    done
    ok "installed ${BINARIES[*]} -> $COPPERCLAW_INSTALL_DIR"
    return 0
}

install_binaries() {
    if [ "${INSTALL_SKIP_BINS:-0}" -eq 1 ]; then
        dim "skipping binary install (user choice)"
        return 0
    fi
    mkdir -p "$COPPERCLAW_INSTALL_DIR"
    if install_via_release; then return 0; fi
    # Prefer a local checkout if we're sitting in one — faster, fewer surprises
    # than reaching for the network.
    if install_via_cargo_path; then return 0; fi
    if install_via_cargo_git;  then return 0; fi
    err "all install strategies failed; see the verbose log above"
    exit 1
}

# ----- PATH warning ----------------------------------------------------------

warn_if_not_on_path() {
    case ":${PATH:-}:" in
        *":$COPPERCLAW_INSTALL_DIR:"*) return 0 ;;
    esac
    local shell_rc=""
    case "${SHELL:-}" in
        */zsh)  shell_rc="$HOME/.zshrc" ;;
        */bash) shell_rc="$HOME/.bashrc" ;;
        */fish) shell_rc="$HOME/.config/fish/config.fish" ;;
        *)      shell_rc="your shell rc" ;;
    esac
    warn "$COPPERCLAW_INSTALL_DIR is not on \$PATH"
    if [ "${SHELL:-}" = "${SHELL%/fish}" ] || [ "${SHELL:-}" = "" ]; then
        warn "  add this to $shell_rc:"
        warn "    export PATH=\"$COPPERCLAW_INSTALL_DIR:\$PATH\""
    else
        warn "  add this to $shell_rc:"
        warn "    set -gx PATH $COPPERCLAW_INSTALL_DIR \$PATH"
    fi
}

# ----- locate setup state & launch setup ------------------------------------

setup_data_dir() {
    if [ -n "${COPPERCLAW_DATA_DIR:-}" ]; then
        echo "$COPPERCLAW_DATA_DIR"; return
    fi
    case "$PLATFORM_OS" in
        linux)
            if [ -n "${XDG_DATA_HOME:-}" ]; then
                echo "$XDG_DATA_HOME/copperclaw"
            else
                echo "$HOME/.local/share/copperclaw"
            fi ;;
        macos) echo "$HOME/Library/Application Support/copperclaw" ;;
    esac
}

state_file_path() {
    echo "$(setup_data_dir)/setup-state.json"
}

run_setup() {
    if [ "$COPPERCLAW_SKIP_SETUP" = "1" ]; then
        dim "skipping copperclaw-setup (COPPERCLAW_SKIP_SETUP=1)"
        return 0
    fi

    local setup_bin="$COPPERCLAW_INSTALL_DIR/copperclaw-setup"
    if [ ! -x "$setup_bin" ]; then
        if command -v copperclaw-setup >/dev/null 2>&1; then
            setup_bin="$(command -v copperclaw-setup)"
        else
            err "copperclaw-setup not found after install — something went wrong"
            exit 1
        fi
    fi

    local state
    state="$(state_file_path)"
    local mode="run"
    if [ -f "$state" ]; then
        if [ -t 0 ] && [ -t 1 ]; then
            printf '%sfound existing setup state at %s%s\n' "${C_BOLD}" "$state" "${C_RESET}"
            printf '  [r] resume (default) — re-runs only incomplete steps\n'
            printf '  [f] re-run from scratch (existing config preserved as defaults)\n'
            printf '  [s] skip setup entirely\n'
            printf 'choice: '
            local ans=""
            read -r ans || ans=""
            case "${ans:-r}" in
                s|S) mode="skip" ;;
                f|F) mode="force" ;;
                *)   mode="resume" ;;
            esac
        else
            mode="resume"
        fi
    fi

    if [ "$mode" = "skip" ]; then
        dim "skipping setup"
        return 0
    fi

    if [ "$mode" = "force" ]; then
        # Move the old state aside; setup will start fresh but keep the data
        # dir intact so existing dbs etc. survive.
        local backup
        backup="${state}.bak.$(date +%s)"
        mv "$state" "$backup"
        dim "moved old setup state to $backup"
    fi

    # If headless, surface the prompt-passthrough envs setup understands so
    # the user can re-run unattended.
    local extra_args=()
    if [ "${COPPERCLAW_SETUP_HEADLESS:-0}" = "1" ]; then
        extra_args+=(--headless)
    fi
    if [ -n "${COPPERCLAW_DATA_DIR:-}" ]; then
        extra_args+=(--data-dir "$COPPERCLAW_DATA_DIR")
    fi

    step "running copperclaw-setup"
    # Run setup *attached* — it's interactive by default. Don't swallow.
    if ! "$setup_bin" "${extra_args[@]}"; then
        err "copperclaw-setup exited non-zero"
        exit 1
    fi
    ok "setup complete"
}

# ----- final guidance --------------------------------------------------------

print_next_steps() {
    local data_dir
    data_dir="$(setup_data_dir)"
    cat <<EOF

${C_BOLD}${C_GREEN}copperclaw is installed.${C_RESET}

Start the host:
  ${C_BOLD}copperclaw run${C_RESET}

In another terminal, talk to it:
  ${C_BOLD}cclaw chat${C_RESET}

Useful one-shots:
  cclaw status            # full wiring digest
  cclaw health            # operator probe (sessions, audit, drops)
  cclaw usage --since 24h # per-group token rollup

Data directory: ${data_dir}
Logs:           ${data_dir}/logs/
Docs:           https://github.com/${COPPERCLAW_REPO}#documentation

EOF
}

# ----- main ------------------------------------------------------------------

print_release_url() {
    local tag="$COPPERCLAW_RELEASE_TAG" base
    if [ "$tag" = "latest" ]; then
        base="https://github.com/${COPPERCLAW_REPO}/releases/latest/download"
    else
        base="https://github.com/${COPPERCLAW_REPO}/releases/download/${tag}"
    fi
    printf '%s/copperclaw-%s.tar.gz\n' "$base" "$PLATFORM_TRIPLE"
}

main() {
    step "copperclaw installer"
    detect_platform

    # TODO(team-j): dry-run prints the tarball URL the installer would fetch
    # and exits 0, without touching docker, cargo, or the filesystem. Used by
    # the platform-detection test case.
    if [ "$COPPERCLAW_INSTALL_DRY_RUN" = "1" ]; then
        print_release_url
        exit 0
    fi

    check_container_runtime

    if already_installed && [ "$COPPERCLAW_FORCE_REINSTALL" != "1" ]; then
        prompt_upgrade_or_skip
    fi

    install_binaries
    warn_if_not_on_path
    run_setup
    print_next_steps
}

main "$@"
