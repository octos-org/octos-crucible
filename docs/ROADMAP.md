# 路线图

## 目标

坩埚只做评测本身：评什么（题目包）、怎么让 agent 干活（运行器）、怎么打分（打分器）、怎么客观计量（用时、token、等价花销）。算力、排队、容器编排都交给成熟方案。具体是五件事：

1. **想测什么都能测**：题目、运行方式、打包、打分都是插件。写代码、交互决策、数学证明只是不同的插件组合。
2. **在哪跑都行**：默认用 GitHub Actions（免费）；有高配服务器时，评测可以在自己的机器上跑。
3. **排名**：用户选择公开的成绩按题目包形成排行榜。
4. **与容器调度解耦**：机器、队列、容器编排都交给成熟方案，坩埚只描述"要跑什么"。
5. **对接成熟方案**：评测步骤可以落到不同的执行后端（GitHub Actions、自托管运行器、Kubernetes 等）。

## 现状

| 目标 | 已做到 | 正在做 | 还没做 |
|---|---|---|---|
| 想测什么都能测 | 结果格式与展示插件化（P1）；Playwright、ARC-Bench 官方格式、巡天打分器；用户上传题目包；用户上传打分器插件（`docs/plugins.md` §14）；用户插件与上传的流程加固（隔离构建、镜像只构建一次、结果封装回传、模型插件与计量代理分离、管理员审核、配额与限流、卡住记录自动失败，§14.6） | 插件登记表、打包器与运行器插件化（P2）；交互运行器与打分时用模型（P3）；数学证明示例（大模型评判） | 用户上传运行器插件 |
| 在哪跑都行 | GitHub Actions 上完整运行；起容器的两步可放到自托管运行器（`docs/self-hosted.md`）；单机运行 `crucible eval local`；Nomad 集群 `crucible eval nomad`（`docs/nomad.md`）；Kubernetes 集群 `crucible eval k8s`（`docs/kubernetes.md`）；两者的多节点（可信/沙箱分节点池，Kubernetes 评测卷用 ReadWriteMany）在单机模拟集群上真跑通过；隔离加固：Kubernetes 命名空间强制 Pod Security `restricted`、多节点强制分节点池、可选 RuntimeClass（gVisor/Kata），Docker 类后端可选 gVisor（`CRUCIBLE_DOCKER_RUNTIME=runsc`）；ARC 官方格式打分器可在 Kubernetes 上运行 | — | 真实多台机器上的多节点运行；Kubernetes 用户命名空间（`hostUsers: false`） |
| 排名 | 公开成绩写入数据分支；排行榜页面（`#/leaderboard`，只计完整阶段的评测） | — | — |
| 与容器调度解耦 | 机器、排队、并发交给 GitHub Actions、Nomad 或 Kubernetes；Kubernetes 后端下每个容器是一个 Pod，构建走集群内 BuildKit | — | Docker 类后端（GitHub、自托管、单机、Nomad）里一台机器内的容器仍由坩埚自己编排 |
| 对接成熟方案 | 评测步骤由工作流加命令行工具组成（`crucible step`）；执行后端接口与 Docker 实现；Nomad 调度后端；原生 Kubernetes 后端（Pod + NetworkPolicy，插件经 `crucible ctr` 起容器） | — | — |

## 下一步

1. ~~**排行榜**~~：已完成（PR #35）。
2. ~~**自托管运行器**~~：已完成，见 `docs/self-hosted.md`。
3. ~~**单机运行**~~（已完成，`crucible eval local`）：`crucible` 命令行在一台装有 Docker 的机器上按同样步骤完成一次评测（生成、交接、打分、汇总），不需要 GitHub。
4. **执行后端抽象**：把"启动一个容器并等待结束"抽成接口，Docker 是第一个实现（已完成）；Nomad 调度后端已完成（`docs/nomad.md`）；原生 Kubernetes 后端已完成（`docs/kubernetes.md`）；两者的多节点支持已完成，单机模拟集群脚本在 `deploy/k8s/sim-multinode.sh`、`deploy/nomad/sim-multinode.sh`；隔离加固（Pod Security `restricted`、强制分节点池、RuntimeClass、Docker 可选 gVisor、挂载检查）已完成，见 `docs/kubernetes.md`"安全加固"、`docs/executors.md` §3.6。
5. **用户可上传的插件**：打分器已开放（`docs/plugins.md` §14）：网页“插件”页或 `crucible plugin upload` 上传 `plugin.json` + `Dockerfile`，在 GitHub 托管机上构建并自检，登记为 `u-<16 hex>`，用户题目包用 `"scorer": {"name": "u-…"}` 引用。运行器插件留后；流程加固已完成，做法与仍然成立的边界见 §14.6。

第 2、3、4 项的设计（步骤层、调度层、执行后端，网络隔离与密钥在各后端的做法，自托管运行器、单机、Kubernetes、Nomad 的落地顺序）见 `docs/executors.md`。

第 1 项与插件化工作互不影响，可以并行；第 2、3、4 项会改动工作流和运行代码，排在插件化 P2、P3 合并之后。
