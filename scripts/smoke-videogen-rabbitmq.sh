#!/usr/bin/env bash
set -euo pipefail

: "${VIDEOGEN_RABBITMQ_AMQPS_URLS:?VIDEOGEN_RABBITMQ_AMQPS_URLS is required}"

cargo test --package storj-interface --test videogen_rabbitmq_smoke -- --ignored
