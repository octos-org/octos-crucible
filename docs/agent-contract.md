# Agent 包规范

内置 agent 和用户上传的 agent 用同一种格式，平台不对任何一方特殊处理。

## 最小要求

一个 agent 只需要做到：

1. **能从命令行启动**：容器里执行一条命令即可开始工作，干完自己退出。
2. **读需求**：本阶段的需求文件在 `/req`（只读）。
3. **在工作目录里干活**：当前目录 `/work` 就是产出所在；阶段结束时 `/work` 里的内容即该阶段的产出。
4. **调模型的 base URL 可配置**：从环境变量 `OPENAI_BASE_URL` 读取（OpenAI 兼容接口）。

**不要求**：理解"阶段"、保存或恢复状态、知道自己是第几遍。平台在阶段之间原样保留 `/work` 和 `HOME`，下一阶段只是"在同一个目录里给出新的需求，再启动一次同一条命令"。

## 包结构

一个目录（或其 zip），根目录下有 `agent.json` 和 `Dockerfile`：

```
my-agent/
  agent.json
  Dockerfile
  run.sh
  agent.py
```

包在宿主机上只被当作字节处理：它是 `docker build` 的上下文，其中的任何东西都只在容器里运行。

## agent.json

```json
{
  "schema": 1,
  "name": "my-agent",
  "version": "0.1.0",
  "entrypoint": ["/opt/agent/run.sh"],
  "streaming": true
}
```

| 字段 | 必填 | 含义 |
|---|---|---|
| `schema` | 是 | 固定为 `1` |
| `name` | 是 | `[a-z0-9][a-z0-9-]{0,39}` |
| `version` | 否 | 自由文本，记入评测清单；缺省 `"0"` |
| `entrypoint` | 否 | 每个阶段执行的命令（字符串数组）；缺省用镜像自己的 ENTRYPOINT/CMD |
| `streaming` | 否 | agent 以 `stream: true` 调模型时设为 `true`。计量代理会在请求没带 `stream_options` 时补上 `include_usage`，以便统计 token |
| `app_start_cmd` | 否 | 仅用于产出类型 `web-app` 且 `/work` 根目录没有 `Dockerfile` 时：平台生成的 Dockerfile 的 CMD（在 `backend/` 下执行）。缺省 `["npm", "start"]` |

类型定义见 `crates/crucible-core/src/agent.rs`（`AgentSpec`）。

内置 agent 若从另一个仓库构建，可在包里带 `upstream.json`（`{"repo", "ref", "build_arg"}`）：平台构建前用 `git ls-remote` 把 `ref` 解析为完整 commit，经 `--build-arg <build_arg>=<commit>` 构建，构建后核对镜像里 `/agent-build.json` 的 `commit` 与之相同（不同则失败），并把它记入评测清单。上传的 agent 忽略此文件。

## 运行环境

每个阶段，平台在同一个容器配置下执行一次 `entrypoint`（每阶段一个新容器，同一个镜像）。

| 环境变量 | 含义 |
|---|---|
| `REQ_DIR` | 需求目录，固定为 `/req`，只读：本阶段的需求（题目包为该阶段声明的 inputs） |
| `WORK_DIR` | 工作目录，固定为 `/work`，也是当前目录；可写；阶段之间原样保留 |
| `OPENAI_BASE_URL` | 计量代理地址（如 `http://172.31.250.1:8787/v1`），只提供 `POST {base}/chat/completions` |
| `OPENAI_API_KEY` | `dummy`。真 key 只在计量代理里，从不进入容器 |
| `MODEL` | 要请求的模型名；请求其他模型返回 403 |
| `DEADLINE_S` | 本阶段的时限（秒，从容器启动算起），请在此之前自行退出 |

另外：`HOME=/home/agent`（可写，阶段之间保留）；`HTTPS_PROXY`/`HTTP_PROXY`（出网代理，只能访问 npm 和 PyPI）；`NO_PROXY` 包含计量代理地址。

限制：2 GB 内存（无 swap）、1 核、1024 个进程、非 root、`cap-drop ALL`、`no-new-privileges`。除计量代理和白名单软件源外没有网络，也没有 DNS。看不到测试。到时限先 SIGTERM，30 秒后 SIGKILL。阶段内平台每 15 分钟对 `/work` 自动快照；agent 自行退出时用 `/work` 的最终状态作为该阶段产出，被强制结束时用最后一份快照，agent 无需配合。

计量代理的返回：上游的 429/500 原样透传；用户设置了预算且已用完时返回 429，`error.type` 为 `budget_exceeded`。预算按整次评测计，跨阶段累计。

## 产出

阶段结束时 `/work` 的内容由题目包声明的打包器（`packager`，旧题目包写作 `output`）打包（符号链接一律丢弃，`.git` 不打包）。打包器是登记在 `plugins.json` 里、编译进 `crucible` 的插件，接口见 `docs/plugins.md` §5。现有两种：

- `web-app`：打成根目录带 `Dockerfile` 的 zip，打分器用 `--network=none` 构建并在 3000 端口访问。
  - `/work` 根目录有 `Dockerfile`：整个 `/work` 原样打包。
  - 否则（ARC-Bench 约定）：打包 `frontend/` 与 `backend/`（不含 `frontend/node_modules`），平台补一个 Dockerfile：`FROM node:24-bookworm-slim`、`WORKDIR /app/backend`、`PORT=3000`、`CMD <app_start_cmd>`。运行时需要的依赖（如 `backend/node_modules`、前端 `dist/`）要留在 `/work` 里。
- `files`：整个 `/work`。题目包可以用 `packager_options.require` 列出根目录必须有的文件（例如 `answer.md`），缺了就没有产出。

## 示例：my-agent

`Dockerfile`：

```dockerfile
FROM python:3.12-slim
RUN pip install --no-cache-dir openai==1.*
COPY run.sh agent.py /opt/agent/
RUN chmod 0755 /opt/agent/run.sh
ENTRYPOINT ["/opt/agent/run.sh"]
```

`run.sh`：

```bash
#!/usr/bin/env bash
set -euo pipefail
mkdir -p "$HOME"
# OpenAI SDK 自动读取 OPENAI_BASE_URL / OPENAI_API_KEY。
exec timeout "$DEADLINE_S" python3 /opt/agent/agent.py --requirements /req --workdir "$PWD" --model "$MODEL"
```

`agent.py` 读 `/req` 下的需求，在 `--workdir` 里写代码，调模型时使用 `OpenAI()`（不指定 base_url，让 SDK 从环境变量读取）。它不需要知道当前是第几阶段：第二阶段启动时，`/work` 里已经有第一阶段留下的代码，`/req` 换成了新的需求。

把目录内容（`agent.json` 在 zip 根目录）打成 zip 上传即可。

## Codex 示例

`agents/codex/` 是内置的 OpenAI Codex CLI，与其他包格式相同：

- `Dockerfile`：从源码编译 `openai/codex`（`upstream.json` 固定为 `rust-v0.93.0`，平台解析为 SHA 后经 `AGENT_REF` 传入），先打 `patches/zai-role-compat.patch`（z.ai 的 chat completions 不接受 `developer` 角色，改为 `system`）。不用官方二进制，是因为需要这个补丁；不升级版本，是因为之后的 Codex 去掉了 `wire_api = "chat"`，而计量代理只提供 `/chat/completions`。
- `entry.sh`：每阶段把 `$HOME/.codex/config.toml` 写成指向 `OPENAI_BASE_URL` 的 chat wire 提供方（`env_key = "OPENAI_API_KEY"`，即 `dummy`），在 `DEADLINE_S` 前略早结束 `codex exec --skip-git-repo-check -s danger-full-access -C /work`。容器本身就是沙箱，所以关掉 Codex 自带的沙箱。
- `prompt.txt`：读 `/req`，在 `/work` 里写；`/work` 里已有代码时就地扩展。`agent.json` 设 `streaming: true`，`app_start_cmd: ["node", "server.js"]`。
