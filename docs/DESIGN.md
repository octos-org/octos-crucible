# octos-crucible 设计

## 1. 定位

octos-crucible 是评测 agent 的通用平台：任何能在沙箱里运行的 agent，在任何题目上按阶段运行、打分，同时客观记录用时、token、缓存命中和等价花销。核心只管运行 agent、计量、打分调度、出报告；题目、运行方式、打包、打分都是插件（§3、`docs/plugins.md`）。它不是排行榜，不绑定任何一套题，也不绑定任何一种打分方式。

写代码只是一类题。现有和计划中的例子：

- 写代码：ARC-Bench 的 GitHub 题（`arcbench-github`），agent 写出网站，Playwright 测试打分。这是第一个题目包。
- 交互决策：GOSIM 智能体巡天（`astro-practice`），观测 agent 与模拟器一问一答，官方评分引擎打分，见 `docs/astro-survey.md`。
- 数学 / 推理：例如 IMO 试题，agent 写证明，评判模型按评分标准打分（`tasksets/math-proof-demo`，打分器 `llm-judge`，打分阶段经计量代理用提交者的模型，`docs/plugins.md` §10）。

将来若发布自己的 benchmark，会作为单独命名的题目集发布在它之上。

## 2. 两种用法

- **上传产出（快速反馈）**：上传已经生成好的产出（zip，内容由题目包决定），只打分，几分钟出结果。
- **上传 agent（完整评测）**：上传 agent 包，填写自己的模型 key、接口地址和模型名，选择题目和运行遍数。平台让 agent 按阶段完成题目、逐阶段打分，给出每一遍每个阶段的分数、用时、token、缓存命中率、等价花销，以及多遍的均值和波动。

使用条件：GitHub 登录即可；每个用户有默认配额（24 小时内的上传次数与字节、评测数、插件与题目包登记数，以及同时进行中的评测数，见 `docs/api.md`“配额”），管理员可按用户调整或豁免，也可封禁。

## 3. 核心与插件

核心只管六件事：调度、隔离与安全、计量、存储、凭据、结果汇总。与某一种评测相关的一切（agent 怎样产生产出、产出打成什么格式、怎样判分、分数表示什么、阶段怎样汇总）都由插件和题目包声明，核心只按名字调用。目标是让新的评测形式（浏览器测试、单元测试、交互式环境、模型评判……）只需要加插件和题目包，不改核心。接口细节见 `docs/plugins.md`；插件化按 §10 分三步落地，落地前现行行为以 `docs/scorer-contract.md` 和 `docs/agent-contract.md` 为准。

**核心**

| 模块 | 职责 |
|---|---|
| 调度 | workflow 与 job 划分、阶段顺序、时限、按题目包声明依次调用插件 |
| 计量代理 meter | 所有模型调用都经过它（agent 生成时、交互运行时、模型评判时各起一个）。记录 token（含缓存命中）和耗时；只转发到用户填的接口（必须 https，不能指向内网）；可选预算按整次评测累计 |
| 出网代理 egress | 生成时 agent 只能访问白名单软件源（npm、PyPI） |
| 价格配置 pricing | 公开价格表，折算等价花销。表里没有的模型只报 token，用户可自填单价 |
| 存储 store | 只有 `put(bytes) -> hash` 和 `get(hash) -> bytes`。当前实现为 GitHub Release，可替换 |
| 凭据 | 模型 key、下载密码的加密收发、交接与删除 |
| 结果 results | 每次评测一份清单（manifest），按题目包声明的汇总方式算总分，按声明的展示方式显示。清单总是加密存档；提交者选择公开分数时，明文提交到数据分支；同时回传 Worker |

**插件与声明**

| 种类 | 作用 | 现有实现 |
|---|---|---|
| 运行器 runner | 让 agent 产生产出。产出运行器在生成 job 里运行，看不到隐藏材料；交互运行器在打分 job 里运行，让沙箱外的裁判进程与沙箱内的 agent 一问一答 | `workdir`（agent 在工作目录里按阶段干活）；`astro-v4`（交互，P3） |
| 打包器 packager | 把工作目录变成产出文件；app 模式下检查上传的产出 | `web-app`（根目录带 Dockerfile 的 zip）、`files`（原样打包） |
| 打分器 scorer | 输入产出、隐藏材料（和交互运行记录），输出连续分数与可选明细 | `playwright`、`astro-survey` |
| 题目包 taskset | 声明阶段（输入、时限、隐藏材料）、每个插槽用哪个插件、阶段内与阶段间怎样汇总（`aggregate`）、分数的名称单位方向范围（`display`）、打分时是否需要模型 | `arcbench-github`、`hello-world`、`astro-practice` |
| Agent 包 | 被评测的对象。默认格式是 `agent.json` + `Dockerfile`（`docs/agent-contract.md`）：能从命令行启动、读需求、在工作目录里干活、调模型的 base URL 可配置；不要求理解"阶段"。交互运行器可以约定自己的 agent 格式 | 内置 Octos、Codex，与上传的格式相同 |

插件只能由维护者加入仓库并登记在 `plugins.json`；题目包和 agent 可以由用户上传，题目包只能引用已登记的插件（见 §8）。

## 4. 一次完整评测

1. **提交**：浏览器用平台公钥加密 agent 包（或产出）和模型 key。包存为块；模型 key 存入 Worker 的 D1（24 小时过期）。明文不经过我们的服务器。
2. **触发**：Worker 校验 GitHub 身份后触发 GitHub Actions，只传评测编号等非秘密参数。
3. **生成**（`generate`，每遍一台机器，可并行；只在 agent 模式）：
   - 在容器里构建 agent；此时机器上没有任何密钥。
   - 解密模型 key，只交给计量代理（经 stdin，不进环境变量和日志）。
   - 按阶段调用产出运行器（默认 `workdir`：每阶段结束保存产出作为 checkpoint，然后在同一工作目录给出下一阶段需求，工作目录和 HOME 在阶段之间原样保留；阶段内每 15 分钟快照，被强制结束时用最后一份快照），再调用打包器把工作目录打成产出。
   - 容器限制：2G 内存、1 核、非 root、cap-drop ALL、no-new-privileges；只能访问计量代理和白名单软件源；看不到隐藏材料。
4. **交接**（`score`）：持钥 job 解开所需阶段的隐藏材料和各遍产出（阶段声明打分时需要模型的，连同模型 key），用一把一次性钥匙重新封好，交给不持有任何平台密钥的 job。这个 job 不运行任何插件。
5. **评测**（`score-tests`）：只持有一次性钥匙。按阶段调用交互运行器（若题目包声明）和打分器，得到每阶段的结果；需要模型的插槽经宿主机上的计量代理调用，单独记账。隐藏材料和产出都可能是不可信代码，只在容器里处理（`docs/scorer-contract.md` §7）。
6. **汇总**（`publish`）：用时取平台进程在运行步骤内的计时，并用 GitHub job/step 时间戳核对（不采信 agent 自报）；token 和花销取计量代理日志，生成阶段与评测阶段分开记；总分按题目包的 `aggregate` 计算，manifest 记下所用的汇总方式、展示方式和插件版本；产出和日志加密存为块；清单加密存档，提交者选择公开时才明文提交到数据分支；为提交者生成用下载密码加密的 zip。
7. **清理**：删除模型 key 和下载密码；D1 中另设 24 小时过期兜底（每小时 Cron 清理）。

上传产出（app 模式）跳过第 3 步：上传的文件直接进入第 4 步，第 5 步打分前由题目包所声明的打包器检查格式。

以上是在 GitHub Actions 上的现状。步骤怎样与调度方、容器后端解耦，以便在自托管运行器、单机、Kubernetes、Nomad 上运行，见 `docs/executors.md`。

## 5. 存储与数据

| 东西 | 位置 | 保留 |
|---|---|---|
| 所有文件（上传的包、产出、日志、题目包） | 加密后按 SHA-256 命名，存于 32 个预发布 Release `blobs-00`…`blobs-31`，分片 = 哈希前 5 位 | 永久 |
| 题目登记表 | 仓库 `tasksets/<name>/taskset.json` | 永久 |
| 用户上传的题目包登记 | D1 `user_tasksets`（默认私有，管理员可设为公开；见 `docs/api.md`） | 永久 |
| 插件注册表 | 仓库 `plugins.json`（P2 起） | 永久 |
| 评测清单 | 加密块（总是）；公开分数的评测另在仓库 `data` 分支 `evals/<eval_id>.json` | 永久 |
| 平台程序 | 正式 Release `vX.Y.Z` | 永久 |
| 用户模型 key、下载密码 | D1 `creds` | 评测结束即删 |

位置完全由哈希算出，不需要索引；文件名即哈希，完整性可直接校验。

**可见性**：产出和日志不公开，只有提交者能下载；分数和统计默认不公开，提交者可选择公开；公开的 Actions 日志里只有进度和数字。

**提交前声明**（同意才能使用）：
- 上传的内容、产出和日志会加密后永久保存，我们会用于研究和改进平台。
- 你的模型 key 在评测结束后立即删除，不会保存。

## 6. 钥匙

只有一对钥匙。

- **公钥**：公开，网页、Worker、仓库里都有（`config/keys.json`），任何环节都可以用它加密。
- **私钥**：只在两处——GitHub Secrets（评测运行时使用）和管理员的密码管理器（备份，研究时使用）。Worker 不持有私钥。
- **提交者取回产出**：提交时设置下载密码；评测结束后产出打成 AES-256 加密的 zip。macOS 和 Windows 自带的解压工具打不开这种格式，下载页提示用户使用 7-Zip、Keka 或 The Unarchiver。系统自带工具支持的旧式 ZipCrypto 已被攻破，不提供。
- **泄露处置**：换新钥匙，之后的文件用新钥匙加密；每个块记录所用钥匙编号。

## 7. 技术选型

- **Rust**：计量代理、加解密、存储、汇总、命令行工具 `crucible`、Cloudflare Worker（workers-rs）。预编译后发布到 Release，评测时按固定版本下载。
- **插件**：打包器与产出运行器编译进 `crucible`（Rust，处理不可信字节的宿主机代码只用评审过的 Rust）；打分器和交互运行器是容器镜像，用各自需要的语言（Playwright 打分器为 TypeScript，巡天引擎为 Python）。
- **网页**：静态页面，GitHub Pages。
- **费用**：全部在免费额度内，不需要绑卡。

### Rust crate 划分

| crate | 职责 |
|---|---|
| `crucible-core` | 共享类型：TaskSet、AgentSpec、UsageRecord、ScoreResult、Manifest、Envelope、插件注册表；可编译到 wasm |
| `crucible-metering` | 纯函数：解析 usage、查价格、计算等价花销 |
| `crucible-meter` | 计量代理：OpenAI 兼容转发、SSE 透传、SSRF 防护、可选预算、JSONL 记录 |
| `crucible-egress` | CONNECT 白名单出网代理 |
| `crucible-crypto` | 信封加密（公钥加密、私钥解密）、密码 zip |
| `crucible-store` | `put`/`get`，GitHub Release 实现（32 分片） |
| `crucible-report` | 多遍、多阶段汇总 |
| `crucible-cli` | 二进制 `crucible`：taskset / plan / fetch / build / run / package / cred / seal-outputs / manifest / score-handoff / score / report / put / get；用户侧 submit / status（个人 API 令牌） |
| `crucible-worker` | Cloudflare Worker 后端 |

## 8. 安全边界

- 上传的代码只在容器里运行；宿主机只执行平台自己的代码。
- 信任分两级。官方插件（运行器、打包器、打分器）只能由维护者经 PR 评审加入并登记在 `plugins.json`，它们能接触解密后的隐藏材料。用户可上传的是题目包和 agent：题目包对核心来说只是数据，只能引用已登记的插件（用户题目包只能引用标为 `user: true` 的）；题目包里的文件是否被当作代码执行取决于所引用的插件，执行它们的插件只在 `score-tests` 的容器里运行。
- 打分或交互时需要模型的阶段，提交者的模型 key 经交接到 `score-tests`，只在宿主机的计量代理进程里，容器只拿到计量代理地址；会执行题目包代码的插件永远拿不到模型（`docs/plugins.md` §10）。
- 每个 job 只拿它需要的东西：生成 job 拿不到题目；跑测试的 job（`score-tests`）拿不到任何平台密钥，`permissions: {}`；只有发布 job 有写仓库权限。workflow 顶层 `permissions: {}`。
- 所有输入经正则校验后通过 env 传入 shell，不做表达式拼接。
- 用户上传的打分器插件（`docs/plugins.md` §14）：镜像在不持有平台密钥的 job 里构建，`RUN` 步骤只能经出网代理访问白名单软件源，基础镜像只来自 Docker Hub 或 ghcr.io，构建限时、镜像限大小；登记时只构建一次，镜像加密存为块，评测时核对镜像 id 后载入，不再重建。构建结果封给平台公钥交回，由持钥 job 核对后登记。`model: true` 的插件以与计量代理进程不同的 uid、在独立网络命名空间里运行（`docs/executors.md` §3.5）。插件设为公开前须管理员按审核清单读过源码与 Dockerfile。
- 已知残余风险：生成 job 里的容器逃逸可拿到该机器上的模型 key 和私钥（打分时的逃逸拿不到，见上）。缓解：机器一次性使用、key 用完即删、钥匙可更换。
- 已知残余风险：只有一对钥匙，题目包的 inputs 块和 tests 块用同一把公钥加密，生成 job 为了解开 inputs 块持有私钥，因此技术上也能解开 tests 块。"生成 job 拿不到题目"靠的是生成 job 只下载 inputs 块（`crucible taskset inputs` 只读 `inputs_blob`），不是密码学隔离。要做到密码学隔离需为 tests 块单设一把只在打分 job 使用的钥匙。

## 9. 维护

- 每周自检：清单引用的块是否都在、抽查哈希与解密、D1 `creds` 中有无残留；异常自动开 issue。
- 管理员命令行：按 agent、日期、分数筛选评测，下载并在本地解密。
- 唯一的人工事项：生成钥匙时把私钥存入密码管理器。

## 10. 实施顺序

第一轮（已完成）：Rust 基础组件 → 生成流程 → 打分器接口与 Playwright 打分器 → Worker 与网页 → 上传产出入口、多遍汇总、命令行提交、打分隔离。

第二轮是插件化，分三步，每步单独上线，细节和验证方式见 `docs/plugins.md` §13：

1. **P1 结果格式、状态与展示**：结果改为连续分数 `score`（可为负）、可选 `max`、`status: scored | error`、可选明细；题目包声明汇总方式和展示方式；旧数据按固定规则换算读取，分数不变。巡天立即按原始分显示。
2. **P2 打包器与运行器插件化**：引入 `plugins.json`；去掉 `OutputKind`，题目包只写插件名；从 `crucible run` 抽出 `workdir` 运行器。
3. **P3 交互运行器**：新增交互运行器插槽，巡天从"打分器内部跑 agent"迁到 `astro-v4` 交互运行器 + `astro-survey` 打分器；打分和交互阶段可以经计量代理使用提交者的模型。
