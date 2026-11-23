FROM python:3.13-slim-bookworm AS lyrics-model
COPY scripts/fetch-lyrics-model.py /fetch-lyrics-model.py
RUN python /fetch-lyrics-model.py /model

FROM rust:1.97.0-bookworm AS chef

RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*

RUN cargo install cargo-chef --version 0.1.74 --locked

WORKDIR /app

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY build.rs ./build.rs
COPY src ./src
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json ./recipe.json
# Keep compiled dependencies in an ordinary layer exported by CI's registry cache.
RUN cargo chef cook --release --locked --recipe-path recipe.json

COPY Cargo.toml Cargo.lock ./
COPY build.rs ./build.rs
COPY prompts ./prompts
COPY src ./src
COPY templates ./templates

RUN cargo build --release --locked --bin furumusic

FROM denoland/deno:bin-2.8.3 AS deno

FROM debian:bookworm-slim

ARG YT_DLP_VERSION=2026.07.04

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        ffmpeg \
        libchromaprint-tools \
        python3 \
        python3-pip \
    && pip3 install --break-system-packages --no-cache-dir --disable-pip-version-check \
        "yt-dlp[default]==${YT_DLP_VERSION}" \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /data
COPY --from=builder /app/target/release/furumusic /usr/local/bin/furumusic
COPY --from=deno /deno /usr/local/bin/deno
COPY --from=lyrics-model /model /opt/furumusic/models/multilingual-e5-small

EXPOSE 8000
CMD ["furumusic", "-l", "0.0.0.0:8000"]
