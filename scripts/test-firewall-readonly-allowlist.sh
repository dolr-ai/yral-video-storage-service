#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "${TMP_DIR}"' EXIT

fail() {
  echo "$1" >&2
  exit 1
}

FAKE_BIN="${TMP_DIR}/bin"
IPTABLES_LOG="${TMP_DIR}/iptables.log"
mkdir -p "${FAKE_BIN}"

cat > "${FAKE_BIN}/ip" <<'IP'
#!/usr/bin/env bash
set -euo pipefail
if [[ "$*" == "-4 route show default" ]]; then
  echo "default via 192.0.2.1 dev eth0 proto static"
  exit 0
fi
exit 1
IP
chmod +x "${FAKE_BIN}/ip"

cat > "${FAKE_BIN}/iptables" <<'IPTABLES'
#!/usr/bin/env bash
set -euo pipefail
printf 'iptables %s\n' "$*" >> "${IPTABLES_LOG}"
# Emulate the DOCKER-USER listing so the readonly-on-5432 block can locate the
# post-DNAT dpt:5432 DROP line (Docker DNATs published 15432 -> container 5432).
case "$*" in
  *"-L DOCKER-USER"*)
    echo "Chain DOCKER-USER (1 references)"
    echo "num  target     prot opt source               destination"
    echo "1    ACCEPT     tcp  --  0.0.0.0/0            0.0.0.0/0            tcp dpt:5432"
    echo "2    DROP       tcp  --  0.0.0.0/0            0.0.0.0/0            tcp dpt:5432"
    ;;
esac
exit 0
IPTABLES
chmod +x "${FAKE_BIN}/iptables"

cat > "${FAKE_BIN}/sleep" <<'SLEEP'
#!/usr/bin/env bash
set -euo pipefail
exit 0
SLEEP
chmod +x "${FAKE_BIN}/sleep"

PATH="${FAKE_BIN}:${PATH}" \
IPTABLES_LOG="${IPTABLES_LOG}" \
SERVER_1_IP=94.130.13.115 \
SERVER_2_IP=88.99.151.102 \
SERVER_3_IP=138.201.129.173 \
POSTGRES_READONLY_ALLOWLIST="88.99.192.144/32 88.99.61.221/32 138.201.196.246/32" \
  sh "${REPO_ROOT}/deploy/scripts/server/apply-firewall.sh" > "${TMP_DIR}/stdout.log"

for ip in 88.99.192.144 88.99.61.221 138.201.196.246; do
  grep -q -- "iptables -A YRAL_POSTGRES_READONLY -s ${ip}/32 -j ACCEPT" "${IPTABLES_LOG}" \
    || fail "expected readonly allowlist chain rule for ${ip}/32"

  if grep -q -- "iptables -A DOCKER-USER -p tcp -i eth0 --dport 18008 -j YRAL_POSTGRES_READONLY" "${IPTABLES_LOG}"; then
    fail "readonly allowlist must not open Patroni API port 18008 for ${ip}/32"
  fi

  if grep -q -- "iptables -A DOCKER-USER -p tcp -i eth0 --dport 12379 -j YRAL_POSTGRES_READONLY" "${IPTABLES_LOG}"; then
    fail "readonly allowlist must not open etcd client port 12379 for ${ip}/32"
  fi

  if grep -q -- "iptables -A DOCKER-USER -p tcp -i eth0 --dport 12380 -j YRAL_POSTGRES_READONLY" "${IPTABLES_LOG}"; then
    fail "readonly allowlist must not open etcd peer port 12380 for ${ip}/32"
  fi
done

grep -q -- "iptables -F YRAL_POSTGRES_READONLY" "${IPTABLES_LOG}" \
  || fail "expected readonly allowlist chain to be flushed before adding current rules"

grep -q -- "iptables -A DOCKER-USER -p tcp -i eth0 --dport 15432 -j YRAL_POSTGRES_READONLY" "${IPTABLES_LOG}" \
  || fail "expected port 15432 to jump into readonly allowlist chain"

drop_line="$(grep -n -- 'iptables -A DOCKER-USER -p tcp -i eth0 --dport 15432 -j DROP' "${IPTABLES_LOG}" | head -n1 | cut -d: -f1)"
jump_line="$(grep -n -- 'iptables -A DOCKER-USER -p tcp -i eth0 --dport 15432 -j YRAL_POSTGRES_READONLY' "${IPTABLES_LOG}" | head -n1 | cut -d: -f1)"

[[ -n "${drop_line}" ]] || fail "expected final drop rule for port 15432"
[[ -n "${jump_line}" ]] || fail "expected readonly allowlist jump before final drop"
[[ "${jump_line}" -lt "${drop_line}" ]] \
  || fail "readonly allowlist jump must be inserted before final drop on port 15432"

grep -q 'POSTGRES_READONLY_ALLOWLIST: "${POSTGRES_READONLY_ALLOWLIST:-}"' "${REPO_ROOT}/deploy/docker-compose.ha.yml" \
  || fail "expected docker-compose.ha.yml to pass POSTGRES_READONLY_ALLOWLIST into firewall service"

# The published Postgres port 15432 is DNAT'd to container port 5432 BEFORE DOCKER-USER,
# so the effective readonly jump must be on dpt:5432 (the mock reports its DROP at line 2),
# inserted before that DROP.
grep -q -- "iptables -I DOCKER-USER 2 -p tcp --dport 5432 -j YRAL_POSTGRES_READONLY" "${IPTABLES_LOG}" \
  || fail "expected readonly allowlist jump on post-DNAT Postgres port 5432 (before its DROP)"

echo "firewall readonly allowlist test ok"
