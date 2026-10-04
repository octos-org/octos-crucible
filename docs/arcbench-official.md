# ARC-Bench 官方格式打分器（`arcbench-official`）

给按 ARC-Bench 官方模板交付的产出打分：zip 根目录是 `frontend/` + `backend/`，没有 Dockerfile，由评测方负责 `npm install`、`npm run build`、启动 backend。现有的 `scorers/playwright` 要求根目录有 Dockerfile 且断网构建，这类产出在那里一律 0 分。

- 打分器：`scorers/arcbench-official/`
- 题目包：`tasksets/arcbench-github-official/`。与 `arcbench-github` 引用**同一组** inputs / tests 块（`taskset.json` 的 `stages` 逐字节相同），只把打分器换成 `arcbench-official`（`version: "1"`）
- 平台核心（crates/、workflows、web/、Worker、现有打分器）没有改动。`score-tests` job 照常构建 `scorers/arcbench-official/image` 并调用 `score.sh`，约定见 `docs/scorer-contract.md`

## 1. 一致性怎样保证

**直接用官方 runner 镜像和其中的官方代码**，不重写构建、启动、测试、判分逻辑：

- 镜像：`gyataro/arcbench-runner@sha256:40e003ed470dbd4c120b9019876ba77303d38dc8b34be7f6e313fe0563dd14de`（tag `local-base`，linux/amd64），只在上面加两个文件（`image/Dockerfile`）。
- `image/official.py` 用与镜像自带 `/opt/arcbench/local_runner.py` 相同的方式加载 `/opt/arcbench/run_submission.py`，然后调用它自己的函数：
  - `build`：`run_web_template()`，在它要启动服务时停下（只做 frontend `npm install` + `npm run build`、backend `npm install`）；
  - `serve`：再次 `run_web_template()`，把已经做完的 `install_node_dependencies` / `run_command`（即 `npm run build`）换成空操作，于是由官方代码启动 `npm run start` 并用官方 `wait_for_http_ready(url, 120)` 等待就绪；
  - `test`：官方 `main()` 的收尾原样调用：`write_playwright_config(WEB_APP_BASE_URL)`、`ensure_test_package()`、`run_playwright_tests_with_progress()`、`parse_playwright_results()`。
- 分数 = 官方 `parse_playwright_results()` 的 `passed`，满分 = `passed + failed`（官方的 total）。

官方流程在一个容器里依次做"生成 → 构建 → 启动 → 测试"。这里去掉生成（产出已给定），其余拆进三个容器，原因见第 3 节。

## 2. 锁定的参数及来源

全部来自镜像内 `/opt/arcbench/run_submission.py`（下称 RS）及镜像配置，本仓库不另设：

| 参数 | 值 | 来源 |
|---|---|---|
| Node / npm / Python | v20.19.3 / 10.8.2 / 3.12.3 | 镜像 |
| 依赖安装 | 先删 `node_modules`，再 `npm install --include=optional --no-audit --no-fund`（frontend、backend 各一次） | RS `install_node_dependencies`、`remove_node_modules` |
| npm 源 | `/root/.npmrc`：`registry=https://repo.huaweicloud.com/repository/npm/`；RS 删掉环境变量 `NPM_CONFIG_REGISTRY`，设 `NPM_CONFIG_REPLACE_REGISTRY_HOST=npmjs`（lockfile 里的 registry.npmjs.org 地址改走上述源） | 镜像、RS `build_npm_environment` |
| 构建 | frontend 目录 `npm run build` | RS `run_web_template` |
| 启动 | backend 目录 `npm run start`，环境 `HOST=0.0.0.0`、`PORT=3000` | RS `run_web_template` |
| 就绪 | 轮询 `GET http://127.0.0.1:3000`，单次 2 s，状态码 < 500 即就绪，间隔 1 s，共 120 s | RS `wait_for_http_ready`、`WEB_APP_BASE_URL` |
| Playwright | `@playwright/test` 1.57.0（镜像预装于 `/opt/arcbench/node_modules`，测试目录里软链过去），Chromium build 1200（`/ms-playwright`） | 镜像、RS `ensure_test_package` |
| 配置 | `testDir: '.'`，单用例 `timeout: 10000`，`expect.timeout: 10000`，`fullyParallel: false`，`workers: 1`（命令行也是 `--workers=1`），retries 未设（= 0），`channel: 'chromium'`，trace/screenshot 关，json reporter | RS `write_playwright_config`、`PLAYWRIGHT_*` 常量 |
| 测试环境变量 | `E2E_BASE_URL` / `PLAYWRIGHT_BASE_URL` / `ARC_WEB_BASE_URL` = `http://127.0.0.1:3000` | RS `playwright_environment` |
| 用例判定 | 任一结果 failed / timedOut / interrupted 即失败；有 passed 即通过；仅 skipped 算失败 | RS `parse_playwright_results` |
| 构建是否联网 | 联网（npm 源） | RS 无断网处理；镜像配置了国内镜像源 |

本打分器自己加的外层上限（官方没有对应值，只为防止卡死）：构建 1200 s、就绪等待 180 s（官方 120 s 判定在容器内照常生效）、测试整体 1800 s。超限记 0 分。

## 3. 隔离：与官方不同的地方及影响

| 项 | 官方 | 这里 | 对分数的影响 |
|---|---|---|---|
| 容器划分 | 构建、应用、测试同一容器（root） | `build`、`serve`、`test` 三个容器；产出放在每次新建的 docker volume 里 | 无：同一份官方代码、同样的地址 `127.0.0.1:3000`（`test` 容器加入 `serve` 容器的网络命名空间）。应用看不到测试材料 |
| 构建联网 | 不限 | 构建容器只在 `--internal` 网络上，唯一出口是 HTTPS CONNECT 代理（`image/egress_proxy.py`），只放行 443 端口的：npm 源 `repo.huaweicloud.com`、`registry.npmjs.org`、`registry.npmmirror.com`（三者都出现在选手 lockfile 里），以及原生模块需要的 `github.com`、`objects.githubusercontent.com`、`release-assets.githubusercontent.com`（sqlite3 5.x / bcrypt 5.x 的预编译包）和 `nodejs.org`（node-gyp 回退编译要的 Node 头文件）。代理拒绝的请求记在日志里，构建失败时 detail 会注明 | 只要依赖都来自这些地址就无影响。实测：只放行 npm 源时，带 sqlite3 5.1.7 的产出 `npm install` 失败（被拒的正是 github.com、nodejs.org），放行后通过 |
| 运行联网 | 不限 | `serve` 容器 `--network none`（只有回环），测试容器共用它 | 前 50 名所有产出的 frontend/backend 源码里没有引用外部地址（脚本、样式、fetch），不影响 |
| 测试进程身份 | root | 调用者 uid（root 调用时用 1000），`--cap-drop ALL` | Playwright 默认不开 Chromium 沙箱，root / 非 root 行为一致 |
| 资源 | 官方生产环境未公开 | 构建 4 GB / 2 CPU；应用 2 GB / 1 CPU / 512 pids；测试 2 GB / 2 CPU / shm 1 GB | 测试单用例 10 s 超时，对机器快慢敏感；不一致时优先怀疑这里 |
| 环境自检 | `run_environment_preflight`（Chromium 能否启动） | 不调用（与产出无关；Chromium 起不来会表现为测试容器报错，记 `system_error`） | 无 |
| 测试目录 | specs 直接在 `/workspace/tests` | 题目包的 tests 块带一层 `tests/` 目录，复制时去掉这一层，放到 `/workspace/tests` | 无 |

与隔离约定（`docs/scorer-contract.md` §5、§7）的关系：打分器仍在不持有任何密钥的 `score-tests` job 里运行；测试容器没有任何对外网络（比 Playwright 打分器的 `--internal` 网络更严）。唯一放宽的是**构建容器**能经代理访问上面 7 个域名的 443 端口：它里面只有本次产出本身（没有测试材料、没有密钥），因此最多能把产出自己发出去。CI 上 `CRUCIBLE_SCORER_FIREWALL=1` 时，构建网络照 Playwright 打分器的做法装 iptables：丢弃发往宿主机和离开该网络的流量，只能经代理出去。

## 4. 状态与计分

| 情况 | `result.json` |
|---|---|
| 测试跑完 | `scored`，`score` = 通过数，`max` = 用例数，`detail` = `N/M tests passed` |
| zip 不合规（绝对路径、`..`、符号链接、> 50000 项、解压后 > 2 GB） | `scored`，0 分 |
| 官方构建步骤失败 | `scored`，0 分，`detail` = `app build failed (<官方步骤名>)`；若代理拒绝过请求，附拒绝次数 |
| 构建时 npm 源连不上（代理记到 `UPSTREAM_FAIL`） | `error`（system），可重试 |
| 120 s 内没就绪 / 进程退出 | `scored`，0 分 |
| 测试容器没产出官方结果 | `error`（system） |

0 分时 `max` 由 `crucible score` 按题目包 `expected_total` 补齐（30 / 29 / 41）。`hidden`（正式评测）不输出用例名。

## 5. 本地运行

```
TMPDIR=$HOME/tmp scorers/arcbench-official/score.sh \
  --artifact app.zip --tests tasksets/hello-world/source/hello-stage-1 \
  --out /tmp/result.json --artifacts /tmp/score-logs
```

需要 docker（能跑 linux/amd64），首次会拉 4.8 GB 的官方镜像。`--artifacts` 下有 `build.log`、`egress.log`（代理放行/拒绝记录）、`app.log`、`test.log`、官方结果 `official.json` 与 Playwright 原始报告。
