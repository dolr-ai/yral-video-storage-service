#!/bin/sh
# Blocks external access to cluster-internal ports.
# Runs as a privileged Docker sidecar so rules survive container restarts
# and are re-applied automatically after host reboots (via restart: always).
#
# Rules are scoped to the external interface (-i $EXT_IF) so that outbound
# traffic FROM Docker containers to other cluster nodes on these ports is not
# accidentally dropped (container traffic arrives in FORWARD from the bridge
# interface, not from the external interface).
set -e

EXT_IF=$(ip -4 route show default | awk '{print $5; exit}')
READONLY_CHAIN="YRAL_POSTGRES_READONLY"
POSTGRES_READONLY_ALLOWLIST="${POSTGRES_READONLY_ALLOWLIST:-}"

iptables -N "$READONLY_CHAIN" 2>/dev/null || true
iptables -F "$READONLY_CHAIN"

for CIDR in $POSTGRES_READONLY_ALLOWLIST; do
  case "$CIDR" in
    */*) ;;
    *) CIDR="${CIDR}/32" ;;
  esac

  iptables -A "$READONLY_CHAIN" -s "$CIDR" -j ACCEPT
done

for PORT in 15432 18008 12379 12380; do
  # Remove stale rules to avoid duplicates on restart
  iptables -D DOCKER-USER -p tcp --dport "$PORT" -j DROP 2>/dev/null || true
  iptables -D DOCKER-USER -p tcp --dport "$PORT" -s "$SERVER_1_IP" -j ACCEPT 2>/dev/null || true
  iptables -D DOCKER-USER -p tcp --dport "$PORT" -s "$SERVER_2_IP" -j ACCEPT 2>/dev/null || true
  iptables -D DOCKER-USER -p tcp --dport "$PORT" -s "$SERVER_3_IP" -j ACCEPT 2>/dev/null || true
  iptables -D DOCKER-USER -p tcp -i "$EXT_IF" --dport "$PORT" -j DROP 2>/dev/null || true
  iptables -D DOCKER-USER -p tcp -i "$EXT_IF" --dport "$PORT" -s "$SERVER_1_IP" -j ACCEPT 2>/dev/null || true
  iptables -D DOCKER-USER -p tcp -i "$EXT_IF" --dport "$PORT" -s "$SERVER_2_IP" -j ACCEPT 2>/dev/null || true
  iptables -D DOCKER-USER -p tcp -i "$EXT_IF" --dport "$PORT" -s "$SERVER_3_IP" -j ACCEPT 2>/dev/null || true
  iptables -D DOCKER-USER -p tcp -i "$EXT_IF" --dport "$PORT" -j "$READONLY_CHAIN" 2>/dev/null || true

  # Allow only cluster nodes, plus explicit readonly clients for Postgres.
  iptables -A DOCKER-USER -p tcp -i "$EXT_IF" --dport "$PORT" -s "$SERVER_1_IP" -j ACCEPT
  iptables -A DOCKER-USER -p tcp -i "$EXT_IF" --dport "$PORT" -s "$SERVER_2_IP" -j ACCEPT
  iptables -A DOCKER-USER -p tcp -i "$EXT_IF" --dport "$PORT" -s "$SERVER_3_IP" -j ACCEPT
  if [ "$PORT" = "15432" ] && [ -n "$POSTGRES_READONLY_ALLOWLIST" ]; then
    iptables -A DOCKER-USER -p tcp -i "$EXT_IF" --dport "$PORT" -j "$READONLY_CHAIN"
  fi
  iptables -A DOCKER-USER -p tcp -i "$EXT_IF" --dport "$PORT" -j DROP
done

# Postgres readonly allowlist — applied on the CONTAINER port (5432), not the host
# port (15432). Docker DNATs the published port 15432 -> 5432 in nat/PREROUTING, which
# runs BEFORE the filter DOCKER-USER chain, so DOCKER-USER sees dpt:5432. The readonly
# jump added on dpt:15432 in the loop above therefore never matches external traffic.
# Apply it on dpt:5432, inserted immediately before that port's existing DROP rule so
# allowlisted clients are accepted before the catch-all drop. Idempotent.
if [ -n "$POSTGRES_READONLY_ALLOWLIST" ]; then
  # Remove a prior jump (at most one is added per run) so re-applies stay idempotent.
  iptables -D DOCKER-USER -p tcp --dport 5432 -j "$READONLY_CHAIN" 2>/dev/null || true
  DROP_LINE=$(iptables -L DOCKER-USER --line-numbers -n 2>/dev/null \
    | awk '$2 == "DROP" && /dpt:5432/ { print $1; exit }')
  if [ -n "$DROP_LINE" ]; then
    iptables -I DOCKER-USER "$DROP_LINE" -p tcp --dport 5432 -j "$READONLY_CHAIN"
    echo "Readonly allowlist applied on post-DNAT Postgres port 5432 (before DROP at line ${DROP_LINE})"
  else
    echo "WARN: no dpt:5432 DROP found in DOCKER-USER; readonly jump NOT applied"
  fi
fi

echo "Firewall rules applied for ports 15432 18008 12379 12380 on interface ${EXT_IF}"
exec sleep infinity
