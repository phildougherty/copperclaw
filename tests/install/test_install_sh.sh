#!/usr/bin/env bash
# tests/install/test_install_sh.sh
#
# End-to-end test for the top-level install.sh.  Spins up a clean Ubuntu
# container, mounts the repo read-only, drives install.sh under several
# scenarios, and asserts post-conditions.
#
# Usage:
#   bash tests/install/test_install_sh.sh           # run all four cases
#   bash tests/install/test_install_sh.sh case_3    # run a single case
#
# Requirements (host): docker or podman in PATH, network reachable to pull
# the ubuntu:24.04 base image once.
#
# The cases use a tiny derived image (built once per run) that pre-installs
# bash, ca-certificates, curl, and the rust toolchain via the apt
# `rustc`/`cargo` packages.  We deliberately *do not* install Docker inside
# the container — that's what INSTALL_SH_SKIP_DOCKER_CHECK is for.

set -euo pipefail

# ----- container runtime detection -------------------------------------------

CONTAINER_BIN="${CONTAINER_BIN:-}"
if [ -z "$CONTAINER_BIN" ]; then
    if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
        CONTAINER_BIN="docker"
    elif command -v podman >/dev/null 2>&1 && podman info >/dev/null 2>&1; then
        CONTAINER_BIN="podman"
    else
        echo "test_install_sh: no usable container runtime found (need docker or podman)" >&2
        echo "                 set CONTAINER_BIN=... to override" >&2
        exit 2
    fi
fi

# ----- paths -----------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
IMAGE_TAG="${IRONCLAW_INSTALL_TEST_IMAGE:-ironclaw-install-test:latest}"

# ----- output helpers --------------------------------------------------------

if [ -t 1 ]; then
    C_RESET=$'\033[0m'; C_BOLD=$'\033[1m'
    C_RED=$'\033[31m'; C_GREEN=$'\033[32m'; C_YELLOW=$'\033[33m'; C_BLUE=$'\033[34m'
else
    C_RESET=""; C_BOLD=""; C_RED=""; C_GREEN=""; C_YELLOW=""; C_BLUE=""
fi

step()  { printf '%s==>%s %s\n' "${C_BLUE}${C_BOLD}" "${C_RESET}" "$*"; }
ok()    { printf '%s ok%s %s\n' "${C_GREEN}" "${C_RESET}" "$*"; }
fail()  { printf '%sFAIL%s %s\n' "${C_RED}${C_BOLD}" "${C_RESET}" "$*" >&2; exit 1; }
note()  { printf '%s  -%s %s\n' "${C_YELLOW}" "${C_RESET}" "$*"; }

# ----- build the test image once --------------------------------------------

build_image() {
    step "building test image ($IMAGE_TAG) via $CONTAINER_BIN"
    local dockerfile="$SCRIPT_DIR/Dockerfile.test"
    if [ ! -f "$dockerfile" ]; then
        fail "missing $dockerfile"
    fi
    # Build with empty context — Dockerfile pulls everything it needs from apt.
    "$CONTAINER_BIN" build --quiet -t "$IMAGE_TAG" -f "$dockerfile" "$SCRIPT_DIR" >/dev/null
    ok "image ready"
}

# ----- run helper ------------------------------------------------------------

# run_in_container <env_assignments...> -- <command>
#   Mounts the repo read-only at /repo and runs the given command as a
#   non-root user inside the container.  Captures combined stdout/stderr;
#   echoes them on failure.
run_in_container() {
    local env_args=()
    while [ "${1-}" != "--" ]; do
        env_args+=(-e "$1")
        shift
    done
    shift  # drop the --
    local cmd=("$@")

    # Use an isolated HOME via a tmpfs so successive cases start clean.
    "$CONTAINER_BIN" run --rm \
        --network none \
        -v "$REPO_ROOT:/repo:ro" \
        -e HOME=/home/tester \
        "${env_args[@]}" \
        "$IMAGE_TAG" \
        bash -c "${cmd[*]}"
}

# ----- cases -----------------------------------------------------------------

# Case 1: fresh Ubuntu, no Docker.  install.sh should detect the missing
# runtime, exit non-zero, and mention Docker.
case_1_missing_docker() {
    step "case 1: missing docker runtime → clean failure"
    local out exit_code=0
    out="$(run_in_container -- 'bash /repo/install.sh 2>&1 || echo "__EXIT_$?__"')" || exit_code=$?
    if [ "$exit_code" -ne 0 ]; then
        fail "expected the inner command to exit 0 (we capture the install.sh exit ourselves); got $exit_code; output: $out"
    fi
    if ! grep -q '__EXIT_' <<<"$out"; then
        fail "install.sh did not exit non-zero. output: $out"
    fi
    local sh_exit
    sh_exit="$(grep -o '__EXIT_[0-9]*__' <<<"$out" | head -1 | sed 's/__EXIT_\(.*\)__/\1/')"
    if [ "$sh_exit" = "0" ]; then
        fail "install.sh exited 0 but should have failed on missing docker. output: $out"
    fi
    if ! grep -qi 'docker\|container runtime' <<<"$out"; then
        fail "install.sh failure did not mention Docker/container runtime. output: $out"
    fi
    ok "exit=$sh_exit, output mentions docker/container runtime"
}

# Case 2: install.sh with INSTALL_SH_SKIP_DOCKER_CHECK=1 + IRONCLAW_SKIP_SETUP=1
# should install the three binaries via cargo --path (we have the workspace
# checkout mounted read-only — copy it into a writable scratch dir first).
case_2_install_binaries() {
    step "case 2: binary install via cargo --path (escape-hatch)"
    local script
    # The /repo mount is read-only, so copy the workspace into /tmp/iclaw-src
    # before running install.sh.  Cargo writes target/ during the build.
    # Hand-build with --offline disabled (we need crates.io); allow network.
    script=$(cat <<'INNER'
set -euo pipefail
cp -a /repo /tmp/iclaw-src
chown -R tester:tester /tmp/iclaw-src
sudo -u tester -H bash -c '
    set -euo pipefail
    export PATH="/home/tester/.local/bin:$PATH"
    export CARGO_HOME="$HOME/.cargo"
    INSTALL_SH_SKIP_DOCKER_CHECK=1 \
    IRONCLAW_SKIP_SETUP=1 \
    bash /tmp/iclaw-src/install.sh
    test -x "$HOME/.local/bin/ironclaw"
    test -x "$HOME/.local/bin/iclaw"
    test -x "$HOME/.local/bin/ironclaw-setup"
'
INNER
)
    # Allow network for crates.io.  This case is the slow one — a clean
    # `cargo install --path` for three crates from scratch dwarfs every
    # other case combined.  Opt-in via IRONCLAW_INSTALL_TEST_RUN_BUILD=1
    # to keep the default suite under the 2-minute budget.
    if [ "${IRONCLAW_INSTALL_TEST_RUN_BUILD:-0}" != "1" ]; then
        note "skipping case 2 (set IRONCLAW_INSTALL_TEST_RUN_BUILD=1 to run; ~5+ min)"
        return 0
    fi
    "$CONTAINER_BIN" run --rm \
        --user 0 \
        -v "$REPO_ROOT:/repo:ro" \
        "$IMAGE_TAG" \
        bash -c "$script"
    ok "three binaries installed under ~/.local/bin"
}

# Case 3: re-running install.sh on an already-installed system should not
# blow away existing binaries.  We mock pre-existing binaries with sentinel
# content, then run install.sh in dry-run mode (so it exits before touching
# them) and assert the sentinels are intact.
case_3_idempotent_rerun() {
    step "case 3: re-run preserves existing binaries (dry-run gate)"
    local script
    script=$(cat <<'INNER'
set -euo pipefail
mkdir -p /home/tester/.local/bin
for b in ironclaw iclaw ironclaw-setup; do
    printf '#!/bin/sh\necho sentinel-%s\n' "$b" > "/home/tester/.local/bin/$b"
    chmod +x "/home/tester/.local/bin/$b"
done
# Dry-run exits before touching the filesystem.
IRONCLAW_INSTALL_DRY_RUN=1 \
INSTALL_SH_SKIP_DOCKER_CHECK=1 \
bash /repo/install.sh >/dev/null
# Sentinels must still be byte-identical.
for b in ironclaw iclaw ironclaw-setup; do
    grep -q "sentinel-$b" "/home/tester/.local/bin/$b" || { echo "sentinel for $b clobbered"; exit 1; }
done
INNER
)
    run_in_container -- "$script"
    ok "pre-existing sentinels intact after re-run"
}

# Case 4: platform detection — IRONCLAW_FORCE_TARGET + IRONCLAW_INSTALL_DRY_RUN
# should print the correct tarball URL for each supported triple.
case_4_platform_detection() {
    step "case 4: platform detection picks the right tarball URL"
    local triple expected out
    for triple in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
                  x86_64-apple-darwin aarch64-apple-darwin; do
        expected="https://github.com/phildougherty/ironclaw/releases/latest/download/ironclaw-${triple}.tar.gz"
        out="$(run_in_container \
            "IRONCLAW_INSTALL_DRY_RUN=1" \
            "IRONCLAW_FORCE_TARGET=$triple" \
            "INSTALL_SH_SKIP_DOCKER_CHECK=1" \
            -- 'bash /repo/install.sh')"
        if ! grep -qF "$expected" <<<"$out"; then
            fail "expected URL not found for $triple. expected=$expected output=$out"
        fi
        note "$triple -> ok"
    done
    # And an explicit tag.
    expected="https://github.com/phildougherty/ironclaw/releases/download/v9.9.9/ironclaw-x86_64-unknown-linux-gnu.tar.gz"
    out="$(run_in_container \
        "IRONCLAW_INSTALL_DRY_RUN=1" \
        "IRONCLAW_FORCE_TARGET=x86_64-unknown-linux-gnu" \
        "IRONCLAW_RELEASE_TAG=v9.9.9" \
        "INSTALL_SH_SKIP_DOCKER_CHECK=1" \
        -- 'bash /repo/install.sh')"
    if ! grep -qF "$expected" <<<"$out"; then
        fail "explicit tag URL mismatch. expected=$expected output=$out"
    fi
    ok "all four triples + explicit-tag URL render correctly"
}

# ----- driver ----------------------------------------------------------------

main() {
    local only="${1-}"
    build_image
    if [ -n "$only" ]; then
        "$only"
    else
        case_1_missing_docker
        case_3_idempotent_rerun
        case_4_platform_detection
        case_2_install_binaries
    fi
    printf '\n%sall install.sh integration cases passed%s\n' "${C_GREEN}${C_BOLD}" "${C_RESET}"
}

main "$@"
