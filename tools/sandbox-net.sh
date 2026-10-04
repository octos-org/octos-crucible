#!/usr/bin/env bash
# Network sandbox for agent containers on a Linux runner (GitHub-hosted or
# self-hosted, docs/self-hosted.md).
#
#   sandbox-net.sh preflight | up | down | check | clean
#
# preflight: the machine has what the sandbox jobs need (docker, jq, git,
# curl, timeout; passwordless `sudo iptables`; Docker's DOCKER-USER chain,
# i.e. Docker on its iptables firewall backend). Exits non-zero with the
# reason otherwise. `up` runs it first.
#
# up: creates the docker bridge network `crucible-sbx` (172.31.250.0/24,
# host side 172.31.250.1 on interface crucible0) with NAT and inter-container
# traffic off, then with iptables:
#   - DOCKER-USER: drop everything forwarded into or out of crucible0, so a
#     container on it has no route to the internet or to other containers;
#   - INPUT: from crucible0 accept only TCP to 172.31.250.1 ports 8787
#     (meter) and 3128 (egress proxy; not in the scoring job, see
#     SANDBOX_PORTS); drop the rest (runner services, the
#     docker daemon's other ports, ...). IPv6 from the bridge is dropped.
# `crucible run` binds both proxies to 172.31.250.1 only and starts the
# container with --dns 127.0.0.1: it never resolves names itself (the meter
# is addressed by IP, the egress proxy resolves CONNECT targets).
#
# check: from a throwaway container on the network, name resolution
# (github.com) and direct connections (example.com by IP, 1.1.1.1:443)
# must fail. Exits non-zero if any of them succeeds.
#
# clean (manual fallback only): removes every crucible container, network,
# volume and iptables rule on the machine, whoever's they are, so never run
# it while evaluations may be running. Steps clean up their own sandbox and
# containers by their run label (`crucible`, scoped), workflows do not call
# this.
#
# Several sandboxes can live on one machine (concurrent evaluations):
# `crucible` picks a free slot and passes its names in SANDBOX_NET,
# SANDBOX_BRIDGE, SANDBOX_SUBNET and SANDBOX_GW (defaults: slot 0 above),
# and the run label in SANDBOX_LABEL (on the network, `crucible.run`).
# `up` exits 3 when the network cannot be created (the slot is taken).
#
# Needs passwordless sudo for iptables. `down` and `clean` ignore what is
# already gone.
set -euo pipefail

NET=${SANDBOX_NET:-crucible-sbx}
BR=${SANDBOX_BRIDGE:-crucible0}
SUBNET=${SANDBOX_SUBNET:-172.31.250.0/24}
GW=${SANDBOX_GW:-172.31.250.1}
# The scoring job opens only the meter port (SANDBOX_PORTS=8787).
PORTS=${SANDBOX_PORTS:-8787,3128}
# busybox 1.37.0, pinned by digest.
PROBE=busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e

rules() {
  # $1 = -I (insert) or -D (delete). Order matters for -I: the ACCEPT is
  # inserted last so it ends up above the INPUT DROP.
  local op=$1
  sudo iptables "$op" DOCKER-USER -i "$BR" -j DROP
  sudo iptables "$op" DOCKER-USER -o "$BR" -j DROP
  sudo iptables "$op" INPUT -i "$BR" -j DROP
  sudo iptables "$op" INPUT -i "$BR" -p tcp -d "$GW" -m multiport --dports "$PORTS" -j ACCEPT
  sudo ip6tables "$op" INPUT -i "$BR" -j DROP
}

preflight() {
  local miss=() t
  for t in docker jq git curl timeout iptables ip6tables; do
    command -v "$t" >/dev/null 2>&1 || miss+=("$t")
  done
  [ ${#miss[@]} -eq 0 ] || { echo "::error::sandbox preflight: missing ${miss[*]}" >&2; return 1; }
  docker info >/dev/null 2>&1 \
    || { echo "::error::sandbox preflight: cannot reach the docker daemon (is $(id -un) in the docker group?)" >&2; return 1; }
  if ! sudo -n iptables -S INPUT >/dev/null 2>&1; then
    echo "::error::sandbox preflight: needs passwordless sudo for iptables and ip6tables (docs/self-hosted.md)" >&2; return 1
  fi
  sudo -n iptables -S DOCKER-USER >/dev/null 2>&1 \
    || { echo "::error::sandbox preflight: no DOCKER-USER chain in $(iptables --version 2>/dev/null). Docker must use its iptables firewall backend (\"firewall-backend\": \"iptables\" in /etc/docker/daemon.json), and iptables must be the same variant (nft or legacy) as Docker's" >&2; return 1; }
}

# Deletes every rule of ours (chains INPUT, DOCKER-USER; never Docker's
# own) that names one of our bridges.
drop_rules() {
  local cmd=$1 line
  sudo -n "$cmd" -S 2>/dev/null | grep -E -- "^-A (INPUT|DOCKER-USER) .*-[io] (crucible[0-9]+|crs[0-9]+|crb[0-9]+) " | while IFS= read -r line; do
    # shellcheck disable=SC2086 # the rule text is split into iptables args
    sudo -n "$cmd" -D ${line#-A } 2>/dev/null || true
  done || true
}

clean() {
  { docker ps -aq --filter name=crucible-; docker ps -aq --filter label=crucible.scorer.run; } \
    | sort -u | xargs -r docker rm -f >/dev/null 2>&1 || true
  { docker network ls -q --filter name=crucible-net-; docker network ls -q --filter name=crucible-bnet-; } \
    | sort -u | xargs -r docker network rm >/dev/null 2>&1 || true
  docker network ls -q --filter name=crucible-sbx | xargs -r docker network rm >/dev/null 2>&1 || true
  docker volume ls -q --filter name=crucible- | xargs -r docker volume rm -f >/dev/null 2>&1 || true
  drop_rules iptables
  drop_rules ip6tables
}

probe() {
  # Prints ok/blocked for one command run inside the sandbox.
  if docker run --rm --network "$NET" --dns 127.0.0.1 --cap-drop ALL \
      --security-opt no-new-privileges --user 65534:65534 "$PROBE" \
      sh -c "$1" >/dev/null 2>&1; then
    echo ok
  else
    echo blocked
  fi
}

case "${1:-}" in
  preflight)
    preflight
    echo "sandbox preflight ok"
    ;;
  up)
    preflight
    docker network create --driver bridge --subnet "$SUBNET" --gateway "$GW" \
      --label "crucible.run=${SANDBOX_LABEL:-}" \
      -o com.docker.network.bridge.name="$BR" \
      -o com.docker.network.bridge.enable_ip_masquerade=false \
      -o com.docker.network.bridge.enable_icc=false \
      "$NET" >/dev/null || exit 3
    rules -I
    echo "sandbox network $NET up ($SUBNET, host $GW, ports $PORTS)"
    ;;
  down)
    rules -D 2>/dev/null || true
    docker network rm "$NET" >/dev/null 2>&1 || true
    echo "sandbox network $NET down"
    ;;
  clean)
    clean
    echo "sandbox clean: no crucible containers, networks, volumes or rules left"
    ;;
  check)
    # Pulled once per machine (several sandboxes may check at once).
    if ! docker image inspect "$PROBE" >/dev/null 2>&1; then
      for attempt in 1 2 3; do
        docker pull -q "$PROBE" >/dev/null && break
        [ "$attempt" = 3 ] && exit 1
        sleep $((attempt * 5))
      done
    fi
    fail=0
    for name in dns_github http_example_com tcp_1_1_1_1_443 tcp_host_22; do
      case $name in
        dns_github)       cmd='nslookup github.com' ;;
        http_example_com) cmd='wget -q -T 5 -O /dev/null http://example.com/' ;;
        tcp_1_1_1_1_443)  cmd='nc -w 5 1.1.1.1 443 </dev/null' ;;
        tcp_host_22)      cmd="nc -w 5 $GW 22 </dev/null" ;;
      esac
      r=$(probe "$cmd")
      echo "sandbox check $name: $r"
      [ "$r" = blocked ] || fail=1
    done
    exit "$fail"
    ;;
  *)
    echo "usage: sandbox-net.sh preflight|up|down|check|clean" >&2
    exit 2
    ;;
esac
