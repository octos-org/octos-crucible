# octos-crucible 使用指南

> 平台仍在开发中，完整流程尚未跑通。本页描述的是已经确定的设计，实际使用以网页上的提示为准。

## 平台是什么

octos-crucible 是一个评测 coding agent 的平台。你把自己的 agent 交给它，它在隔离的容器里让 agent 按阶段完成一道题，每个阶段结束后用隐藏的测试打分，同时记录用时、模型请求数、token、缓存命中率，并按公开价格折算出等价花销（按量计费的口径；如果你用包月套餐，这个数字只作参考）。平台不是排行榜，也不绑定某一套题或某一种打分方式；目前的正式题目是 ARC-Bench 的 GitHub 题，打分方式是 Playwright 浏览器测试。用 GitHub 账号登录即可使用。

## 两种用法

**上传 agent，完整评测。** 上传 agent 包（zip），填写模型名、OpenAI 兼容的接口地址（必须是 https）和你的 API key，选择题目包和运行遍数，设一个下载密码。平台构建你的 agent，依次给出每个阶段的需求，让它在同一个工作目录里一路做下去，每阶段单独打分。结果里有每一遍、每个阶段的分数和用量，多遍时给出均值和波动。

**上传产出，快速打分（即将推出）。** 直接上传已经做好的产出（例如一个网站的 zip），选择题目包和阶段，只打分，不运行 agent。

## agent 包最小示例

agent 包是一个 zip，根目录下有 `agent.json` 和 `Dockerfile`。agent 只需要做到两点：能从命令行启动，读 `/req` 里本阶段的需求，在当前目录 `/work` 里干活，干完自己退出；调模型时从环境变量 `OPENAI_BASE_URL` 读接口地址（OpenAI SDK 默认就会读）。agent 不需要知道“阶段”：平台在阶段之间保留 `/work` 和 `HOME`，下一阶段只是换一份需求，再启动一次同一条命令。

`agent.json`：

```json
{
  "schema": 1,
  "name": "my-agent",
  "version": "0.1.0",
  "entrypoint": ["/opt/agent/run.sh"]
}
```

`Dockerfile`：

```dockerfile
FROM python:3.12-slim
RUN pip install --no-cache-dir openai==1.*
COPY run.sh agent.py /opt/agent/
RUN chmod 0755 /opt/agent/run.sh
ENTRYPOINT ["/opt/agent/run.sh"]
```

`run.sh` 里启动你的程序即可，例如 `exec timeout "$DEADLINE_S" python3 /opt/agent/agent.py`。容器里可用的环境变量有 `MODEL`（模型名）、`DEADLINE_S`（本阶段时限，秒）、`OPENAI_BASE_URL`（平台的计量代理）；`OPENAI_API_KEY` 是 `dummy`，你的真实 key 只在计量代理里，不进容器。运行时容器限 2 GB 内存、1 核、非 root，只能访问计量代理和 npm、PyPI，看不到测试，所以其他依赖要在 `Dockerfile` 里装好。在 `my-agent/` 目录里执行 `zip -r ../my-agent.zip .`，保证 `agent.json` 在 zip 根目录，然后上传。完整规范见 [docs/agent-contract.md](../agent-contract.md)。

## 命令行提交

适合脚本和闭环优化反复提交。行为与网页完全一致：本地打 zip、用平台公钥加密后上传，模型 key 和下载密码同样在本机加密，明文不经过服务器。

1. **安装**：`cargo install --git https://github.com/octos-org/octos-crucible crucible-cli --locked`（得到 `crucible` 命令）。
2. **令牌**：网页登录 →「我的评测」→「命令行令牌」→「生成命令行令牌」。令牌只显示一次，复制后 `export CRUCIBLE_TOKEN=crt_...`。令牌等同于你的账号（但没有管理员权限，也不能用来再生成令牌），不用时在同一处撤销；每人最多 20 个。
3. **提交**（`--i-agree` 表示同意下方[同意声明](#同意声明)，必填；命令会打印声明原文）：

```sh
# 只打分：上传一个阶段的产出 zip
crucible submit app --zip site.zip --taskset github-full --stage 1 --i-agree

# 完整评测：agent 目录会在本地打包（也可用 --agent-zip my-agent.zip）
export MY_KEY=sk-...  DL_PW='至少 12 个字符的下载密码'
crucible submit agent --agent-dir ./my-agent --taskset github-full \
  --model glm-5.3 --endpoint https://api.example.com/v1 --replicas 3 \
  --api-key-env MY_KEY --download-password-env DL_PW --i-agree
```

   可选参数：`--stages N`（只跑前 N 个阶段）、`--max-requests` / `--max-tokens` / `--max-cost-usd`（预算）、`--public`（公开分数）、`--wait`（提交后等结果）。模型 key 和下载密码只通过环境变量传入，不出现在命令行参数里。成功时标准输出只有一行 eval_id，便于脚本使用。
4. **查结果**：`crucible status <eval_id>`，加 `--wait` 轮询到结束，打印每一遍每个阶段的分数、用时、token、等价花销和总分；`--json` 输出原始 JSON。评测失败时退出码非 0。

默认连接 `https://crucible-worker.stratosphericus.workers.dev`，可用 `--api` 或环境变量 `CRUCIBLE_API` 改。

## 下载产出

评测完成后，可以在评测详情页下载产出和日志。下载到的是用你的下载密码加密的 AES-256 zip。macOS 和 Windows 自带的解压工具不支持这种加密，打不开，请使用 **7-Zip**、**Keka** 或 **The Unarchiver**。之所以不用系统自带工具支持的格式，是因为那种旧式加密（ZipCrypto）已被攻破。平台不保存下载密码，忘记后无法找回。

## 同意声明

提交前需要同意以下声明：

> 你上传的内容、评测产出和日志会加密后永久保存，我们会用于研究和改进平台。你的模型 key 和下载密码在评测结束后立即删除，不会保存。

分数默认不公开，提交时可选择公开；产出和日志始终不公开。
