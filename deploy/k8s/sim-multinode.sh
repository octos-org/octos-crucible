#!/usr/bin/env bash
# A multi-node Kubernetes cluster for crucible on ONE machine (docs/kubernetes.md
# "多节点（含单机模拟）"): k3d runs k3s in Docker containers, 1 server + 2 agents.
#
#   server  crucible/pool=trusted   handoff, publish, the loader; registry,
#                                   BuildKit, the NFS server, CoreDNS
#   agent-0 crucible/pool=sandbox   generate, score-tests and every Pod they
#   agent-1 crucible/pool=sandbox   start (tainted crucible/pool=sandbox:NoSchedule)
#
# Shared (ReadWriteMany) evaluation volumes: an NFS server inside the cluster
# (kernel nfsd, a privileged Pod with its export on a local-path volume on the
# server) and csi-driver-nfs, storage class `nfs-rwx`.
#
#   deploy/k8s/sim-multinode.sh up     # create (about 5 minutes the first time)
#   deploy/k8s/sim-multinode.sh down   # delete the cluster and its volumes
#   export KUBECONFIG=~/.kube/crucible-sim.yaml
#   crucible eval k8s ... --rwx --storage-class nfs-rwx --pids-limited \
#     --trusted-selector crucible/pool=trusted --sandbox-selector crucible/pool=sandbox
#
# Needs: Docker, kubectl, curl; the kernel modules nfsd and nfs (loaded here
# with sudo if missing). CRUCIBLE_SIM_PROXY=http://host:port: the nodes'
# image pulls, BuildKit's base images and the downloads here go through
# that proxy. `up` again after a failure continues where it stopped.
# Nothing outside Docker is changed except k3d itself
# (~/.local/bin/k3d) and the kubeconfig file.
set -euo pipefail

NAME=${CRUCIBLE_SIM_NAME:-crucible-sim}
K3S_IMAGE=${K3S_IMAGE:-rancher/k3s:v1.36.5-k3s1}
K3D_VERSION=${K3D_VERSION:-v5.9.0}
CSI_NFS=${CSI_NFS:-v4.13.4}
KCFG=${KUBECONFIG_OUT:-$HOME/.kube/crucible-sim.yaml}
HERE=$(cd "$(dirname "$0")" && pwd)

k3d_bin() {
  if command -v k3d >/dev/null; then command -v k3d; return; fi
  local b=$HOME/.local/bin/k3d
  if [ ! -x "$b" ]; then
    mkdir -p "$(dirname "$b")"
    curl -fsSL --retry 5 --max-time 120 -o "$b.tmp" \
      "https://github.com/k3d-io/k3d/releases/download/$K3D_VERSION/k3d-linux-amd64"
    chmod +x "$b.tmp" && mv "$b.tmp" "$b"
  fi
  echo "$b"
}

up() {
  local K3D; K3D=$(k3d_bin)
  for m in nfsd nfs nfsv4; do
    grep -qw "^$m" /proc/modules || sudo modprobe "$m"
  done
  local tmp; tmp=$(mktemp -d)
  # Every node pulls the evaluation images from the cluster registry.
  cat > "$tmp/registries.yaml" <<'EOF'
mirrors:
  "10.43.200.200:5000":
    endpoint:
      - "http://10.43.200.200:5000"
EOF
  local proxy=()
  local noproxy=10.0.0.0/8,172.16.0.0/12,192.168.0.0/16,127.0.0.1,localhost,.svc,.cluster.local
  if [ -n "${CRUCIBLE_SIM_PROXY:-}" ]; then
    for v in CONTAINERD_HTTP_PROXY CONTAINERD_HTTPS_PROXY; do
      proxy+=(-e "$v=$CRUCIBLE_SIM_PROXY@all")
    done
    proxy+=(-e "CONTAINERD_NO_PROXY=$noproxy@all")
  fi
  # Again after a failure: keeps the cluster, re-applies the rest.
  "$K3D" cluster get "$NAME" >/dev/null 2>&1 ||
  "$K3D" cluster create "$NAME" --image "$K3S_IMAGE" --servers 1 --agents 2 --no-lb \
    --registry-config "$tmp/registries.yaml" "${proxy[@]}" \
    --k3s-arg "--disable=traefik@server:0" \
    --k3s-arg "--disable=servicelb@server:0" \
    --k3s-arg "--disable=metrics-server@server:0" \
    --k3s-arg "--kubelet-arg=pod-max-pids=1024@all" \
    --kubeconfig-update-default=false --kubeconfig-switch-context=false --wait
  mkdir -p "$(dirname "$KCFG")"
  "$K3D" kubeconfig get "$NAME" > "$KCFG"
  chmod 600 "$KCFG"
  export KUBECONFIG=$KCFG
  rm -rf "$tmp"

  # Pools.
  kubectl label node "k3d-$NAME-server-0" crucible/pool=trusted --overwrite
  for i in 0 1; do
    kubectl label node "k3d-$NAME-agent-$i" crucible/pool=sandbox --overwrite
    kubectl taint node "k3d-$NAME-agent-$i" crucible/pool=sandbox:NoSchedule --overwrite
  done

  # NFS server + csi-driver-nfs + storage class nfs-rwx.
  kubectl apply -f "$HERE/sim-nfs.yaml"
  local base=https://raw.githubusercontent.com/kubernetes-csi/csi-driver-nfs/$CSI_NFS/deploy/$CSI_NFS
  for f in rbac-csi-nfs.yaml csi-nfs-driverinfo.yaml csi-nfs-controller.yaml csi-nfs-node.yaml; do
    curl -fsSL --retry 5 --max-time 120 ${CRUCIBLE_SIM_PROXY:+-x "$CRUCIBLE_SIM_PROXY"} "$base/$f" | kubectl apply -f -
  done
  # No snapshot CRDs here: drop the snapshotter sidecar.
  kubectl -n kube-system get deploy csi-nfs-controller -o json \
    | python3 -c 'import json,sys; d=json.load(sys.stdin); c=d["spec"]["template"]["spec"]["containers"]; d["spec"]["template"]["spec"]["containers"]=[x for x in c if x["name"]!="csi-snapshotter"]; print(json.dumps(d))' \
    | kubectl apply -f -
  kubectl -n crucible-system rollout status deploy/nfs-server --timeout=10m
  kubectl -n kube-system rollout status deploy/csi-nfs-controller --timeout=10m
  kubectl -n kube-system rollout status ds/csi-nfs-node --timeout=10m

  # Registry, BuildKit, the steps' ClusterRole, the step image.
  kubectl apply -f "$HERE/crucible-system.yaml"
  if [ -n "${CRUCIBLE_SIM_PROXY:-}" ]; then
    kubectl -n crucible-system set env deploy/buildkitd HTTP_PROXY="$CRUCIBLE_SIM_PROXY" \
      HTTPS_PROXY="$CRUCIBLE_SIM_PROXY" NO_PROXY="$noproxy"
  fi
  kubectl -n crucible-system rollout status deploy/registry deploy/buildkitd --timeout=10m
  kubectl -n crucible-system delete job step-image --ignore-not-found
  kubectl apply -f "$HERE/step-image.yaml"
  kubectl -n crucible-system wait --for=condition=complete job/step-image --timeout=20m
  kubectl get nodes -L crucible/pool -o wide
  echo "ready: export KUBECONFIG=$KCFG"
}

down() {
  local K3D; K3D=$(k3d_bin)
  if [ -f "$KCFG" ]; then
    # Evaluations first (their volumes unmount while the NFS server runs).
    KUBECONFIG=$KCFG kubectl delete ns -l crucible/eval --wait --timeout=5m || true
  fi
  # A hard NFS mount whose server is gone hangs the node's teardown
  # forever (unkillable container): fail whatever is left fast (kernel 6.8+).
  for n in $(docker ps --filter "label=k3d.cluster=$NAME" --format '{{.Names}}'); do
    docker exec "$n" sh -c 'for f in /sys/fs/nfs/*/shutdown; do [ -e "$f" ] && echo 1 > "$f"; done; true' || true
  done
  "$K3D" cluster delete "$NAME" || true
  rm -f "$KCFG"
}

case "${1:-}" in
  up) up ;;
  down) down ;;
  *) echo "usage: $0 up|down" >&2; exit 2 ;;
esac
