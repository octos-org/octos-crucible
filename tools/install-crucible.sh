#!/usr/bin/env bash
# 从 GitHub Release 下载指定版本的 crucible，校验 SHA-256 后安装到给定目录。
#
# 用法：tools/install-crucible.sh <版本> <安装目录>
#   版本：0.1.0 或 v0.1.0
#   例：  tools/install-crucible.sh 0.1.0 "$HOME/.local/bin"
#
# 可选环境变量：
#   CRUCIBLE_REPO      Release 所在仓库，默认 octos-org/octos-crucible
#   CRUCIBLE_BASE_URL  下载根地址（默认 https://github.com/$CRUCIBLE_REPO/releases/download/v<版本>），
#                      用于镜像或本地测试
#
# 任一步骤失败即以非零退出，不会留下半装的二进制。
set -euo pipefail

die() {
  echo "install-crucible: $*" >&2
  exit 1
}

[ "$#" -eq 2 ] || die "用法：$0 <版本> <安装目录>"

version="${1#v}"
dest="$2"
printf '%s' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || die "版本格式应为 X.Y.Z：$1"
[ -n "$dest" ] || die "安装目录不能为空"

# 目前只发布 x86_64-unknown-linux-gnu（GitHub Actions runner 用）
target="x86_64-unknown-linux-gnu"
os="$(uname -s)"
arch="$(uname -m)"
[ "$os/$arch" = "Linux/x86_64" ] || die "只有 Linux x86_64 的预编译包（当前 $os/$arch），其他平台请从源码构建"

repo="${CRUCIBLE_REPO:-octos-org/octos-crucible}"
base_url="${CRUCIBLE_BASE_URL:-https://github.com/${repo}/releases/download/v${version}}"
archive="crucible-${version}-${target}.tar.gz"

command -v curl >/dev/null 2>&1 || die "需要 curl"
command -v tar >/dev/null 2>&1 || die "需要 tar"
if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | awk '{print $1}'; }
else
  die "需要 sha256sum 或 shasum"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fetch() {
  curl -fsSL --proto '=https,file' --retry 3 --retry-delay 2 -o "$tmp/$1" "$base_url/$1" \
    || die "下载失败：$base_url/$1"
}

echo "install-crucible: 下载 $archive"
fetch SHA256SUMS
fetch "$archive"

# 在 SHA256SUMS 中精确匹配文件名（兼容 "hash  name" 与 "hash *name" 两种格式）
expected="$(awk -v f="$archive" '$2 == f || $2 == "*" f { print $1; exit }' "$tmp/SHA256SUMS")"
[ -n "$expected" ] || die "SHA256SUMS 中没有 $archive"
printf '%s' "$expected" | grep -Eq '^[0-9a-f]{64}$' || die "SHA256SUMS 中 $archive 的哈希格式不对"
actual="$(sha256 "$tmp/$archive")"
[ "$actual" = "$expected" ] || die "SHA-256 不匹配：期望 $expected，实际 $actual"
echo "install-crucible: SHA-256 校验通过 $actual"

mkdir -p "$tmp/x"
tar -xzf "$tmp/$archive" -C "$tmp/x" || die "解压失败"
[ -f "$tmp/x/crucible" ] || die "包内没有 crucible"

mkdir -p "$dest"
# 先拷到目标目录内的临时名，再原子改名，避免半装
cp "$tmp/x/crucible" "$dest/.crucible.tmp.$$"
chmod 0755 "$dest/.crucible.tmp.$$"
mv -f "$dest/.crucible.tmp.$$" "$dest/crucible"

echo "install-crucible: 已安装 $dest/crucible"
"$dest/crucible" --version || die "安装后的 crucible 无法运行"
