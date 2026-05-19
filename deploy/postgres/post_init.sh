#!/bin/sh
# Called by Patroni after initial cluster bootstrap with the superuser connection URL as $1.
# Runs only once on first-time cluster initialization.
set -e
psql "$1" -c "CREATE DATABASE video_fingerprint_index OWNER storj;"
