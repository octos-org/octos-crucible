# octos-crucible 设计

## 1. 定位

octos-crucible 是评测 coding agent 的基础设施：运行 agent、计量、打分、出报告。它不是排行榜，不绑定任何一套题，也不绑定任何一种打分方式。ARC-Bench 的 GitHub 题是第一个题目包；将来若发布自己的 benchmark，会作为单独命名的题目集发布在它之上。

## 2. 两种用法

- **上传产出（快速反馈）**：上传已经生成好的产出（如一个网站的 zip），只打分，几分钟出结果。
- **上传 agent（完整评测）**：上传 agent 包，填写自己的模型 key、接口地址和模型名，选择题目和运行遍数。平台让 agent 按阶段完成题目、逐阶段打分，给出每一遍每个阶段的分数、用时、token、缓存命中率、等价花销，以及多遍的均值和波动。

使用条件：GitHub 登录即可，不限次数，管理员可封禁。

## 3. 模块

模块之间只通过约定好的文件格式交互，每一块都可单独替换。

| 模块 | 职责 |
|---|---|
| 题目包 taskset | 声明阶段（每阶段给 agent 的输入、要求的产出类型、限时）、打分器、总分算法。登记时检查总时长不超过平台上限。 |
| Agent 包 | `agent.json`（如何启动）+ `Dockerfile`。对 agent 的要求只有两条：能从命令行启动、读需求、在工作目录里干活；调模型的 base URL 可配置。不要求理解"阶段"，不要求保存状态。内置 agent 与上传的格式相同。 |
| 打分器 scorer | 一个容器：输入产出 + 隐藏的测试材料，输出固定格式 `result.json`。第一个实现是 Playwright；以后可加单元测试、脚本比对、模型评判等。 |
| 计量代理 meter | agent 调模型都经过它。记录每次调用的 token（含缓存命中）和耗时；只转发到用户填的接口（必须 https，不能指向内网）。默认无预算上限，用户可自设。 |
| 价格配置 pricing | 公开价格表，折算等价花销。表里没有的模型只报 token，用户可自填单价。 |
| 存储 store | 只有 `put(bytes) -> hash` 和 `get(hash) -> bytes`。当前实现为 GitHub Release，可替换。 |
| 结果 results | 每次评测一份清单（manifest），汇总表和网页都由清单生成。清单的去处（results sink）可组合：总是加密存为块（存档）；提交者选择公开分数时，明文提交到数据分支；第 4 步起回传给 Worker 存入 KV。 |

## 4. 一次完整评测

1. **提交**：浏览器用平台公钥加密 agent 包和模型 key。agent 包存为块；模型 key 存入 Workers KV。明文不经过我们的服务器。
2. **触发**：Worker 校验 GitHub 身份后触发 GitHub Actions，只传评测编号。
3. **生成**（每遍一台机器，可并行）：
   - 在容器里构建 agent；此时机器上没有任何密钥。
   - 解密模型 key，只交给计量代理（经 stdin，不进环境变量和日志）。
   - 按阶段运行 agent：每阶段结束保存产出作为该阶段 checkpoint，然后在同一工作目录给出下一阶段需求。工作目录和 agent 的 HOME 在阶段之间原样保留。
   - 阶段内每 15 分钟自动快照；被强制结束时，用最后一份快照打分。
   - 容器限制：2G 内存、1 核、非 root、cap-drop ALL、no-new-privileges；只能访问计量代理和白名单软件源（npm、PyPI）；看不到测试。
4. **打分**：在另一台机器上解密题目包，用指定打分器给每个阶段的 checkpoint 打分。
5. **汇总**：用时取平台进程在运行步骤内的计时，并用 GitHub job/step 时间戳核对（不采信 agent 自报），token 和花销取计量代理日志；产出和日志加密存为块；清单加密存档，提交者选择公开时才明文提交到数据分支；为提交者生成用下载密码加密的 zip。
6. **清理**：删除模型 key 和下载密码；KV 另设 24 小时过期兜底。

## 5. 存储与数据

| 东西 | 位置 | 保留 |
|---|---|---|
| 所有文件（上传的包、产出、日志、题目包） | 加密后按 SHA-256 命名，存于 32 个预发布 Release `blobs-00`…`blobs-31`，分片 = 哈希前 5 位 | 永久 |
| 题目登记表 | 仓库 `tasksets/<name>/taskset.json` | 永久 |
| 评测清单 | 加密块（总是）；公开分数的评测另在仓库 `data` 分支 `evals/<eval_id>.json` | 永久 |
| 平台程序 | 正式 Release `vX.Y.Z` | 永久 |
| 用户模型 key、下载密码 | Workers KV | 评测结束即删 |

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
- **打分器**：用各自需要的语言，Playwright 打分器为 TypeScript。
- **网页**：静态页面，GitHub Pages。
- **费用**：全部在免费额度内，不需要绑卡。

### Rust crate 划分

| crate | 职责 |
|---|---|
| `crucible-core` | 共享类型：TaskSet、AgentSpec、UsageRecord、Result、Manifest、Envelope；可编译到 wasm |
| `crucible-metering` | 纯函数：解析 usage、查价格、计算等价花销 |
| `crucible-meter` | 计量代理：OpenAI 兼容转发、SSE 透传、SSRF 防护、可选预算、JSONL 记录 |
| `crucible-egress` | CONNECT 白名单出网代理 |
| `crucible-crypto` | 信封加密（公钥加密、私钥解密）、密码 zip |
| `crucible-store` | `put`/`get`，GitHub Release 实现（32 分片） |
| `crucible-report` | 多遍、多阶段汇总 |
| `crucible-cli` | 二进制 `crucible`：taskset / plan / fetch / build / run / package / cred / seal-outputs / manifest / score / report / put / get |
| `crucible-worker` | Cloudflare Worker 后端 |

## 8. 安全边界

- 上传的代码只在容器里运行；宿主机只执行平台自己的代码。
- 每个 job 只拿它需要的东西：生成 job 拿不到题目；打分 job 不运行 agent；只有发布 job 有写仓库权限。workflow 顶层 `permissions: {}`。
- 所有输入经正则校验后通过 env 传入 shell，不做表达式拼接。
- 构建缓存：`crucible` 只在 setup job 编译一次（`actions/cache` 缓存依赖与 target），作为 artifact 交给 generate 和 publish。内置 agent（`builtin:`）的镜像用 `docker buildx` 的 GitHub Actions 层缓存，作用域 = agent 名 + 包内容哈希 + 上游源码 commit（`agents/<name>/upstream.json` 声明的仓库与分支，构建时解析为 commit 并作为 build arg 固定），上游一有新 commit 就换作用域，不会复用过期的层。用户上传的 agent（`url:`/`git:`/`blob:`）不读也不写共享缓存，防止一个提交投毒其他构建所复用的层。缓存所需的运行时令牌只在构建步骤的环境里，构建完即清空。
- 已知残余风险：容器逃逸可拿到该机器上的模型 key 和私钥。缓解：机器一次性使用、key 用完即删、钥匙可更换。
- 已知残余风险：只有一对钥匙，题目包的 inputs 块和 tests 块用同一把公钥加密，生成 job 为了解开 inputs 块持有私钥，因此技术上也能解开 tests 块。"生成 job 拿不到题目"靠的是生成 job 只下载 inputs 块（`crucible taskset inputs` 只读 `inputs_blob`），不是密码学隔离。要做到密码学隔离需为 tests 块单设一把只在打分 job 使用的钥匙。

## 9. 维护

- 每周自检：清单引用的块是否都在、抽查哈希与解密、KV 中有无残留；异常自动开 issue。
- 管理员命令行：按 agent、日期、分数筛选评测，下载并在本地解密。
- 唯一的人工事项：生成钥匙时把私钥存入密码管理器。

## 10. 实施顺序

1. Rust 基础组件：计量代理、加解密、存储、命令行（对齐 Python 原型的测试）。
2. 生成流程（含阶段和快照）：用内置 Octos 在 GitHub 上真跑一次，验证联网限制、token 记录、无泄露。
3. 打分器接口与 Playwright 打分器：打通生成 → 打分 → 汇总。
4. Cloudflare Worker 与网页：上传、加密、提交、查看结果、下载产出。
5. 补齐：上传产出的快速打分入口、多遍汇总、每周自检、内置 Codex、文档。
