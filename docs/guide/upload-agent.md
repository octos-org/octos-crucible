# 上传自己的 agent

平台评测的对象是一个“agent 包”：一个 zip 文件，根目录下有 `agent.json` 和 `Dockerfile`，以及构建镜像需要的其他文件。平台用 `docker build` 把它构建成镜像，然后在容器里运行。平台内置的 agent（例如 Octos）也是同样的格式，平台不对任何 agent 特殊处理。

## agent 只需要做到两点

第一，**能从命令行启动，读需求，在工作目录里干活**。平台在容器里执行一条命令，agent 读取 `/req` 目录里本阶段的需求，在当前目录 `/work` 里写代码，干完自己退出。阶段结束时 `/work` 里的内容就是这个阶段的产出。

第二，**调模型的 base URL 可配置**。agent 必须从环境变量 `OPENAI_BASE_URL` 读取模型接口地址，用 OpenAI 兼容的 `chat/completions` 接口调用模型。平台把这个变量指向自己的计量代理，代理再把请求转发到你在网页上填写的接口。如果你用的是 OpenAI 官方 SDK，不需要写任何额外代码，SDK 默认就会读取 `OPENAI_BASE_URL` 和 `OPENAI_API_KEY`。

除此之外没有别的要求。agent 不需要理解“阶段”，不需要保存或恢复状态，也不需要知道自己是第几遍。

## 完整最小示例：my-agent

目录结构如下：

```
my-agent/
  agent.json
  Dockerfile
  run.sh
  agent.py
```

`agent.json` 告诉平台这个 agent 叫什么、每个阶段执行什么命令：

```json
{
  "schema": 1,
  "name": "my-agent",
  "version": "0.1.0",
  "entrypoint": ["/opt/agent/run.sh"],
  "streaming": true
}
```

`schema` 固定为 `1`。`name` 必填，只能用小写字母、数字和连字符，以字母或数字开头，最长 40 个字符。`version` 是自由文本，会记入评测结果，缺省为 `"0"`。`entrypoint` 是每个阶段执行的命令，写成字符串数组；不写时使用镜像自己的 `ENTRYPOINT`/`CMD`。`streaming` 表示 agent 是否以 `stream: true` 方式调用模型；设为 `true` 时，计量代理会在流式请求里自动要求上游返回用量（`stream_options.include_usage`），这样才能统计 token。如果你的 agent 用流式调用却没有设这一项，并且自己也没有带 `stream_options`，token 可能统计不到。

还有一个可选字段 `app_start_cmd`，只在题目要求产出一个网站、而你的 `/work` 根目录里又没有 `Dockerfile` 时用到，见下文“产出”一节。

`Dockerfile`：

```dockerfile
FROM python:3.12-slim
RUN pip install --no-cache-dir openai==1.*
COPY run.sh agent.py /opt/agent/
RUN chmod 0755 /opt/agent/run.sh
ENTRYPOINT ["/opt/agent/run.sh"]
```

`run.sh`：

```bash
#!/usr/bin/env bash
set -euo pipefail
mkdir -p "$HOME"
# OpenAI SDK 自动读取 OPENAI_BASE_URL / OPENAI_API_KEY。
exec timeout "$DEADLINE_S" python3 /opt/agent/agent.py --requirements /req --workdir "$PWD" --model "$MODEL"
```

`agent.py` 读 `/req` 下的需求文件，在 `--workdir` 指定的目录里写代码。调用模型时用 `OpenAI()` 创建客户端，不要传 `base_url`，让 SDK 从环境变量读取；模型名用环境变量 `MODEL` 的值。

打包时进入 `my-agent/` 目录，把里面的内容打成 zip，保证 `agent.json` 在 zip 的根目录，而不是在一层子目录里：

```sh
cd my-agent
zip -r ../my-agent.zip .
```

然后在网页“提交评测”页选择“上传 agent”，选这个 zip 上传。zip 最大 25 MB，上传前会在浏览器里加密。

## 运行环境

每个阶段，平台用同一个镜像新起一个容器，执行一次 `entrypoint`。容器里能用到的环境如下。

`/req` 是本阶段的需求目录，只读，内容由题目包决定（例如 ARC-Bench 题是一份 `requirements.yaml` 和一个 `reference` 目录）。`/work` 是工作目录，也是启动时的当前目录，可写。`HOME` 是 `/home/agent`，可写。环境变量 `REQ_DIR` 和 `WORK_DIR` 分别等于 `/req` 和 `/work`。

模型相关的环境变量有三个。`OPENAI_BASE_URL` 是计量代理的地址，只提供 `POST {base}/chat/completions`。`OPENAI_API_KEY` 的值是 `dummy`，你的真实 key 只保存在计量代理里，从不进入容器。`MODEL` 是你在网页上填的模型名，请求其他模型名会被拒绝（HTTP 403）。上游返回的 429 和 500 会原样转给 agent，agent 最好自己做重试。

`DEADLINE_S` 是本阶段的时限，单位秒，从容器启动时算起。agent 应该在时限前自己退出。到时限后平台先发 SIGTERM，30 秒后发 SIGKILL。

容器的资源和权限是受限的：2 GB 内存（没有 swap）、1 个 CPU 核、最多 1024 个进程，以非 root 用户运行，去掉了所有 Linux capability，并禁止提权。网络方面，容器只能访问计量代理和白名单里的软件源，目前是 npm（`registry.npmjs.org`）和 PyPI（`pypi.org`、`files.pythonhosted.org`），通过环境变量 `HTTPS_PROXY`/`HTTP_PROXY` 指定的代理出网；容器里没有 DNS，访问其他任何地址都会失败，包括 GitHub 和你自己的模型接口。容器里也看不到测试材料。

因为运行时只能访问 npm 和 PyPI，agent 需要的其他东西（二进制工具、模型以外的数据、需要从 GitHub 下载的依赖）都应该在 `Dockerfile` 里提前装好。构建镜像这一步是可以联网的，而且此时机器上还没有任何密钥。

## 阶段如何衔接

一道题通常分几个阶段，后一个阶段在前一个阶段的基础上增加需求。平台的做法很简单：第一阶段结束后，保存 `/work` 作为这一阶段的产出，然后把 `/req` 换成第二阶段的需求，再用同一个镜像、同一条命令启动一次 agent。`/work` 和 `HOME` 在阶段之间原样保留，所以第二阶段开始时，`/work` 里已经有第一阶段写好的代码，`HOME` 里也还留着 agent 自己存下的任何东西（比如缓存、记忆文件）。

所以 agent 不需要知道当前是第几阶段，只要每次启动时读 `/req` 的需求、看 `/work` 里已有的内容、在此基础上继续干活就行。如果 agent 想跨阶段保留自己的状态，写到 `HOME` 或 `/work` 下即可。

阶段进行中，平台每 15 分钟给 `/work` 拍一次快照。agent 在时限内自己退出时，用 `/work` 的最终状态作为产出；如果被强制结束，就用最后一份快照。这一切 agent 都不需要配合。

## 产出

阶段结束时，平台按题目包声明的产出类型打包 `/work`。符号链接一律丢弃，`.git` 目录不打包。

产出类型为 `files` 时，打包整个 `/work`。

产出类型为 `web-app` 时（目前所有题都是这种），打分器需要一个根目录带 `Dockerfile` 的 zip，在不联网的情况下构建镜像，启动后访问 3000 端口。有两种方式满足它。如果 `/work` 根目录有 `Dockerfile`，整个 `/work` 原样打包，由你的 `Dockerfile` 决定如何启动。否则平台按 ARC-Bench 的约定，只打包 `frontend/` 和 `backend/` 两个目录（不含 `frontend/node_modules`），并补一个 Dockerfile：基于 `node:24-bookworm-slim`，工作目录 `/app/backend`，环境变量 `PORT=3000`，启动命令是 `agent.json` 里的 `app_start_cmd`，缺省为 `npm start`。

要注意打分时构建镜像是不联网的，所以运行时需要的依赖（例如 `backend/node_modules`、前端构建出来的 `dist/`）必须留在 `/work` 里，不能指望打分时再安装。

## 常见问题

**我的 agent 不是 Python 写的，可以吗？** 可以。平台只关心 `Dockerfile` 能构建、`entrypoint` 能启动。用什么语言、什么框架都行。

**我的 agent 不用 OpenAI SDK，怎么接计量代理？** 读取环境变量 `OPENAI_BASE_URL`，向 `{OPENAI_BASE_URL}/chat/completions` 发 OpenAI 格式的 POST 请求即可，`Authorization` 头随便带什么值都可以，计量代理会换成你的真实 key。

**我能用 Anthropic 格式或其他非 OpenAI 格式的接口吗？** 目前不能。计量代理只提供 OpenAI 兼容的 `chat/completions`。你在网页上填的接口地址也必须是 OpenAI 兼容的，并且必须是 https，不能指向本机或内网地址。

**agent 运行时 `pip install` 或 `npm install` 失败了。** 先确认你的工具会使用 `HTTPS_PROXY` 环境变量。pip 和 npm 默认都会读取它。如果要装的包需要从 GitHub 或其他网站下载额外文件，运行时是下载不了的，请在 `Dockerfile` 里提前装好。

**我需要在镜像里放自己的 key 或 token 吗？** 不需要，也不应该。模型 key 在网页上填写，平台只交给计量代理；容器里的 `OPENAI_API_KEY` 永远是 `dummy`。除了模型，容器访问不到任何需要凭据的服务。

**如何在本地自测？** 可以模拟平台的环境：准备一个 `req/` 目录放需求、一个空的 `work/` 目录，然后用类似下面的命令运行，把 `OPENAI_BASE_URL` 指向一个你能用的 OpenAI 兼容接口：

```sh
docker build -t my-agent ./my-agent
docker run --rm --user 1000:1000 --memory 2g --cpus 1 \
  -v "$PWD/req:/req:ro" -v "$PWD/work:/work" -w /work \
  -e HOME=/tmp/home -e MODEL=<模型名> -e DEADLINE_S=600 \
  -e OPENAI_BASE_URL=<接口地址> -e OPENAI_API_KEY=<你的 key> \
  my-agent
```

本地自测时容器可以访问外网，平台上不行，这一点要特别留意。平台题目里的 `demo-todo` 是公开的小题，适合拿来先跑通流程，见 [tasksets.md](tasksets.md)。

**agent 包里的代码会在平台的机器上直接执行吗？** 不会。agent 包在宿主机上只被当作构建上下文传给 `docker build`，里面的任何东西都只在容器里运行。

**我的 agent 包会被公开吗？** 不会。上传的内容会加密保存，详见 [privacy.md](privacy.md)。
