#!/usr/bin/env bash
# ThreadSanitizer 测试档：nightly 插桩跑纯 Rust 用例，抓真实数据竞争。
# std 无预编译插桩版，须 -Zbuild-std 就地重建。
# 排除规则：
#   驱动用例已标 #[ignore = "requires GPU"]，默认不进入本档（GPU 作业用 --ignored 单独跑）
#   encode::software —— OpenH264 原生库无插桩，不在 TSan 语义内
set -euo pipefail
cd "$(dirname "$0")/.."
RUSTFLAGS="-Zsanitizer=thread" cargo +nightly test --target x86_64-unknown-linux-gnu \
    -Zbuild-std=std --lib -- --skip encode::software "$@"
