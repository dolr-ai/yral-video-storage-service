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
COPY storj-interface .

EXPOSE 3000
ENTRYPOINT ["/app/storj-interface"]
