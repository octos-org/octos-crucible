#!/usr/bin/env bash
# Network sandbox for agent containers on a GitHub-hosted Linux runner.
#
#   sandbox-net.sh up | down | check
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
# Needs sudo (passwordless on GitHub-hosted runners). `down` ignores rules
# and networks that are already gone.
set -euo pipefail

NET=crucible-sbx
BR=crucible0
SUBNET=172.31.250.0/24
GW=172.31.250.1
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
  up)
    docker network create --driver bridge --subnet "$SUBNET" --gateway "$GW" \
      -o com.docker.network.bridge.name="$BR" \
      -o com.docker.network.bridge.enable_ip_masquerade=false \
      -o com.docker.network.bridge.enable_icc=false \
      "$NET" >/dev/null
    rules -I
    echo "sandbox network $NET up ($SUBNET, host $GW, ports $PORTS)"
    ;;
  down)
    rules -D 2>/dev/null || true
    docker network rm "$NET" >/dev/null 2>&1 || true
    echo "sandbox network $NET down"
    ;;
  check)
    docker pull -q "$PROBE" >/dev/null
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
    echo "usage: sandbox-net.sh up|down|check" >&2
    exit 2
    ;;
esac
