# syntax=docker/dockerfile:1
# Neolink Docker image build scripts
# Copyright (c) 2020 George Hilliard,
#                    Andrew King,
#                    Miroslav Šedivý
# SPDX-License-Identifier: AGPL-3.0-only

FROM rust:1-slim AS build
ARG TARGETPLATFORM

ENV DEBIAN_FRONTEND=noninteractive
WORKDIR /usr/local/src/neolink
COPY . /usr/local/src/neolink

# Build the main program or copy from artifact
#
# We prefer copying from artifact to reduce
# build time on the github runners
#
# Because of this though, during normal
# github runner ops we are not testing the
# docker to see if it will build from scratch
# so if it is failing please make a PR
RUN rm -f /etc/apt/apt.conf.d/docker-clean; \
    echo 'Binary::apt::APT::Keep-Downloaded-Packages "true";' > /etc/apt/apt.conf.d/keep-cache

RUN --mount=type=cache,target=/usr/local/src/neolink/target \
  --mount=type=cache,target=/usr/local/cargo/git/db \
  --mount=type=cache,target=/usr/local/cargo/registry/ \
  --mount=type=cache,target=/var/cache/apt,sharing=locked \
  --mount=type=cache,target=/var/lib/apt,sharing=locked \
  echo "TARGETPLATFORM: ${TARGETPLATFORM}"; \
  if [ -f "${TARGETPLATFORM}/neolink" ]; then \
    echo "Restoring from artifact"; \
    cp "${TARGETPLATFORM}/neolink" /usr/local/bin/neolink; \
  else \
    echo "Building from scratch"; \
    apt-get update && \
        apt-get upgrade -y && \
        apt-get install -y --no-install-recommends \
          build-essential \
          openssl \
          libssl-dev \
          ca-certificates \
          libgstrtspserver-1.0-dev \
          libgstreamer1.0-dev \
          libgtk2.0-dev \
          protobuf-compiler \
          libglib2.0-dev && \
    cargo build --release && \
    cp target/release/neolink /usr/local/bin/neolink; \
  fi

# Create the release container. Match the base OS used to build
FROM debian:stable-slim
ARG REPO
ARG VERSION
ARG OWNER

LABEL org.opencontainers.image.source="$REPO" \
      org.opencontainers.image.description="Reolink camera to RTSP translator" \
      org.opencontainers.image.version="$VERSION" \
      org.opencontainers.image.vendor="$OWNER"

RUN rm -f /etc/apt/apt.conf.d/docker-clean; \
    echo 'Binary::apt::APT::Keep-Downloaded-Packages "true";' > /etc/apt/apt.conf.d/keep-cache

RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update && \
    apt-get upgrade -y && \
    apt-get install -y --no-install-recommends \
        openssl \
        dnsutils \
        iputils-ping \
        ca-certificates \
        libgstrtspserver-1.0-0 \
        libgstreamer1.0-0 \
        gstreamer1.0-tools \
        gstreamer1.0-x \
        gstreamer1.0-plugins-base \
        gstreamer1.0-plugins-good \
        gstreamer1.0-plugins-bad \
        gstreamer1.0-libav

COPY --from=build /usr/local/bin/neolink /usr/local/bin/neolink
COPY docker/entrypoint.sh /entrypoint.sh

RUN gst-inspect-1.0; \
    chmod +x "/usr/local/bin/neolink" && \
    "/usr/local/bin/neolink" --version

RUN groupadd -r -g 10001 neolink && useradd -r -u 10001 -g neolink -d /home/neolink -m neolink && \
    mkdir -p /home/neolink/.config && chown neolink:neolink /home/neolink/.config

USER neolink

ENV NEO_LINK_MODE="rtsp" NEO_LINK_PORT=8554

ENTRYPOINT ["/entrypoint.sh"]
CMD ["/usr/local/bin/neolink", "rtsp", "--config", "/etc/neolink.toml"]
EXPOSE 8554

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
  CMD /usr/local/bin/neolink --version || exit 1
