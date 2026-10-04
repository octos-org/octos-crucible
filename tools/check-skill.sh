#!/usr/bin/env bash
# 检查 web/public/skill.md 里写到的 crucible 命令是否仍然存在：
# 提取代码块和行内代码里的每条 `crucible ...` 命令，逐级核对子命令
# （必须出现在上一级 --help 的 Commands 列表里），再核对每个 --参数
# 出现在该子命令的 --help 输出里；正文里单独写的 `--参数` 必须出现在
# 文中用到的某个子命令的 --help 里。任何一处不存在即失败。
#
# 用法：tools/check-skill.sh [skill.md]
#   CRUCIBLE=path/to/crucible 指定二进制（默认 PATH 里的 crucible）
set -euo pipefail

SKILL=${1:-"$(dirname "$0")/../web/public/skill.md"}
BIN=${CRUCIBLE:-crucible}
command -v "$BIN" >/dev/null || { echo "check-skill: $BIN not found" >&2; exit 2; }

# One command per line: code-block lines (backslash continuations joined)
# and inline code spans, cut at shell separators, starting at `crucible `.
cmds=$(perl -0777 -ne '
  my @chunks;
  while (/^```[^\n]*\n(.*?)^```/msg) { (my $b = $1) =~ s/\\\n/ /g; push @chunks, split /\n/, $b; }
  (my $prose = $_) =~ s/^```.*?^```//msg;
  push @chunks, $prose =~ /`([^`\n]+)`/g;
  for (@chunks) {
    s/\s#.*$//;
    for my $part (split /\s*(?:\|\||&&|[|;]|\$\()\s*/) {
      print "$1\n" if $part =~ /(?:^|\s)(crucible(?:\s.*)?)$/;
    }
  }' "$SKILL" | sort -u)

# Inline spans that start with a flag, e.g. `--stages N`.
loose=$(perl -0777 -ne 's/^```.*?^```//msg; print "$1\n" while /`(--[a-z0-9][a-z0-9-]*)/g' "$SKILL" | sort -u)

[ -n "$cmds" ] || { echo "check-skill: no crucible commands found in $SKILL" >&2; exit 1; }

fail=0
n=0
all_help=""
while IFS= read -r line; do
  n=$((n + 1))
  if [ -n "${VERBOSE-}" ]; then echo "  $line"; fi
  read -ra words <<<"$line"
  path=()
  help=$("$BIN" --help)
  flags=()
  descending=1
  for w in "${words[@]:1}"; do
    case $w in
      --*) flags+=("${w%%=*}") ;;
      -*) ;;
      *)
        # Only descend while the current level lists subcommands.
        if [ $descending = 1 ] && grep -q '^Commands:' <<<"$help"; then
          if awk '/^Commands:/{c=1;next} /^[^ ]/{c=0} c{print $1}' <<<"$help" | grep -qxF -- "$w"; then
            path+=("$w")
            help=$("$BIN" "${path[@]}" --help)
          else
            echo "FAIL: unknown subcommand '$w' in: $line"
            fail=1
            descending=0
          fi
        else
          descending=0
        fi
        ;;
    esac
  done
  all_help+=$help$'\n'
  for f in "${flags[@]+"${flags[@]}"}"; do
    if ! grep -qE -- "(^|[[:space:],])${f}([[:space:],=]|$)" <<<"$help"; then
      echo "FAIL: 'crucible ${path[*]-}' has no option $f (in: $line)"
      fail=1
    fi
  done
done <<<"$cmds"

for f in $loose; do
  if ! grep -qE -- "(^|[[:space:],])${f}([[:space:],=]|$)" <<<"$all_help"; then
    echo "FAIL: no crucible command used in the skill has option $f"
    fail=1
  fi
done

[ $fail = 0 ] && echo "check-skill: $n crucible command(s) in $(basename "$SKILL") match the CLI"
exit $fail
