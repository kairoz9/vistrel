#!/usr/bin/env bash
# 生成单个 jar 的 per-version changelog。
#
# 0.1.0 首版把 "Initial release." 插在标题行之后、正文之前；其余版本直接复用
# annotated tag 的正文。三个发布 job（CurseForge / Modrinth / BBSMC）共用同一份
# 逻辑，避免首版特例在多个 workflow 里各写一遍后漂移。
#
# 用法: compose-changelog.sh <version> <release-body.md> <out.md>
set -euo pipefail

version="$1"
body="$2"
out="$3"

{
  if [ "$version" = "0.1.0" ]; then
    head -1 "$body"
    echo
    echo "Initial release."
    tail -n +2 "$body"
  else
    cat "$body"
  fi
} > "$out"

echo "changelog: $out"
head -5 "$out"
