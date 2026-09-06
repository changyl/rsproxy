#!/bin/bash
# NewProxy Rust 编译脚本
# 用法: ./tools/build.sh [debug|release|check|clean|ci]
#   debug   — cargo build (默认)
#   release — cargo build --release (生产二进制 target/release/newproxy)
#   check   — cargo check (仅类型检查,不生成产物)
#   clean   — cargo clean
#   ci      — fmt + clippy + build + test,等同 .github/workflows/ci.yml
#
# 产物二进制: target/<mode>/newproxy

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

MODE="${1:-release}"
BIN_PATH="target/release/newproxy"
START_TIME=$SECONDS

color() { printf "\033[%sm%s\033[0m\n" "$1" "$2"; }
step()  { echo ""; echo -n "── $1 ── "; }

case "$MODE" in
    debug)
        step "编译 (debug)"
        cargo build
        BIN_PATH="target/debug/newproxy"
        ;;

    release)
        step "编译 (release, LTO + strip)"
        cargo build --release
        ;;

    check)
        step "类型检查 (cargo check)"
        cargo check --all-targets
        echo ""
        echo "=== check 完成 (无产物) ==="
        exit 0
        ;;

    clean)
        step "清理 target/"
        cargo clean
        echo ""
        echo "=== 清理完成 ==="
        exit 0
        ;;

    ci)
        export CARGO_TERM_COLOR=always
        export RUSTFLAGS="-D warnings"

        step "fmt --check"
        cargo fmt --check
        echo "PASS"

        step "clippy (-D warnings)"
        cargo clippy --all-targets -- -D warnings
        echo "PASS"

        step "build --release"
        cargo build --release

        step "test --all-targets"
        cargo test --all-targets
        echo ""
        echo "=== CI 全部通过 ==="
        exit 0
        ;;

    *)
        echo "用法: $0 [debug|release|check|clean|ci]"
        echo "  debug   — cargo build"
        echo "  release — cargo build --release (默认)"
        echo "  check   — cargo check (仅类型检查)"
        echo "  clean   — cargo clean"
        echo "  ci      — fmt + clippy + build + test"
        exit 1
        ;;
esac

# ─── 验证产物 ───
echo ""
step "产物"
if [ -f "$BIN_PATH" ]; then
    SIZE=$(du -h "$BIN_PATH" | cut -f1)
    color "32" "✓ $BIN_PATH ($SIZE)"
else
    color "31" "✗ 未找到产物 $BIN_PATH"
    exit 1
fi

ELAPSED=$((SECONDS - START_TIME))
echo ""
echo "=== 编译完成 (${ELAPSED}s) ==="
