# 发布 crucible 预编译二进制

`.github/workflows/release.yml` 在推送版本标签时编译 `crucible`，并发布到 GitHub Release。
评测流程直接下载固定版本的二进制，不用每次从源码编译。

## 产物

目前只发布一个目标：`x86_64-unknown-linux-gnu`（GitHub Actions 的 ubuntu runner 用）。

每个 Release 包含：

- `crucible-<版本>-x86_64-unknown-linux-gnu.tar.gz`：包内只有一个可执行文件 `crucible`
- `SHA256SUMS`：`sha256sum` 格式的校验和

## 发版步骤

1. 改 `Cargo.toml` 里 `[workspace.package]` 的 `version`（如 `0.2.0`），提 PR 合并到 main。
   第一版不用改，当前就是 `0.1.0`。
2. 在 main 的这个提交上打标签并推送（标签必须与 Cargo 版本一致，格式只能是 `vX.Y.Z`）：

   ```sh
   git checkout main && git pull
   git tag -a v0.1.0 -m "crucible 0.1.0"
   git push origin v0.1.0
   ```

3. 到 Actions 看 `release` 运行：
   - `build`：检查标签与 Cargo 版本一致，`cargo build --release --locked`，跑 `crucible --version`，打包并生成 `SHA256SUMS`。
   - `publish`：复核校验和，`gh release create` 创建 Release。
4. Release 不是 prerelease，并显式设为 latest。仓库里另有 `blobs-00..31` 这些存储用的预发布，
   它们不会被当成 latest，也不影响版本 Release。

标签与 Cargo 版本不一致、或标签带预发布后缀（如 `v0.2.0-rc.1`）时，workflow 直接失败，不会发布。
同一标签重复运行时，`gh release create` 会因为 Release 已存在而失败；要重发请先删掉旧 Release。

## 演练

Actions 页面手动运行 `release`（workflow_dispatch）：只构建和打包，版本号为 `<Cargo 版本>-dryrun.<短 SHA>`，
产物以 workflow artifact `crucible-dist` 保存 7 天，不创建 Release。
workflow_dispatch 只能对默认分支上已有的 workflow 文件使用，所以本 PR 合并后才能演练。

## 权限

- 顶层 `permissions: {}`。
- `build`：`contents: read`。
- `publish`：`contents: write`（创建 Release），只用 `github.token`，不需要额外 secret。
- 所有第三方 action 锁定到提交 SHA；发布构建不使用缓存。

## 安装脚本

`tools/install-crucible.sh <版本> <安装目录>`：下载对应的包和 `SHA256SUMS`，校验 SHA-256，
解压并安装 `<安装目录>/crucible`，最后执行 `crucible --version`。任何一步失败都以非零退出。

```sh
tools/install-crucible.sh 0.1.0 "$HOME/.local/bin"
```

只支持 Linux x86_64；其他平台（如管理员的 macOS）请从源码 `cargo install --path crates/crucible-cli`。
可用 `CRUCIBLE_REPO` 改仓库，`CRUCIBLE_BASE_URL` 改下载根地址（镜像或本地测试）。

## 评测 workflow 如何改为下载固定版本

（这里只是说明，`eval.yml` 在另一个分支修改。）

把“安装 Rust 工具链 + `cargo build`”这几步换成下载固定版本：

```yaml
env:
  CRUCIBLE_VERSION: 0.1.0   # 固定版本，升级时改这里

steps:
  - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
    with:
      persist-credentials: false
  - name: install crucible
    run: |
      tools/install-crucible.sh "$CRUCIBLE_VERSION" "$RUNNER_TEMP/bin"
      echo "$RUNNER_TEMP/bin" >> "$GITHUB_PATH"
  - run: crucible --version
```

要点：

- 版本写死在 workflow 里，评测结果可复现；升级 crucible 就是改 `CRUCIBLE_VERSION` 的一个 PR。
- 用仓库内的 `tools/install-crucible.sh`（随 checkout 一起拿到），不要 `curl | bash` 远程脚本。
- 评测 job 不再需要 Rust 工具链和缓存，权限保持 `contents: read` 即可（下载公开 Release 不需要 token）。
- runner 要用 `ubuntu-*`（x86_64）。
