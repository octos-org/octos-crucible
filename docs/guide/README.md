# octos-crucible 使用指南

> 平台仍在开发中，完整流程尚未跑通。本页描述的是已经确定的设计，实际使用以网页上的提示为准。

## 平台是什么

octos-crucible 是评测 agent 的通用平台：任何能在沙箱里运行的 agent，在任何题目上按阶段运行、打分，同时客观记录用时、模型请求数、token、缓存命中率，并按公开价格折算出等价花销（按量计费的口径；如果你用包月套餐，这个数字只作参考）。你把自己的 agent 交给它，它在隔离的容器里让 agent 按阶段完成一道题，每个阶段结束后用隐藏的测试材料打分。

写代码只是其中一类题。现有和计划中的例子：

- **写代码**：ARC-Bench 的 GitHub 题，agent 写出网站，用 Playwright 浏览器测试打分。
- **交互决策**：GOSIM 智能体巡天（题目包 `astro-practice`），观测 agent 与模拟器一问一答，用官方评分引擎打分。
- **数学 / 推理**：题目包 `math-proof-demo`（两道经典 IMO 题），agent 把证明写进 `answer.md`，固定的评判模型按隐藏的参考解答和评分细则逐项给分（每题 7 分，评 3 次取中位数）。评判用的是你自己的模型 key（经平台计量代理），这部分用量单独显示为“评测阶段的模型用量”，不算进 agent 的用量。上传产出打分时同样要填模型和 key（命令行 `crucible submit app` 加 `--model --endpoint --api-key-env --download-password-env`）。

题目、运行方式、打包、打分都是可替换的插件。平台不是排行榜，也不绑定某一套题或某一种打分方式。用 GitHub 账号登录即可使用。

## 两种用法

**上传 agent，完整评测。** 上传 agent 包（zip），填写模型名、OpenAI 兼容的接口地址（必须是 https）和你的 API key，选择题目包和运行遍数，设一个下载密码。平台构建你的 agent，依次给出每个阶段的需求，让它在同一个工作目录里一路做下去，每阶段单独打分。结果里有每一遍、每个阶段的分数和用量，多遍时给出均值和波动。

**上传产出，快速打分（即将推出）。** 直接上传已经做好的产出（zip，内容由题目包决定，例如写网站的题就是网站的 zip），选择题目包和阶段，只打分，不运行 agent。

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

## 上传自己的题目包

在网页“题目包”页登录后点“上传题目包”。

- **格式**：一个 zip，结构同仓库里的 `tasksets/hello-world/source`：根目录（或 zip 里唯一的顶层文件夹）放 `source.json`，每个阶段一个目录，里面是给 agent 的需求文件（如 `requirements.yaml`）和只给打分器的测试（如 `tests/`）。`source.json` 示例：

  ```json
  {
    "schema": 1, "name": "my-tasks", "description": "…",
    "scorer": {"name": "playwright"}, "aggregate": "sum", "total_time_limit_s": 1200,
    "stages": [
      {"id": "stage-1", "dir": "stage-1", "inputs": ["requirements.yaml"], "tests": ["tests"],
       "packager": "web-app", "time_limit_s": 600, "expected_total": 1}
    ]
  }
  ```
- **限制**：各阶段限时之和 ≤ `total_time_limit_s` ≤ 18000 秒；只能引用插件注册表 `plugins.json` 里标为 `user: true` 的插件，目前打分器只有 `playwright`（打包器 `web-app`；旧写法 `"output": "web-app"` 同样有效）；给 agent 的文件和测试文件不能重叠；不能有符号链接；加密后不超过 25 MB。
- **先在本地检查**：`crucible taskset validate my-tasks.zip`（也可以传目录或 `source.json`），和平台用的是同一套检查。
- **登记**：文件在浏览器里用平台公钥加密后上传。平台解密、检查、按阶段拆成“给 agent 的输入”和“测试”两份分别加密保存，几分钟后状态变为“可用”；没通过时页面上显示原因。
- **可见性**：默认私有，只有你能看到和使用（提交页的题目包列表、命令行 `--taskset u-…`）；管理员可以把它设为公开。仓库内置的题目包不受影响。
- **安全**：你的测试会被当作不可信代码运行：运行测试的机器上没有任何平台密钥，测试容器不能联网、不能访问宿主机（见 `docs/scorer-contract.md` §7）。

## 下载产出

评测完成后，可以在评测详情页下载产出和日志。下载到的是用你的下载密码加密的 AES-256 zip。macOS 和 Windows 自带的解压工具不支持这种加密，打不开，请使用 **7-Zip**、**Keka** 或 **The Unarchiver**。之所以不用系统自带工具支持的格式，是因为那种旧式加密（ZipCrypto）已被攻破。平台不保存下载密码，忘记后无法找回。

## 同意声明

提交前需要同意以下声明：

> 你上传的内容、评测产出和日志会加密后永久保存，我们会用于研究和改进平台。你的模型 key 和下载密码在评测结束后立即删除，不会保存。

分数默认不公开，提交时可选择公开；产出和日志始终不公开。
