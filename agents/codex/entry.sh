#!/usr/bin/env bash
# Container entry, run once per stage. Codex does not know about stages: on
# stage 2+ /work already holds the previous stage's code and /req holds this
# stage's requirements only; the prompt tells codex to extend what is there.
# The model provider points at the meter (OPENAI_BASE_URL, dummy key, chat
# wire, streaming); the real key never enters the container.
set -uo pipefail

: "${OPENAI_BASE_URL:?}" "${MODEL:?}"
export CODEX_HOME="$HOME/.codex"
mkdir -p "$CODEX_HOME"
# Rewritten every stage: MODEL and the meter address may differ per run.
cat > "$CODEX_HOME/config.toml" <<EOF
model = "$MODEL"
model_provider = "meter"
approval_policy = "never"
sandbox_mode = "danger-full-access"
check_for_update_on_startup = false

[model_providers.meter]
name = "crucible meter"
base_url = "$OPENAI_BASE_URL"
env_key = "OPENAI_API_KEY"
wire_api = "chat"
request_max_retries = 6
stream_max_retries = 6
stream_idle_timeout_ms = 600000
EOF
git config --global --add safe.directory /work 2>/dev/null || true
codex --version

# Leave the platform's SIGTERM/SIGKILL window unused: stop codex a little
# before DEADLINE_S (counted from container start) so it exits on its own.
deadline=${DEADLINE_S:-3600}
limit=$(( deadline - 45 )); [ "$limit" -lt 60 ] && limit=$deadline
start=$(date +%s)
# The container is the sandbox (non-root, no caps, network only to the meter
# and package registries), so codex's own landlock sandbox is off.
timeout --signal=TERM --kill-after=20s "$limit" \
  codex exec --skip-git-repo-check -s danger-full-access -C /work \
  "$(cat /opt/agent/prompt.txt)" </dev/null
rc=$?
echo "codex exit: $rc, elapsed: $(( $(date +%s) - start ))s"
find /work -path /work/node_modules -prune -o -path '*/node_modules' -prune -o -path /work/.git -prune -o -type f -print 2>/dev/null | head -80
exit "$rc"
