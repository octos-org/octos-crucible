---
name: octos-crucible
description: Evaluate an agent (or its output) on octos-crucible, a general platform for evaluating agents on any task set, from writing code to interactive decision-making. Package the agent, submit it with the crucible CLI, then read per-stage scores, time, tokens, cache hits and cost. Use this when asked to benchmark, score or iterate on an agent with octos-crucible.
---

# octos-crucible

octos-crucible is a general platform for evaluating agents. Any agent that runs in a sandbox can be run on any task set, stage by stage, and scored; for every stage the platform objectively records the score, wall time, model requests, tokens, cache hits and an equivalent cost. Writing code is just one kind of task. Current task sets include:

- **Coding**: ARC-Bench GitHub tasks; the agent builds a website, scored by Playwright tests.
- **Interactive decision-making**: GOSIM Agentic Observer (`astro-practice`); an observing agent answers a simulator turn by turn, scored by the official scoring engine.
- **Math / reasoning**: `math-proof-demo` (two classic IMO problems); the agent writes a proof to `answer.md`, a fixed judge model grades it against a hidden reference solution and marking scheme (7 points per problem, median of 3 judgements). The judge runs on your own model key, through the platform's meter; its usage is reported separately as `eval_usage`.

Task sets, how the agent is run, how its output is packaged and how it is scored are all pluggable (`docs/plugins.md`). You hand the platform an agent package (a Docker image recipe); it runs the agent in an isolated container, one stage after another in the same working directory, and scores each stage with hidden test material. You can also skip the agent and just score an output you already have.

- Website: https://octos-org.github.io/octos-crucible/
- API (Worker): https://crucible-worker.stratosphericus.workers.dev
- Source and docs: https://github.com/octos-org/octos-crucible (`docs/agent-contract.md`, `docs/api.md`, `docs/guide/README.md`)

This file always describes the current platform. Re-fetch it rather than relying on an old copy.

## Setup

1. Install the CLI (requires a Rust toolchain). It provides the `crucible` command:

   ```sh
   cargo install --git https://github.com/octos-org/octos-crucible crucible-cli --locked
   ```

2. Get a token. **You (the agent) cannot log in.** Ask your human user to sign in on the website with GitHub, open 我的评测 (My evals) → 命令行令牌 (CLI tokens) → 生成命令行令牌 (Generate), and give you the token, or set it themselves:

   ```sh
   export CRUCIBLE_TOKEN=crt_...
   ```

   The token acts as the user's account (without admin rights). It is shown once; the user can revoke it at the same place. Never print it, commit it or put it on a command line.

## Package an agent

A package is a directory (or zip) with `agent.json` and `Dockerfile` at its root. Full spec: `docs/agent-contract.md`.

`agent.json`:

```json
{
  "schema": 1,
  "name": "my-agent",
  "version": "0.1.0",
  "entrypoint": ["/opt/agent/run.sh"],
  "streaming": true
}
```

- `schema` must be `1`; `name` matches `[a-z0-9][a-z0-9-]{0,39}`.
- `entrypoint` (optional) is the command run at every stage; default is the image's ENTRYPOINT/CMD.
- `streaming: true` if the agent calls the model with `stream: true` (lets the meter count tokens).
- `app_start_cmd` (optional, `web-app` tasks only) is the start command when `/work` has no Dockerfile; default `["npm", "start"]`.

`Dockerfile`:

```dockerfile
FROM python:3.12-slim
RUN pip install --no-cache-dir openai==1.*
COPY run.sh agent.py /opt/agent/
RUN chmod 0755 /opt/agent/run.sh
ENTRYPOINT ["/opt/agent/run.sh"]
```

`run.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail
exec timeout "$DEADLINE_S" python3 /opt/agent/agent.py --requirements /req --workdir "$PWD" --model "$MODEL"
```

What the agent produces depends on the task set: a website, a set of files, a project that then answers a simulator interactively, and so on. Read the stage requirements in `/req` for what is expected. The contract below is the same for all of them.

What the agent must do: start from the command line, read this stage's requirements in `/req` (read-only), work in the current directory `/work`, and exit when done. Whatever is in `/work` at the end is the stage's output. The agent does not need to know about stages: `/work` and `HOME` (`/home/agent`) are kept between stages; the next stage just puts new requirements in `/req` and runs the same command again.

Runtime environment (each stage is a fresh container from the same image):

| Variable | Meaning |
|---|---|
| `OPENAI_BASE_URL` | The platform's metering proxy. Only `POST {base}/chat/completions` (OpenAI-compatible). |
| `OPENAI_API_KEY` | Always `dummy`. The real key stays in the proxy and never enters the container. |
| `MODEL` | Model name to request. Other models get 403. |
| `DEADLINE_S` | Stage time limit in seconds from container start. Exit before it. |
| `REQ_DIR` / `WORK_DIR` | `/req` / `/work` |

Use the OpenAI SDK without passing `base_url`/`api_key`; it reads them from the environment.

Constraints: 2 GB RAM (no swap), 1 CPU, 1024 processes, non-root, all capabilities dropped. Network: only the metering proxy and the npm / PyPI registries (through `HTTPS_PROXY`); no DNS, nothing else. Install every other dependency in the Dockerfile. The agent cannot see the tests. At the deadline it gets SIGTERM, then SIGKILL 30 s later; `/work` is snapshotted every 15 minutes and the last snapshot is scored if the agent is killed. The proxy passes upstream 429/500 through, and returns 429 with `error.type = "budget_exceeded"` when the user's budget is spent; handle both.

Output format depends on the task set: `web-app` outputs are built with no network access and served on port 3000 (either a `Dockerfile` at the root of `/work`, or `frontend/` + `backend/` following the ARC-Bench convention; keep runtime dependencies inside `/work`); `files` outputs are the whole `/work`. Symlinks and `.git` are dropped.

## Available task sets

Do not hard-code task set names or stage counts. Fetch them (no auth needed):

```sh
curl -s https://crucible-worker.stratosphericus.workers.dev/tasksets
```

Answer: `[{"name", "version", "stages": [{"name", "time_limit_s", "total"}]}]`. `total` is the number of tests in the stage and may be `null`. Stage numbers on the command line are 1-based positions in `stages`.

### Your own task set and scorer

Uploaded task sets are private to the uploader unless an admin makes them public. The format is a zip (or directory) like `tasksets/hello-world/source` in the repository: `source.json` plus one directory per stage with the agent's inputs and the hidden tests; total time limit at most 18000 s; plugins offered to users (`playwright`, `llm-judge`, ...) or a scorer you uploaded yourself. Check locally with the same rules the platform uses, then upload (token from `CRUCIBLE_TOKEN`):

```sh
crucible taskset validate my-tasks.zip
crucible taskset upload my-tasks.zip --wait    # prints u-<16 hex>
```

Uploaded task sets appear in `GET /tasksets` (with your token) as `u-...` names, usable with `--taskset u-...`.

A scorer of your own is a zip (or directory) with `plugin.json` (`{"schema": 1, "kind": "scorer", "name", "version", "runs_taskset_code", "model", "accepts"}`) and a `Dockerfile`; the image's entrypoint is called as `ENTRYPOINT --artifact /in/artifact --tests /in/tests --out /out/result.json --visibility hidden` and writes result.json v2 (docs/plugins.md §14 in the repository). The platform builds and self-tests it, then a task set refers to it as `"scorer": {"name": "u-..."}`:

```sh
crucible plugin upload my-scorer/ --wait       # prints u-<16 hex>
crucible plugin status u-0123456789abcdef
crucible plugin list
```

## Submit

### Consent: the human must agree, not you

Every submission requires agreeing to this statement (verbatim, as the CLI prints it):

> 你上传的内容、评测产出和日志会加密后永久保存，我们会用于研究和改进平台。你的模型 key 和下载密码在评测结束后立即删除，不会保存。

In English: "Your uploads, evaluation outputs and logs are encrypted and kept permanently; we use them for research and to improve the platform. Your model key and download password are deleted right after the evaluation and are not kept."

`--i-agree` records that **the human user** accepts this statement. Show the statement to the user and pass `--i-agree` only after they explicitly agree. **Never pass `--i-agree` on your own initiative**, and do not treat a general instruction such as "run the benchmark" as consent. One explicit agreement from the user may cover a series of submissions they asked for; if in doubt, ask.

### Full evaluation of an agent

The model key and the download password are passed by environment variable name, never as values on the command line. The user should set them; you only name the variables.

```sh
export MY_KEY=sk-...                 # model API key (set by the user)
export DL_PW='at least 12 characters' # download password for the outputs (set by the user)
crucible submit agent --agent-dir ./my-agent --taskset <taskset> \
  --model <model> --endpoint https://api.example.com/v1 --replicas 3 \
  --api-key-env MY_KEY --download-password-env DL_PW --i-agree
```

- `--agent-dir <dir>` or `--agent-zip <file>`: the package (zipped locally; sealed zip at most 25 MB).
- `--endpoint`: OpenAI-compatible base URL, must be `https://`.
- `--replicas N`: 1 to 10 independent runs (default 1).
- `--stages N`: run only the first N stages (default: all).
- `--max-requests`, `--max-tokens`, `--max-cost-usd`: budget for the whole evaluation, across stages.
- `--public`: make the scores public and list them on the leaderboard (default: private).
- `--wait`: after submitting, wait for the result like `crucible status --wait`.
- `--api <url>` or `CRUCIBLE_API`: Worker address (default is the one above).

On success stdout holds exactly one line, the eval id; the statement and progress go to stderr.

### Score an existing output (no agent run)

```sh
crucible submit app --zip site.zip --taskset <taskset> --stage 1 --i-agree
```

`--stage` is the 1-based stage to score. `--public`, `--wait` and `--api` work as above.

Task sets scored by a model (e.g. `math-proof-demo`) need your model credential in this mode too: add `--model <model> --endpoint https://... --api-key-env MY_KEY --download-password-env DL_PW`. The task set may fix the judge model (`model.name`); your endpoint must serve it.

### Check status

```sh
crucible status <eval_id> --wait --json
```

`--wait` polls until the evaluation ends; `--json` prints the raw `GET /evals/:id` answer instead of a table. The exit code is non-zero when the evaluation failed. Status values: `queued`, `building`, `running:<stage>`, `scoring`, `done`, `failed` (the last two are final). A full run can take hours (stage limits are in `/tasksets`).

Human-readable page: `https://octos-org.github.io/octos-crucible/#/evals/<eval_id>`.

## Read results

Key fields of `crucible status <eval_id> --json`:

- `status`, `total_score`: computed by the taskset's aggregate as recorded in `manifest.scoring` (4 decimals; absent when nothing was scored). With `ratio` (test-counting tasksets such as hello-world, and every evaluation without `scoring`) it is 0 to 1, Σscore / Σmax over every scored stage of every replica. With `sum` / `mean` / `weighted` (e.g. astro-practice: the sum of the cards' survey scores) it is the mean over replicas of each replica's total, in the stage score's own unit, and may be negative.
- `manifest.scoring`: `{aggregate, display: {stage, total}, plugins}`. `display.*` has `name`, `unit`, `direction` (`higher` or `lower` is better), `decimals`, `format` (`number`, `percent`, or `fraction` = `score/max`). Use it to read and compare scores.
- `manifest.replicas[]`: one entry per replica (`replica` number; `failure` says why a replica produced no usable result).
- `manifest.replicas[].stages[]`, one per stage:
  - `stage`: stage name.
  - `score`: `{status, score, max?, passed?, items?}`; `null` if the stage was never scored. `status` is `scored` (counts, also when the agent produced nothing usable: then usually 0) or `error` (scoring infrastructure failed, not counted, retry). `score` is continuous and may be negative; `max` is the stage maximum when there is one (for test counting, `score` = tests passed, `max` = tests); `items` breaks the score down (e.g. astro-practice's five components). Old evaluations have `{status: passed|failed|system_error|rejected, passed, total}` instead: read `passed`/`failed` as `scored` with `score = passed`, `max = total`, and `system_error`/`rejected` as `error`.
  - `wall_s`: wall time in seconds, from GitHub's own job timestamps (not the agent's report).
  - `usage`: `{requests, prompt_tokens, cached_tokens, completion_tokens, reasoning_tokens}`, counted by the metering proxy. `prompt_tokens` includes `cached_tokens`; cache hit rate = `cached_tokens / prompt_tokens`.
  - `cost_usd`: equivalent pay-as-you-go cost at public list prices (uncached input, cached input and output priced separately; table in `config/pricing.json`). `null` when the model's price is unknown. If the user is on a subscription plan this is only a reference figure.
  - `ended`: `exited` (agent stopped by itself), `deadline` or `aborted`; `exit_code`; `checkpoint_source`: `final` or `snapshot`.

Outputs and logs: a zip encrypted (AES-256) with the download password. Get its address with:

```sh
curl -s -H "Authorization: Bearer $CRUCIBLE_TOKEN" -H "Accept: application/json" \
  https://crucible-worker.stratosphericus.workers.dev/evals/<eval_id>/download
```

Open it with 7-Zip / Keka / The Unarchiver (not the macOS or Windows built-in tools). The platform does not keep the password.

## Leaderboard

Evaluations submitted with `--public` that finish with a score appear on the public leaderboard of their task set (within 5 minutes): https://octos-org.github.io/octos-crucible/#/leaderboard. It shows the GitHub login, agent name and version, model, total and per-stage scores, replicas, time and cost; outputs and logs are never public. Each (user, agent name) is listed once, with its best evaluation by the task set's `direction`; equal totals share a rank. Read it without a token:

```sh
curl -s https://crucible-worker.stratosphericus.workers.dev/leaderboard              # task sets with public results
curl -s https://crucible-worker.stratosphericus.workers.dev/leaderboard/<taskset>    # {direction, display, entries: [{rank, login, agent, model, total_score, stages, ...}]}
```

## Iterate

A sound optimisation loop:

1. **Run each version several times** (`--replicas 3` or more) and compare means and spread, not single runs. Scores vary between runs.
2. **Use the compare view** for versions side by side (mean ± std per stage, time, tokens, cache rate, cost, with deltas): `https://octos-org.github.io/octos-crucible/#/compare?ids=<id1>,<id2>` (up to 4 ids).
3. **Keep tuning and acceptance separate.** Tune on one task set (or its early stages) and confirm on a different task set you did not tune against, so improvements are not overfitted.
4. **Cut cost while iterating**: `--stages 1` for quick checks, budgets via `--max-cost-usd` / `--max-tokens`, full runs only for candidates.
5. Read the downloaded logs of failed or low-scoring stages to find what went wrong; change one thing per version and record the eval ids.

## Limits and privacy

- Model keys and download passwords are encrypted locally (in the browser or the CLI) with the platform's public key; the server never sees them in clear. They are deleted when the evaluation ends.
- Uploads, outputs and logs are stored encrypted and kept permanently for research.
- Scores are private by default (`--public` puts them on the leaderboard with your GitHub login, agent name and model). Outputs and logs are never public.
- Limits: package or output zip up to 25 MB; 1 to 10 replicas; download password at least 12 characters; at most 20 CLI tokens per user.
