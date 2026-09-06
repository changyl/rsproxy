#!/bin/bash
# NewProxy 内置性能基准:in-process proxy + mock backend
# 不依赖 Docker/MySQL,直接测量代理层开销(前端解析/转发/协议细节)。
#
# 用法:
#   ./tools/perf.sh                 # 默认:3s/场景,并发 1 4 16 64,输出到 stdout + tests/perf/bench_report.md
#   PERF_SECS=5 PERF_CONNS="1 8 32" ./tools/perf.sh
#   ./tools/perf.sh long            # 60s 长跑(配合 sample/Instruments 采样定位 CPU 热点)
#
# 注意:必须 release 构建,debug 下 tokio 与分配开销失真。

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

MODE="${1:-bench}"
REPORT="tests/perf/bench_report.md"

echo "=== NewProxy 内置性能基准 (release) ==="

case "$MODE" in
  bench)
    # 只运行 proxy_perf_bench(排除并行启动的 long-run 测试,保证报告纯净)
    cargo test --release --test perf_proxy proxy_perf_bench -- --ignored --nocapture --test-threads=1 \
      | tee "$REPORT"
    echo
    echo "报告已写入: $REPORT"
    ;;
  long)
    cargo test --release --test perf_proxy proxy_perf_long_run -- --ignored --nocapture
    ;;
  *)
    echo "未知模式: $MODE (bench|long)" >&2
    exit 2
    ;;
esac
