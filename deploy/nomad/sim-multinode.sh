#!/usr/bin/env bash
# A multi-node Nomad cluster for crucible on ONE machine (docs/nomad.md
# "多节点（含单机模拟）"): three containers on a Docker network.
#
#   crucible-nomad-server   Nomad server (API on 127.0.0.1:$PORT)
#   crucible-nomad-trusted  client, node pool crucible-trusted (handoff, publish)
#   crucible-nomad-sandbox  client, node pool crucible-sandbox (generate, score-tests)
#
# Each client is a node as on real hardware: its own Docker daemon (Docker in
# Docker, privileged container) with its own images, bridges and iptables in
# its own network namespace; the evaluation user (same name and uid as here)
# with passwordless `sudo iptables`; Nomad running as that user (raw_exec,
# docker). The shared disk is $SHARED, bind-mounted at the same path into
# both clients (on real nodes: an NFS/CephFS mount at the same path): put
# the repository, the `crucible` binary, --out and --store under it.
#
#   deploy/nomad/sim-multinode.sh up
#   deploy/nomad/sim-multinode.sh load IMAGE...   # copy local images into the sandbox node
#   NOMAD_ADDR=http://127.0.0.1:14646 crucible eval nomad ... \
#     --trusted-pool crucible-trusted --sandbox-pool crucible-sandbox
#   deploy/nomad/sim-multinode.sh down            # containers, network, node Docker volumes
#
# Needs Docker and the nomad binary (mounted into the containers).
# CRUCIBLE_SIM_PROXY=http://host:port: apt in the image build and the nodes'
# Docker daemons go through that proxy (the base image comes from this
# machine's Docker; CRUCIBLE_SIM_BASE names a local copy of it).
set -euo pipefail

PORT=${CRUCIBLE_SIM_NOMAD_PORT:-14646}
SHARED=${CRUCIBLE_SIM_SHARED:-$HOME/crucible-sim-shared}
NET=crucible-nomad-sim
IMG=crucible-nomad-sim-node:1
NOMAD=$(command -v nomad || true)
HERE=$(cd "$(dirname "$0")" && pwd)

conf() { # $1 = server | pool name
  if [ "$1" = server ]; then
    cat <<'EOF'
data_dir  = "/var/lib/crucible-nomad/data"
bind_addr = "0.0.0.0"
server {
  enabled          = true
  bootstrap_expect = 1
}
EOF
  else
    cat <<EOF
data_dir  = "/var/lib/crucible-nomad/data"
bind_addr = "0.0.0.0"
client {
  enabled   = true
  servers   = ["crucible-nomad-server:4647"]
  node_pool = "$1"
}
plugin "raw_exec" {
  config { enabled = true }
}
plugin "docker" {
  config { allow_privileged = false }
}
EOF
  fi
}

up() {
  [ -n "$NOMAD" ] || { echo "nomad not found" >&2; exit 1; }
  mkdir -p "$SHARED"
  local build=(-t "$IMG" --build-arg USER_NAME="$(id -un)" --build-arg USER_ID="$(id -u)"
    --build-arg BASE="${CRUCIBLE_SIM_BASE:-ubuntu:resolute}" -f "$HERE/sim-node.Dockerfile")
  local penv=()
  if [ -n "${CRUCIBLE_SIM_PROXY:-}" ]; then
    local p=$CRUCIBLE_SIM_PROXY
    build+=(--build-arg "http_proxy=$p" --build-arg "https_proxy=$p")
    penv=(-e "HTTP_PROXY=$p" -e "HTTPS_PROXY=$p" -e "NO_PROXY=localhost,127.0.0.1")
  fi
  docker build "${build[@]}" "$HERE"
  docker network inspect "$NET" >/dev/null 2>&1 || docker network create "$NET"
  local cfg=$SHARED/.sim-nomad
  for n in server trusted sandbox; do
    mkdir -p "$cfg/$n"
    case $n in
      server) conf server ;;
      *) conf "crucible-$n" ;;
    esac > "$cfg/$n/node.hcl"
    chmod -R a+rX "$cfg"
    local args=(-d --name "crucible-nomad-$n" --hostname "crucible-nomad-$n" --network "$NET"
      -v "$NOMAD:/usr/local/bin/nomad:ro" -v "$cfg/$n:/etc/nomad.d:ro")
    if [ "$n" = server ]; then
      args+=(-p "127.0.0.1:$PORT:4646")
    else
      # Its own Docker: privileged, its own /var/lib/docker.
      args+=(--privileged -e SIM_DOCKER=1 "${penv[@]}" -v "crucible-nomad-$n-docker:/var/lib/docker"
        -v "$SHARED:$SHARED")
    fi
    docker run "${args[@]}" "$IMG"
  done
  export NOMAD_ADDR=http://127.0.0.1:$PORT
  for _ in $(seq 90); do
    [ "$(nomad node status -json 2>/dev/null | jq '[.[] | select(.Status=="ready")] | length')" = 2 ] && break
    sleep 2
  done
  nomad node status -verbose
  for p in crucible-trusted crucible-sandbox; do
    nomad node pool nodes "$p" | tail -n +2 | grep -q ready || { echo "no ready node in $p" >&2; exit 1; }
  done
  echo "ready: export NOMAD_ADDR=$NOMAD_ADDR; shared disk $SHARED"
}

load() {
  for i in "$@"; do
    docker save "$i" | docker exec -i crucible-nomad-sandbox docker load
  done
}

down() {
  for n in server trusted sandbox; do docker rm -f "crucible-nomad-$n" 2>/dev/null || true; done
  docker volume rm crucible-nomad-trusted-docker crucible-nomad-sandbox-docker 2>/dev/null || true
  docker network rm "$NET" 2>/dev/null || true
  docker image rm "$IMG" 2>/dev/null || true
  rm -rf "$SHARED/.sim-nomad"
}

case "${1:-}" in
  up) up ;;
  load) shift; load "$@" ;;
  down) down ;;
  *) echo "usage: $0 up|load IMAGE...|down" >&2; exit 2 ;;
esac
