# 打分器约定（scorer contract）

适用于所有打分器（Playwright、单元测试、脚本比对、模型评判……）。`crucible score` 只依赖这里写的东西，打分器内部怎么实现不限。

## 1. 交付形式

一个打分器 = **一个容器镜像 + 一个编排脚本**，放在 `scorers/<name>/`，并登记在仓库根目录的插件注册表 `plugins.json`（`kind: "scorer"`，`impl: "scorers/<name>"`，以及 `user`、`runs_taskset_code`、`model`、`accepts`，见 `docs/plugins.md` §3）。没登记的名字题目包不能引用；`crucible score` 按注册表找到 `score.sh`，打分前先用该阶段打分器对应的镜像（`score-tests` 预先构建为 `crucible-scorer-<name>:run`，经 `CRUCIBLE_SCORER_IMAGE` 传入）。

| 文件 | 作用 |
|---|---|
| `score.sh` | 编排脚本，命令行接口见下；宿主机只需要 bash 和 docker（外加 coreutils `timeout`） |
| `image/` | 打分器镜像的构建目录（`Dockerfile` 等）；版本全部钉死 |

agent 的产出和测试材料**只在容器里**处理；宿主机脚本只做编排（建网络、起容器、限时、清理）。

## 2. 输入

1. **agent 产出**（`--artifact`）：一个文件。格式由该阶段的打包器（`packager`）决定；上传的产出先经打包器的格式检查（`docs/plugins.md` §5），不符合的直接记 0 分，打分器不运行：
   - `web-app`：zip，**根目录**带 `Dockerfile`；容器监听 `PORT` 环境变量指定的端口（默认 3000），`GET /` 返回 200 即视为就绪。
2. **测试材料**（`--tests`）：已解密的目录，只读挂载给打分器。打分器不得把其中内容写进日志或公开输出。

## 3. 命令行接口（固定）

```
score.sh --artifact FILE --tests DIR --out result.json
         [--visibility public|hidden]   # 默认 public
         [--artifacts DIR]              # 可选：日志、原始报告、截图拷到这里
         [--task-id ID] [--submission-id ID]   # 只回填到 result.json，[A-Za-z0-9._-]
```

各打分器可以追加自己的可选参数（例如 Playwright 的 `--app-port`、各类超时），但不能改上面这些参数的含义。

退出码：

| 退出码 | 含义 |
|---|---|
| 0 | `result.json` 已写出（无论分数如何、无论 status 是什么） |
| 2 | 调用方错误（参数缺失、文件不存在）；不写 `result.json` |
| 其他 | 连 `result.json` 都没写出来，按 `system_error` 处理并可重试 |

调用方只看 `result.json`，不从退出码推断分数。

## 4. 输出：`result.json`

格式是 result v2，即 `crucible-core` 的 `ScoreResult`（`crates/crucible-core/src/score.rs`；设计见 `docs/plugins.md` §7）：

```json
{
  "schema": 2,
  "status": "scored",
  "score": 4458.556,
  "max": null,
  "passed": null,
  "detail": "survey_complete",
  "items": [
    {"name": "sum_best_scores", "score": 4820.1},
    {"name": "required_penalty", "score": -361.5}
  ],
  "visibility": "hidden",
  "task_id": "可选",
  "submission_id": "可选"
}
```

| 字段 | 含义 |
|---|---|
| `schema` | 固定 `2` |
| `status` | `scored`：分数说明了 agent 的水平，计入汇总（agent 没交出可用产出、构建失败、应用起不来也是 `scored`，分数按打分器规则，通常为 0）。`error`：打分基础设施的问题（镜像拉不到、测试浏览器起不来、跑完没有报告），不计分，可重试 |
| `error` | `status: error` 时的类别：`system`（打分器自身问题）或 `rejected`（请求本身无效，留给调用方用） |
| `score` | 连续分数，有限数，可为负，绝对值 ≤ 1e12。`scored` 时必填，除非给了 `items` |
| `max` | 可选，本阶段满分 |
| `passed` | 可选，是否"通过"（全部用例通过、达到阈值等），只用于展示 |
| `detail` | 打分器自己的固定文案，≤ 300 字符，不含测试内容 |
| `items` | 可选明细，≤ 100 项，每项 `{"name", "score"?, "max"?, "passed"?}`，`name` ≤ 100 字符 |

按用例计数是其中的特例：`items` 每项只给 `passed` 时，每项按 1 / 0 分、满分 1 计；没给 `score` 时由 `crucible score` 按题目包的 `aggregate.items`（缺省求和）算出，于是 `score` = 通过数，`max` = 用例数。

`crucible score` 对每个结果做同样的收尾（`ScoreResult::finish` 与 `normalise`）：没有分数或越界 → `error`（system）；题目包给了 `expected_total` 的阶段，`scored` 但没有用例结果时记 `score = 0`、`max = expected_total`，用例数与 `expected_total` 不符时改为 `error`。它写出的 `score.json` 总是 v2。

旧格式（`status: passed | failed | system_error | rejected` 加整数 `passed / total`、`tests[]`）仍被接受，按固定表换算，Playwright 打分器目前就输出旧格式：

| 旧 `status` | 新 `status` | `score` | `max` | `passed` |
|---|---|---|---|---|
| `passed` | `scored` | 旧 `passed` | 旧 `total` | `true` |
| `failed` | `scored` | 旧 `passed` | 旧 `total` | `false` |
| `system_error` | `error`（`system`） | — | — | — |
| `rejected` | `error`（`rejected`） | — | — | — |

旧 `tests[]` 换算成 `items`：`{"name": title, "passed": ok}`。同一张表也用于读取旧 manifest（crucible-core、Worker、网页一致），旧数据不改写。

`visibility`：

- `public`：可以带完整明细（Playwright 的 `tests`：用例标题、错误文本、截图）。
- `hidden`（正式评测一律如此）：`items` 只能保留名字由打分器代码固定的项（例如巡天的五个分项）；名字或内容来自测试材料的项（例如 Playwright 的用例标题）必须去掉。`detail` 只会是打分器自己的固定文案（如 `3/30 tests failed`、`app build failed`）。

`screenshot` 是相对 `--artifacts` 目录的路径；没给 `--artifacts` 时该路径不可取用。

## 5. 隔离与清理（所有打分器）

- 产出的构建和运行都不联网（`--network=none` 构建、`--internal` 网络运行），有内存/CPU/进程数上限。
- 被测应用与测试运行在不同容器里；应用看不到测试材料，测试只通过网络访问应用。
- 打分结束（包括被中断）后删除本次创建的所有容器、网络、应用镜像和临时目录。容器、网络名带每次运行唯一的后缀，多个打分可以共用一个 docker daemon。
- 打分器不调用模型、不读任何密钥。
- 测试容器（跑测试材料的那个）：只有 `BASE_URL`、`READY_TIMEOUT`、`CHROMIUM_SANDBOX=0` 三个环境变量；非 root、`--cap-drop ALL`、`no-new-privileges`、Docker 默认 seccomp；只在 `--internal` 网络上。设 `CRUCIBLE_SCORER_FIREWALL=1`（CI 上）时，再用 iptables 丢弃这个网络发往宿主机的一切流量（INPUT，v4 与 v6）以及离开这个网络的流量（DOCKER-USER），因此只能访问被测应用；规则装不上即 `system_error`。

## 6. Playwright 打分器（`scorers/playwright`）

第一个实现，从原型评分器（BH3GEI/arcbench-grader `main` 的 `scripts/grade.sh` + `runner/` + `common/`）移植。与旧自测站分数一致靠的是**锁定同样的参数**（下表），测试文件本身的一致性由题目包的块哈希保证。下表是固定值；`score.sh` 的超时参数只用于打分器自己的测试，正式评测（`crucible score`）不传，按默认值运行。

### 6.1 锁定的参数及来源

| 参数 | 值 | 本仓库位置 | 旧评分器来源 |
|---|---|---|---|
| 基础镜像 | `node:24-bookworm-slim@sha256:0e0ff40c…f9b6`（多架构 index digest） | `image/Dockerfile` | `runner/Dockerfile`（同一 tag，旧版未钉 digest） |
| Playwright | `@playwright/test@1.57.0` | `image/Dockerfile` | `runner/Dockerfile` `ARG PLAYWRIGHT_VERSION=1.57.0` |
| 浏览器 | Chromium，随 Playwright 1.57.0 固定（Chromium 143.0.7499.4 / headless shell build v1200），`playwright install --with-deps chromium` | `image/Dockerfile` | `runner/Dockerfile` |
| retries | 0 | `image/playwright.config.js` | `runner/playwright.config.js` |
| workers | 2 | `image/playwright.config.js` | `runner/playwright.config.js` |
| 单用例超时 | 60 s | `image/playwright.config.js` | `runner/playwright.config.js` |
| 失败截图 | `screenshot: 'only-on-failure'`，headless | `image/playwright.config.js` | `runner/playwright.config.js` |
| 整体测试超时 | 900 s（含就绪等待） | `score.sh` `RUN_TIMEOUT_S` | `common/taskspec.py` `run_timeout_s` 默认值 |
| 构建超时 | 600 s，超时即杀 | `score.sh` `BUILD_TIMEOUT_S` | `common/taskspec.py` `build_timeout_s` |
| 就绪超时 | 60 s，每 0.5 s 轮询 `GET /`，HTTP 2xx 即就绪 | `score.sh` + `image/wait-ready.mjs` | `taskspec.py` `ready_timeout_s` + `runner/wait-ready.mjs` |
| 应用端口 | 3000（`PORT` 环境变量） | `score.sh` `APP_PORT` | `taskspec.py` `app_port` |
| 构建隔离 | `docker build --network=none`，2 GB 内存、2 CPU | `score.sh` 第 2 步 | `scripts/grade.sh` 第 3 步 |
| 应用容器 | `--internal` 网络，512 MB（无 swap）、1 CPU、256 pids，`/tmp` 64 MB noexec，no-new-privileges，丢弃 NET_RAW 等 5 项 capability | `score.sh` 第 3 步 | `scripts/grade.sh` 第 4 步 |
| 测试容器 | 2 GB、2 CPU、1024 pids、shm 1 GB | `score.sh` 第 4 步 | `scripts/grade.sh` 第 5 步 |
| zip 检查 | 拒绝绝对路径、`..`、符号链接；≤2000 个文件；解压后 ≤50 MB；不恢复权限位 | `image/src/zip.ts` | `common/zipsafety.py`（+ Python `zipfile.extractall`） |
| 用例展开与判定 | 每个 spec 一条；所有运行结果为 passed/expected 且 spec.ok 才算通过；错误文本去 ANSI、≤2000 字符 | `image/src/report.ts` | `common/resultshape.py` |
| status / hidden 过滤 | 见第 4 节 | `image/src/report.ts`、`score.sh` | `scripts/parse_report.py`、`scripts/grade.sh` |
| 重试 | 只在测试容器没产出报告就退出时重试 1 次 | `score.sh` | `scripts/grade.sh` |

与旧评分器的已知差异（不影响打分）：镜像里把 `install --with-deps` 拆成两层以降低构建时的磁盘峰值；测试容器以调用者 uid 运行（便于清理）、直接调用全局 `playwright` 而不是 `npx playwright`；没有 buildx 时退回 legacy builder；测试容器 `--cap-drop ALL`、用 Docker 默认 seccomp、关闭 Chromium 自带沙箱（测试本身可能是不可信代码，容器才是边界，见 §7）。

### 6.2 计分

每个阶段的分数固定为 **passed / total**（打分器仍输出旧格式，`crucible score` 换算成 `score` = 通过数、`max` = 用例数）：`total` 是 Playwright 报告里收集到的用例数，`passed` 是通过数。构建失败、应用没就绪、测试整体超时等情况下没有用例结果，`passed = total = 0`，该阶段记 0 分（`max` 取题目包的 `expected_total`）。`system_error` 不计分，应重试。

环境变量：`CRUCIBLE_SCORER_IMAGE`（用预构建镜像，默认现场构建 `image/`）、`CRUCIBLE_SCORER_FIREWALL=1`（见 §5）、`CRUCIBLE_PRUNE_BUILD_CACHE=1`（一次性 CI 机器上打完清 build cache；默认关，因为会清掉整个 daemon 的缓存）。

注意：`TMPDIR` 必须是 docker daemon 能挂载的路径（例如 colima / Docker Desktop 默认只共享用户主目录时，要把 `TMPDIR` 设到主目录下）。

## 7. 打分隔离边界（不可信的测试）

用户可以上传题目包，测试材料因此和 agent 产出一样是**不可信代码**。边界靠 job 划分保证，不靠测试"老实"：

| job | 持有 | 运行 |
|---|---|---|
| `score`（eval.yml / score.yml） | 平台私钥 `CRUCIBLE_AGE_KEY`、Worker 令牌 | 只跑平台自己的 `crucible score-handoff`：解开所选阶段的 tests 块和各副本的 checkpoint，**用一把本次新生成的一次性钥匙重新封好**，连同 `taskset.json`、`crucible`、插件目录（`scorers/`）作为 artifact `score-handoff`（保留 1 天）交出；一次性钥匙作为 job output 交出。不跑任何测试或 agent 代码。 |
| `score-tests`（`score-tests.yml`，`workflow_call`） | 只有一次性钥匙（以 workflow_call secret 传入，GitHub 自动打码）；`permissions: {}`；不 checkout | `crucible score --tests-dir`：用一次性钥匙解开 handoff，调用打分器（打分器进程的环境里删掉了这把钥匙）。只输出 `score.json`（artifact `scores`）。 |
| `publish` | 平台私钥、写仓库权限 | 收尾：读 `scores`，生成清单、回传 Worker。 |

要点：
- 跑测试的机器上没有平台私钥、Worker 令牌、模型 key，`GITHUB_TOKEN` 没有任何权限。一次性钥匙只能打开本次 handoff 里的东西，而这些东西本来就要交给这台机器。
- handoff 是密文：仓库是公开的，公开仓库的 Actions artifact 任何登录用户都能下载，所以绝不能把明文测试或产出放进 artifact。
- 一次性钥匙经 job output 传递。GitHub 不在网页或 API 里展示 job output；它只在 `score-tests` 里以 secret 的身份出现，日志中打码。
- 已知残余风险：从测试或应用容器逃逸的代码可以伪造本次 `score.json`（它能写 `scores` artifact）。这只影响它自己参与的这次分数：题目包作者本来就决定测试怎么判，agent 产出逃逸则与以前相同。

验证：`scorers/playwright/test/fixtures/hostile/` 是一份"恶意"测试包（读环境变量里的密钥、看自身权限、连外网和 DNS、连宿主机网关的常见端口、连云元数据服务），每个用例只有在尝试**被挡住**时才通过，最后一个用例确认被测应用照常可访问。`scorer.yml` 的 e2e 在 `CRUCIBLE_SCORER_FIREWALL=1` 下要求它 5/5。
