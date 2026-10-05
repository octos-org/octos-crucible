# 插件接口规范

本文是插件化设计的接口细节，总览与原则见 `docs/DESIGN.md`。P1（§7–§9 的结果格式、汇总与展示）、P2（§3 注册表、§4.1 `workdir` 运行器、§5 打包器）和 P3（§4.2 交互运行器、§10 打分时用模型、§12 巡天迁移）已落地。本文描述的是目标形态，按 §13 的三个阶段（P1–P3）落地；某一部分落地之前，现行行为以 `docs/scorer-contract.md`、`docs/agent-contract.md` 为准，落地时这两份文档同步改写。

## 1. 核心与插件的分工

核心只做六件事：调度（workflow、job 划分、阶段顺序、时限）、隔离与安全（容器、网络、钥匙、一次性钥匙交接）、计量（meter、出网代理、价格）、存储（加密块）、凭据（模型 key、下载密码的收发与删除）、结果汇总（manifest、总分、网页数据）。

与某一种评测相关的事情都由插件和题目包声明：agent 怎样产生产出（运行器）、产出打成什么格式（打包器）、怎样判分（打分器）、分数表示什么以及怎样展示（题目包里的 `display`）、各阶段怎样汇总（题目包里的 `aggregate`）。核心代码里不出现 `web-app`、`playwright`、`astro` 这类名字，只按名字到注册表里查插件。

插件有三种：运行器 runner、打包器 packager、打分器 scorer。展示和汇总不是插件，是题目包里的声明式数据，不含代码。

## 2. 一个阶段怎样走完

每个阶段固定按下面的顺序经过几个插槽，每个插槽填一个插件名：

| 顺序 | 插槽 | 默认 | 在哪个 job 运行 | 何时运行 |
|---|---|---|---|---|
| 1 | `runner`（产出运行器） | `workdir` | `generate` | 只在 agent 模式 |
| 2 | `packager` | 无，必填 | `generate` | 只在 agent 模式 |
| 3 | `interactive`（交互运行器） | 不设 | `score-tests` | 设了才运行，两种模式都运行 |
| 4 | `scorer` | 无，必填 | `score-tests` | 总是运行 |

app 模式（上传产出）跳过 1、2：上传的文件就是"产出"，由所声明打包器的 `check` 检查格式后进入第 3 或第 4 步。

job 的划分沿用打分隔离（`docs/scorer-contract.md` §7）的结构，插件化只是把每个 job 里做的事换成按插件名调用：

| job | 持有 | 做什么 |
|---|---|---|
| `generate`（每遍一个） | 平台私钥（只解开 inputs 块）、提交者模型 key（只在 meter 进程里） | 构建 agent 镜像，按阶段调用产出运行器和打包器，封存产出 |
| `score`（交接） | 平台私钥、Worker 令牌 | 解开所需阶段的隐藏材料和各遍产出，必要时连同模型 key，重新封给一次性钥匙；不运行任何插件 |
| `score-tests` | 只有一次性钥匙，`permissions: {}` | 按阶段调用交互运行器（若有）和打分器，输出 `score.json` 和本 job 的计量日志 |
| `publish` | 平台私钥、写仓库权限 | 汇总 manifest、回传 Worker；不运行任何插件 |

P3 起 `score-tests` 按遍拆成矩阵（每遍一个 job），因为交互运行要占用真实时间，所有遍串行可能超过 GitHub 单个 job 6 小时的上限。

## 3. 插件注册表

注册表是仓库根目录的 `plugins.json`，只有维护者能改（与改代码一样走 PR 评审）。用户自己上传的打分器不进这张表，登记在 Worker 的 D1 里，见 §14。它编译进 `crucible-core`，命令行、Worker、`taskset-pack` job 都只认这张表里的名字。

```json
{
  "schema": 1,
  "plugins": [
    {"kind": "runner",   "name": "workdir",      "version": "1", "impl": "builtin"},
    {"kind": "packager", "name": "web-app",      "version": "1", "impl": "builtin", "user": true},
    {"kind": "packager", "name": "files",        "version": "1", "impl": "builtin", "user": true},
    {"kind": "scorer",   "name": "playwright",   "version": "1", "impl": "scorers/playwright",
     "user": true, "runs_taskset_code": true, "model": false, "accepts": ["web-app"]},
    {"kind": "scorer",   "name": "astro-survey", "version": "2", "impl": "scorers/astro-survey",
     "runs_taskset_code": false, "model": false, "accepts": ["files"]}
  ]
}
```

（节选；完整内容以仓库里的 `plugins.json` 为准。）

| 字段 | 含义 |
|---|---|
| `kind` | `runner` / `packager` / `scorer` |
| `name` | `[a-z0-9][a-z0-9-]{0,39}`，同一 `kind` 内唯一 |
| `version` | 插件的版本号，改变打分行为时必须加一；记入 manifest |
| `impl` | `builtin`（编译进 `crucible` 的 Rust 实现）或仓库内目录 |
| `interactive` | 仅 runner：`true` 表示交互运行器（插槽 3），否则是产出运行器（插槽 1） |
| `user` | 用户上传的题目包能否引用它。缺省 `false`。取代 `crucible-core` 里的常量 `USER_SCORERS`（用户题目包登记时的打分器白名单） |
| `runs_taskset_code` | 它是否把题目包里的文件当代码执行（Playwright 执行测试文件，是 `true`；巡天引擎只读卡片数据，是 `false`）。缺省 `true`（保守） |
| `model` | 它能否被题目包声明为需要模型（§10）。`runs_taskset_code: true` 的插件不得为 `true`，注册表校验时检查 |
| `accepts` | 仅 scorer：它能打分的打包器（例如 Playwright 只收 `web-app`）；缺省不限。题目包登记时检查 |

注册表本身的规则由单元测试保证（名字唯一、打包器只能是 builtin、执行题目包代码的插件不能有模型等），所以编译出的程序里注册表总是合法的。`crucible plugins --taskset FILE` 列出一个题目包用到的容器插件，`score-tests` 据此预先构建镜像（`crucible-<kind>-<name>:run`）。

目录约定：容器形式的插件放在 `runners/<name>/`、`scorers/<name>/`（打分器保持现有位置），目录里有入口脚本和 `image/`（镜像构建目录，版本全部钉死）。打包器只有 `builtin` 一种形式（§5 说明原因）。

题目包引用插件时可以写 `name` 或 `name@version`。不写版本时用注册表当前版本，manifest 记下实际用到的每个插件的名字、版本和镜像摘要，旧评测因此可追溯。

## 4. 运行器 runner

### 4.1 产出运行器 `workdir`（插槽 1）

就是现在 `crucible run` 的行为，P2 把它从 `run.rs` 抽成一个实现了运行器接口的模块，行为不变。

- **位置**：`builtin`（`crates/crucible-cli/src/runners/workdir.rs`）。
- **声明**：题目包 `runner: "workdir"`（缺省即此值）。agent 包格式见 `docs/agent-contract.md`。
- **输入**：agent 镜像、本阶段的 inputs 目录、上一阶段留下的 `/work` 和 `HOME`、核心给的 meter 地址和出网代理地址、时限。
- **输出**：阶段结束时的工作目录（或最后一份快照）、`timing.json`、`agent.log`。打包由下一插槽完成。
- **运行位置**：`generate` job。
- **能拿到**：核心建好的沙箱网络（只通 meter 和白名单软件源）。
- **拿不到**：隐藏材料、模型 key（agent 容器里只有 `OPENAI_API_KEY=dummy`）。

### 4.2 交互运行器（插槽 3）

用于"agent 不是交一份产出就完事，而是要和裁判一问一答"的题目，例如巡天：裁判进程持有真值和时间墙，agent 每一步收到观测状态、回一条指令。

- **位置**：`runners/<name>/`，例如 `runners/astro-v4/`。
- **声明**：题目包 `interactive: {"name": "astro-v4", "time_limit_s": 1200, "options": {...}}`（`options` 可选，经 `--options` 传给插件）。
- **形式**：入口 `run.sh` + `image/`。命令行固定为：

```
run.sh --agent FILE        # 被运行的 agent：上一插槽的产出，或 app 模式上传的文件
       --material DIR      # 本阶段已解密的隐藏材料，只读
       --out DIR           # 运行记录写到这里，交给打分器
       --time-limit S
       [--agent-network NAME --model-base-url URL --model NAME]   # 只在题目包声明需要模型且提交者给了凭据时出现
       [--options FILE]    # 题目包给这个插件的参数（JSON），可选
```

- **两个容器**：裁判容器挂隐藏材料（只读）和 `--out`，`--network none`；agent 容器只挂 agent 文件和管道。两者之间只有一个私有 docker volume 里的两条 FIFO。两个容器都只读根、`--cap-drop ALL`、`no-new-privileges`、有内存/CPU/进程数上限、调用者 uid。
- **agent 容器的网络**：没有模型时 `--network none`；有模型时接到核心建好的 `--agent-network`（`--internal`，唯一可达的地址是宿主机上的 meter），容器里 `OPENAI_BASE_URL` 指向 meter、`OPENAI_API_KEY=dummy`。网络由核心建立并装好防火墙，插件只负责把容器接上去。
- **输出**：`--out` 下至少有 `run.json`：`{"status": "completed" | "agent_failed" | "error", "detail": "…"}`。`agent_failed` 指 agent 没能启动、协议出错、超时等 agent 自身的问题，打分器据此给 0 分或按规则计分；`error` 指运行器自身出错，阶段记为 `error`。其余文件（裁判的计分报告、对话记录）由插件自己约定，只给同一题目包的打分器读。
- **退出码**：同打分器（0 = `run.json` 已写，2 = 调用错误，其他 = 当作 `error`）。
- **运行位置**：`score-tests` job。
- **能拿到**：隐藏材料（只在裁判容器里）、meter 地址（只给 agent 容器）。
- **拿不到**：平台私钥、Worker 令牌、一次性钥匙（核心从插件进程的环境里删掉）、模型 key 本身。

### 4.3 预留

其他运行器（例如 agent 要通过 HTTP 接口和环境交互、或多 agent 对战）按同样的规则加入：先确定它属于插槽 1（不需要隐藏材料，在 `generate` 里）还是插槽 3（需要隐藏材料，在 `score-tests` 里），再写进注册表。

## 5. 打包器 packager

- **位置**：`builtin`，`crates/crucible-cli/src/packagers/<name>.rs`，实现同一个 trait：

```rust
trait Packager {
    /// generate job：把工作目录打成产出文件。只读字节，不执行工作目录里的任何东西。
    fn package(&self, work: &Path, agent: &AgentSpec, opts: &Value) -> Result<(Vec<u8>, ZipStats)>;
    /// app 模式：检查上传的产出是否符合本格式（例如 zip 根目录有 Dockerfile）。
    fn check(&self, artifact: &[u8], opts: &Value) -> Result<()>;
}
```

- **声明**：题目包 `packager: "web-app"`，可带 `packager_options`。
- **现有两种**：`web-app`（根目录带 Dockerfile 的 zip；没有 Dockerfile 时按 ARC-Bench 约定打 `frontend/` + `backend/` 并补 Dockerfile，`app_start_cmd` 来自 agent.json）、`files`（整个工作目录原样打包；可选 `packager_options.require` 列出根目录必须存在的文件，app 模式的 `check` 和 agent 模式打包时都检查）。除新增的 `require` 外，规则与现在 `package.rs` 相同，只是从 `OutputKind` 的分支变成两个注册的实现。
- **运行位置**：`generate` job 的宿主机进程（agent 容器已停止之后）；app 模式下 `check` 在 `score-tests` 里打分之前运行。
- **能拿到**：工作目录（不可信的字节）、agent.json。
- **拿不到**：隐藏材料、模型 key。
- **为什么只能是 builtin**：打包在持有平台私钥的机器上直接处理不可信字节，所以只接受编译进 `crucible`、经过评审的 Rust 代码（不跟随符号链接、限制文件数和总大小，见 `zipdir.rs`）。如果将来需要无法用 Rust 写的打包方式，作为插槽 1 运行器的一部分在容器里完成，而不是在宿主机上运行脚本。

核心不再有 `OutputKind` 枚举：`Stage.output` 改为 `Stage.packager: String`。旧题目包的 `"output": "web-app"` / `"files"` 读作同名打包器。

## 6. 打分器 scorer

接口沿用 `docs/scorer-contract.md`（一个镜像 + 一个 `score.sh`，命令行参数含义不变），改动三处：

1. 输出改为 §7 的结果格式（旧格式仍被接受，自动换算）。
2. 新增可选参数：`--run DIR`（本阶段有交互运行器时，它的 `--out` 目录）、`--options FILE`（题目包给打分器的参数）、`--model-base-url URL --model NAME`（只在注册表 `model: true` 且题目包声明需要时出现，用于模型评判）。
3. 有交互运行器时，`--artifact` 仍是 agent 文件，打分器通常只读 `--run`。

- **运行位置**：`score-tests` job。
- **能拿到**：产出、隐藏材料、交互运行记录；声明了模型时可访问 meter。
- **拿不到**：平台私钥、Worker 令牌、一次性钥匙、模型 key 本身。
- **visibility**：正式评测一律 `--visibility hidden`。hidden 时 `items` 只能保留名字由打分器代码固定的项（例如巡天的五个分项），名字或内容来自测试材料的项（例如 Playwright 的用例标题）必须去掉。

## 7. 结果格式（`result.json` v2）

替代只有整数 `passed / total` 的 `ScoreResult`。

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
| `schema` | 固定 `2`。没有这个字段的按旧格式读（见下） |
| `status` | `scored`：分数说明了 agent 的水平，计入汇总（agent 没交出可用产出也是 `scored`，分数按打分器规则，通常为 0）。`error`：打分基础设施出了问题，不计分，可重试 |
| `error` | `status: error` 时的类别：`system`（打分器自身问题，可重试）或 `rejected`（这次请求本身无效，留给调用方用） |
| `score` | 连续分数，有限的 f64，可为负，绝对值 ≤ 1e12。`status: scored` 时必填，除非给了 `items` 让核心按题目包规则算出 |
| `max` | 可选，本阶段的满分；`ratio` 汇总（§8）要求每个阶段都有 `max > 0` |
| `passed` | 可选，本阶段是否"通过"（全部用例通过、达到阈值等），只用于展示 |
| `detail` | 打分器自己的固定文案，≤ 300 字符，不含测试内容 |
| `items` | 可选明细，≤ 100 项。每项 `{"name", "score"?, "max"?, "passed"?}`，`name` ≤ 100 字符 |

`passed / total` 是其中一种打分方式：`items` 每项只给 `passed`（布尔）而不给 `score` 时，每项按 1 / 0 分、满分 1 计；阶段的 `score` 和 `max` 没给时按题目包的阶段内汇总（§8，缺省求和）算出，于是 `score` = 通过数，`max` = 用例数。

`expected_total` 保留原意：只对按用例计数的打分器有效。`scored` 但没有任何用例结果（构建失败、应用没起来）时 `score = 0`、`max = expected_total`；用例数与 `expected_total` 不符时改为 `error`（system），与现在 `normalise` 的规则相同。

### 7.1 旧数据的兼容读取

读取方（`crucible-core`、Worker、网页）都按同一张表把旧格式换算成新格式。旧数据不改写：数据分支上的 `evals/*.json`、加密存档里的 manifest、Worker KV 里的记录都保持原样，读取时换算。

| 旧 `status` | 新 `status` | `score` | `max` | `passed` |
|---|---|---|---|---|
| `passed` | `scored` | 旧 `passed` | 旧 `total` | `true` |
| `failed` | `scored` | 旧 `passed` | 旧 `total` | `false` |
| `system_error` | `error`（`system`） | — | — | — |
| `rejected` | `error`（`rejected`） | — | — | — |

旧的 `tests[]` 换算成 `items`：`{"name": title, "passed": ok}`。manifest 里的阶段分数 `{status, passed, total}` 用同一张表换算。旧 manifest 没有 §8、§9 所说的 `scoring` 快照，读取时按 `ratio` 汇总、缺省展示处理，因此旧评测的总分数值和现在显示的完全一样。

写入方（打分器、`crucible score`、`publish`）在 P1 之后只写新格式；Playwright 打分器可以继续输出旧格式，由 `crucible score` 换算后再写 `score.json`。

## 8. 汇总 `aggregate`

题目包声明阶段内与阶段间怎样汇总，manifest 的 `total_score` 用它计算。只能选下面的方式和权重，不能写代码。

```json
"aggregate": {
  "items":  "sum",
  "stages": "weighted",
  "weights": {"l1": 1, "l2": 1, "l3": 2, "l4": 2}
}
```

| 键 | 取值 | 含义 |
|---|---|---|
| `items` | `sum`（缺省）/ `mean` / `weighted` | 打分器没给阶段 `score`、只给 `items` 时，用 items 算出阶段分；`weighted` 时用 `item_weights`（按 item 名） |
| `stages` | `ratio` / `sum` / `mean` / `weighted` | 阶段间。`ratio`：Σscore / Σmax（0–1）；`sum`：Σscore；`mean`：阶段分的平均；`weighted`：Σ wᵢ·scoreᵢ，`weights` 按阶段 id 给出，没列出的阶段权重为 0 |
| `normalize` | `true` / `false`（缺省） | 对 `mean`、`weighted`：先把每阶段换成 score / max 再汇总 |

多遍：`ratio` 保持现在的算法，对所有遍、所有已计分阶段一起求 Σscore / Σmax；其余方式先算每一遍的总分，再对遍取平均（网页另显示标准差和范围）。某一遍有阶段是 `error` 时，这一遍不计入总分（`ratio` 除外，与现在一致）。只跑了前 N 个阶段的开发评测，按实际跑了的阶段汇总。

兼容：taskset `schema: 1` 里的 `"aggregate": "sum"` 意思是 Σpassed / Σtotal，读作 `{"stages": "ratio"}`。新题目包用 `schema: 2` 和对象形式。

## 9. 展示 `display`

题目包声明分数的含义，网页的评测详情、列表和对比视图都按它格式化。纯数据，不含代码。

```json
"display": {
  "stage": {"name": "巡天得分", "unit": "分", "direction": "higher", "decimals": 1},
  "total": {"name": "总分", "unit": "分", "direction": "higher", "decimals": 1, "min": 0, "max": 40000}
}
```

| 字段 | 含义 |
|---|---|
| `name` | 显示名，≤ 40 字符 |
| `unit` | 单位，≤ 10 字符，可空 |
| `direction` | `higher`（越高越好，缺省）/ `lower`（越低越好）。对比视图据此给差值标"更好 / 更差"的颜色 |
| `min` / `max` | 可选，画进度条和图表的范围；不给时阶段用 `max`（若有），否则不画条 |
| `decimals` | 小数位，0–6，缺省 2 |
| `format` | `number`（缺省）/ `percent`（值乘 100 加 %）/ `fraction`（显示 `score/max`，用于按用例计数） |

`stage` 和 `total` 都可省。缺省时：阶段分有 `max` 且来自 0/1 用例时显示 `通过数/用例数`，否则按 `number`；`ratio` 汇总的总分显示为百分比。各阶段可以在自己的 `display` 里覆盖题目包级的 `stage`。

文本字段来自用户上传的题目包，网页只当纯文本渲染；登记时校验长度、不允许控制字符。

`publish` 把本次评测用到的 `aggregate`、`display` 和插件版本作为 `scoring` 快照写进 manifest，网页和 Worker 用快照展示。题目包以后改了展示方式，旧评测的显示不受影响。

## 10. 打分 / 交互时使用大模型

有的题目在 `score-tests` 里也需要模型：交互运行中的 agent 本身调模型（巡天的观测 agent），或打分器用模型评判。题目包声明：

```json
"model": {
  "interactive": "optional",
  "scorer": "none",
  "name": null,
  "max_requests": 2000
}
```

| 键 | 取值 | 含义 |
|---|---|---|
| `interactive` / `scorer` | `none`（缺省）/ `optional` / `required` | 该插槽是否使用模型。`required` 时 app 模式提交必须带凭据；`optional` 时没带凭据就没有 meter，`--model-base-url` 不出现 |
| `name` | 模型名或 `null` | `null`：用提交者填的模型。写了名字：固定用这个模型（评判类题目需要统一评判模型），提交者的接口必须提供它 |
| `max_requests` / `max_tokens` | 可选 | 本阶段 `score-tests` 内模型调用的上限，与提交者自设的整次评测预算同时生效 |

规则：

- 只能给注册表里 `model: true` 的插件声明模型；`runs_taskset_code: true` 的插件（会执行题目包文件的，如 Playwright）永远拿不到模型。这由注册表校验和题目包登记校验保证。
- 用的总是提交者自己的 key 和接口，平台不提供模型。
- 计量与记账：`score-tests` 里每个用到模型的插槽各起一个 meter（`crucible score` 进程内，监听沙箱网络宿主机一侧 `172.31.250.1:8787`，网络由 `tools/sandbox-net.sh` 建立，`SANDBOX_PORTS=8787` 只放行这一个端口），各写一份 `<遍>/<阶段>/eval_usage/{interactive,scorer}.jsonl`，随 `score.json` 一起交给 `publish`。manifest 的阶段条目在现有 `usage`（产出阶段）之外加 `eval_usage: {"interactive": {usage, cost_usd}, "scorer": {usage, cost_usd}}`。网页分开展示，不并入 agent 产出阶段的 token 和花销，对比视图也分开比较。三处用量都计入提交者设的预算。

凭据怎样到 `score-tests`：`score`（交接）job 判断题目包声明了打分用模型且有凭据时，`crucible cred open … | crucible score-handoff --cred-stdin --model M --budget B`，凭据只经管道进 `score-handoff`，被封给一次性钥匙写成 handoff 里的 `cred.sealed`，同时写 `model.json`（模型名、每遍剩余预算 = 整次预算 − 该遍产出阶段用量）。`score-tests` 里 `crucible score --cred cred.sealed --model-plan model.json` 用一次性钥匙在自己进程里解开。预算按遍累计：产出阶段、交互运行、模型评判三处用量共用提交者设的上限；题目包的 `max_requests` / `max_tokens` 另按阶段限制。app 模式（`score.yml`）的 `cred_source` 可为 `workers-kv`（网页提交）或 `github-secret`（开发），`model` 是新增的输入。

### 10.1 与打分隔离的关系

打分隔离的约束是：运行不可信代码的 job 不持有任何平台密钥。加入模型后这一点不变，变化只在于 `score-tests` 在需要时多持有提交者自己的模型 key：

| job | 平台私钥 / Worker 令牌 | 一次性钥匙 | 提交者模型 key |
|---|---|---|---|
| `generate` | 私钥有（现有残余风险，见 DESIGN §8） | 无 | 有，只在 meter 进程 |
| `score`（交接） | 有 | 生成 | 阶段声明需要模型时：从 Worker 取出解开，与隐藏材料一起重新封给一次性钥匙 |
| `score-tests` | 无 | 有 | 阶段声明需要模型时有，只在宿主机 meter 进程；不进环境变量、不进容器、不进日志 |
| `publish` | 有 | 无 | 无，并负责删除 Worker 中的凭据 |

为什么可以接受：模型 key 只在 meter 里，容器只能拿到 meter 地址和 `dummy`；meter 只转发到提交者填的 https 接口、只放行声明的模型。能在这台机器上运行的不可信代码只有两类：提交者自己的 agent（它本来就用这把 key），以及不执行题目包代码的官方插件处理的题目数据。会执行第三方题目包代码的插件拿不到模型，因此第三方题目包作者无法借逃逸拿到别人的 key。剩下的风险是提交者自己的 agent 从容器逃逸后拿到提交者自己的 key，与 `generate` job 现状相同。模型评判还有提示注入的问题（产出里写"给满分"），这属于评判插件自己的设计（固定评判提示、只让模型输出受限格式），不由核心处理。

## 11. 权限与信任分级

| 东西 | 谁能加入 | 形式 | 能接触什么 |
|---|---|---|---|
| 运行器、打包器、打分器 | 只有维护者（PR 评审后合入 `main`，登记在 `plugins.json`） | 仓库里的代码和钉死版本的镜像 | 打分器和交互运行器在 `score-tests` 里接触解密后的隐藏材料；打包器在 `generate` 里处理工作目录 |
| 用户上传的打分器 | 任何登录用户（§14），登记为 `u-<16 hex>`，默认只有上传者的题目包能用，管理员可设为公开 | `plugin.json` + `Dockerfile` 的插件包，镜像在没有平台密钥的机器上构建 | 只在 `score-tests` 的容器里接触本阶段的产出和隐藏材料；声明了模型时只经计量代理 |
| 题目包 | 内置的由维护者加入（`tasksets/<name>/`）；用户上传的经 `taskset-pack` 登记为 `u-<16 hex>` | 数据：`source.json` + 各阶段文件 | 只能引用注册表里的插件，用户题目包只能引用 `user: true` 的插件，或自己能用的上传插件（§14） |
| agent | 任何登录用户 | `agent.json` + `Dockerfile`，或运行器约定的格式（如巡天的 observer 项目 zip） | 只在容器里运行，看不到隐藏材料，模型只经 meter |

题目包里的文件对核心来说永远是数据。它们是否会被当成代码执行，取决于所引用插件的 `runs_taskset_code`；凡是 `true` 的插件，只在 `score-tests` 的容器里执行这些文件，且那台机器上没有平台密钥，也没有模型 key。

登记检查（内置题目包在 CI 里，用户题目包在 `taskset-pack` 里）：引用的插件都在注册表里且 `kind` 对得上；用户题目包只用 `user: true` 的；`model` 只声明给 `model: true` 的插件；`display` 文本合规；`aggregate` 的权重只引用存在的阶段；总时长不超过平台上限（现在是产出阶段时限之和；加上交互运行时限后，按 job 分别检查）。

## 12. 样例：巡天迁移

现状（`docs/astro-survey.md`）：打分器 `astro-survey` 在 `score.sh` 里同时起 agent 容器和引擎容器，让它们通过 FIFO 跑完一局，再把连续分换算成"毫分 / 10,000,000"塞进 `passed / total`；打分时没有模型，示例 agent 退回规则路径。

迁移后的对应关系：

| 现在 | 迁移后 |
|---|---|
| `scorers/astro-survey/score.sh` 的第 1–3 步（建 FIFO、agent 容器、引擎容器） | 交互运行器 `runners/astro-v4/run.sh`，容器参数不变；`agent_entry.py`、`bridge.py`、`engine/`、`ENGINE_MANIFEST.json` 校验一起搬过去 |
| agent 容器 `--network none`，没有模型 | 题目包声明 `model.interactive: "optional"`：有凭据时 agent 容器接到只通 meter 的内部网络，观测 agent 能真正调模型；没有凭据时与现在相同 |
| 打分器读引擎输出并换算 | `scorers/astro-survey` 只读 `--run` 目录里的 `score_report`，输出 `score` = 引擎原始分，`items` = 五个分项，`passed` 不填 |
| 毫分 / 10,000,000、`expected_total: 10000000` | 去掉 `expected_total`；`display.stage = {"name": "巡天得分", "unit": "分", "decimals": 1}` |
| `aggregate: "sum"`（实为比例） | `aggregate: {"stages": "sum"}`，总分是四张卡分数之和 |
| app 模式：上传 observer 项目 zip，`output: files` | 不变：`packager: "files"` 的 `check` 检查 zip 安全性，以及 `packager_options.require` 列出的 `observer.project.json` 在根目录，然后进入交互运行器 |
| agent 模式：可行但未实现 | 插槽 1 `workdir` 让 coding agent 读卡片说明写出 observer 项目，插槽 2 `files` 打包，插槽 3 运行它，插槽 4 计分 |
| 运行发生在 `score-tests` 里的打分器内部 | 运行发生在 `score-tests` 里的交互运行器内，时限由 `interactive.time_limit_s` 声明；`score-tests` 按遍拆成矩阵 |

迁移后的 `tasksets/astro-practice/source.json` 片段：

```json
{
  "schema": 2,
  "name": "astro-practice",
  "runner": "workdir",
  "packager": "files",
  "packager_options": {"require": ["observer.project.json"]},
  "interactive": {"name": "astro-v4", "time_limit_s": 1200},
  "scorer": {"name": "astro-survey"},
  "model": {"interactive": "optional"},
  "aggregate": {"stages": "sum"},
  "total_time_limit_s": 14400,
  "display": {"stage": {"name": "巡天得分", "unit": "分", "decimals": 1},
              "total": {"name": "四卡总分", "unit": "分", "decimals": 1}},
  "stages": [
    {"id": "l1", "dir": "L1", "inputs": ["card.md"], "tests": ["config", "public", "truth"], "time_limit_s": 3600}
  ]
}
```

插槽字段写在题目包级，阶段可以逐项覆盖（`runner`、`packager`、`packager_options`、`scorer`）。`crucible taskset pack` 把题目包级的缺省值落到每个阶段，登记后的 `taskset.json` 里每个阶段都写明 `packager`；`runner` 为缺省 `workdir`、`scorer` 与题目包级相同时不写。

## 13. 分阶段实施

每一阶段单独一个 PR，单元测试之外只做最小真跑：hello-world（Playwright，验证旧路径不变）和巡天 L1（验证连续分）。

### P1：结果格式、状态、展示

改动最小、收益最大：巡天立刻按原始分正常显示，旧数据不受影响。

| 文件 | 改动 |
|---|---|
| `crates/crucible-core/src/score.rs` | v2 `ScoreResult`（`status: scored/error`、`score`、`max`、`passed`、`items`）；旧格式反序列化换算（§7.1）；items 推出 score/max |
| `crates/crucible-core/src/taskset.rs` | `schema: 2`；`aggregate` 对象形式（`"sum"` 字符串读作 `ratio`）；`display`；`model` 字段先只解析不使用 |
| `crates/crucible-core/src/manifest.rs` | `StageScore` 新字段与旧字段兼容读取；`scoring` 快照；`compute_total_score` 按 `aggregate` 计算 |
| `crates/crucible-cli/src/score.rs` | `normalise` 改到新格式（`expected_total` 规则不变） |
| `crates/crucible-cli/src/publish.rs`、`submit.rs` | 写 `scoring` 快照；`crucible status` 按 `display` 输出 |
| `crates/crucible-worker/src/model.rs`、`app.rs` | `total_score` 按快照计算；列表摘要带 `display` |
| `web/src/types.ts`、`stats.ts`、`compare.ts`、`ui.tsx`、`pages/EvalDetail.tsx`、`EvalList.tsx`、`Compare.tsx`、`Tasksets.tsx` | 按 `display` 格式化；对比差值按 `direction` 着色；兼容旧 manifest |
| `scorers/astro-survey/image/scorer/engine_main.py`、`score.sh` | 直接输出 v2：`score` = 原始分，`items` = 五个分项 |
| `tasksets/astro-practice/source.json`、`taskset.json` | 去掉 `expected_total`，加 `aggregate`、`display`（只改元数据，块不变） |
| `docs/scorer-contract.md` §4、`docs/api.md`（`total_score` 的含义）、`docs/astro-survey.md`（计分） | 同步 |

验证：用数据分支上已有的旧 manifest 和 Playwright 旧格式 `result.json` 做单元测试，确认总分与改动前一致；hello-world 跑一次 agent 模式，网页显示 `1/1`、总分 100%；巡天 L1 用示例 agent 走 app 模式 `score.yml`，网页显示与引擎原始分相同的"巡天得分"。

### P2：打包器与运行器插件化

| 文件 | 改动 |
|---|---|
| `plugins.json`（新） | 注册表，§3 |
| `crates/crucible-core/src/taskset.rs` | 去掉 `OutputKind`，`Stage.packager: String`（`output` 作为旧字段读入）；`runner`、`scorer` 可在题目包级和阶段级声明；新增 `plugins.rs`（注册表类型与校验） |
| `crates/crucible-cli/src/package.rs` → `packagers/{mod,web_app,files}.rs` | `Packager` trait 与两个实现，行为不变；app 模式的 `check` |
| `crates/crucible-cli/src/run.rs` → `runners/{mod,workdir}.rs` | 抽出 `workdir` 运行器，`crucible run` 按题目包的 `runner` 名调用 |
| `crates/crucible-cli/src/plan.rs`、`taskset_cmd.rs`、`score.rs` | 按注册表校验插件名；打分器路径来自注册表 `impl` |
| `.github/workflows/score-tests.yml` | "Prepare the scorer" 从注册表取打分器目录，不再拼 `scorers/$name` |
| `crucible-core` 的 `USER_SCORERS`，及使用它的 `taskset validate`、`taskset-pack`、Worker `model.rs` | 改为查注册表的 `user` 字段。`plugins.json` 用 `include_str!` 编译进 `crucible-core`，命令行和 Worker 用同一份；加插件后 Worker 需重新部署才认 |
| `docs/agent-contract.md`（产出一节）、`docs/scorer-contract.md` | 指向本文 |

验证：hello-world agent 模式真跑，产出 zip 的哈希规则和分数与 P1 相同；旧 `"output": "web-app"` 的题目包照常解析；注册表里没有的名字、用户题目包引用 `user: false` 的插件都被拒绝。

### P3：交互运行器、巡天迁移、打分阶段用模型

| 文件 | 改动 |
|---|---|
| `runners/astro-v4/`（新） | 从 `scorers/astro-survey` 搬入 agent 容器、引擎容器、FIFO 编排；`run.sh` 按 §4.2 |
| `scorers/astro-survey/` | 只读 `--run` 目录计分 |
| `crates/crucible-cli/src/score.rs`、`main.rs` | `crucible score` 在打分前调用交互运行器；需要模型时在宿主机起 meter（复用 `crucible-meter`），建只通 meter 的内部网络，各插槽分开写 `usage.jsonl` |
| `crates/crucible-cli/src/score.rs`（`score-handoff`）、`cred.rs` | 阶段声明需要模型时把凭据一起重新封给一次性钥匙 |
| `crates/crucible-core/src/manifest.rs`、`crates/crucible-cli/src/publish.rs` | `eval_usage` |
| `.github/workflows/eval.yml`、`score.yml`、`score-tests.yml` | `score-tests` 按遍矩阵；凭据经交接；凭据仍在 `publish` 结束时从 Worker 删除 |
| `crates/crucible-worker/src/model.rs` | app 模式下题目包 `model.*: required` 时 `cred_envelope` 必填 |
| `web/src/pages/EvalDetail.tsx`、`Compare.tsx` | 分开显示评测阶段的模型用量 |
| `tasksets/astro-practice/source.json` | §12 的插槽与 `model` 声明 |
| `docs/astro-survey.md`、`docs/api.md` | 同步 |

验证：巡天 L1 app 模式不带凭据，分数与 P1 相同（规则路径是确定的）；带一把真实 key 再跑一次，`eval_usage.interactive` 有请求数和 token，`usage` 不变，`score-tests` 日志里没有 key；hello-world 回归一次。

## 14. 用户上传的插件

用户可以上传自己的**打分器**插件，在自己上传的题目包里引用。运行器（产出运行器、交互运行器）暂不开放上传：它们要接 agent 容器、FIFO 或计量网络，接口面比打分器大得多，留到打分器跑稳之后再开。打包器始终只有 `builtin`（§5）。

### 14.1 插件包格式

一个 zip，根目录（或 zip 里唯一的顶层文件夹）放：

| 文件 | 作用 |
|---|---|
| `plugin.json` | 声明，见下表；未知字段拒绝 |
| `Dockerfile` | 打分器镜像；打分逻辑全在镜像里。建议基础镜像钉 digest。构建规则见 §14.3：基础镜像只能来自 Docker Hub 或 ghcr.io 且写成字面量（不能用 `$变量`），不能 `ADD` 网址或 git 仓库，不能用 `ONBUILD`；构建用 Docker 经典构建器，不支持 BuildKit 专有语法（`RUN --mount`、heredoc） |
| 其他文件 | 构建上下文（脚本、模型权重等），解压后不超过 100 MB、2000 个文件，不能有符号链接 |
| `selftest/artifact`、`selftest/tests/`（可选） | 自检样例：一份产出和一份测试材料。没有时自检用空文件和空目录 |

```json
{
  "schema": 1,
  "kind": "scorer",
  "name": "keyword-scorer",
  "version": "1",
  "description": "按关键字给文本产出打分",
  "runs_taskset_code": false,
  "model": false,
  "accepts": ["files"]
}
```

| 字段 | 含义 |
|---|---|
| `kind` | 目前只能是 `scorer` |
| `name` | 上传者起的名字，`[a-z0-9][a-z0-9-]{0,39}`，只用于展示；平台登记的 id 是 `u-<16 hex>` |
| `version` | `[A-Za-z0-9.]`，1–20 个字符，记入 manifest |
| `runs_taskset_code` / `model` / `accepts` | 含义同注册表（§3）。缺省 `runs_taskset_code: true`；`model: true` 时 `runs_taskset_code` 必须是 `false`；`accepts` 只能是已有的打包器 |

示例：`examples/plugins/keyword-scorer`（按关键字给文本产出打分），引用它的题目包 `examples/tasksets/keyword-demo`。

### 14.2 镜像的约定

平台不在宿主机上执行插件包里的任何东西，只读 `plugin.json`。打分时由仓库里的通用外壳 `scorers/_user/score.sh`（打分器约定的 `score.sh` 接口，`docs/scorer-contract.md` §3）用 `crucible ctr run` 起插件镜像，所以 Docker、Nomad、Kubernetes 执行后端都一样可用。镜像的 ENTRYPOINT 收到：

```
--artifact /in/artifact          # 本阶段产出（打包器的格式，如 files 的 zip），只读
--tests /in/tests                # 本阶段的隐藏材料，只读：source.json 里 tests 列出的路径，相对阶段目录
--out /out/result.json           # 写 result.json v2（§7）
--visibility hidden|public       # 正式评测一律 hidden
[--run /in/run]                  # 本阶段有交互运行器时的运行记录，只读
[--options /out/options.json]    # 题目包的 scorer_options
[--model-base-url URL --model NAME]   # 只在 model: true 且题目包声明 model.scorer、提交者给了凭据时
```

容器的墙与平台自带打分器相同：只读根、`/tmp` 是 256 MB tmpfs、`--cap-drop ALL`、`no-new-privileges`、2 GB 内存、2 CPU、256 个进程、非 root uid；没有网络（`--network none`），声明了模型时只接到只通计量代理的网络，`OPENAI_BASE_URL` 指向计量代理、`OPENAI_API_KEY=dummy`，模型 key 拿不到；墙钟 1200 秒，超时记 `error`。没写出 `result.json` 也记 `error`（system）。`crucible score` 对结果做与其他打分器相同的收尾（§7，`expected_total` 规则照旧）。

### 14.3 登记流程

和用户题目包一样：

1. 网页“插件”页上传，或 `crucible plugin upload <目录或 zip> --wait`。插件包在浏览器 / 命令行里用平台公钥封好，`POST /uploads`（`X-Upload-Kind: plugin`）后 `POST /plugins`。
2. Worker 在 D1 表 `user_plugins` 登记 `u-<16 hex>`（`building`，私有），触发 `plugin-pack.yml`：
   - `open`（持平台私钥，不运行插件包里的任何东西）：解密、检查 `plugin.json`、`Dockerfile`、zip 限制；不通过直接把原因回传 Worker。通过后把插件包重新封给一次性钥匙交给下一个 job。
   - `build`（只有一次性钥匙，`permissions: {}`，GitHub 托管机）：`crucible step plugin-build` 构建镜像，再经通用外壳在 `selftest/` 样例（或空输入）上跑一次，能写出合法的 `result.json` 即自检通过。构建是隔离的：
     - `RUN` 步骤接在沙箱网络上（与 agent 运行时同一套 `tools/sandbox-net.sh`），唯一出口是本 job 的出网代理，只放行 `config/egress.json` 白名单里的主机（npm、PyPI）的 HTTPS；代理地址经 `HTTP_PROXY`/`HTTPS_PROXY` 构建参数给出。基础镜像由 Docker 守护进程拉取，`open` 已检查过只来自 Docker Hub 或 ghcr.io；`ADD` 网址被拒（否则守护进程会绕过代理去下载）。
     - 构建最长 20 分钟、内存 4 GB；镜像（`docker save` 后）不超过 2 GB。
     - 日志里只打印代理放行过的主机和被拒的连接数；被拒的主机名只回给上传者。
     - 镜像（`docker save`，gzip）和结果（`ready` + 自检分数和镜像 id，或失败原因）都用平台公钥封好，作为 artifact `plugin-outcome` 交出，不再用 job output。
   - `report`（持平台私钥、Worker 令牌、存储写权限）：用平台私钥解开结果，核对它属于这个插件 id 和这个插件包（解不开或对不上就按"平台侧失败"登记为 `failed`）；自己从镜像文件算出镜像 id（配置的 SHA-256，并逐层核对摘要），与结果里的一致才把封好的镜像存为块，连同镜像 id 写进登记（`plugin.image = {blob, id}`），回传 Worker。
3. 状态变为 `ready`（网页和 `crucible plugin status` 显示自检分数和镜像 id），或 `failed`（原因只给上传者看，不进公开日志）。

插件登记后不可修改；要改就重新上传，得到新的 id。镜像只在登记时构建这一次。

### 14.4 在题目包里使用

用户题目包的 `source.json` 里写 `"scorer": {"name": "u-<16 hex>"}`（题目包级或阶段级都可以）。`taskset-pack` 登记题目包时向 Worker 查询（`GET /internal/plugins/:id?taskset=<题目包 id>`）：插件必须 `ready`，且是题目包上传者自己的或已公开的，否则题目包登记失败。查到的插件以固定形式写进 `taskset.json` 的 `user_plugins`（id、版本、能力、插件包的块引用），之后的评测都用这一份；Worker 收到登记结果时再按 D1 核对一次。

权限：私有插件只有上传者的题目包能用；管理员 `POST /plugins/:id/public` 设为公开后，任何人的题目包都能用。内置题目包不受影响。本地 `crucible taskset validate` 遇到 `u-...` 时先按最宽松的能力放行，真正的检查在登记时做。

评测时：`score`（交接）job 把登记时存下的镜像和隐藏材料一起重新封给一次性钥匙；`score-tests` 解开后先核对镜像 id 与题目包里记的一致，再载入为 `crucible-scorer-<id>:run`（Docker 后端 `docker load`；Kubernetes 后端推到集群镜像仓库），由通用外壳运行，不再重新构建。manifest 的 `scoring.plugins` 记下 `u-...` 的名字、版本和镜像 id（`image`）。镜像功能上线前登记的插件没有 `image`，仍按插件包在打分机上构建。

### 14.5 用户插件能拿到什么

| | 能拿到 | 拿不到 |
|---|---|---|
| 构建（`plugin-pack` 的 `build`） | 自己的插件包；经出网代理访问白名单软件源 | 平台私钥、Worker 令牌（这个 job 不持有）；白名单以外的网络 |
| 打分（`score-tests` 的插件容器） | 产出、本阶段隐藏材料、交互运行记录；`model: true` 时计量代理地址 | 平台私钥、Worker 令牌、一次性钥匙、模型 key 本身、网络（除计量代理） |

需要模型时只能经计量代理用提交者的 key，用量记进 `eval_usage.scorer`，计入提交者的预算（§10）。

### 14.6 公开运营前再做的加固

原型阶段只保留已有的沙箱隔离，下面这些记为公开运营前再做：

- ~~构建可以自由联网~~（已做）：`RUN` 只经出网代理访问白名单软件源，构建限时 20 分钟、镜像不超过 2 GB，见 §14.3。
- ~~每次评测重新构建~~（已做）：登记时构建一次，镜像加密存为块，题目包和 manifest 记镜像 id，评测只载入这个镜像，见 §14.3、§14.4。
- 模型：用户插件 `model: true` 时，插件代码与提交者的计量代理在同一台机器上；容器逃逸即可能拿到提交者的 key。公开运营前应把计量代理与插件容器分到不同机器，或禁止公开插件使用模型。
- ~~自检只证明“能写出 result.json”，不检查打分是否合理；公开插件需要人工审核~~ 已做：plugin-pack 回传审核材料（文件清单、Dockerfile、小的文本文件），网页管理员视图可读；`POST /plugins/:id/public` 设为公开必须带审核清单确认（源码、Dockerfile、hidden 时不泄露测试内容、模型用法四项全勾），审核人与时间记入 `approval`（`docs/api.md`）。
- ~~自检结果经 job output 交出~~（已做）：改为封给平台公钥的文件，`report` 解开后核对插件 id、插件包和镜像 id（镜像 id 由 `report` 自己从镜像算出）才登记。仍然成立的边界：能控制 `build` 机器的人可以用公钥封一份假的自检分数，但改不了镜像 id 与镜像内容的对应关系，也改不了评测时实际运行的镜像。
- 用户插件的 `detail`、`items` 由插件自己写，hidden 时是否泄露测试内容由插件作者负责；公开题目包引用的插件需要审核这一点。
- 资源：插件容器的内存、CPU、时间上限是固定值，没有按用户或题目包计配额。~~上传频率没有限制~~ 已做：Worker 按用户限制 24 小时内的上传次数与字节、插件与题目包登记数、评测数和同时进行的评测数，超限返回 429 并写明恢复时间；管理员可按用户调整或豁免（`docs/api.md`“配额”）。登记超过 2 小时仍在构建/打包的由每小时的定时任务标为失败并写明原因。
