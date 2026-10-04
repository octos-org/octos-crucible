#!/bin/bash
# Entry of a simulated Nomad node (sim-node.Dockerfile): clients start their
# own Docker daemon first; then the Nomad agent runs as the evaluation user
# with the config mounted at /etc/nomad.d.
set -euo pipefail
if [ "${SIM_DOCKER:-0}" = 1 ]; then
  # cgroup v2: move ourselves out of the container's root cgroup so Docker
  # can delegate controllers to its containers (as docker:dind does).
  if [ -f /sys/fs/cgroup/cgroup.controllers ]; then
    mkdir -p /sys/fs/cgroup/init
    xargs -rn1 < /sys/fs/cgroup/cgroup.procs > /sys/fs/cgroup/init/cgroup.procs || true
    sed -e 's/ / +/g' -e 's/^/+/' < /sys/fs/cgroup/cgroup.controllers \
      > /sys/fs/cgroup/cgroup.subtree_control
  fi
  dockerd --host unix:///var/run/docker.sock >/var/log/dockerd.log 2>&1 &
  for _ in $(seq 60); do docker info >/dev/null 2>&1 && break; sleep 1; done
  docker info >/dev/null 2>&1 || { cat /var/log/dockerd.log >&2; exit 1; }
fi
exec sudo -u "$SIM_USER" -H -- nomad agent -config /etc/nomad.d
