# A Nomad node for the one-machine multi-node simulation
# (deploy/nomad/sim-multinode.sh, docs/nomad.md "多节点（含单机模拟）").
# A client is what a real node of a crucible Nomad cluster is: its own
# Docker daemon (Docker-in-Docker: own containers, images, bridges and
# iptables, in the node's own network namespace), the evaluation user with
# passwordless `sudo iptables`, and the Nomad agent running as that user.
# Same Ubuntu as the machine `crucible` is built on (glibc).
ARG BASE=ubuntu:resolute
FROM $BASE
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    docker.io docker-buildx iptables sudo jq git curl ca-certificates tar gzip unzip \
    procps iproute2 coreutils \
 && rm -rf /var/lib/apt/lists/*
ARG USER_NAME=yao
ARG USER_ID=1000
RUN (userdel -r ubuntu 2>/dev/null || true) \
 && useradd -m -u "$USER_ID" -G docker -s /bin/bash "$USER_NAME" \
 && printf '%s ALL=(root) NOPASSWD: /usr/sbin/iptables, /usr/sbin/ip6tables\n' "$USER_NAME" \
    > /etc/sudoers.d/crucible && chmod 440 /etc/sudoers.d/crucible \
 && mkdir -p /var/lib/crucible-nomad && chown "$USER_ID:$USER_ID" /var/lib/crucible-nomad
ENV SIM_USER=$USER_NAME
COPY sim-node-entry.sh /usr/local/bin/sim-node-entry.sh
ENTRYPOINT ["/usr/local/bin/sim-node-entry.sh"]
