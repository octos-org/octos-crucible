# Worker API

Cloudflare Worker（`crates/crucible-worker`，Rust + workers-rs）的接口约定。本文件是网页、Worker 和 GitHub Actions 三方的权威约定，改动需三方同步。

Worker 只经手密文：上传文件和模型凭据都是浏览器用平台公钥封好的 crucible envelope；Worker 不持有私钥，只校验信封头。

## 通用约定

- **认证**：`GET /auth/callback` 把会话令牌放在 Pages 地址的片段里（`#token=<令牌>`）；之后每个请求带 `Authorization: Bearer <令牌>`。用户接口也接受个人 API 令牌（`crt_...`，见下文“命令行令牌”），它代表本人但从不具备管理员权限。不用 cookie 认证（OAuth 往返期间有一个只作用于 `/auth/callback` 的短期 cookie，用来防登录 CSRF）。令牌是 HMAC-SHA256 签名，有效期 24 小时。被封禁的用户（D1 表 `bans`）每次请求都会被拒绝（403 `banned`）。
- **CORS**：只对 `PAGES_ORIGIN` 放行，允许的方法为 `GET, POST, DELETE, OPTIONS`，允许的请求头为 `Authorization, Content-Type, X-Upload-Kind`。`/internal/*` 不响应跨域请求。
- **错误**：所有错误都是 `{"error": {"code": "...", "message": "..."}}`，HTTP 状态码见下表。

| code | HTTP | 含义 |
|---|---|---|
| `bad_request` | 400 | 参数不合法（message 说明是哪一项） |
| `consent_required` | 400 | `consent` 不是 `true` |
| `not_sealed` | 400 | 上传内容不是 crucible envelope |
| `wrong_key` | 400 | 信封的 key_id 不是当前公钥（重新 `GET /pubkey`） |
| `unauthorized` | 401 | 缺少令牌、令牌无效或已过期 |
| `forbidden` | 403 | 无权限（不是 owner、不是管理员） |
| `banned` | 403 | 账号已被封禁 |
| `not_found` | 404 | 资源或接口不存在 |
| `not_ready` | 404 | 评测还没有可下载的产出 |
| `method_not_allowed` | 405 | |
| `conflict` | 409 | eval_id 已被使用 / 该块属于别人 / 评测已结束 |
| `payload_too_large` | 413 | |
| `upstream_error` | 502 | GitHub 调用失败 |
| `internal` | 500 | 存储错误或配置错误 |
| `storage_quota` | 503 | 当天的写入额度已用完（免费版 D1 每天 10 万行，UTC 0 点重置）；稍后重试 |

- **评测状态**（`status`）只有这几种取值：`queued` | `building` | `running:<阶段名>` | `scoring` | `done` | `failed`。`done` 和 `failed` 是终态。状态只会前进，不会倒退。
- **eval_id**：前端生成的小写 UUID v4。

## 登录

### `GET /auth/login`
302 到 GitHub OAuth 授权页（不申请任何 scope，只读公开资料）。

### `GET /auth/callback`
GitHub 回调。换取 token、读取用户之后，用户的 GitHub token 立即丢弃。
- 成功时 302 到 `PAGES_URL#token=<会话令牌>`。
- 失败时 302 到 `PAGES_URL#error=<code>`，code 可能是 `oauth_denied`、`oauth_state`（登录过期或 state 不匹配，重新登录）、`oauth_failed`、`banned`、`upstream_error`、`bad_request`。

### `GET /me`
→ `{"github_id": 42, "login": "octocat", "is_admin": false}`

### `GET /auth/dev-login?github_id=<n>&login=<name>`（仅开发）
同时满足 `DEV_AUTH=1` 且请求的 host 是 localhost 时才生效，否则返回 404。→ `{"token": "..."}`。

## 命令行令牌

供 `crucible submit` / `crucible status` 使用。令牌格式 `crt_<id 16 位十六进制>_<64 位十六进制>`；D1 中只存整串令牌的 SHA-256，明文只在创建时返回一次。令牌可代替会话令牌调用用户接口（`/me`、`/uploads`、`/evals...`），身份为令牌的主人，`is_admin` 恒为 false（因此 `/admin/*`、`?all=1` 都会 403）；`/internal/*` 只认 `CRUCIBLE_WORKER_TOKEN`。主人被封禁时令牌同样被拒（403 `banned`）。下面三个接口**只接受会话令牌**（用 API 令牌调用返回 403），所以泄露的令牌不能自我续命。

### `POST /tokens`
请求体可为空，或 `{"name": "laptop"}`（≤ 60 个字符，无控制字符，默认 `cli`）。每人最多 20 个，超出返回 409。
→ `201 {"id": "<16 hex>", "name": "laptop", "created_at": "...", "token": "crt_..."}`

### `GET /tokens`
→ `[{"id", "name", "created_at"}]`，按创建时间倒序，不含令牌本身。

### `DELETE /tokens/:id`
→ 204；不是本人的或不存在 → 404。撤销后令牌立即失效。

## 公开信息

### `GET /pubkey`（无需登录）
→ `{"key_id": "1ffa702796eb5ee8", "public_key": "age1..."}`，来自 `config/keys.json` 的当前公钥（编译时嵌入）。`key_id` = SHA-256(public_key) 的前 16 个十六进制字符。

### `GET /tasksets`（无需登录；带令牌时多返回用户上传的题目包）
→ `[{"name": "github-full", "version": "1.0", "stages": [{"name": "stage-1", "time_limit_s": 3600, "total": 30}]}]`

数据读自仓库 `EVAL_REF`（默认 main）分支的 `tasksets/*/taskset.json`，在 D1 表 `cache` 中缓存 5 分钟。`version` 取文件里的 `version` 字段，没有时用 `git-<blob sha 前 12 位>`。`total` 取 `expected_total`，可能为 `null`（不按用例计数的题目包）。题目包声明了 `display`（`docs/plugins.md` §9）时原样带上 `display`。目录名必须等于 `name`，不合规的题目包会被跳过。

之后是用户上传的题目包（见下节），按上传时间倒序，最多 50 个：匿名只看到已公开且可用的；带令牌时另有自己上传的（任何状态）；管理员加 `?all=1` 看到全部。令牌无效时按匿名处理。上传的题目包多几个字段：
`{"name": "u-0123456789abcdef", "version": "upload", "stages": [...], "title": "<source.json 里的 name>", "owner_login": "...", "public": false, "status": "packing|ready|failed", "error"?: "..."}`。`stages` 在 `ready` 之前为空。

## 排行榜

两个接口都无需登录，结果在 D1 表 `cache` 中缓存 5 分钟（键 `leaderboard`、`leaderboard/<题目包>`），所以新成绩最多 5 分钟后出现。

只有同时满足下面几条的评测才会上榜：提交时 `score_public = true`、结果状态 `done` 且有总分、提交者未被封禁、题目包是内置的或已公开的用户题目包。私有评测不在查询范围内（查询走只包含公开评测的部分索引 `evals_public_taskset`，不扫全表）。榜上只有下面列出的字段；产出、日志、下载地址始终不公开。

### `GET /leaderboard`
→ 有公开成绩的题目包，最近有成绩的在前：`[{"taskset": "hello-world", "evals": 3, "latest_at": "<最近一次公开评测的时间>"}]`（`evals` 是公开且完成的评测数）。

### `GET /leaderboard/:taskset`
```json
{"taskset": "hello-world", "direction": "higher",
 "display": {"decimals": 1, "format": "percent", ...}, "stage_display": {...},
 "entries": [{"rank": 1, "login": "octocat", "agent": "my-agent", "agent_version": "0.3.0",
   "model": "glm-5.3", "total_score": 0.9,
   "stages": [{"stage": "stage-1", "score": 27, "max": 30}],
   "replicas": 3, "wall_s": 1834.5, "cost_usd": 0.42,
   "created_at": "...", "eval_id": "..."}],
 "partial": [{"stages": ["stage-1"], "entries": [ ... ]}]}
```
- `entries` 只含**跑完题目包全部阶段**的评测（按题目包当前的阶段列表判断：评测的 `stage_names` 覆盖全部阶段）。只跑了部分阶段的评测（agent 模式只跑前 N 个阶段、app 模式单阶段打分）放在 `partial`：按所跑的阶段集合分组，每组单独排名，组按阶段在题目包中的顺序排列；没有时不带该字段。题目包的展示名（`display.name`，如“四卡总分”）只用于 `entries`。
- 候选为该题目包最近 2000 次公开完成的评测。每个（用户, agent 名）只取最好的一次：方向 `direction` 取最近一次公开评测快照里总分的 `display.direction`（没有快照时为 `higher`），`higher` 取总分最大、`lower` 取最小；总分相同取更早的那次。最多 100 行。
- 排序同上；总分相同的名次并列（1, 1, 3），更早的排前。
- `total_score` 即结果表里按快照 aggregate 算出的总分（同 `GET /evals`）；`display` / `stage_display` 是最近一次公开评测快照里的总分与阶段分展示方式（没有快照的旧评测不带，按百分比与 `通过数/总数` 显示）。
- `stages`：每阶段在各遍之间的平均分（没有一遍打出分时为 `null`）和满分；`replicas` 为遍数；`wall_s` / `cost_usd` 为每遍各阶段之和的平均，有一遍价格未知时 `cost_usd` 为 `null`。`model` 在 app 模式未填时为 `null`。
- 名称不合法 → 404；没有公开成绩 → `entries: []`。题目包已不存在（查不到阶段列表）时全部视为完整评测。

## 配额

每个用户的用量直接从现有表统计（过去 24 小时滑动窗口；进行中的评测按未结束的状态），超限的请求返回 `429 quota_exceeded`，错误信息（中英文）写明哪一项、上限、已用、何时恢复（最早一次用量移出 24 小时窗口的时刻，UTC；进行中的评测数则是"进行中的评测结束一个后"）。

| 项 | 默认 | 检查于 |
|---|---|---|
| `uploads_per_day` | 50 | `POST /uploads`（重传自己已上传的同一份不计） |
| `upload_bytes_per_day` | 500 MiB | `POST /uploads`（加上本次大小） |
| `evals_running` | 3 | `POST /evals` |
| `evals_per_day` | 20 | `POST /evals` |
| `plugins_per_day` | 10 | `POST /plugins` |
| `tasksets_per_day` | 10 | `POST /tasksets` |

默认值在 `wrangler.toml` 的 `QUOTA_*`（如 `QUOTA_EVALS_RUNNING`）。管理员默认豁免；管理员可按用户覆盖任一项或豁免（D1 表 `quotas`，见“管理”）。

### `GET /quota`
→ `{"github_id", "exempt": bool, "window_s": 86400, "items": [{"name", "description", "limit", "used", "remaining", "frees_at"?}]}`。`frees_at`：最早一次计入的用量移出窗口的时刻。网页在提交评测、上传题目包、上传插件处显示剩余额度；命令行 `crucible quota`。

## 用户上传的题目包

zip 的格式同 `tasksets/hello-world/source`（`source.json` + 各阶段目录），检查规则即 `crucible taskset validate`：格式（未知字段拒绝）、阶段 id、每阶段的输入与测试都存在且不重叠、无符号链接、各阶段限时之和 ≤ 总限时 ≤ 18000 s、插件只能是注册表里 `user: true` 的，或上传者能用的上传插件（`u-...`，见下节）。命令行：`crucible taskset upload <zip 或目录> --wait`。

### `POST /tasksets`
`{"upload_hash": "<POST /uploads 返回的 hash，X-Upload-Kind: taskset>"}` → `201 {"id": "u-<16 hex>", "status": "packing"}`

`upload_hash` 必须是本人以 `taskset` 类型上传的。Worker 写入 D1 表 `user_tasksets`（`packing`，私有），并触发 `TASKSET_WORKFLOW`（默认 `taskset-pack.yml`），参数：`taskset_id`、`source` = `blob:<upload_hash>`、`results_url` = `<worker>/internal/tasksets/<id>`。触发失败时记为 `failed`，接口返回 502。

workflow（持私钥，不运行上传的代码）解密 zip、检查、按阶段拆成 inputs / tests 两个块分别封存，把 `taskset.json`（`name` 为 id，`title` 为原名）回传 Worker；检查不通过时回传原因（只给上传者看，不进公开日志）。

### `GET /tasksets/:id`
仅上传者、管理员，或已公开的题目包可见，否则 404。→ 上面列表里的一项，外加 `created_at`、`updated_at`。

### `POST /tasksets/:id/public`（仅管理员）
`{"public": true|false}` → `{"id", "public"}`。只有 `ready` 的题目包能设为公开（否则 409）。

### 使用
`POST /evals` 的 `taskset` 可以填 `u-...`：必须 `ready`，且是本人上传或已公开，否则 400 `unknown taskset`。workflow（eval.yml / score.yml 的 plan 步骤）用 `GET /internal/tasksets/:id?github_id=<owner>` 取 `taskset.json`，Worker 按同样规则再查一次提交者是否有权使用。

## 用户上传的插件

插件包格式、镜像约定和登记流程见 `docs/plugins.md` §14：zip 里有 `plugin.json`（目前只能是 `kind: scorer`）和 `Dockerfile`。

### `POST /plugins`
`{"upload_hash": "<POST /uploads 返回的 hash，X-Upload-Kind: plugin>"}` → `201 {"id": "u-<16 hex>", "status": "building"}`

`upload_hash` 必须是本人以 `plugin` 类型上传的。Worker 写入 D1 表 `user_plugins`（`building`，私有），并触发 `PLUGIN_WORKFLOW`（默认 `plugin-pack.yml`），参数：`plugin_id`、`source` = `blob:<upload_hash>`、`results_url` = `<worker>/internal/plugins/<id>`。触发失败时记为 `failed`，接口返回 502。

### `GET /plugins`（无需登录；带令牌时多返回自己上传的）
按上传时间倒序，最多 50 个：匿名只看到已公开且可用的；带令牌时另有自己上传的（任何状态）；管理员加 `?all=1` 看到全部。每项：
`{"id": "u-...", "title": "<plugin.json 的 name>", "owner_login", "public", "status": "building|ready|failed", "error"?, "kind"?, "version"?, "description"?, "runs_taskset_code"?, "model"?, "accepts"?, "selftest"?: {"status", "score", "max", "detail"}, "reviewable"?, "approval"?: {"by_id", "by_login", "at", "checked", "note"}, "created_at", "updated_at"}`。`kind` 之后的字段在 `ready` 之后才有；`reviewable` 表示有审核材料，`approval` 是设为公开时的审核记录。

### `GET /plugins/:id`
仅上传者、管理员，或已公开的插件可见，否则 404。→ 同上面列表里的一项。

### `GET /plugins/:id/review`（仅上传者和管理员）
审核材料：`{"id", "status", "review": {"files": [{"path", "size"}], "dockerfile", "texts": [{"path", "content"}], "truncated"?} | null, "approval", "checklist": ["source", "dockerfile", "detail_leak", "model_use"]}`。材料由 plugin-pack 的 report 步骤解开插件包后回传（Worker 自己打不开加密的包）：完整文件清单、Dockerfile、小的文本文件；Dockerfile 与文本合计最多保留 64K 字符，超出截断并标 `truncated`。审核材料上线前登记的插件为 `null`。其他人 404。

### `POST /plugins/:id/public`（仅管理员）
设为公开：`{"public": true, "review": {"checked": ["source", "dockerfile", "detail_leak", "model_use"], "note": "<可选，≤1000 字符>"}}`；撤回公开：`{"public": false}`。→ `{"id", "public"}`。

设为公开前管理员应读过 `GET /plugins/:id/review`，并逐项确认审核清单：`source` 源码与说明一致、无可疑下载/外联/混淆；`dockerfile` 基础镜像可信、只装声明的依赖；`detail_leak` hidden 时 `detail`/`items` 不泄露测试内容；`model_use` 声明了模型时用法合理。缺 `review` 或清单不全返回 400（错误信息列出未勾选项）。审核人、时间、清单与备注记入 `approval`（撤回公开时保留）。只有 `ready` 的插件能设为公开（否则 409）。

### 使用
用户题目包的 `scorer.name` 写 `u-...`。`taskset-pack` 用 `GET /internal/plugins/:id?taskset=<题目包 id>` 取插件的固定形式（插件 `ready`，且题目包上传者是插件上传者或插件已公开，否则 403），写进 `taskset.json` 的 `user_plugins`；`POST /internal/tasksets/:id` 时 Worker 按 D1 再核对一次（不符返回 400）。

## 上传

### `POST /uploads`
- 请求头：`Authorization`，`X-Upload-Kind: agent|app|taskset|plugin`，`Content-Type: application/octet-stream`
- 请求体：浏览器封好的 envelope 字节，不超过 25 × 1024 × 1024 字节。
- Worker 只检查：内容是 crucible envelope 头、`key_id` 是当前公钥、头后面有密文。
- 计算 SHA-256 后存为 GitHub Release asset：预发布 release `blobs-NN`，NN = 哈希第一个字节 >> 3（00…31），asset 名为完整的十六进制哈希。release 不存在时会创建为 prerelease。（规则与 crucible-store 一致。）
- → 新上传返回 `201 {"hash": "<sha256>"}`；同一用户重复上传同样的字节返回 `200`，结果相同。
- 上传记录写入 D1 表 `uploads`（主键 hash，owner、kind）。如果这些字节已经存在于存储里但不是本人上传的，返回 409；提交评测时也只能引用自己上传的块。这样任何人都不能拿别人的密文块让平台去解密。
- 顺序：先写 D1 记录（认领：`INSERT ... ON CONFLICT DO NOTHING` 后再查 owner，主键保证并发安全），再传 GitHub。所以请求在传完 GitHub 后中断（应答丢失、超时），同一用户重试同样的字节时会找到自己的记录；GitHub 报 asset 已存在时，Worker 下载该 asset 核对 SHA-256，一致即返回 `200`。没有记录却已存在的字节（别人的、或工作流产出）仍返回 409，并撤回这次认领。记录写不进去时（`storage_quota` / `internal`）不会上传到 GitHub。

## 评测

### `POST /evals`
```json
{
  "mode": "agent" | "app",
  "eval_id": "<uuid v4>",
  "upload_hash": "<POST /uploads 返回的 hash>",
  "taskset": "github-full",
  "stages": 2,
  "model": "glm-5.3",
  "replicas": 1,
  "budget": {"max_requests": 500, "max_tokens": 1000000, "max_cost_usd": 10},
  "cred_envelope": "<base64>",
  "score_public": false,
  "consent": true
}
```
→ `201 {"eval_id": "..."}`

校验规则（多出的字段会被拒绝）：

| 字段 | agent 模式 | app 模式 |
|---|---|---|
| `stages` | 可选，跑前 N 个阶段，范围 1..=阶段数，默认全部 | **必填**，要打分的阶段序号（从 1 开始） |
| `model` | 必填，`[A-Za-z0-9][A-Za-z0-9._:/-]{0,79}` | 可选，规则相同 |
| `replicas` | 1..=10，默认 1 | 只能不填或填 1 |
| `budget` | 可选；`max_requests` 取 1..=1e7，`max_tokens` 取 1..=1e10，`max_cost_usd` 取 (0, 10000] | 不接受 |
| `cred_envelope` | 必填 | 可选；题目包声明打分要用模型（`model.*: required`，`GET /tasksets` 里 `model_required: true`）时必填 |
| `score_public` | 必填（布尔） | 必填（布尔） |
| `consent` | 必须为 `true` | 必须为 `true` |

此外：`upload_hash` 必须是本人上传的，且上传时的 `X-Upload-Kind` 与 `mode` 一致；`taskset` 必须出现在 `GET /tasksets` 里；`eval_id` 不能重复（409）。

`cred_envelope` 是用平台公钥封装的 JSON `{api_key, endpoint, download_password, eval_id}`，再整体做标准 base64。Worker 无法解密，只检查信封头和 key_id，并限制大小不超过 16 KiB。解码后的字节原样存入 D1 表 `creds`，24 小时过期（读时过期即视为不存在，每小时的 Cron 删除过期行）。解封以及核对其中的 `eval_id` 由 Actions 中的 `crucible cred open` 负责。

提交之后，Worker 写入 D1 表 `evals` 一行（状态为 `queued`，主键 eval_id 保证不重复），并调用 `workflow_dispatch`（只传非秘密参数，见下文“触发参数”）。触发失败时删除凭据，评测记为 `failed`，接口返回 502。

### `GET /evals`
→ 当前用户的评测列表，按创建时间倒序：
`[{"eval_id", "mode", "taskset", "model", "created_at", "status", "total_score"?, "display"?}]`

管理员可以用 `?all=1` 查看全部评测。列表里的状态在两次 `GET /evals/:id` 之间可能稍有滞后，以详情为准。

`total_score` 按该评测 manifest 里的 `scoring` 快照计算（与 `crucible manifest` 同一个函数 `crucible-core` `Manifest::compute_total_score`），保留 4 位小数；没有分数时不出现该字段。`aggregate.stages` 为 `ratio`（旧评测没有快照时也按它）时是 0–1 之间的 Σscore / Σmax（即旧的 Σpassed / Σtotal）；`sum` / `mean` / `weighted` 时是各遍总分的平均，单位与阶段分相同，可为负。`display` 是快照里总分的展示方式（`name`、`unit`、`direction`、`decimals`、`format` 等），旧评测没有此字段，按百分比显示。

manifest（`schema: 2`）的阶段分数 `replicas[].stages[].score` 是 result v2 去掉文本字段：`{status: "scored"|"error", error?, score?, max?, passed?, items?}`；旧 manifest（`schema: 1`）里是 `{status: passed|failed|system_error|rejected, passed, total}`，读取方按 `docs/scorer-contract.md` §4 的换算表读，数据不改写。新 manifest 另有 `scoring`：`{aggregate, display: {stage, total}, plugins: [{kind, name, version}]}`，是 publish 时从题目包取的快照，题目包以后改了展示方式，旧评测不受影响。阶段条目可以有 `eval_usage: {interactive?: {usage, cost_usd}, scorer?: {usage, cost_usd}}`：打分 job 里交互运行和模型评判用掉的模型（提交者的 key），与 `usage`（agent 产出阶段）分开记，网页也分开展示（`docs/plugins.md` §10）。

**失败原因**（网页详情页“失败原因”卡片、`crucible status` 都会显示）：
- `replicas[].failure`：这一遍为什么没有可用结果，只是类别，可以出现在公开日志与数据分支：`agent package could not be fetched`、`agent image build failed`、`platform setup failed`、`agent run failed`（生成 job，`crucible step generate`）；`scorer image build failed`、`scoring failed`（打分 job，`crucible step score-tests`，写 `failure.json` 随 `scores-r*` 上传）；以及 `stage <id> did not run`、`stage <id> left no checkpoint`、`stage <id> was not scored (the scoring job failed)`。
- `replicas[].failure_detail`：只给提交者看的细节（错误链、agent 镜像构建日志的末尾，最多约 8000 字）。只来自生成 job（那台机器上没有测试），在 `agent.sealed` 里加密传到 publish；公开日志只打印类别，数据分支上的 manifest 去掉这个字段。
- `replicas[].stages[].reason`：阶段报错或得 0 分时打分器给的说明（例如 `app build failed`、`no Dockerfile at the root of the submitted app`、`the output is not in the format the stage asks for`）。打分器以 `--visibility hidden` 运行，这段说明是固定文字，不含测试内容；没有说明时只写 `output rejected` / `scoring error`。

所有遍都失败时（例如 agent 镜像都没建成）publish 照常回传 manifest，状态为 `failed`，没有产出时跳过下载包。

### `GET /evals/:id`（仅 owner 或管理员，否则 403）
```json
{"eval_id": "...", "status": "running:stage-2", "run_url": "https://github.com/.../actions/runs/1",
 "manifest": { ... }, "total_score": 0.775,
 "mode": "agent", "taskset": "...", "stages": 2, "stage_names": ["stage-1","stage-2"],
 "model": "...", "replicas": 1, "score_public": false, "owner_login": "...",
 "created_at": "...", "updated_at": "...", "download_available": false, "complete": true}
```
`run_url`、`manifest`、`total_score` 可能不存在。被平台判定卡住而标为失败的评测另有 `error`（原因，见“定时清理”）。`complete`：这次评测是否跑了题目包的全部阶段（题目包已不存在时不带）；为 `false` 时总分只是已跑阶段的合计，网页不用题目包的总分名称（如“四卡总分”），也不进完整排行榜。评测未结束时，Worker 会顺便查询 GitHub 上对应的 run（按 run-name 中的 eval_id 匹配），据此粗略估计状态：
- run 排队中 → `queued`
- 当前 job 名以 `generate`/`run` 开头 → `running:<第一个阶段>`
- 当前 job 名以 `score`/`publish` 开头 → `scoring`
- 其他 job → `building`
- run 以失败或取消结束 → `failed`
- run 成功结束但 10 分钟内仍未收到结果 → `failed`

workflow 通过 `/internal/status` 上报的精确状态优先。估计值只能让状态前进，不会覆盖已上报的更靠后的状态。

估计值只用于展示，不写库；只有发现 run 已结束（`failed`），或第一次发现 run 成功结束但没有结果（记下开始宽限的时间）时才写一行。`evals` 的状态更新都带 `WHERE status NOT IN ('done','failed')`，终态不会被覆盖。

回传的结果单独存在 D1 表 `results`（主键 eval_id），状态更新只写 `evals`、从不碰结果。展示时（详情和列表都一样）结果优先：结果状态为终态（`done`/`failed`）时以它为准，否则取两者中更靠后的状态（记录已是 `failed` 则保持 `failed`）。列表用 `evals LEFT JOIN results` 一次查出，所以结果一到列表就同步。

### `GET /evals/:id/download`（仅 owner 或管理员）
- 请求头带 `Accept: application/json` 时 → `200 {"url": "<下载地址>"}`（网页使用这种方式）。
- 否则 → 302 到同一地址。
- 地址为 `https://github.com/<repo>/releases/download/blobs-NN/<zip 的 sha256>`，指向用下载密码加密的 AES zip。没有产出时返回 404 `not_ready`。

## 给 GitHub Actions 的内部接口

认证：`Authorization: Bearer <CRUCIBLE_WORKER_TOKEN>`，Worker 端用常量时间比较。`:id` 必须是 UUID v4。

| 接口 | 说明 |
|---|---|
| `GET /internal/cred/:id` | → `200 application/octet-stream`：原样保存的 envelope 字节（即 `cred_envelope` 经 base64 解码后的内容）。不存在或已过期返回 404。`crucible cred open` 同时接受原始字节和 base64。 |
| `DELETE /internal/cred/:id` | → 204（幂等）。 |
| `POST /internal/status/:id` | `{"status": "building" \| "running:<阶段名>" \| "scoring" \| "failed"}` → `200 {"ok":true,"status"}`。评测已结束时返回 409；与当前状态相同时不写入，直接返回 200。`building` 不写库（查询时按 GitHub 估计得到的也是 `building`），返回当前状态。其他状态每次更新一行；第一次写入时顺带记下 run（查 GitHub 一次）。 |
| `GET /internal/tasksets/:id?github_id=N` | `:id` 为 `u-<16 hex>`。→ 该题目包的 `taskset.json`；未就绪 409；该用户既不是上传者、题目包也未公开时 403。 |
| `POST /internal/tasksets/:id` | taskset-pack 的结果：`{"status": "ready", "taskset": {...}}`（`name` 必须等于 id，打分器必须是用户可用的，校验通过）或 `{"status": "failed", "error": "<≤500 字符>"}`。只接受一次（之后 409）。 |
| `GET /internal/plugins/:id?taskset=T` | `:id`、`T` 都是 `u-<16 hex>`。→ 插件的固定形式 `{"kind", "name", "version", "blob", "image", "runs_taskset_code", "model", "accepts"}`（`image` = `{"blob", "id": "sha256:<64 hex>"}`，登记时构建一次的镜像；旧插件没有）；插件未就绪 409；题目包 `T` 的上传者既不是插件上传者、插件也未公开时 403。 |
| `POST /internal/plugins/:id` | plugin-pack 的结果（≤ 128 KB）：`{"status": "ready", "plugin": {...}, "title", "description", "selftest", "review"?}`（`review` 为审核材料，格式见 `GET /plugins/:id/review`，可缺）（`plugin.name` 必须等于 id，`plugin.blob.sha256` 必须是上传的 hash）或 `{"status": "failed", "error": "<≤500 字符>"}`。只接受一次（之后 409）。 |
| `POST /internal/results/:id` | 请求体是一个 crucible-core `Manifest`（`eval_id` 必须与 URL 一致；`download` 就是 Manifest 自带的字段，由 `crucible download-zip` 写入），外加两个可选的顶层字段：`download: {"sha256": "<密码 zip 的哈希>"}`，以及 `status`（默认 `done`；如果只是回传部分结果、run 还在继续，可填 `running:<阶段>` 或 `scoring`）。不超过 1,900,000 字节（D1 单行上限 2 MB）。Worker 把 manifest（去掉这两个字段）存进 `results` 表（只写这一行）；进入终态时删除凭据。终态结果只会被新的终态结果替换，之后到达的部分结果会被忽略。→ `200 {"ok":true,"status"}`。幂等：再次回传相同内容时什么也不写，照样返回 200。 |

## 触发参数（workflow_dispatch）

只传非秘密参数，每个值在 workflow 内还会再校验一次。GitHub 会拒绝 workflow 没有声明的 input（422），所以下面的参数名与 workflow 文件必须完全一致。

**agent 模式 → `EVAL_WORKFLOW`（默认 `eval.yml`，与 PR #2 中 eval.yml 声明的 10 个 input 一致）**

| input | 值 |
|---|---|
| `agent_source` | `blob:<upload_hash>` |
| `taskset` | 题目包名 |
| `model` | 模型名 |
| `endpoint` | `""`（接口地址在加密凭据里） |
| `replicas` | `"1"`…`"10"` |
| `cred_source` | `workers-kv`（历史名称，表示“向 Worker 取凭据”；凭据现存 D1） |
| `eval_id` | UUID v4 |
| `score_public` | `"true"` / `"false"` |
| `owner` | `<github_id>:<login>` |
| `options` | JSON 字符串 `{"stages": N, "results_url": "<worker>/internal/results/<eval_id>", "budget"?: {...}}` |

**app 模式 → `SCORE_WORKFLOW`（默认 `score.yml`）**

`eval_id`，`artifact_source` = `blob:<upload_hash>`，`taskset`，`stage`（从 1 开始的序号），`cred_source`（有凭据时为 `workers-kv`，否则为 `none`），`model`（提交时填的模型，可为空；打分阶段用模型时使用，题目包的 `model.name` 优先），`score_public`，`owner`，`results_url`。

两个 workflow 的 `run-name` 都必须包含 eval_id（Worker 靠它找到对应的 run）。workflow 用 `results_url` 的 origin 作为 Worker 地址，调用上面的内部接口。

## 存储

### D1（绑定名 `CRUCIBLE_DB`，数据库 `octos-crucible`）

表结构见 `crates/crucible-worker/migrations/`（`0001_init.sql` … `0006_quotas.sql`，`wrangler d1 migrations apply` 按序执行）。

| 表 | 内容 | 索引 |
|---|---|---|
| `uploads` | 主键 `hash`；`owner_id, kind, size, created_at` | 主键 |
| `evals` | 主键 `eval_id`；提交参数、`status`、`run_id`、`run_url`、`run_completed_s`、时间、`error`（平台判定卡住时的原因） | `(owner_id, created_s DESC)`（本人列表、配额）、`(created_s DESC)`（管理员 `?all=1`）、`(taskset, created_s DESC) WHERE score_public = 1`（排行榜）、`(status, updated_at)`（定时清理） |
| `results` | 主键 `eval_id`；`manifest`（JSON）、`download_sha256`、`status`、`total_score`、`display`、`updated_at` | 主键（列表 JOIN） |
| `user_tasksets` | 主键 `id`；用户题目包 | `(owner_id)`、`(public, status)` |
| `user_plugins` | 主键 `id`；用户插件：`status`、`public`、`title`、`plugin`（固定形式 JSON）、`info`（说明与自检结果）、`review`（审核材料 JSON）、`approval`（设为公开时的审核记录 JSON） | `(owner_id)`、`(public, status)` |
| `tokens` | 主键 `id`；`owner_id, login, name, hash, created_at`（hash = SHA-256(令牌)） | `(owner_id)` |
| `bans` | 主键 `github_id`；`by_id, at, reason` | 主键 |
| `quotas` | 主键 `github_id`；各项覆盖值（空 = 默认）、`exempt`（空 = 管理员豁免、其他人不豁免；1 豁免；0 不豁免，管理员也不豁免）、`by_id, at` | 主键 |
| `cache` | 主键 `key`；`tasksets`、`leaderboard`、`leaderboard/<题目包>`（5 分钟）、`release/<tag>`（不过期） | 主键 |
| `creds` | 主键 `eval_id`；`envelope`（凭据 envelope 字节的标准 base64）、`expires_s`（提交时 + 24 小时）。读时过期即视为不存在；run 结束时删除 | `(expires_s)`（Cron 清理） |

常用查询都走主键或索引：详情 2 次主键查询，列表按 `owner_id` 索引取最多 1000 行并按主键 JOIN `results`；排行榜只读公开评测的部分索引，再按主键 JOIN `results`，最后按主键取上榜的至多 100 份 manifest。

一次评测的写入（行数，不含索引）：上传 1、提交 1、每个存下来的进度上报 1（agent 模式 `running:<阶段>` × 阶段数 + `scoring`；app 模式 `scoring`）、结果 1；带凭据时另加凭据写入 1、删除 1。2 个阶段的 agent 评测共 8 行，不带凭据的 app 评测 4 行。查询详情/列表不写（只在发现 run 已失败时写 1 行）。

### 定时清理（Cron Trigger）

`wrangler.toml` 的 `[triggers] crons = ["17 * * * *"]`：每小时一次：

1. 删除 `creds` 与 `cache` 中已过期的行，日志写 `cron: purged N expired rows`。
2. 卡住的登记：创建超过 `STUCK_REGISTRATION_S`（默认 2 小时）仍是 `packing` 的题目包、`building` 的插件标为 `failed`，`error` 写明"打包/构建超时（N 小时内没有收到结果），请重新上传"。
3. 卡住的评测：未结束、且超过 1 小时没有任何更新（记录和结果都没动）的评测，每次最多 15 个，先按 `GET /evals/:id` 同样的逻辑查 GitHub run（run 已结束的照常收尾）；仍未结束的，若找不到 run 且已超过 `STUCK_REGISTRATION_S`，或提交已超过 `STUCK_EVAL_S`（默认 30 小时），标为 `failed`，原因写入 `evals.error`（网页详情和 `crucible status` 显示），并删除残留凭据。

Worker 不再使用 Workers KV（免费版每天 1000 次写入，曾因此整天无法提交）。带凭据的评测额外写 D1 2 行（写入 + 删除）。

## 管理

- `POST /admin/ban`：`{"github_id": 123, "reason"?: "..."}` → `{"ok":true,"github_id":123,"banned":true}`。只有管理员能调用；不能封禁管理员。
- `POST /admin/unban`：`{"github_id": 123}` → `{"ok":true,"github_id":123,"banned":false}`。
- `GET /admin/quotas/:github_id` → 该用户的 `GET /quota` 视图，另带 `override`（覆盖行）。
- `PUT /admin/quotas/:github_id`：`{"evals_per_day"?: n|null, ...各项, "exempt"?: true|false|null}`，整行替换（缺省或 null = 用默认）；`{}` 删除覆盖。→ 同 GET。管理员接口只接受网页会话，不接受命令行令牌。

管理员名单来自环境变量 `ADMIN_GITHUB_IDS`。

## 配置

`crates/crucible-worker/wrangler.toml`：

| 名称 | 类型 | 说明 |
|---|---|---|
| `CRUCIBLE_DB` | D1 绑定 | 数据库 `octos-crucible`，`migrations_dir = "migrations"` |
| `PAGES_ORIGIN` | var | CORS 源，例如 `https://octos-org.github.io` |
| `PAGES_URL` | var | 登录后跳转的地址，必须以 `PAGES_ORIGIN/` 开头，默认 `PAGES_ORIGIN/` |
| `GITHUB_REPO` | var | `octos-org/octos-crucible` |
| `ADMIN_GITHUB_IDS` | var | 逗号分隔的 GitHub 数字 id |
| `WORKER_URL` | var，可选 | `results_url` 的前缀，默认取请求的 origin |
| `EVAL_WORKFLOW` / `SCORE_WORKFLOW` / `TASKSET_WORKFLOW` / `PLUGIN_WORKFLOW` / `EVAL_REF` | var，可选 | 默认分别为 `eval.yml` / `score.yml` / `taskset-pack.yml` / `plugin-pack.yml` / `main` |
| `GITHUB_CLIENT_ID`、`GITHUB_CLIENT_SECRET` | secret | GitHub OAuth App |
| `GITHUB_TOKEN` | secret | 细粒度 token，只授权本仓库 |
| `SESSION_HMAC_KEY` | secret | ≥ 32 字节随机值 |
| `CRUCIBLE_WORKER_TOKEN` | secret | ≥ 32 个字符；Actions 里存同一个值 |
| `QUOTA_UPLOADS_PER_DAY`、`QUOTA_UPLOAD_BYTES_PER_DAY`、`QUOTA_EVALS_RUNNING`、`QUOTA_EVALS_PER_DAY`、`QUOTA_PLUGINS_PER_DAY`、`QUOTA_TASKSETS_PER_DAY` | var，可选 | 每个用户的默认配额（见“配额”），缺省 50 / 500 MiB / 3 / 20 / 10 / 10 |
| `STUCK_REGISTRATION_S`、`STUCK_EVAL_S` | var，可选 | 定时清理的时限（秒，≥ 600），缺省 7200 / 108000 |
| `DEV_AUTH`、`GITHUB_API_BASE`、`GITHUB_WEB_BASE` | 仅本地 | 开启 dev-login，并把 GitHub 指向 mock；只对 localhost 的请求生效，生产环境不要设置 |

配置缺失或格式不对时，所有请求都返回 500 `internal`，日志里只写出有问题的配置名，不写值。

## 部署步骤（需要仓库维护者执行）

需要准备的东西：
1. **Cloudflare 账号**（免费计划即可），本机执行 `npx wrangler login`。
2. **GitHub OAuth App**（组织设置 → Developer settings → OAuth Apps）：Homepage 填 Pages 地址，Authorization callback URL 填 `https://<worker 域名>/auth/callback`。记下 Client ID，并生成 Client Secret。
3. **细粒度 Personal Access Token 或 GitHub App**：Repository access 只选 `octos-org/octos-crucible`，权限为 **Contents: Read and write**（上传 release asset、创建 blobs release、读取 tasksets）和 **Actions: Read and write**（workflow_dispatch、查询 run），其余权限都不给。
4. 两个随机值：`openssl rand -hex 32` 分别生成 `SESSION_HMAC_KEY` 和 `CRUCIBLE_WORKER_TOKEN`。

执行：
```sh
cargo install worker-build --version 0.8.6 --locked
rustup target add wasm32-unknown-unknown
cd crates/crucible-worker
npx wrangler d1 create octos-crucible            # 把 database_id 填进 wrangler.toml
npx wrangler d1 migrations apply octos-crucible --remote
# 修改 wrangler.toml 的 [vars]：ADMIN_GITHUB_IDS，需要时也改 PAGES_URL / WORKER_URL
for s in GITHUB_CLIENT_ID GITHUB_CLIENT_SECRET GITHUB_TOKEN SESSION_HMAC_KEY CRUCIBLE_WORKER_TOKEN; do
  npx wrangler secret put "$s"
done
npx wrangler deploy
```
然后在仓库的 Actions secrets 中加入 `CRUCIBLE_WORKER_TOKEN`（与 Worker 中的值相同），供 eval.yml 调用内部接口。最后把网页的 API 地址指向这个 Worker 的域名。

## 本地联调

```sh
cd crates/crucible-worker
cp .dev.vars.example .dev.vars          # 全是假值，GitHub 指向本地 mock
npx wrangler d1 migrations apply octos-crucible --local
node dev/mock-github.mjs &              # 127.0.0.1:9911
npx wrangler dev --test-scheduled --port 8787 &
node dev/e2e.mjs                        # 依次测试：登录 → 上传 → 提交 → 取凭据 → 删除凭据 → 上报进度 → 回传结果 → 查询 → 下载 → 定时清理 → 封禁
```
CI（`.github/workflows/worker.yml`）在 PR 上运行同样的流程（只构建和测试）；push 到 main 时 `deploy` job 先执行 `wrangler d1 migrations apply octos-crucible --remote`，再 `wrangler deploy`（Cloudflare 令牌需要 Workers 与 D1 的编辑权限；`wrangler deploy` 同时设置 Cron Trigger）。本地 `wrangler dev --test-scheduled` 后可用 `curl 'http://localhost:8787/__scheduled?cron=17+*+*+*+*'` 触发一次清理。
