FROM rust:bookworm AS builder

RUN apt-get update && apt-get install -y \
    libavformat-dev libavcodec-dev libavutil-dev libswscale-dev \
    libavfilter-dev libavdevice-dev libswresample-dev \
    clang pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ffmpeg ca-certificates curl unzip \
    && rm -rf /var/lib/apt/lists/*

RUN curl -L https://github.com/storj/storj/releases/latest/download/uplink_linux_amd64.zip \
    -o /tmp/uplink.zip \
    && unzip /tmp/uplink.zip uplink -d /tmp \
    && install /tmp/uplink /usr/local/bin/uplink \
    && rm -rf /tmp/uplink /tmp/uplink.zip

WORKDIR /app
COPY --from=builder /app/target/release/storj-interface .

EXPOSE 3000
ENTRYPOINT ["/app/storj-interface"]
