# ARC-Bench official-format scorer: the official ARC-Bench runner image,
# pinned by digest, plus a thin bridge (official.py) that calls the
# runner's own functions and an egress proxy for the npm install step.
# Nothing in the runner image is changed: /opt/arcbench (run_submission.py,
# the preinstalled @playwright/test 1.57.0, /ms-playwright browsers,
# /root/.npmrc) is used as shipped. linux/amd64 only (the image is).
FROM gyataro/arcbench-runner@sha256:40e003ed470dbd4c120b9019876ba77303d38dc8b34be7f6e313fe0563dd14de

COPY official.py egress_proxy.py /opt/crucible/
RUN chmod 0755 /opt/crucible && chmod 0644 /opt/crucible/*.py
