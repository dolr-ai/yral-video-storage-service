# Stage 1: builder — needs ffmpeg dev headers to compile ffmpeg-next
FROM rust:bookworm AS builder

RUN apt-get update && apt-get install -y \
    libavformat-dev libavcodec-dev libavutil-dev libswscale-dev \
    clang pkg-config curl unzip \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .
RUN cargo build --release --target x86_64-unknown-linux-gnu

# Stage 2: runtime — only ffmpeg runtime libs + uplink CLI
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ffmpeg ca-certificates curl unzip \
    && rm -rf /var/lib/apt/lists/*

# Pin uplink version for reproducible builds
RUN curl -L https://github.com/storj/storj/releases/download/v1.112.4/uplink_linux_amd64.zip \
    -o /tmp/uplink.zip \
    && unzip /tmp/uplink.zip -d /tmp \
    && install /tmp/uplink /usr/local/bin/uplink \
    && rm -rf /tmp/uplink /tmp/uplink.zip

WORKDIR /app
COPY --from=builder /app/target/x86_64-unknown-linux-gnu/release/storj-interface .

EXPOSE 3000
ENTRYPOINT ["/app/storj-interface"]
