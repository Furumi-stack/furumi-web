FROM rust:1-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY Cargo.toml Cargo.lock* ./
COPY build.rs ./build.rs
COPY prompts ./prompts
COPY src ./src
COPY templates ./templates

RUN cargo build --release

FROM denoland/deno:bin-2.8.3 AS deno

FROM debian:bookworm-slim

ARG YT_DLP_VERSION=2026.07.04

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        ffmpeg \
        python3 \
        python3-pip \
    && pip3 install --break-system-packages --no-cache-dir --disable-pip-version-check \
        "yt-dlp[default]==${YT_DLP_VERSION}" \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /data
COPY --from=builder /app/target/release/furumusic /usr/local/bin/furumusic
COPY --from=deno /deno /usr/local/bin/deno

EXPOSE 8000
CMD ["furumusic", "-l", "0.0.0.0:8000"]
