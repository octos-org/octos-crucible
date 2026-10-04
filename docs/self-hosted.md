# 自托管运行器

评测里起容器的两步（`eval.yml` 的 `generate`、`score-tests.yml` 的跑测试 job）可以放到自己的 Linux 机器上跑；其余步骤（`setup`、`score` 交接、`publish`、`taskset-pack`）以及 `ci.yml`、`scorer.yml` 等永远在 GitHub 托管机上。设计见 `docs/executors.md` 第 5.1 节。

目前是原型验证阶段：自托管机上会持有平台私钥（`generate` 需要它解开阶段输入），没有为防攻击额外加限制，沙箱隔离与托管机上相同。

## 怎样选机器

仓库变量 `CRUCIBLE_SANDBOX_RUNNER`：一个 JSON 数组，写运行器标签，例如

```
["self-hosted","linux","x64","magicbook"]
```

- 不设或为空：GitHub 托管机 `ubuntu-latest`（默认行为）。
- `crucible plan` / `crucible plan-score` 校验它（1–10 个 `[A-Za-z0-9._-]` 标签），作为 job output `sandbox_runner` 交给 `generate` 和 `score-tests` 的 `runs-on`。
- 单次评测可以在 `eval.yml` 的 `options` 里临时改：`{"runner": "github-hosted"}` 强制托管机，`{"runner": "sandbox"}`（默认）用变量。只能在这两者之间选，不能随手写标签。

```bash
gh variable set CRUCIBLE_SANDBOX_RUNNER -R octos-org/octos-crucible --body '["self-hosted","linux","x64","magicbook"]'
gh variable delete CRUCIBLE_SANDBOX_RUNNER -R octos-org/octos-crucible   # 恢复默认
```

## 前置条件

- x86-64 Linux（WSL2 要开 systemd），Docker（含 buildx），Docker 用 iptables 防火墙后端（有 `DOCKER-USER` 链；`/etc/docker/daemon.json` 里不要设 `"firewall-backend": "nftables"`）；系统 `iptables` 与 Docker 用同一套（都是 nft 或都是 legacy）。
- 运行器用户在 `docker` 组（这等于 root，整台机器按"交给评测"对待）。
- 运行器用户免密 sudo，只需 iptables：

  ```bash
  echo 'yao ALL=(root) NOPASSWD: /usr/sbin/iptables, /usr/sbin/ip6tables' | sudo tee /etc/sudoers.d/crucible-runner
  sudo chmod 0440 /etc/sudoers.d/crucible-runner && sudo visudo -c
  ```
- `jq`、`git`、`curl`、coreutils `timeout`；50 GB 以上空闲磁盘；172.31.250.0/24 不与本机或局域网冲突。
- 一台机器只注册一个运行器（沙箱网络的网桥名、网段、端口是固定的，同一时间只能跑一个 job）。

`tools/sandbox-net.sh preflight` 逐项检查以上条件，缺什么就报什么；两步开头都会先跑它，不满足直接失败，不会悄悄降级。

## 注册运行器

在机器上用运行器用户执行（版本号以 <https://github.com/actions/runner/releases> 最新为准）：

```bash
mkdir -p ~/crucible-runner && cd ~/crucible-runner
V=2.337.0
curl -fsSL -o r.tgz https://github.com/actions/runner/releases/download/v$V/actions-runner-linux-x64-$V.tar.gz
tar xzf r.tgz && rm r.tgz
sudo ./bin/installdependencies.sh        # 缺 libicu 等时
```

注册令牌由仓库管理员取，经文件或管道交给 `config.sh`，不要打印：

```bash
# 管理员机器上
gh api -X POST repos/octos-org/octos-crucible/actions/runners/registration-token --jq .token \
  | ssh <机器> 'umask 077; cat > ~/crucible-runner/.regtoken'
# 运行器机器上
cd ~/crucible-runner
./config.sh --unattended --url https://github.com/octos-org/octos-crucible \
  --token "$(cat .regtoken)" --name <名字> --labels magicbook --work _work --replace
rm -f .regtoken
```

`self-hosted`、`Linux`、`X64` 三个标签自动加上（标签匹配不分大小写），`--labels` 只写自定义的那个。

启动（测试用，前台程序放后台；长期使用再考虑 `svc.sh install`）：

```bash
cd ~/crucible-runner && setsid nohup ./run.sh > runner.log 2>&1 < /dev/null &
gh api repos/octos-org/octos-crucible/actions/runners --jq '.runners[] | {name, status, labels: [.labels[].name]}'
```

**网络不稳时用代理。** 运行器要长时间连着 GitHub（取任务、下载 action 和 artifact、回传日志），连接时断时续会让 job 卡在下载上直到超时。在运行器目录的 `.env` 里写代理，运行器和它起的每个 job 都会用上；`no_proxy` 必须包含沙箱网桥地址 `172.31.250.1`。改完重启 `run.sh`。WSL（NAT 模式）用不了 Windows 的 `127.0.0.1` 代理，要写 Windows 主机在 WSL 里的地址（`ip route` 的默认网关），且代理要监听所有网卡：

```
https_proxy=http://172.28.160.1:10808
http_proxy=http://172.28.160.1:10808
no_proxy=localhost,127.0.0.1,::1,172.31.250.1
```

然后设好 `CRUCIBLE_SANDBOX_RUNNER`，照常用 `eval.yml` / `score.yml` 发起评测。

## 每个 job 的清理

托管机一次性使用，自托管机不是，所以两步都在开头和结尾清理：

- 开头：`sandbox-net.sh preflight`，再 `sandbox-net.sh clean`；`score-tests` 先删掉工作区里旧的 `handoff/`。
- 结尾（总会执行）：`sandbox-net.sh down` 与 `clean`，删掉 agent 镜像 `crucible-agent:run`、阶段输入和工作目录；`score-tests` 删掉 `handoff/`。
- `clean` 删的是：名字以 `crucible-` 开头的容器和卷、带 `crucible.scorer.run` 标签的容器、打分器网络（`crucible-net-*`、`crucible-bnet-*`）、沙箱网络 `crucible-sbx`，以及 `INPUT`、`DOCKER-USER` 链里指向坩埚网桥（`crucible0`、`crs*`、`crb*`）的规则。不碰 Docker 自己的链。
- `CRUCIBLE_PRUNE_BUILD_CACHE` 只在托管机上开，自托管机保留构建缓存（否则会清掉整台机器的缓存）。
- `$RUNNER_TEMP` 由运行器在每个 job 开始时清空。

## 停止与注销

```bash
gh variable delete CRUCIBLE_SANDBOX_RUNNER -R octos-org/octos-crucible     # 先切回托管机
pkill -f 'crucible-runner/bin/Runner.Listener'                                # 停止运行器（在运行器机器上）
# 移除令牌同样不打印：
gh api -X POST repos/octos-org/octos-crucible/actions/runners/remove-token --jq .token \
  | ssh <机器> 'umask 077; cat > ~/crucible-runner/.rmtoken'
cd ~/crucible-runner && ./config.sh remove --token "$(cat .rmtoken)" && rm -f .rmtoken
gh api repos/octos-org/octos-crucible/actions/runners --jq '.runners | length'   # 应为 0
rm -rf ~/crucible-runner
```

已拉取的镜像可以留着，下次省时间。

## 安全提醒

公开仓库会在这台机器上运行用户上传的 agent 和测试；一次容器逃逸就能在机器上留下后门，等之后的 job 带着平台私钥和模型 key 上来。所以：测试机只在测试时开着运行器，机器上不放别的凭据；长期使用要用专门的隔离机器（最好每个 job 重建，`--ephemeral`），细节见 `docs/executors.md` 第 5.1 节。
