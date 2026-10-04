# 执行层设计：在哪跑都行、与容器调度解耦、对接成熟方案

本文对应路线图（`docs/ROADMAP.md`）的目标 2、4、5。结论先说：坩埚把"一次评测"拆成几个**步骤**，每个步骤是一条 `crucible step <名字>` 命令，写清它要什么输入、产出什么、需要哪些密钥和能力；**谁来排队、分到哪台机器**交给调度方（GitHub Actions、自托管运行器、Kubernetes、Nomad，或者一台机器上顺序执行）；**一步里面要起的容器**通过"执行后端"接口启动，Docker 是第一个实现。三层各自可以替换，互不牵连。

插件化（`docs/plugins.md`，P2 的 `plugins.json`、产出运行器 `workdir`、打包器、打分器）是本文的前提：插件决定"一步里做什么"，本文只决定"这一步在哪、用什么起容器、密钥怎么送到"。插件接口不因执行层而改变。

第 5 节给出实施顺序，每一步单独上线。**已实现**：步骤层（`crucible step <名字>`，`crucible step list` 输出全部声明）、执行后端接口与 Docker 实现（沙箱按槽位分配，一台机器可同时跑多个评测）、单机运行 `crucible eval local`（用法见 §5.2 末尾）、Nomad 后端 `crucible eval nomad`（整步托管，用法见 `docs/nomad.md`）。Kubernetes 尚未开始。

---

## 1. 现状

### 1.1 一次评测由哪些步骤组成

今天所有步骤都是 GitHub Actions 的 job，在 GitHub 托管的一次性 Ubuntu 虚拟机上运行（`ubuntu-24.04`，`runs-on` 写死）。每个 job 用完即销毁，这是今天安全设计的一块基石：一台机器上发生的事不会带到下一个 job。

**题目打包**（`taskset-pack.yml` 的 `pack`）：用户上传题目包后由 Worker 触发。它用平台私钥解开上传的 zip，检查格式，把每个阶段切成 inputs 块和 tests 块，用平台公钥封好存进 blob 存储，再把生成的 `taskset.json` 报给 Worker。它不起任何容器，也不执行上传的文件。

**一次完整评测**（`eval.yml`）有五步：

1. `setup`：编译 `crucible`，`crucible plan` 校验所有输入、算出矩阵；需要时用 Worker 令牌取用户题目包的 `taskset.json`。产出两个 artifact：`crucible-bin`（程序本身）和 `taskset`。
2. `generate`（每遍一台机器）：取 agent 包（blob 来源时要用平台私钥解开），`crucible build` 用 Docker 构建 agent 镜像（此时机器上还没有模型 key）；用平台私钥只解开各阶段的 inputs 块；`tools/sandbox-net.sh up` 建沙箱网络；`crucible cred open` 解开模型 key，经管道交给 `crucible run`，后者在**自己进程里**起计量代理（8787）和出网代理（3128），都只监听沙箱网桥地址 172.31.250.1，然后按阶段 `docker run` agent 容器（2G、1 核、非 root、cap-drop ALL、no-new-privileges、`--dns 127.0.0.1`、pids 1024），每 15 分钟从宿主机直接读工作目录做快照。最后 `crucible seal-outputs` 把产出和日志用平台公钥封好，作为 artifact `gen-r<N>` 交出。
3. `score`（交接）：持平台私钥，不起容器、不跑插件。`crucible score-handoff` 解开所需阶段的 tests 块和各遍产出，用一把本次新生成的一次性钥匙重新封好，连同 `crucible`、`scorers/`、`taskset.json` 作为 artifact `score-handoff` 交出；一次性钥匙作为 job output 交出。
4. `score-tests`（`score-tests.yml`，被调用的可复用工作流）：`permissions: {}`，不 checkout，只拿到一次性钥匙（以 workflow_call secret 传入）。先 `docker build` 打分器镜像，再 `crucible score` 调用 `scorers/<名字>/score.sh`。打分器脚本自己用 Docker 起容器：Playwright 是"构建应用（`--network=none`）+ 应用容器 + 测试容器，二者在 `--internal` 网络上"；ARC-Bench 官方格式是"构建容器经出网代理容器装 npm 包 + 服务容器（无网络）+ 测试容器共享服务容器的网络"；巡天是"agent 容器 + 引擎容器，都无网络，靠 docker volume 里的两个 FIFO 对话"。`CRUCIBLE_SCORER_FIREWALL=1` 时还用 `sudo iptables` 把这些网络与宿主机、外网隔开。只交出 artifact `scores`（`score.json`）。
5. `publish`：唯一有写仓库权限的 job，不跑插件。把封好的文件存成 blob、用 `gh api` 取本次运行各 job/step 的时间戳核对用时、生成 manifest、打下载 zip、存档、按需写数据分支、回传 Worker，最后删除 Worker 里的模型 key。

**快速打分**（`score.yml`）是同一套的缩短版：`score`（交接）→ `score-tests` → `publish`，没有 `generate`。

### 1.2 步骤与信任边界

| 步骤 | 运行位置 | 起哪些容器 | 持有的密钥与权限 | 输入 | 输出 |
|---|---|---|---|---|---|
| pack | GitHub 托管机 | 无 | 平台私钥、Worker 令牌、`contents: write`（写 blob） | 上传块 hash | 题目块（blob）、`taskset.json`（发给 Worker） |
| setup | GitHub 托管机 | 无 | Worker 令牌（仅取用户题目包）、`contents: read` | 工作流参数 | artifact `crucible-bin`、`taskset`；job outputs（非秘密） |
| generate ×N | GitHub 托管机，每遍一台 | agent 镜像构建（BuildKit）、每阶段一个 agent 容器 | 平台私钥（解 inputs 块和 agent 块）、Worker 令牌（取封好的模型 key）、模型 key（只在计量代理进程里）、`sudo`（iptables） | 上一步 artifact、inputs 块（blob） | artifact `gen-r<N>`（全部已封） |
| score（交接） | GitHub 托管机 | 无 | 平台私钥、Worker 令牌 | `gen-r*`、tests 块（blob） | artifact `score-handoff`（封给一次性钥匙）、job output 一次性钥匙 |
| score-tests | GitHub 托管机 | 打分器镜像构建、应用构建、应用、测试、（巡天）agent 与引擎、（ARC 官方）出网代理 | **只有**一次性钥匙；`permissions: {}`；`sudo`（iptables） | `score-handoff` | artifact `scores`（明文分数，不含测试内容） |
| publish | GitHub 托管机 | 无 | 平台私钥、Worker 令牌、`contents: write`、`actions: read` | `gen-r*`、`scores` | blob、manifest、数据分支提交、Worker 回传 |

P3（交互运行器、打分时用模型）之后，`score-tests` 按遍拆成矩阵，并在需要时多持有提交者的模型 key（只在宿主机计量代理进程里，见 `docs/plugins.md` §10.1）。本文所有设计都按 P3 之后的形态考虑。

### 1.3 产物怎样在步骤间传递

有三条通道，规则是"凡是跨步骤的东西要么本来就公开，要么已经加密"：

- **Actions artifact**：`crucible-bin`、`taskset` 公开无妨；`gen-r*` 用平台公钥封；`score-handoff` 用一次性钥匙封；`scores` 只有分数。公开仓库的 artifact 任何登录用户都能下载，所以这条规则不能破。
- **blob 存储**（`crucible-store`，`put/get` 按 SHA-256 寻址，今天是 32 个 GitHub Release；还有一个 `dir:<路径>` 的本地目录实现，目前只用于测试）：题目块、上传的包、最终存档。
- **job output**：只用来传一次性钥匙（`score` → `score-tests`），GitHub 在 `score-tests` 里把它当 secret 打码。

### 1.4 今天哪些地方绑死在 GitHub 上

把"GitHub 特有"的东西列出来，后面每一项都要有替代：`runs-on` 写死；一次性虚拟机（安全前提）；步骤逻辑写在 YAML 的 shell 里；artifact 与 job output 传递；secrets 注入；`GITHUB_TOKEN` 用于 blob 存储读写和写数据分支；`gh api` 的 job 时间戳用于核对用时；Worker 通过 `workflow_dispatch` 触发；无密码 `sudo`；Docker 和 iptables 由托管机自带。

---

## 2. 三层拆分

```
调度层   谁来排队、分机器、并发、超时、重试、把产物从上一步搬到下一步
         GitHub Actions | 自托管运行器 | Kubernetes Job | Nomad batch job | 单机顺序执行
           │  只看见：步骤名、参数、要哪些输入包、产出哪些包、要哪些密钥、要哪类机器
步骤层   坩埚自己定义：pack / plan / generate / handoff / score-tests / publish
         每步一条命令 `crucible step <名字>`，与调度无关
           │  需要起容器时只调用：
容器层   执行后端 Executor：构建镜像、建网络、起容器、等待、停止、取日志、读写卷、自检
         Docker（第一个实现）| Kubernetes（原生 Pod）| …
```

三层的边界有一条硬规则：**上层只按名字和声明使用下层，不知道下层怎么实现**。调度层不知道一步里起几个容器；步骤层不知道自己跑在 GitHub 还是 Nomad；执行后端不知道这是评测的哪一步。

### 2.1 步骤层（坩埚定义，先做）

步骤层是最重要的一层，也是今后各种落地形态共用的部分。做法是把今天写在 YAML 里的 shell 逻辑搬进 `crucible step <名字>`，YAML 里只剩"准备好输入目录、放好密钥文件、调一条命令、把输出目录交出去"。每一步遵守同一个约定：

- 输入是若干个**目录**（`--in <包名>=<目录>`），输出是一个目录（`--out <目录>`）。跨步骤的目录里只能有公开或已加密的文件，`crucible step` 在退出前自己检查（今天 `generate` 末尾 `find ... ! -name '*.sealed' -delete` 的做法变成统一规则）。
- 密钥只从**文件**读（`--secret <名字>=<文件>`），读完不写进环境变量；调度层负责把文件放在只有这一步可读的地方（tmpfs）。今天的 `--identity-env` 保留给 GitHub，同时加 `--identity-file`。
- 非秘密参数只从命令行来，经 `crucible plan` 已有的正则校验。
- 每一步声明自己需要的"能力"（是否起容器、是否要沙箱网络、是否要访问外网），调度层据此选机器，执行后端据此自检，不满足就拒绝启动，而不是悄悄降级。

用 Rust 描述步骤的声明（伪代码）：

```rust
/// 一个步骤的静态声明：写死在 crucible 里，调度层只读它。
pub struct StepSpec {
    pub name: &'static str,                 // "generate", "handoff", ...
    pub inputs: &'static [&'static str],    // 需要的输入包名，如 ["plan", "taskset"]
    pub output: &'static str,               // 产出的包名，如 "gen-r{replica}"
    pub secrets: &'static [Secret],         // 只能是这些，见第 4 节
    pub needs: Needs,
    pub timeout: Duration,                  // 默认上限；实际由 plan 算出
    pub retry: Retry,                       // 能否整步重试（只有不改外部状态的步骤可以）
}

pub enum Secret {
    PlatformKey,        // 平台私钥
    WorkerToken,        // Worker 令牌
    StoreWrite,         // blob 存储写权限（今天是 GITHUB_TOKEN contents:write）
    RepoWrite,          // 写数据分支
    RunKey,             // 一次性钥匙（私钥那一半）
    DevModelCred,       // 开发用模型 key（封好的）
}

pub struct Needs {
    pub containers: bool,       // 要不要执行后端
    pub sandbox_net: bool,      // 要不要"只能到计量代理和出网代理"的网络
    pub internet: bool,         // 平台自己的进程（计量代理上游、出网代理、拉镜像）要不要外网
    pub pool: Pool,             // Trusted（不跑不可信代码）| Sandbox（跑不可信代码）
}
```

具体步骤：

| 步骤 | 输入包 | 输出包 | 密钥 | 能力 / 机器池 |
|---|---|---|---|---|
| `pack` | 无（参数给上传块 hash） | `taskset` | PlatformKey、WorkerToken、StoreWrite | 无容器；Trusted |
| `plan` | 无 | `plan`（含 `taskset.json`） | WorkerToken（仅用户题目包） | 无容器；Trusted |
| `generate` | `plan` | `gen-r<N>`（已封） | PlatformKey、WorkerToken 或 DevModelCred | 容器、沙箱网络、外网；Sandbox |
| `handoff` | `plan`、`gen-r*` | `handoff`（封给一次性钥匙） | PlatformKey、WorkerToken | 无容器；Trusted |
| `score-tests` | `handoff` | `scores-r<N>` | **只有 RunKey** | 容器、沙箱网络（P3 交互/评判）、外网（拉镜像、计量代理上游）；Sandbox |
| `publish` | `plan`、`gen-r*`、`scores-*` | 无（写存储、仓库、Worker） | PlatformKey、WorkerToken、StoreWrite、RepoWrite | 无容器；Trusted |

能看出一个有用的事实：**不起容器的四步（pack、plan、handoff、publish）只需要一个可信的小环境，起容器的两步（generate、score-tests）才需要"沙箱机器"**。后面各种落地形态都按这条线分机器池。

`score-tests` 的密钥清单写死为 `[RunKey]`，调度层只按清单送密钥，于是"跑测试的那一步拿不到任何平台密钥"是**构造上**保证的，不靠每个后端各自小心。加一个单元测试锁住这张表即可。

### 2.2 调度层

调度层只做四件事：按依赖顺序发起步骤（plan → generate×N → handoff → score-tests×N → publish）、选机器（按 `Pool`）、超时与重试、在步骤之间搬运输入输出包。

在 GitHub Actions 上，这些事就是工作流 YAML 本身，不需要坩埚写任何调度代码：YAML 的 `needs:` 是依赖，`runs-on` 是选机器，artifact 是搬运。所以 GitHub 这一路只是"YAML 改成调 `crucible step`"。

离开 GitHub 时需要一个"驱动"（`crucible eval <后端>`）按同样的顺序发起步骤。它很小：五步、一处扇出，不需要通用的工作流引擎。驱动通过下面的接口把步骤交给具体调度方：

```rust
/// 把一个步骤交给某个调度方运行。
pub trait Scheduler {
    /// 提交：调度方负责找机器、放好输入包和密钥文件、运行
    /// `crucible step <spec.name> ...`、收回输出包。
    async fn submit(&self, run: StepRun) -> Result<RunId>;
    /// 等待结束；返回调度方自己记录的开始/结束时间，用于核对用时
    /// （代替今天的 GitHub job 时间戳）。
    async fn wait(&self, id: &RunId) -> Result<StepOutcome>;
    async fn cancel(&self, id: &RunId) -> Result<()>;
}

pub struct StepRun {
    pub spec: &'static StepSpec,
    pub args: Vec<String>,                  // 已校验的非秘密参数
    pub inputs: Vec<(String, BundleRef)>,   // 包名 -> 包的位置
    pub secrets: Vec<(Secret, SecretRef)>,  // 必须正好等于 spec.secrets 的子集，否则 submit 报错
    pub timeout: Duration,
}

pub struct StepOutcome {
    pub ok: bool,
    pub output: Option<BundleRef>,
    pub started_at: Timestamp,              // 调度方的记录，不是步骤自报
    pub ended_at: Timestamp,
}

/// 步骤间搬运"包"（目录，内容公开或已加密）。
pub trait Bundles {
    async fn put(&self, name: &str, dir: &Path) -> Result<BundleRef>;
    async fn get(&self, r: &BundleRef, dir: &Path) -> Result<()>;
}
```

实现：

| 调度方 | `Scheduler` | `Bundles` | 时间核对来源 |
|---|---|---|---|
| GitHub Actions（托管或自托管运行器） | 不需要，YAML 就是调度 | Actions artifact | `gh api` job/step 时间戳（现状） |
| 单机 | 直接起子进程，顺序执行 | 本地目录 | 驱动进程自己的计时 |
| Kubernetes | 每步一个 `batch/v1 Job` | blob 存储（打成 tar 再 `put`） | Job 的 `status.startTime / completionTime` |
| Nomad | 每步一个 `type = "batch"` job | blob 存储 | allocation 的 task 事件时间 |

包搬运走 blob 存储是安全的，因为包里只有公开或已加密的东西；`BundleRef` 就是一个 hash，可以当普通参数传。不过 GitHub Release 存储是永久的，中间产物放进去会越积越多，所以离开 GitHub 时建议加一个 S3 兼容的 `BlobStore` 实现（MinIO、Cloudflare R2 都可以），给中间产物所在的前缀设生命周期（例如 3 天自动删除）。`BlobStore` 只有 `put/get/exists` 三个方法，新增一个实现的工作量不大。

更大的编排系统（Argo Workflows、Temporal 等）可以替代驱动，但五步的线性流程用不上它们的能力，先不引入；将来真的需要时，它们只是 `Scheduler` 的另一种用法。

### 2.3 容器层：执行后端

今天起容器的代码散在三处：`crucible build`（`docker build`）、产出运行器 `workdir`（`docker run/wait/stop/logs/rm`，见 P2 分支的 `runners/workdir.rs` 里的 `docker_run_args`）、三个打分器的 `score.sh`（直接调 `docker` 和 `sudo iptables`）。执行后端把这些收成一个接口：

```rust
/// 一步内部起容器用的接口。Docker 是第一个实现。
pub trait Executor {
    /// 这个后端能做到什么；步骤按 Needs 检查，做不到就拒绝。
    fn caps(&self) -> Caps;

    /// 构建镜像。network = None 时构建过程不能联网（今天 web-app 产出的
    /// `docker build --network=none`）；Proxy 时只能经出网代理。
    async fn build(&self, b: &BuildSpec) -> Result<ImageRef>;

    /// 建一个网络。见第 3 节。
    async fn network(&self, n: &NetSpec) -> Result<Net>;

    /// 平台进程（计量代理、出网代理）应该监听在哪、容器用什么地址找到它们。
    /// Docker：都是 172.31.250.1；Kubernetes：监听 0.0.0.0，容器用步骤 Pod 的 IP。
    fn host_endpoint(&self, net: &Net) -> HostEndpoint;

    /// 卷：工作目录、HOME、/req、打分时的产出与测试材料。
    async fn volume(&self, v: &VolSpec) -> Result<Vol>;
    async fn copy_in(&self, v: &Vol, from: &Path) -> Result<()>;
    async fn copy_out(&self, v: &Vol, to: &Path) -> Result<()>;   // 快照、取产出

    async fn start(&self, c: &ContainerSpec) -> Result<Ctr>;
    async fn wait(&self, c: &Ctr, until: Instant) -> Result<Exit>; // Exited(code) | Deadline
    async fn stop(&self, c: &Ctr, grace: Duration) -> Result<()>;
    async fn logs(&self, c: &Ctr, tail_lines: usize) -> Result<Vec<u8>>;

    /// 删除本次运行（按标签）创建的一切：容器、网络、卷、镜像。
    async fn cleanup(&self, run: &RunLabel) -> Result<()>;

    /// 网络隔离自检：在沙箱网络里跑一组探测，全部被挡住才算通过（第 3.4 节）。
    async fn self_check(&self, net: &Net) -> Result<()>;
}

pub struct ContainerSpec {
    pub image: ImageRef,
    pub cmd: Vec<String>,
    pub env: Vec<(String, String)>,     // 只放非秘密值；类型上不接受 Secret
    pub user: u32,                      // 必须非 0
    pub limits: Limits,                 // 内存（无 swap）、CPU、进程数
    pub add_caps: Vec<Cap>,             // 默认空 = cap-drop ALL；ARC 官方构建需要 CHOWN 等三项
    pub net: NetAttach,                 // 某个 Net（可带别名）| 共享另一个容器的网络 | 无网络
    pub mounts: Vec<Mount>,             // (Vol, 容器内路径, 只读?)
    pub read_only_root: bool,
    pub tmpfs: Vec<Tmpfs>,
    pub label: RunLabel,
}

pub struct Caps {
    pub sandbox_net: bool,          // 能否做到"只能到平台进程的两个端口"
    pub internal_net: bool,         // 能否做到"只能互相访问、出不去"
    pub shared_netns: bool,         // 能否让一个容器加入另一个容器的网络
    pub pids_limit: bool,
    pub runtime: Option<&'static str>, // 额外的隔离运行时，如 "gvisor"、"kata"
}
```

接口里刻意没有"特权模式""挂宿主机目录""宿主机网络"之类的选项：步骤和插件**表达不出**这些需求，后端也就不用防。

**打分器怎么接上执行后端。** 打分器是 `score.sh` 脚本，今天直接调 `docker`。为了不重写三个打分器的逻辑（构建、等就绪、重试一次这些顺序控制写在脚本里很自然），给 `crucible` 加一组对应的子命令 `crucible ctr build | net | run | wait | logs | rm | cp`，参数一一对应上面的接口；脚本把 `docker ...` 换成 `"$CRUCIBLE" ctr ...`，`sudo iptables` 那段删掉（防火墙变成 `net` 的一部分，由后端负责）。脚本因此不再关心底下是 Docker 还是 Kubernetes。这件事只在需要"原生 Kubernetes"时才必须做（第 5.3 节），自托管运行器、单机、Nomad 都继续用 Docker，`score.sh` 可以原样不动。

### 2.4 现有代码怎样迁移

按"每步都能单独上线、行为不变"的原则：

1. **`DockerExecutor`**：把 `runners/workdir.rs` 的 `docker_run_args` / `docker_out` / 快照读目录，和 `build.rs` 的 `docker build`，改成调用 `Executor`。Docker 实现里 `copy_out` 就是直接读绑定挂载的宿主机目录，`network(Sandbox)` 就是今天 `sandbox-net.sh up` 的内容（搬进 Rust 或者由 Rust 调这个脚本都可以），`self_check` 就是 `sandbox-net.sh check`。行为不变，单元测试比较生成的 `docker` 参数即可。
2. **`crucible step`**：把 `eval.yml`、`score.yml`、`taskset-pack.yml` 里每个 job 的 shell 段落搬进 `crucible step <名字>`，YAML 只剩 checkout、下载 artifact、调命令、上传 artifact。`score-tests.yml` 同理。
3. **驱动与 `Scheduler`**：先做单机实现（`crucible eval local`），再按需要做 Nomad、Kubernetes。
4. **`crucible ctr` 与打分器迁移**：只在做原生 Kubernetes 时做。

---

## 3. 网络隔离在各后端怎样实现

### 3.1 今天的做法（Docker，单机）

要隔离的有两类网络：

- **沙箱网络**（agent 生成时；P3 起交互运行和模型评判时也用）：`sandbox-net.sh` 建一个专用网桥 `crucible0`（172.31.250.0/24，关 NAT、关容器互通），在 `DOCKER-USER` 链丢弃一切进出这个网桥的转发流量（出不去，也到不了别的容器），在 `INPUT` 链只放行"从网桥到 172.31.250.1 的 TCP 8787（计量代理）和 3128（出网代理）"，其余（宿主机上的其他服务、Docker 守护进程端口等）全部丢弃，IPv6 全部丢弃。容器 `--dns 127.0.0.1`，自己解析不了任何域名；计量代理按 IP 访问，出网代理替它解析 CONNECT 目标并按白名单放行。
- **内部网络**（打分时）：`docker network create --internal`，容器只能互相访问；`CRUCIBLE_SCORER_FIREWALL=1` 时再用 iptables 丢弃发往宿主机和离开网络的流量。构建应用用 `--network=none`；ARC 官方的构建容器只能经同一内部网络上的出网代理容器装 npm 包；测试容器加入服务容器的网络命名空间。

再加上容器本身的限制：非 root、cap-drop ALL、no-new-privileges、内存（不给 swap）、CPU、进程数。

### 3.2 Kubernetes 上的等价做法

Kubernetes 的网络隔离单位是 **Pod**（同一 Pod 里的容器共享网络），规则用 **NetworkPolicy** 写，由集群的网络插件（CNI）执行。对应关系：

**命名空间。** 每次评测一个沙箱命名空间 `crucible-e-<eval_id>`，里面只放 agent、应用、测试这类跑不可信代码的 Pod，不放任何 Secret；开启 Pod Security Admission 的 `restricted` 级别（强制非 root、禁止提权、必须丢弃全部 capability、必须用 RuntimeDefault seccomp），并设 ResourceQuota。步骤 Pod（运行 `crucible step` 的那个）在另一个命名空间 `crucible-steps`。评测结束整个沙箱命名空间删除，`cleanup` 就是删命名空间。

**默认全拒。** 沙箱命名空间里放一条"选中所有 Pod、ingress 和 egress 都为空"的 NetworkPolicy，等于今天的 `DOCKER-USER` 全部丢弃。

**agent Pod 只能到计量代理和出网代理。** 计量代理和出网代理仍在步骤进程里运行（和今天一样，模型 key 只在这个进程里），步骤 Pod 带标签 `crucible/role=step, crucible/eval=<id>`。agent Pod 的 egress 规则只放行"到 `crucible-steps` 命名空间里带这两个标签的 Pod 的 TCP 8787 和 3128"。步骤 Pod 自己的 ingress 规则也只接受来自本次沙箱命名空间的这两个端口。容器拿到的地址是步骤 Pod 的 IP（`host_endpoint`）。

**没有 DNS。** agent Pod 设 `dnsPolicy: None`、`nameservers: [127.0.0.1]`，与今天的 `--dns 127.0.0.1` 相同；默认全拒本来也挡住了到集群 DNS 的流量。另外设 `enableServiceLinks: false`（不往容器里注入集群服务的环境变量）、`automountServiceAccountToken: false`（容器里没有任何集群凭据）。

**容器限制。** `runAsNonRoot`、`runAsUser`、`allowPrivilegeEscalation: false`、`capabilities.drop: [ALL]`、`seccompProfile: RuntimeDefault`；`resources.limits` 与 `requests` 相同（2Gi、1 CPU）。

**内部网络（打分）。** 应用 Pod 和测试 Pod 分开，应用 Pod 只接受来自测试 Pod 的 3000 端口，自己没有 egress；测试 Pod 的 egress 只放行到应用 Pod。构建 web-app 产出用 BuildKit，强制构建时无网络（`docker build --network=none` 在 BuildKit 里对应的就是 Dockerfile 前端的 `force-network-mode=none`）；ARC 官方构建 Pod 的 egress 只放行到同命名空间的出网代理 Pod，出网代理 Pod 才能出外网。

**可选的更强隔离。** 给 agent、应用、测试 Pod 设 `runtimeClassName: gvisor`（或 `kata`），把"共享宿主机内核"换成用户态内核或轻量虚拟机。这不是等价所必需的（今天的 Docker 也是共享内核），但公开部署建议打开。Playwright 的 Chromium 在 gVisor 下能运行，只是慢一些，打分超时要留余量。

做不到完全一样、需要处理的地方：

| 今天的做法 | Kubernetes 上的情况 | 处理 |
|---|---|---|
| iptables 由坩埚自己装 | NetworkPolicy 由 CNI 执行；有的 CNI（例如 flannel 单独使用）**根本不执行** NetworkPolicy，规则形同虚设 | `self_check` 必须真跑探测，不通过就拒绝；部署文档要求 Calico、Cilium 或 k3s 自带的策略控制器 |
| `INPUT` 链挡住到宿主机的流量 | NetworkPolicy 对"到 Pod 所在节点本身"的流量，不同 CNI 处理不同 | 探测清单加"连本节点 IP 的 kubelet 端口 10250"和"云元数据 169.254.169.254"，必须被挡住，否则拒绝 |
| `--pids-limit 1024` | Pod 规格里没有进程数上限，只能在节点上设 kubelet 的 `podPidsLimit` | 沙箱节点池统一配置；`caps().pids_limit` 由部署声明 |
| `--memory-swap 2g`（不用 swap） | 节点默认不开 swap，等价 | 无 |
| `--init`（回收僵尸进程） | 没有对应选项 | 不影响隔离，接受；agent 自己负责 |
| 测试容器加入服务容器的网络命名空间（ARC 官方） | 运行中的 Pod 不能再加普通容器 | 服务和测试放进同一个 Pod，测试容器等一个"就绪"文件再开始；或者分两个 Pod，用 NetworkPolicy 只放行测试 → 服务 |
| 巡天 agent 与引擎各自无网络、靠 FIFO 对话 | 放进同一个 Pod 后它们共享 loopback | 二者本来就要对话，只是多了一条通道，接受；Pod 本身默认全拒，出不去 |
| 每 15 分钟从宿主机直接读工作目录做快照 | 步骤 Pod 和 agent Pod 不在一起，读不到对方的卷 | agent Pod 里加一个可信的小边车容器（坩埚自己的镜像，只读挂工作目录），步骤进程经 Kubernetes exec 让它打 tar 流出来（`copy_out`）；不信任 agent 镜像里的任何程序。工作目录和 HOME 用每遍一个的 PVC（ReadWriteOnce 即可，各阶段的 agent Pod 是先后运行的） |

### 3.3 Nomad 上的等价做法

Nomad 自己没有 NetworkPolicy 这样的通用网络规则，隔离最终都要落到节点上的 iptables（或 Consul 服务网格）。有三种做法：

1. **整步托管（推荐）。** Nomad 只负责"把一个步骤派到一台沙箱节点上"：步骤是一个 batch job，用 `raw_exec`（或 `exec`）驱动直接在节点上运行 `crucible step generate ...`，步骤内部照旧用 Docker 后端起容器、照旧用 `sandbox-net.sh` 建网络。隔离与今天**完全相同**，打分器脚本一行不改。节点需要事先准备好 Docker、iptables、无密码 sudo（只限 iptables）。这其实就是"自托管运行器，只是换了个派活的人"。
2. **CNI 网络。** 每个容器是一个用 docker 驱动的 Nomad task，task group 用 `network { mode = "cni/crucible-sbx" }`，节点上放一份 CNI 配置（bridge 插件 + firewall 插件），再由节点准备脚本装上与 `sandbox-net.sh` 相同的 iptables 规则。能做到等价，但计量代理和出网代理必须和 agent 在同一节点上监听网桥地址，等于还是要节点上有一个坩埚进程在控制，比做法 1 多了一层，却没有多带来什么。
3. **Consul Connect。** 透明代理模式加上"只允许访问网格内目标"（mesh destinations only）和 intentions，可以让 agent 只能访问"计量代理"和"出网代理"两个服务。但这要求整套 Consul，并在 agent 所在的网络命名空间里跑 Envoy，UDP、DNS 等非 TCP 流量的处理也要一一核对。为了坩埚一个场景引入一整套服务网格，不划算，不采用。

所以 Nomad 上的结论是：**容器层继续用 Docker，Nomad 只做调度层**。这也符合"容器编排交给成熟方案"的本意：Nomad 管排队、选节点、资源、超时、重试，坩埚只是在被派到的节点上用本地 Docker。

### 3.4 做不到等价时：拒绝，不降级

每个后端的 `self_check` 跑同一份探测清单，从沙箱网络里的一个临时容器（非 root、无 capability）发起：

- 解析任何域名（`github.com`）；
- 直连外网 HTTP（`example.com`）和外网 IP（`1.1.1.1:443`）；
- 连宿主机或节点的常见端口（22、Docker/kubelet 端口）；
- 连云元数据地址 `169.254.169.254`；
- 连同一网络里的其他容器或 Pod；
- 连局域网网关（自托管时尤其重要，见第 5.1 节）。

任何一项**通了**，步骤立即失败，记为平台错误（可重试），不运行 agent。只有不削弱隔离的差异（`--init`、swap）允许接受。不允许出现"这个后端做不到，就不隔离了"的情况。

---

## 4. 密钥与交接在各后端

### 4.1 有哪些密钥、谁能拿到

| 密钥 | pack | plan | generate | handoff | score-tests | publish |
|---|---|---|---|---|---|---|
| 平台私钥 | ✓ | | ✓ | ✓ | **✗** | ✓ |
| Worker 令牌 | ✓ | ✓ | ✓ | ✓ | **✗** | ✓ |
| 存储写 / 仓库写 | ✓ | | | | **✗** | ✓ |
| 提交者模型 key | | | 只在计量代理进程 | 需要时解开再封给一次性钥匙 | 需要时（P3）只在计量代理进程 | 负责删除 |
| 一次性钥匙 | | | | 生成（见下） | ✓ | |

三条不变的规则：

1. **按清单送。** 调度层只送 `StepSpec.secrets` 里列的东西；`score-tests` 的清单是 `[RunKey]`，构造上就拿不到别的。
2. **模型 key 只进计量代理。** 封好的 key 由平台私钥解开后经管道交给计量代理进程（今天 `crucible cred open | crucible run` 的做法），不进环境变量、不进文件、不进容器；容器只拿到计量代理地址和 `dummy`。各后端都保持这一点：Kubernetes 原生模式下计量代理在步骤 Pod 里，agent Pod 只拿到步骤 Pod 的 IP。
3. **以文件形式交付，放在内存盘上。** 密钥以文件（tmpfs，权限 0400）交给步骤，`crucible` 读一次；不再用环境变量（环境变量会被子进程继承，今天靠 `env -u` 逐个去掉）。

**一次性钥匙改由驱动生成。** 今天一次性钥匙由交接步骤生成，经 GitHub job output 传给 `score-tests`。离开 GitHub 后没有"带打码的 job output"，所以改成：驱动在交接之前生成一对一次性钥匙，**公钥**作为普通参数交给交接步骤（`crucible score-handoff --recipient <公钥>`），**私钥**作为 `RunKey` 交给 `score-tests`。交接步骤从此不再产生任何秘密输出，也就不需要秘密通道把东西"往下游递"。GitHub 工作流可以保持现状不变。

### 4.2 Kubernetes

- 平台私钥、Worker 令牌、存储写凭据各是 `crucible-steps` 命名空间里的一个 Secret，以只读卷挂进需要它的步骤 Job（`defaultMode: 0400`，卷类型本身就在内存里）。Job 的模板由驱动按 `StepSpec.secrets` 生成，`score-tests` 的 Job 模板里只会出现一次性钥匙那个 Secret。
- 一次性钥匙：驱动创建 `crucible-runkey-<eval_id>` Secret，只挂给本次的 `score-tests` Job，评测结束删除（同时设一个兜底清理，例如标签加定时清理）。
- 要注意 Kubernetes 的一条规则：**能在某个命名空间建 Pod，就能读该命名空间所有 Secret**（把它挂进自己建的 Pod 即可）。所以：沙箱命名空间里不放任何 Secret；原生模式下步骤 Pod 需要在沙箱命名空间建 Pod，它的 ServiceAccount 只有"在 `crucible-e-*` 命名空间建删 Pod、PVC、NetworkPolicy"的权限，而那里没有 Secret；加上 `restricted` 准入，被攻破的步骤也建不出特权 Pod。所有沙箱 Pod 都 `automountServiceAccountToken: false`。
- 驱动自己只需要"在 `crucible-steps` 里建 Job、建删一次性钥匙 Secret"的权限，它引用平台私钥 Secret 的**名字**，自己读不到内容。

### 4.3 Nomad

- 平台私钥等放在 Nomad Variables 里，路径按 job 划分（例如 `nomad/jobs/crucible-generate`），Nomad 的工作负载身份（workload identity）默认只允许一个 job 读自己路径下的变量。job 里用 `template` 把变量渲染进 task 的 `secrets/` 目录（task 私有的内存盘），`crucible step` 从那里读文件。
- 一次性钥匙：驱动写 `nomad/jobs/crucible-score-tests-<eval_id>/runkey`，`score-tests` job 读取，结束后驱动删除。驱动的 ACL 令牌只有 `nomad/jobs/crucible-*` 的写权限。
- 注意：`raw_exec` 驱动的 task 和同一节点上其他 `raw_exec` task 是同一个系统用户，彼此能读对方的文件。所以 Nomad 上**可信步骤和沙箱步骤必须分节点池**（`crucible-trusted` / `crucible-sandbox`），沙箱节点同一时间只跑一个步骤（见第 5.1 节的并发说明）。

### 4.4 单机

一台机器上所有步骤先后运行，进程之间可以做到"`score-tests` 进程的环境和可读文件里没有平台私钥"（密钥文件只在需要它的步骤运行期间存在，用完删除），但挡不住**容器逃逸后留后门、等下一次 generate 时偷钥匙**。所以单机运行默认用**本机自己生成的一对钥匙**（`crucible keys gen`），不用平台私钥，结果也不进官方排行榜（见第 6 节）。管理员确实要在单机上用平台私钥跑官方题目，属于明知风险的自用，不对外开放上传。

---

## 5. 四种落地形态与实施顺序

推荐顺序：**a 自托管运行器 → 步骤层抽出 → b 单机 → d Nomad → c Kubernetes**。a 几乎只改配置，马上能用；b 逼着把步骤层和驱动做出来，c、d 都复用它；d 因为容器层继续用 Docker，工作量小；c 需要原生执行后端和打分器迁移，放最后。

验证一律只做最小真跑：**hello-world 两阶段**（产出运行器 + Playwright 打分）和 **巡天 L1**（`astro-practice` 的 `l1`，files 打包 + 巡天打分器），确认两次都出分、沙箱自检通过、manifest 格式正确。不做与 GitHub 托管结果逐项对比的一致性检查。

### 5.1 a. 自托管运行器

**要改什么：**

- 工作流的 `runs-on` 可配置，只对**起容器的两步**（`generate`、`score-tests`）开放，用仓库变量而不是工作流参数（参数可由 Worker 代用户填写，不能让用户决定跑在哪台机器上）：
  ```yaml
  runs-on: ${{ fromJSON(vars.CRUCIBLE_RUNS_ON_GENERATE || '"ubuntu-24.04"') }}
  ```
  设成 `["self-hosted","linux","x64","magicbook"]` 即切到测试机。`score-tests.yml` 是被调用的工作流，加一个 `runs_on` 输入，由调用方传 `vars.CRUCIBLE_RUNS_ON_TESTS`。`setup`、`score`、`publish`、`pack` 继续在 GitHub 托管机上跑：它们持平台私钥和写权限，又很轻，没有理由搬走。`ci.yml`、`scorer.yml` 由 pull_request 触发，**永远写死 GitHub 托管机**。
- 每个起容器的 job 开头先清理上一次可能残留的东西：`docker ps -aq --filter name=crucible-` 删除、`sandbox-net.sh down`。托管机一次性，自托管机不是。
- `CRUCIBLE_PRUNE_BUILD_CACHE=1` 只在托管机上设（`runner.environment == 'github-hosted'`），否则会清掉整台机器的构建缓存。
- `sandbox-net.sh` 适配常见发行版：
  - 检查 Docker 的防火墙后端。新版 Docker 可以选 nftables 后端，那种模式下没有 `DOCKER-USER` 链，脚本必须检测到并直接报错，提示在 `daemon.json` 里用 iptables 后端；
  - 系统 `iptables` 是 nf_tables 版（`iptables-nft`）没问题，但要和 Docker 用的是同一套（不能一个 legacy 一个 nft）；
  - 探测清单加上"宿主机默认网关"和"局域网常见地址"（自托管机通常在办公网里，这是托管机上没有的风险）。
- 网桥名、网段、端口目前写死，一台机器同一时间只能跑一个 `generate`。先保持"一台机器只注册一个运行器"；以后需要并发时再给 `sandbox-net.sh` 和 `crucible run` 加"槽位号"（不同槽位用不同网段和端口）。

**前置条件清单（测试机 magicbook，WSL Ubuntu）：**

- x86-64 Linux（WSL2 需开启 systemd），Docker（含 buildx），iptables 防火墙后端；运行器用户 `yao` 在 docker 组；
- `sudo` 免密，**只限** `/usr/sbin/iptables`、`/usr/sbin/ip6tables`（sudoers 写成 `yao ALL=(root) NOPASSWD: /usr/sbin/iptables, /usr/sbin/ip6tables`）。注意 docker 组本身就等于 root，所以这台机器要按"整机交给评测"来对待；
- `jq`、`git`、`curl`、`unzip`、coreutils `timeout`；
- 磁盘 50 GB 以上空闲（agent 镜像、打分器镜像、Playwright 浏览器）；
- 172.31.250.0/24 不与 WSL 或办公网冲突；
- 运行器按仓库注册（不是组织级），标签 `self-hosted, linux, x64, magicbook`；只在测试时启动，测完停止并注销。

**安全建议（写进部署文档）：**

公开仓库会在这台机器上运行用户上传的不可信 agent 和不可信测试。托管机一次性使用，自托管机不是，于是第 1 节说的那块基石没了：一次容器逃逸可以在机器上留下后门，等之后的 `generate` job 带着平台私钥和模型 key 上来时偷走它们。WSL 还多一层：逃逸后能访问 Windows 的文件（`/mnt/c`）和办公网。所以：

- magicbook 只用于测试，不常驻；测试期间机器上不放其他凭据，测完停掉运行器；
- 长期自托管必须用**专门的隔离机器**：最好每个 job 重建（运行器 `--ephemeral`，配合每次从干净镜像起的虚拟机），放在独立网段，出口只放行 GitHub、模型接口和软件源；
- 运行器只服务本仓库，并限定只能被 `eval.yml`、`score.yml`、`score-tests.yml` 使用（组织级运行器组可以限定工作流）；仓库设置"外部贡献者的工作流需要审批"；
- 如果只能有一台长期机器，优先只把 `generate` 放上去（agent 不可信，但题目包测试代码不在这里），`score-tests` 留在托管机；或者反过来，二者不要长期共用一台。

**工作量：** 半天到一天（工作流变量、清理步骤、`sandbox-net.sh` 检查、部署文档）。

**验证：** 在 magicbook 注册运行器，设好两个变量，用 `workflow_dispatch` 跑 hello-world 两阶段（内置 agent）和巡天 L1；看 `sandbox check` 全部 blocked、两次都出分；再在自托管机上跑一次 `scorer.yml` 的恶意测试包（`hostile/`），要求 5/5。

### 5.2 b. 单机运行：`crucible eval local`

**要改什么：**

- 前提是步骤层抽出（第 2.4 节第 1、2 步）：`DockerExecutor`、`crucible step <名字>`、密钥从文件读、`score-handoff --recipient`。
- 新命令：
  ```
  crucible eval local --agent builtin:octos | dir:<路径> \
      --taskset hello-world | <题目包目录> --stages 2 --replicas 1 \
      --model <名字> --endpoint https://... --cred-file <文件，或 - 表示 stdin> \
      --store dir:./crucible-store | github:<owner>/<repo> \
      --keys <本地钥匙文件> --out ./eval-<id>
  ```
  驱动按顺序跑 plan → generate（每遍一次，先后进行）→ handoff → score-tests → publish，包就是 `--out` 下的子目录，一次性钥匙由驱动生成。`publish` 在本地模式下只写 `manifest.json` 和加密存档到 `--out`，不写数据分支、不回传 Worker；用时取驱动计时，manifest 记 `timing_source: local`（今天 `--jobs` 本来就是可选的）。
- 题目从哪来：官方题目包的块是用平台公钥封的，没有平台私钥打不开。所以本地运行的题目要么是用本地钥匙重新打包的（`crucible taskset pack --zip <源> --keys <本地钥匙> --store dir:...`，仓库里 `tasksets/*/source/` 就是公开的源），要么是管理员持平台私钥读 `github:` 存储。
- 只要求 Linux + Docker + 免密 sudo（iptables）；macOS 上的 Docker Desktop / colima 没有 `DOCKER-USER`，`self_check` 不通过就拒绝。

**工作量：** 步骤层抽出 2–3 天；本地驱动与本地钥匙 1–2 天。

**验证：** 在 magicbook 上用本地钥匙打包 hello-world 和 astro-practice，`crucible eval local` 跑 hello-world 两阶段和巡天 L1，两次都出分。不需要 GitHub 网络以外的任何服务（模型接口除外）。

**用法（已实现）：**

```
# app 模式：给一个产出 zip 打分（不需要模型）
crucible eval local --root <仓库> --taskset hello-world --app app.zip --stage 1
# agent 模式：模型 key 以文件给出，{"api_key": "...", "endpoint": "https://..."}
crucible eval local --root <仓库> --taskset hello-world --agent builtin:octos \
    --stages 2 --replicas 1 --model glm-5.3-flash --cred-file ~/model-cred.json
```

- `--taskset` 是 `tasksets/` 下的名字，或一个含 `source.json` 的目录；每次从公开源用本地钥匙打包（内容寻址，重复打包不占空间）。
- 本地钥匙默认 `~/.crucible/keys`（缺则自动生成，也可 `crucible keys gen --out <目录>`），存储默认 `dir:~/.crucible/store`，结果在 `--out`（默认 `./crucible-evals`）`/<eval id>/`：`gen/`（封好的产出与日志）、`scores/`、`publish/manifest.json` 与 `manifest.sealed`、`steps.json`（驱动计时）。manifest 记 `timing_source: local`。
- 模型 key 只在 `--cred-file` 里（`-` 表示从标准输入读），驱动读到后立即用本地公钥封好，之后与 GitHub 上的开发凭据走同一条路：只在需要它的步骤进程里、只在计量代理里解开。app 模式只有题目包的打分槽位用模型时才需要它。
- 每步是一个子进程，密钥以文件放在 `/dev/shm` 下的新目录、只放该步清单里的、步骤结束即删；一次性钥匙由驱动生成，私钥只交给 `score-tests`。
- 同时跑多个：直接再开一个 `crucible eval local`。每个用沙箱的步骤自己占一个空闲槽位（`crucible-sbx[-k]`、网桥 `crucible<k>`、`172.31.(250-k).0/24`），agent 镜像按评测打标签，结束时网桥、iptables 规则、容器、镜像都回收。
- 前提：Linux、Docker（iptables 防火墙后端）、当前用户在 docker 组、`sudo -n iptables` 免密。

### 5.3 c. Kubernetes 后端

**要改什么：**

- `K8sScheduler`：每步一个 Job（`backoffLimit: 0`，重试由驱动按 `StepSpec.retry` 决定；`activeDeadlineSeconds` = 步骤超时）。可信步骤和沙箱步骤用不同节点池（nodeSelector + taint）。
- `K8sExecutor`：第 3.2 节的全部内容。镜像构建用集群里的 BuildKit（无根模式），镜像推到集群内的镜像仓库，按评测打标签、结束后删除。
- `crucible ctr` 子命令和三个打分器脚本的迁移（`docker` → `crucible ctr`，删去 iptables 段）。
- `S3Store`：S3 兼容的 `BlobStore` 实现，用于包搬运（也可以作为正式存储，见第 6 节）。
- 部署清单 `deploy/k8s/`：命名空间、RBAC、Pod Security 标签、默认全拒策略、可选 RuntimeClass、BuildKit、镜像仓库、MinIO。

另一条更快的路：Job 里直接跑"Docker in Docker"，步骤内部照用 Docker 后端，打分器不改。但这要求特权容器，只有在节点本身是一次性虚拟机或用 Kata 这类虚拟机运行时时才可接受，等于把 Kubernetes 当成"派虚拟机的工具"。这条路是否值得作为过渡，见第 6 节。

**工作量：** 约 2–3 周（执行后端 1 周，打分器迁移与 BuildKit/镜像仓库 1 周，部署清单和自检 2–3 天）。

**验证：** magicbook 上起一个 k3s（自带 NetworkPolicy 执行）或 kind + Calico；先跑 `self_check`（第 3.4 节清单全部被挡住，故意换成不执行策略的 CNI 时必须拒绝）；再跑 hello-world 两阶段和巡天 L1。

### 5.4 d. Nomad 后端

**要改什么：**

- `NomadScheduler`：每步一个 `type = "batch"` 的 job，通过 Nomad HTTP API 提交；`raw_exec` 运行 `crucible step`；沙箱步骤约束到 `crucible-sandbox` 节点池，可信步骤到 `crucible-trusted`；密钥按第 4.3 节放在 Variables 里，用 `template` 渲染到 `secrets/`。
- 容器层不变（Docker 后端），沙箱节点按第 5.1 节的前置条件准备。为保证"同一节点同一时间只跑一个沙箱步骤"，沙箱步骤申请足够大的资源（例如节点全部 CPU），或者等槽位支持做好再放开。
- 包搬运用 blob 存储（`S3Store` 或现有 GitHub Release 存储）。

**工作量：** 约 1 周（调度实现 3 天，job 模板、Variables、节点准备文档 2 天）。

**验证：** magicbook 上 `nomad agent -dev`（客户端配置里启用 `raw_exec`），跑 hello-world 两阶段和巡天 L1。

**已实现（`crucible eval nomad`，`docs/nomad.md`）。** 与上面的差别：包搬运不走 blob 存储，而是评测目录与 `dir:` 存储放在各节点同一路径的共享盘上（单节点就是本机硬盘）；节点池默认都是 `default`，多节点时用 `--trusted-pool` / `--sandbox-pool` 分开；沙箱槽位已支持并发，所以不再要求"一个节点同时只跑一个沙箱步骤"。密钥按 §4.3：每个 job 一个自己的 Variable，`template` 渲染到 `secrets/`，job 规格按步骤清单生成。

### 5.5 完全离开 GitHub 还差什么

执行离开 GitHub 之后，还有三处依赖：**触发**（Worker 用 `workflow_dispatch` 触发）、**存储**（GitHub Release）、**登录**（GitHub OAuth）。触发建议改成"拉"：自己机器上常驻一个 `crucible agentd`，定时向 Worker 领取排队中的评测，再交给本地/Nomad/Kubernetes 驱动执行。这样自己的机器不需要开放任何入站端口，Worker 也不需要知道执行在哪里。存储换成 S3 兼容存储（Cloudflare R2 与 Worker 同一家，免费额度够用）。登录与执行无关，可以继续用 GitHub 账号。

---

## 6. 需要拍板的问题

1. **Kubernetes 走哪条路。** 原生执行后端（Pod + NetworkPolicy，隔离粒度最好，约 2–3 周，要迁移打分器）还是先做"Job + 虚拟机运行时 + 内部 Docker"的过渡（快，但依赖 Kata 一类的节点配置）？建议：直接做原生，不做过渡；而且排在 Nomad 之后，到真有 Kubernetes 集群要用时再开工。
2. **自托管机上能不能放平台私钥。** `generate` 需要平台私钥解开 inputs 块和 agent 块。长期自托管机若不是每次重建，就有被留后门偷钥匙的风险。选项：只允许一次性重建的机器；或先做 DESIGN §8 提到的"inputs 与 tests 分两把钥匙"，让 `generate` 只持有 inputs 钥匙（泄露也看不到测试）。
3. **`score-tests` 能不能上自托管机。** 它运行用户上传的测试代码。建议默认留在 GitHub 托管机，只把 `generate` 放到自托管机。
4. **非 GitHub 运行的成绩能不能进排行榜。** 用时核对的来源变成了自己的调度方，单机运行甚至只有自己的计时。建议：单机运行一律不进榜；Nomad/Kubernetes 只有平台自己运营的集群算官方，manifest 记下执行后端和时间来源，网页标注。
5. **离开 GitHub 时的触发和存储。** 是否同意用"`crucible agentd` 向 Worker 领取任务"的拉模式，以及用 Cloudflare R2（或其他 S3 兼容存储）替代 GitHub Release？
6. **单台机器的并发。** 现在一台机器同一时间只能跑一个 `generate`（网桥和端口写死）。高配服务器上是否马上需要并发？需要的话，槽位支持放进第 5.1 节一起做（约 1 天）。
