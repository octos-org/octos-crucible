# 打分器约定（scorer contract）

适用于所有打分器（Playwright、单元测试、脚本比对、模型评判……）。`crucible score` 只依赖这里写的东西，打分器内部怎么实现不限。

## 1. 交付形式

一个打分器 = **一个容器镜像 + 一个编排脚本**，放在 `scorers/<name>/`：

| 文件 | 作用 |
|---|---|
| `score.sh` | 编排脚本，命令行接口见下；宿主机只需要 bash 和 docker（外加 coreutils `timeout`） |
| `image/` | 打分器镜像的构建目录（`Dockerfile` 等）；版本全部钉死 |

agent 的产出和测试材料**只在容器里**处理；宿主机脚本只做编排（建网络、起容器、限时、清理）。

## 2. 输入

1. **agent 产出**（`--artifact`）：一个文件。格式由题目包声明的产出类型决定：
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

格式即 `crucible-core` 的 `ScoreResult`（`crates/crucible-core/src/score.rs`），与原型评分器的 result.json 相同：

```json
{
  "submission_id": "可选",
  "task_id": "可选",
  "visibility": "public",
  "status": "passed | failed | system_error | rejected",
  "passed": 27,
  "total": 30,
  "detail": "3/30 tests failed",
  "tests": [
    {"title": "…", "ok": false, "error": "…（≤2000 字符）", "screenshot": "output/…/test-failed-1.png"}
  ]
}
```

`status` 的含义：

| status | 含义 | 计分 |
|---|---|---|
| `passed` | 全部用例通过 | 计 |
| `failed` | agent 的问题：产出格式不对、构建失败/超时、应用起不来、用例失败、测试整体超时 | 计（未跑测试时 passed/total 为 0/0） |
| `system_error` | 打分器自身的问题：镜像拉不到、测试浏览器起不来、跑完没有报告 | 不计，可重试 |
| `rejected` | 请求本身不是一次有效提交（留给调用方，例如签名校验失败）；打分器自己不产生 | 不计 |

`visibility`：

- `public`：带 `tests` 明细。
- `hidden`：只保留 `status`、`passed`、`total`、`detail` 和 `submission_id`/`task_id`/`visibility`；不含用例标题、错误文本、截图。`detail` 只会是打分器自己的固定文案（如 `3/30 tests failed`、`app build failed`），不含测试内容。

`screenshot` 是相对 `--artifacts` 目录的路径；没给 `--artifacts` 时该路径不可取用。

## 5. 隔离与清理（所有打分器）

- 产出的构建和运行都不联网（`--network=none` 构建、`--internal` 网络运行），有内存/CPU/进程数上限。
- 被测应用与测试运行在不同容器里；应用看不到测试材料，测试只通过网络访问应用。
- 打分结束（包括被中断）后删除本次创建的所有容器、网络、应用镜像和临时目录。容器、网络名带每次运行唯一的后缀，多个打分可以共用一个 docker daemon。
- 打分器不调用模型、不读任何密钥。

## 6. Playwright 打分器（`scorers/playwright`）

第一个实现，从原型评分器（arcbench-grader 的 `scripts/grade.sh` + `runner/` + `common/`）移植，行为对齐：

| 项 | 值 |
|---|---|
| zip 检查 | 拒绝绝对路径、`..`、符号链接；≤2000 个文件；解压后 ≤50 MB；CRC 校验；不恢复权限位（与 Python `zipfile.extractall` 一致） |
| 构建 | `docker build --network=none`，2 GB / 2 CPU，超时 600 s（`--build-timeout`）；有 buildx 时用 BuildKit |
| 应用容器 | `--internal` 网络，512 MB、1 CPU、256 pids，`/tmp` 64 MB noexec，no-new-privileges |
| 就绪 | 轮询 `GET /`，60 s（`--ready-timeout`） |
| 测试 | Playwright 1.57.0 + Chromium（沙箱开启），retries 0，workers 2，单用例 60 s，整体 900 s（`--run-timeout`）；失败时截图 |
| 重试 | 只在测试容器没产出报告就退出（打分器故障）时重试 1 次 |

环境变量：`CRUCIBLE_SCORER_IMAGE`（用预构建镜像，默认现场构建 `image/`）、`CHROMIUM_SANDBOX=0`（宿主不支持非特权 user namespace 时关闭浏览器沙箱）、`CRUCIBLE_PRUNE_BUILD_CACHE=1`（一次性 CI 机器上打完清 build cache；默认关，因为会清掉整个 daemon 的缓存）。

注意：`TMPDIR` 必须是 docker daemon 能挂载的路径（例如 colima / Docker Desktop 默认只共享用户主目录时，要把 `TMPDIR` 设到主目录下）。
