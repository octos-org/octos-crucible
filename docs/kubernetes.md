# Kubernetes 后端：`crucible eval k8s`

原生 Kubernetes 后端：每次评测一个命名空间，每个步骤一个 Job，步骤里起的每个容器（agent、应用、测试、打分器）都是一个 Pod，网络隔离用 NetworkPolicy，镜像用集群内的 BuildKit 构建、存在集群内的镜像仓库。步骤顺序与 `crucible eval local`、`crucible eval nomad` 相同（generate × 遍数 → handoff → score-tests × 遍数 → publish）。设计见 `docs/executors.md` §3.2、§4.2、§5.3。

目前是原型验证阶段：没有为防攻击额外加限制，已有的隔离都保留；密钥用本机钥匙（`crucible keys gen`），结果不进排行榜。数据和存储都在集群自己的卷上，不用付费服务。

## 组成

| 部分 | 在哪 | 做什么 |
|---|---|---|
| 驱动 `crucible eval k8s` | 有 `kubectl`（集群管理员权限）的机器 | 建命名空间、把评测目录拷到卷上、按顺序提交每步的 Job 和 Secret、取回结果、删命名空间 |
| 步骤 Job | 评测命名空间 `crucible-e-<eval id>` | 在步骤镜像里运行卷上的 `crucible step <名字>` |
| 执行后端 `K8sExecutor` | 步骤 Pod 里（`CRUCIBLE_EXECUTOR=k8s`） | 用 `kubectl` 在同一命名空间里起 Pod、建 NetworkPolicy；`buildctl` 构建镜像 |
| 镜像仓库、BuildKit | 命名空间 `crucible-system`（`deploy/k8s/crucible-system.yaml`） | 存放 agent、打分器、被测应用的镜像；在集群内构建 |
| 步骤镜像 `crucible-step` | 集群内构建（`deploy/k8s/step-image.yaml`） | bash、coreutils、git、curl、tar、kubectl、buildctl；不含 `crucible` 本身 |

## 安装（单节点 k3s）

k3s 自带 NetworkPolicy 控制器（kube-router），flannel 单独使用时不执行 NetworkPolicy，所以不能换成"只有 flannel"的发行版；其他集群要用 Calico、Cilium 这类执行策略的 CNI。后端在每次用沙箱前都会实测，不执行策略就拒绝运行（见"网络隔离"）。

```bash
# 1. k3s：不装用不到的组件；限制每个 Pod 的进程数（容器的 pids 上限由节点给）
curl -sfL https://get.k3s.io -o k3s-install.sh
sudo INSTALL_K3S_SKIP_ENABLE=true \
  INSTALL_K3S_EXEC="server --disable traefik --disable servicelb --disable metrics-server --write-kubeconfig-mode 644 --kubelet-arg=pod-max-pids=1024" \
  sh k3s-install.sh

# 2. 让节点能从集群内镜像仓库（固定地址 10.43.200.200:5000，HTTP）拉镜像
sudo mkdir -p /etc/rancher/k3s
sudo tee /etc/rancher/k3s/registries.yaml <<'EOF'
mirrors:
  "10.43.200.200:5000":
    endpoint:
      - "http://10.43.200.200:5000"
EOF
sudo systemctl start k3s

# 3. 仓库、BuildKit、步骤的 ClusterRole；然后在集群内构建步骤镜像
kubectl apply -f deploy/k8s/crucible-system.yaml
kubectl -n crucible-system rollout status deploy/registry deploy/buildkitd
kubectl apply -f deploy/k8s/step-image.yaml
kubectl -n crucible-system wait --for=condition=complete job/step-image --timeout=15m
```

`10.43.200.200` 在 k3s 默认的服务网段（10.43.0.0/16）里；集群服务网段不同时，改 `crucible-system.yaml` 里的 `clusterIP`、`registries.yaml`、`step-image.yaml` 的输出地址，并给 `crucible eval k8s` 传 `--registry`、`--step-image`。集群 DNS 不是 `10.43.0.10` 时传 `--cluster-dns`。

`crucible` 程序在每次评测时由驱动拷到卷上，在步骤镜像里运行，所以它必须是 Linux 程序，且与步骤镜像的 glibc 兼容：步骤镜像用与编译机相同的 Ubuntu（26.04）。换编译机时相应改 `step-image.yaml` 的基础镜像。

## 用法

参数与 `crucible eval local` 相同（`docs/executors.md` §5.2 末尾），另加：

```bash
# app 模式
crucible eval k8s --root <仓库> --taskset hello-world --app app.zip --stage 1 --pids-limited
# agent 模式
crucible eval k8s --root <仓库> --taskset hello-world --agent builtin:octos \
    --stages 2 --model glm-5.3-flash --cred-file ~/model-cred.json --pids-limited
```

| 参数 | 默认 | 说明 |
|---|---|---|
| `--step-image` | `10.43.200.200:5000/crucible-step:1` | 步骤 Job 的镜像 |
| `--registry` | `10.43.200.200:5000` | 集群内镜像仓库 |
| `--buildkit` | `tcp://buildkitd.crucible-system:1234` | BuildKit 地址 |
| `--cluster-dns` | `10.43.0.10` | 集群 DNS 服务地址（网络闸门和探测用） |
| `--volume-size` | `20Gi` | 每次评测的卷大小 |
| `--storage-class` | 集群默认 | 卷的存储类 |
| `--pids-limited` | 否 | 声明节点限制了每个 Pod 的进程数（kubelet `pod-max-pids`）；不声明时起容器的步骤拒绝运行 |
| `--crucible-bin` | 驱动自己 | 拷到卷上的 `crucible`（Linux） |
| `--step-timeout-s` | 21600 | 每步上限（Job 的 `activeDeadlineSeconds`） |

`--store` 不可用：每次评测的存储（`dir:`）就在评测目录里、跟着卷走。结果在 `--out`（默认 `./crucible-evals`）`/<eval id>/`：`gen`、`scores`、`publish`（manifest，`timing_source: k8s`）、`store`、`steps.json`（时间取 Pod 里容器的开始/结束时间）。各步输出实时打印到驱动的标准错误。eval id 默认 `k8s-<时间>-<随机>`。

中途打断驱动时，残留的东西都在命名空间里：`kubectl delete namespace crucible-e-<eval id>`。

## 一次评测在集群里是什么样

**命名空间** `crucible-e-<eval id>`（每次评测一个，结束即删，卷一起删）：

- `default-deny`：选中所有 Pod，入站、出站都为空；`steps-out`：步骤 Pod 可以出站（模型上游、git、API server、BuildKit）。
- PVC `data`：评测卷。驱动先经一个装载 Pod（`kubectl exec ... tar`）把本地评测目录拷进去：步骤根目录（agents、scorers、runners、tools、config 的拷贝）、generate 专用的根目录（只有 agents 和 config）、题目包 `taskset.json`、本次评测的 `dir:` 存储（题目包块、上传的产出，都是封好的）、`bin/crucible`。结束时只取回 `gen`、`scores`、`publish`、`store`；交接包、工作目录、临时文件不离开集群。
- ServiceAccount `crucible-step`，用 RoleBinding 绑到 ClusterRole `crucible-step`：只能管**本命名空间**的 Pod（建、删、看、日志、打标签）和 NetworkPolicy，没有 Secret 权限。

**每步一个 Job**（`backoffLimit: 0`，失败不由 Kubernetes 重试）：Pod 以 1000:1000 非 root 运行，drop ALL，`allowPrivilegeEscalation: false`，卷挂在 `/crucible`，`TMPDIR` 在卷上。只有起容器的步骤（generate、score-tests）挂服务账号令牌、带执行后端的设置（`CRUCIBLE_EXECUTOR=k8s` 等，Pod IP / 节点名经 downward API）；handoff、publish 没有任何集群凭据。

**密钥**：每步一个 Secret，内容按 `crucible step list` 里这一步的清单生成，清单外的密钥直接报错；Pod 里每个密钥用 `subPath` 挂成 `/run/crucible/secrets/<名字>`（目录里没有别的文件；`crucible step` 自己也拒绝清单外的文件）。所以 `score-tests` 只可能拿到一次性钥匙（单元测试锁住）。步骤结束（无论成败）驱动删 Job 和 Secret。步骤是一个接一个跑的，一个起容器的步骤运行时命名空间里只有它自己的 Secret。

## 容器怎样变成 Pod

步骤（和插件脚本经 `crucible ctr`）给出的容器规格逐项对应：

| 容器规格 | Pod |
|---|---|
| `--user uid:gid` | `runAsUser`、`runAsGroup`，非 0 时 `runAsNonRoot` |
| `--cap-drop` / `--cap-add` | `capabilities.drop` / `add`（agent：drop ALL） |
| `no-new-privileges` | `allowPrivilegeEscalation: false` |
| — | `seccompProfile: RuntimeDefault`、`automountServiceAccountToken: false`、`enableServiceLinks: false`、`restartPolicy: Never` |
| `--read-only` | `readOnlyRootFilesystem` |
| `--memory`、`--cpus` | `limits`，`requests` 与之相同 |
| `--pids-limit` | 节点的 `pod-max-pids`（`--pids-limited` 声明） |
| `--tmpfs`、`--shm-size` | 内存 `emptyDir`（`/dev/shm`） |
| `--init` | `shareProcessNamespace`（pause 进程回收僵尸进程） |
| 宿主机路径挂载 | 评测卷的 `subPath`；不在卷下的路径直接报错 |
| 命名卷 | 卷上的 `.volumes/<名字>` |
| `--platform` | `nodeSelector kubernetes.io/arch` |
| — | 节点亲和：与步骤 Pod 同一节点（`local-path` 这种 ReadWriteOnce 卷可用） |

agent 的工作目录、HOME、`/req` 都是卷上的目录，步骤进程直接读工作目录打快照，不需要边车容器。

## 网络隔离

- **默认全拒**：命名空间的 `default-deny`。
- **内部网络**（打分时的应用与测试）：成员带标签 `net.crucible/<网络>=1`，这个网络的策略只允许成员互访；别名（例如 `app`）写成后起 Pod 的 `hostAliases`，Pod 没有 DNS（`dnsPolicy: None`，`127.0.0.1`）。
- **沙箱**（agent、要用模型的交互运行器和打分器）：成员只能到步骤 Pod 的给定 TCP 端口（计量代理 8787、出网代理 3128），步骤 Pod 也只接受它们的这几个端口；成员之间不通。计量代理和出网代理在步骤进程里，模型 key 不离开步骤进程。
- **默认网络**（不带 `--network` 的容器，例如 ARC 官方打分器的出网代理）：只能出公网（私网、CGNAT、链路本地、回环地址除外）和集群 DNS。
- **新 Pod 的策略空窗**：在 k3s 上实测，新 Pod 的第一秒策略还没生效（能连上集群 DNS）。所以每个非公网 Pod 都有一个 init 容器 `netgate`，等到集群 DNS 连不上（策略已生效）才让主容器启动；2 分钟内一直连得上（CNI 不执行 NetworkPolicy）Pod 就失败。
- **自检**：每次建沙箱后，在沙箱里跑一个探测 Pod：集群 DNS、外部 DNS、`1.1.1.1` 的 HTTP 和 443、API server、元数据地址 `169.254.169.254`、网关和节点的 kubelet 10250、节点 22、集群内镜像仓库。任何一项连通就拒绝运行，记为步骤失败。单独跑：在步骤 Pod 里 `crucible ctr sandbox-check`。

## 镜像

`crucible-*` 名字的镜像（agent、打分器、交互运行器、被测应用）映射到集群仓库 `10.43.200.200:5000/crucible-*`，构建走 BuildKit（`buildctl`，Dockerfile 前端，推到仓库）。`--network=none` 的构建（被测应用）对应 `force-network-mode=none`。agent 镜像的大小和 `/agent-build.json` 从仓库里的层读出，不运行镜像。每次评测的 agent 镜像（`crucible-agent-<eval id>:r<N>`）和被测应用镜像在用完后删除清单；仓库的磁盘空间要定期回收：

```bash
kubectl -n crucible-system exec deploy/registry -- registry garbage-collect /etc/distribution/config.yml --delete-untagged
```

BuildKit 是无根模式（需要非受限的 seccomp/AppArmor 以使用用户命名空间），构建缓存在它自己的 `emptyDir` 里，Pod 重建即清空。

## 多节点与存储注意事项

- **评测卷**：所有步骤和容器都挂同一个 PVC，并通过节点亲和跑在步骤所在节点上。单节点用 `local-path` 即可；多节点时，`local-path` 的卷绑定在第一个使用它的节点上（`WaitForFirstConsumer`），之后整次评测都在该节点上，能用但不分散负载。要让不同步骤落到不同节点，换成 ReadWriteMany 的存储类（NFS、CephFS 等集群自己的存储）并去掉节点亲和（目前写死在执行后端里，待做）。
- **节点池**：目前可信步骤与沙箱步骤不区分节点（Job 上带了 `crucible/pool` 标签，未用于调度）。公开运营前应把 generate、score-tests 放到专用节点（nodeSelector + taint），可信步骤放别处。
- **镜像仓库**：单副本、HTTP、`local-path` 卷；多节点时每个节点都要有同样的 `registries.yaml`。
- **进程数**：每个节点都要设 `pod-max-pids`。
- **更强的隔离**：可以给 agent、应用、测试 Pod 设 `runtimeClassName: gvisor`/`kata`（未实现）。

## 已知限制

- 打分器 arcbench-official 的测试容器要加入服务容器的网络命名空间（`--network container:<名字>`），Kubernetes 后端不支持，直接报错；这个打分器目前只能在 Docker 后端（GitHub、自托管、单机、Nomad）上用。
- 没有给命名空间打 Pod Security Admission `restricted` 标签（ARC 官方打分器的构建容器需要以 root 运行加 CHOWN 等能力；原型阶段只保留现有隔离）。
- 停止一个容器用的是把 Pod 的 `activeDeadlineSeconds` 改成 1，宽限期固定为 30 秒。

## 停止与恢复（magicbook 上的做法）

```bash
sudo systemctl stop k3s && sudo /usr/local/bin/k3s-killall.sh   # 停掉所有 Pod、清掉 k3s 的网络规则
sudo systemctl start k3s                                         # 下次再用；未设开机自启
```

`k3s-killall.sh` 只删 k3s 自己的接口（cni0、flannel.1）和 `KUBE-`/`CNI-`/flannel 规则，Docker 和坩埚沙箱（`sandbox-net.sh`）的规则不受影响。k3s 用自己的 containerd，Pod 网段 10.42.0.0/16、服务网段 10.43.0.0/16，与 Docker（172.17.0.0/16）和坩埚沙箱（172.31.x.0/24）不重叠。magicbook 上停掉 k3s 后，iptables 与装 k3s 之前相比只多了一张空的 mangle 表，路由相同，Docker 沙箱自检和 `crucible eval local` 照常。
