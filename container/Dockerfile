# Ironclaw session base image.
#
# Published to ghcr.io/<repo>/session by the .github/workflows/container-image.yml
# workflow on every push to main (as :edge) and on tagged releases (as
# :<version> and :latest). Setup pulls a pre-built image to avoid a
# 1-2 minute cold-start `docker build` on first run.
#
# Keep this file's package set in sync with the implicit baseline that
# `ImageBuildSpec::dockerfile()` renders in `ironclaw-container-rt`.
# The setup step layers session-specific apt/npm packages and the
# runner binary on top of this image at install time.

FROM debian:trixie-slim

# Fingerprint of the spec inputs that produced this image. Stamped at
# build time so `ironclaw-setup` can verify that a pulled image matches
# the locally-expected build before adopting it.
ARG IRONCLAW_FINGERPRINT=unknown
LABEL ironclaw.fingerprint="${IRONCLAW_FINGERPRINT}"
LABEL org.opencontainers.image.source="https://github.com/phildougherty/ironclaw"
LABEL org.opencontainers.image.description="Ironclaw session container base image"
LABEL org.opencontainers.image.licenses="MIT"

# Minimal runtime prerequisites every session container needs: TLS roots
# (for outbound HTTPS) and a current ca-certificates bundle. Keep this
# list tight — anything session-specific belongs in the per-install
# layer rendered by ironclaw-container-rt.
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      ca-certificates \
 && rm -rf /var/lib/apt/lists/*
