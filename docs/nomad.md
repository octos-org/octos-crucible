# Nomad 后端：`crucible eval nomad`

与 `crucible eval local` 是同一个驱动、同样的步骤顺序（generate × 遍数 → handoff → score-tests × 遍数 → publish），区别只在**每一步由 Nomad 派出去**：每步一个 `type = "batch"` 的 job，job 里唯一的 task 用 `raw_exec` 在节点上运行 `crucible step <名字>`。步骤里要起的容器仍由**节点上的 Docker** 启动（沙箱槽位、网络规则、清理与单机完全相同），Nomad 只管排队、选节点、记时间。设计见 `docs/executors.md` §3.3 的"整步托管"、§4.3、§5.4。

目前是原型验证阶段：已有的沙箱隔离保留，可选 gVisor（见下）；密钥用本机钥匙（`crucible keys gen`），结果不进排行榜。

## 安装（单节点，Ubuntu）

```bash
# HashiCorp 官方 apt 源（Ubuntu 新版本没有对应源时用 noble）
wget -qO- https://apt.releases.hashicorp.com/gpg | sudo gpg --dearmor -o /usr/share/keyrings/hashicorp-archive-keyring.gpg
echo "deb [signed-by=/usr/share/keyrings/hashicorp-archive-keyring.gpg] https://apt.releases.hashicorp.com noble main" \
  | sudo tee /etc/apt/sources.list.d/hashicorp.list
sudo apt-get update && sudo apt-get install -y nomad
```

配置 `/etc/nomad.d/crucible-dev.hcl`：

```hcl
data_dir = "/var/lib/crucible-nomad/data"   # 父目录要对运行用户可写（客户端在旁边建 alloc_mounts）
plugin "raw_exec" {
  config { enabled = true }
}
plugin "docker" {
  config { allow_privileged = false }
}
```

用 systemd 以**运行评测的用户**（在 docker 组、有免密 `sudo iptables`，见 `docs/self-hosted.md` 的前置条件）跑 dev 模式（server 与 client 同一进程，只监听 127.0.0.1）：

```ini
# /etc/systemd/system/crucible-nomad.service
[Service]
User=yao
Group=yao
ExecStart=/usr/bin/nomad agent -dev -config /etc/nomad.d/crucible-dev.hcl
KillSignal=SIGINT
```

```bash
sudo mkdir -p /var/lib/crucible-nomad && sudo chown yao:yao /var/lib/crucible-nomad
sudo systemctl daemon-reload && sudo systemctl start crucible-nomad
nomad node status -self -verbose | grep -E 'raw_exec|docker'   # 两个都应是 Healthy
```

`raw_exec` 的 task 以 Nomad 客户端的用户运行，所以客户端用评测用户跑，步骤就和单机时一样是这个用户。非 root 运行时 Nomad 不能给 task 的 `secrets/` 挂内存盘，它是普通目录（job 结束即随 alloc 目录回收，驱动同时删掉变量）；要内存盘就以 root 跑客户端并在 job 里设 `user`（多节点时建议这样）。

## 用法

参数与 `crucible eval local` 相同（见 `docs/executors.md` §5.2 末尾），另加 Nomad 的几项：

```bash
# app 模式
crucible eval nomad --root <仓库> --taskset hello-world --app app.zip --stage 1
# agent 模式
crucible eval nomad --root <仓库> --taskset hello-world --agent builtin:octos \
    --stages 2 --model glm-5.3-flash --cred-file ~/model-cred.json
```

| 参数 | 默认 | 说明 |
|---|---|---|
| `--addr` / `NOMAD_ADDR` | `http://127.0.0.1:4646` | Nomad HTTP API |
| `--token` / `NOMAD_TOKEN` | 无 | 开了 ACL 时用；只需能提交、清除 `crucible-*` job，写删 `nomad/jobs/crucible-*` 变量 |
| `--crucible-bin` | 驱动自己 | 节点上看到的 `crucible` 路径 |
| `--trusted-pool` | `default` | handoff、publish 的节点池 |
| `--sandbox-pool` | `default` | generate、score-tests 的节点池 |

结果与单机一样在 `--out`（默认 `./crucible-evals`）`/<eval id>/`，eval id 默认 `nomad-<时间>-<随机>`；`steps.json` 的时间取 Nomad 记录的 task 开始/结束时间，manifest 记 `timing_source: nomad`。各步的标准输出与错误在步骤结束后从 Nomad 取回，打印到驱动的标准错误。

**gVisor**：驱动的环境里设了 `CRUCIBLE_DOCKER_RUNTIME=runsc` 时，它随 `HOME`、`PATH` 一起传给每个步骤，节点上的 Docker 用 gVisor 起步骤的所有容器（每个节点都要装好并在 `daemon.json` 注册，见 `docs/executors.md` §3.6）。

## 每一步怎样提交

- job id：`crucible-<eval id>-<步骤>[-r<遍>]`，例如 `crucible-nomad-xxx-score-tests-r1`。同时跑多个评测时 job 名按评测区分，互不冲突；沙箱槽位由步骤自己在节点上选空闲的（与单机相同）。
- 一个 task group、一个 task，`raw_exec`，`RestartPolicy.Attempts = 0`、`ReschedulePolicy.Attempts = 0`：失败不由 Nomad 重试，由驱动决定。申请 500 MHz CPU、1 GB 内存，只用于排队（步骤里的容器归 Docker 管，资源限制在容器上）。
- **密钥**：提交前驱动把这一步的密钥写进**这个 job 自己的** Nomad Variable `nomad/jobs/<job id>`（每个密钥一项，base64）；job 里每个密钥一个 `template`，渲染到 task 的 `secrets/crucible/<名字>`（权限 0400），命令行带 `--secrets-dir ${NOMAD_SECRETS_DIR}/crucible`。job 规格和变量都按 `crucible step list` 里该步的清单生成，清单外的密钥直接报错；`crucible step` 自己也拒绝目录里出现清单外的文件。所以 `score-tests` 只可能拿到一次性钥匙（单元测试锁住）。Nomad 默认的工作负载身份只允许 job 读 `nomad/jobs/<自己的 job id>`，别的 job 读不到。
- **结束**：驱动等到 allocation 结束，取回日志，然后删除变量、`purge` job（无论成败）。中途按 Ctrl-C 打断驱动时，残留的 job 和变量用 `nomad job stop -purge crucible-<eval id>-...`、`nomad var purge nomad/jobs/...` 手动清。
- **包（步骤之间的产物）**：评测目录 `--out/<eval id>/`（gen、handoff、scores、publish）、存储（`dir:`，默认 `~/.crucible/store`）、仓库 `--root` 和 `crucible` 程序都按**绝对路径**传给步骤。单节点上就是本机硬盘。

## 多节点（含单机模拟）

- **共享存储**：评测目录、`dir:` 存储、仓库、`crucible` 程序必须在每个节点上的**同一路径**可读写，用集群自己的共享盘（NFS、CephFS 等）挂到相同位置即可，驱动不需要改。包里只有公开或已加密的东西，放共享盘是安全的；明文（agent 工作目录、交接时解开的内容）只在步骤进程里或节点本地。
- **用户与环境**：任务继承驱动的 `HOME`、`PATH`，节点上要有同名用户、同样的目录布局，Docker、buildx、`sudo -n iptables` 按 `docs/self-hosted.md` 准备。
- **节点池**：可信步骤和沙箱步骤分池（`--trusted-pool crucible-trusted --sandbox-pool crucible-sandbox`），因为同一节点上的 `raw_exec` task 是同一个系统用户，能读彼此的文件；沙箱节点上不要跑持平台私钥的步骤。
- **ACL**：开 ACL 后驱动的令牌只给 `crucible-*` job 的提交/读取/清除和 `nomad/jobs/crucible-*` 变量的写删；工作负载身份默认只能读自己 job 路径下的变量，不用额外策略。
- 同一节点能同时跑的沙箱步骤数取决于沙箱槽位（单机已支持并发，见 `docs/executors.md` §5.2），Nomad 的资源申请只是排队用的粗略值，可按节点规模调大。

### 单机模拟：`deploy/nomad/sim-multinode.sh`

一台机器上用三个容器组成集群（同一个 Docker 网络）：

| 容器 | 角色 | 跑什么 |
|---|---|---|
| `crucible-nomad-server` | Nomad server，API 在 `127.0.0.1:14646` | — |
| `crucible-nomad-trusted` | client，节点池 `crucible-trusted` | handoff、publish |
| `crucible-nomad-sandbox` | client，节点池 `crucible-sandbox` | generate、score-tests 及其容器 |

每个 client 就是一台真节点的样子：**自己的 Docker**（Docker in Docker，特权容器，镜像、网桥、iptables 都在 client 自己的网络命名空间里），同名同 uid 的评测用户、免密 `sudo iptables`，Nomad 以这个用户运行（`raw_exec`、docker 驱动）。所以沙箱和单机完全一样：`sandbox-net.sh` 的规则加在 sandbox 节点自己的 DOCKER-USER/INPUT 链上，沙箱自检在节点里跑，节点之间互不可见。没有选"把宿主机的 Docker socket 挂给 client"：那样所有 client 共用宿主机的 Docker 和 iptables，等于一台机器，可信/沙箱分池就没有意义。

共享盘：宿主机目录 `$CRUCIBLE_SIM_SHARED`（默认 `~/crucible-sim-shared`）以**同一路径**绑定挂载进两个 client，代替真集群里的 NFS/CephFS 挂载。仓库、`crucible` 程序、`--out`、`--store` 都放在它下面。HOME 是各节点自己的。

```bash
deploy/nomad/sim-multinode.sh up                      # 构建节点镜像，起三个容器，等两个 client 就绪
deploy/nomad/sim-multinode.sh load crucible-scorer-playwright:run ...   # 可选：把本机已有镜像拷进 sandbox 节点，省得重建
SH=~/crucible-sim-shared
NOMAD_ADDR=http://127.0.0.1:14646 crucible eval nomad --root $SH/repo --out $SH/evals \
    --store dir:$SH/store --crucible-bin $SH/bin/crucible \
    --trusted-pool crucible-trusted --sandbox-pool crucible-sandbox --taskset ... --app ...
deploy/nomad/sim-multinode.sh down                    # 删容器、网络、节点的 Docker 卷、节点镜像
```

需要 Docker 和 `nomad`（挂进容器用，节点镜像不另装）。`CRUCIBLE_SIM_PROXY` 让构建节点镜像时的 apt 和节点里的 Docker 走代理；`CRUCIBLE_SIM_BASE` 指定本机已有的 Ubuntu 基础镜像（拉不到 Docker Hub 时用）。节点入口在 cgroup v2 上先把自己移出容器的根 cgroup（与 docker:dind 相同），否则节点里的 Docker 起不了带资源限制的容器。

**magicbook 实测**（2026-10-04，Nomad 2.0.7）：

| 项目 | 结果 | 落在哪 |
|---|---|---|
| hello-world app | 1.0（通过 1/1） | handoff、publish 在 trusted；score-tests 及应用、测试容器在 sandbox |
| 巡天 L1 app | 4458.556163（与单机相同） | 同上，agent、引擎容器在 sandbox |
| 两个评测并发（巡天 app + 假模型凭据；hello-world agent `builtin:math-prover` + 假模型凭据） | 4458.556163、0.0（与单机相同） | 两个评测的 generate、score-tests 都在 sandbox，各占一个沙箱槽位（`crucible-sbx`、`crucible-sbx-1`），沙箱自检全部拦住；trusted 节点的 Docker 里没有任何容器 |

## 停止

```bash
sudo systemctl stop crucible-nomad      # 安装与配置保留，下次 start 即可
```
