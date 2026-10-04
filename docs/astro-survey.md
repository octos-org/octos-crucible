# 智能体巡天（GOSIM Agentic Observer）接入说明

把 GOSIM「智能体巡天黑客松」的公开入门包接进 crucible：题目包 `tasksets/astro-practice/`，交互运行器 `runners/astro-v4/`（裁判进程在沙箱外，与沙箱内的 agent 一问一答），打分器 `scorers/astro-survey/`（只读运行记录）。两者都是插件（`plugins.json`，`docs/plugins.md` §4.2、§12）。

## 来源（锁定）

| 内容 | 来源 |
|---|---|
| 入门包 | 比赛网站资源页的公开下载 `gosim-observer-examples.zip`（GitHub release `gosimfoundation/hackathon-survey26` `examples-2026-10-02`），sha256 `55aa1e72bf97b8e27801793fbc48b749061ed12aa2f01f151e2bd56674e42711` |
| 引擎 + 评分 | 入门包 `runner/` 原样拷到 `runners/astro-v4/image/engine/`；`ENGINE_MANIFEST.json` 把 16 个文件钉在 survey26 commit `db4bddf3`，镜像构建时跑 `verify_engine.py`，有改动即构建失败 |
| 练习卡 | 入门包 `local-cards/L1`–`L4` → `tasksets/astro-practice/source/L1`–`L4` |
| 示例 agent | 入门包 `python/` → `tasksets/astro-practice/example-agent/`，只改了 `observer.project.json` 的 `environment`（见下） |

入门包材料按 CC BY-NC 4.0 授权（`LICENSE.md` 随拷贝保留），引用：GOSIM 2026 Agentic Observer Hackathon（https://create.gosim.org/survey26/）。

## 映射

| 比赛 | crucible |
|---|---|
| 参赛者的观测 agent 项目（zip，根目录 `observer.project.json`，`protocol: jsonl-v4`） | 产出，打包器 `files`（`packager_options.require: ["observer.project.json"]`）；app 模式上传的 zip，agent 模式下 coding agent 的工作目录打包 |
| 一张练习卡 | 一个阶段（`l1`–`l4`） |
| 卡片 `card.md` | 阶段输入（`inputs`） |
| 卡片 `config/` `public/` `truth/`（真值、天气、事件） | 测试材料（`tests`，只有打分容器能看到） |
| 引擎的 `score_report.total`（连续分） | 阶段 `score`（result v2，原样，见「计分」） |

## 交互运行器 `runners/astro-v4` 与打分器 `scorers/astro-survey`

题目包每个阶段声明 `interactive: {"name": "astro-v4", "time_limit_s": 1500}`（时限只是兜底，卡片自己的时间墙 ≤ 900 s）。`score-tests` 里先由 `crucible score` 调交互运行器，再把运行记录交给打分器。

`run.sh` 接口按 `docs/plugins.md` §4.2：`--agent ZIP --material CARD_DIR --out DIR --time-limit S [--agent-network NAME --model-base-url URL --model NAME]`。流程：

1. 建一个私有 docker volume，里面两个 FIFO（`to_agent` / `from_agent`）。
2. **agent 容器**：只挂 agent zip 和 FIFO；`agent_entry.py` 安全解压（拒绝绝对路径、`..`、符号链接；≤5000 文件、≤200 MB），读 `observer.project.json`，在 FIFO 上启动 `run` 命令。
3. **引擎容器**：只挂卡片（只读）、输出目录和 FIFO；运行入门包自己的 `run_local.py`，它把 agent 命令当成 `bridge.py`——一个把 stdin/stdout 原样转发到 FIFO 的中继。时间墙、消息流、计分全部由未改动的引擎完成，引擎的结果写成 `summary.json`。
4. 两个容器都是只读根、`--cap-drop ALL`、2 GB / 2 CPU / 256 pids、调用者 uid。引擎容器 `--network none`。agent 容器默认也是 `--network none`；题目包声明 `model.interactive: "optional"`，提交者又给了凭据时，agent 容器接到平台建好的沙箱网络（唯一可达的是宿主机上的计量代理），环境里 `OPENAI_BASE_URL` 指向计量代理、`OPENAI_API_KEY=dummy`、`OPENAI_MODEL` / `MODEL` 为模型名，用量记进 `eval_usage.interactive`。
5. `--out/run.json`：`completed`（引擎跑完）、`agent_failed`（zip 不是有效项目、超时）或 `error`（运行器自身出错，阶段记 `error`）。

`run_local.py` 发给 agent 的环境变量（`PARTICIPANT_PROTOCOL`、`SAC_SCENARIO`、`SAC_WALLCLOCK_SECONDS`、`SAC_LOCAL_RUNNER` 等）由 `bridge.py` 经 `/pipes/env.json` 转给 agent 容器，再叠加模型变量（若有）和项目的 `environment`。

打分器 `score.sh --run DIR ...` 只读运行记录，在一个无网络、只读根的小容器里换算成 result v2。

### 计分

打分器输出 result v2（`docs/scorer-contract.md` §4），不做任何换算：

- `score` = 引擎的 `score_report.total`（连续分，可为负），不设 `max`、不填 `passed`；
- `items` = 5 个分项（sum_best_scores、required_penalty、uniformity_penalty、report_settlement、observation_request_reward），名字由打分器代码固定，hidden 可见性下也保留；
- `detail` = 引擎的结束原因（如 `survey_complete`）；
- agent 没能启动/初始化、zip 不是有效的 observer 项目、超时：`scored`，0 分；引擎没产出或运行器出错：`error`（system）。

题目包 `astro-practice`（`schema: 2`）声明 `aggregate: {"stages": "sum"}`（总分 = 各卡得分之和，暂定）和 `display`：阶段分"观测得分"、总分"四卡总分"，单位"分"，越高越好，2 位小数。网页、`crucible status` 按此显示，例如示例 agent 在 L1 得 `4458.56 分`。

P1 之前的评测（毫分 / 10,000,000 存为 `passed / total`）不改写，仍按旧的比例总分显示。

### 限制

- 只支持入门包 `python:3.12-slim` 能直接运行的项目：`build` 步骤不支持（TypeScript / Rust 示例需要先构建，暂不能用）；`image` 字段忽略。
- 模型可选：不带凭据时 agent 没有网络。入门包的 Python 示例要求 `OPENAI_API_KEY` 非空，`example-agent/observer.project.json` 里写了占位值 `offline-no-model` 和不可达的 `OPENAI_BASE_URL`（项目的 `environment` 优先于平台给的模型变量），所以示例 agent 无论有没有凭据都走它自带的规则路径，结果确定。要让自己的 agent 用模型，不要在 `environment` 里覆盖 `OPENAI_*`。

## 用法

```bash
# 打包示例 agent
python3 tasksets/astro-practice/example-agent/pack_agent.py --out /tmp/astro-agent.zip

# 本地跑一局再打分（colima / Docker Desktop 下 TMPDIR 与 zip 要在主目录下）
runners/astro-v4/run.sh --agent /tmp/astro-agent.zip --material tasksets/astro-practice/source/L1 \
  --out /tmp/astro-run --time-limit 1500
scorers/astro-survey/score.sh --run /tmp/astro-run --out /tmp/result.json

# 走平台（app 模式）：seal + put，然后 score.yml
crucible seal --recipient "$(jq -r '.keys[0].public_key' config/keys.json)" --in /tmp/astro-agent.zip --out /tmp/a.sealed
crucible put --store github:octos-org/octos-crucible /tmp/a.sealed     # 输出 <hash>
gh workflow run score.yml -f artifact_source=blob:<hash> -f taskset=astro-practice -f stage=1

# 对照：入门包自带的本地裁判
OPENAI_API_KEY=offline-no-model OPENAI_BASE_URL=http://127.0.0.1:9/v1 \
  python3 runner/run_local.py --inherit-env --card local-cards/L1 --agent "python3 agent.py" --agent-cwd python
```

题目包重打：`crucible taskset pack --source tasksets/astro-practice/source.json --src-dir tasksets/astro-practice/source --keys config/keys.json --store github:octos-org/octos-crucible --out tasksets/astro-practice/taskset.json`。

## agent 模式

插槽 1 是缺省的 `workdir` 运行器：`files` 打包器会把 coding agent 的整个工作目录打包成 zip，正好就是 observer 项目。让 Octos 之类的 agent 读阶段输入（`card.md`，最好再把入门包 `docs/participant-guide.*.md` 加进 `inputs`），在工作目录写出带 `observer.project.json` 的 Python 项目，产出再经交互运行器和打分器计分，不需要改核心。
