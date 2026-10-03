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

类型定义见 `crates/crucible-core/src/agent.rs`（`AgentSpec`）。

## 运行环境

每个阶段，平台在同一个容器配置下执行一次 `entrypoint`：

| | |
|---|---|
| 当前目录 `/work` | 工作目录，可写；阶段之间保留 |
| `/req` | 本阶段的需求（题目包为该阶段声明的 inputs），只读 |
| `HOME` | 可写；阶段之间保留 |
| `OPENAI_BASE_URL` | 计量代理地址，只提供 `POST {base}/chat/completions` |
| `OPENAI_API_KEY` | `dummy`。真 key 只在计量代理里，从不进入容器 |
| `MODEL` | 要请求的模型名；请求其他模型返回 403 |
| `DEADLINE_S` | 本阶段的时限（秒），请在此之前自行退出 |
| `HTTPS_PROXY` / `HTTP_PROXY` | 出网代理，只能访问 npm 和 PyPI |

限制：2 GB 内存、1 核、非 root、`cap-drop ALL`、`no-new-privileges`。除计量代理和白名单软件源外没有网络。看不到测试。超时后先 SIGTERM，再 SIGKILL。阶段内平台每 15 分钟对 `/work` 自动快照；被强制结束时用最后一份快照打分，agent 无需配合。

计量代理的返回：上游的 429/500 原样透传；用户设置了预算且已用完时返回 429，`error.type` 为 `budget_exceeded`。

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
