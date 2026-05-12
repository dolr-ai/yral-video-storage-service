#!/bin/sh
set -e

mkdir -p /run
install -m 600 -o postgres /dev/null /run/patroni.yml
envsubst < /etc/patroni.yml.template > /run/patroni.yml

mkdir -p /var/lib/postgresql/data
chown postgres:postgres /var/lib/postgresql/data
chmod 700 /var/lib/postgresql/data

# pgpass must be owned by postgres with mode 600 (Postgres requirement)
touch /var/lib/postgresql/.pgpass
chown postgres:postgres /var/lib/postgresql/.pgpass
chmod 600 /var/lib/postgresql/.pgpass

exec su-exec postgres patroni /run/patroni.yml
