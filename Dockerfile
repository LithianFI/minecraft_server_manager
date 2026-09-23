# syntax=docker/dockerfile:1

# ── Build ──────────────────────────────────────────────────────────────────────
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release && cp target/release/minecraft_server_manager /msm

# ── Java runtimes ──────────────────────────────────────────────────────────────
# Modpacks need different Java versions: 8 (≤1.16), 17 (1.17–1.20.4), 21 (1.20.5–1.21.x), 25 (26.x)
FROM eclipse-temurin:8-jre  AS java8
FROM eclipse-temurin:17-jre AS java17
FROM eclipse-temurin:21-jre AS java21
FROM eclipse-temurin:25-jre AS java25

# ── Runtime ────────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends bash ca-certificates tini \
    && rm -rf /var/lib/apt/lists/*

# MSM discovers Java installs in /usr/lib/jvm by directory name (temurin-<version>)
COPY --from=java8  /opt/java/openjdk /usr/lib/jvm/temurin-8
COPY --from=java17 /opt/java/openjdk /usr/lib/jvm/temurin-17
COPY --from=java21 /opt/java/openjdk /usr/lib/jvm/temurin-21
COPY --from=java25 /opt/java/openjdk /usr/lib/jvm/temurin-25

COPY --from=build /msm /usr/local/bin/msm

RUN useradd --uid 1000 --create-home --home-dir /home/msm msm \
    && mkdir -p /data/msm \
    && chown -R msm:msm /data

# All MSM state (config, instances, servers, backups) lives in /data/msm
ENV XDG_DATA_HOME=/data \
    PATH=/usr/lib/jvm/temurin-21/bin:$PATH

USER msm
VOLUME /data
EXPOSE 8080 25565

# tini forwards SIGTERM to MSM, which stops running servers cleanly before exiting
ENTRYPOINT ["tini", "--", "msm"]
