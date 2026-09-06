#!/bin/bash
# 集成测试增强版 — 输出详细日志到文件
# 用法: ./tools/itest.sh [smoke|full|stop|env]
set -euo pipefail
cd "$(dirname "$0")"
ROOT="$(cd ../.. && pwd)"

MODE="${1:-smoke}"
LOG_FILE="$ROOT/tools/itest_output.txt"

exec > >(tee "$LOG_FILE") 2>&1

echo "=== NewProxy Rust 集成测试 ($MODE) ==="
echo ""

# ─── stop ───
if [ "$MODE" = "stop" ]; then
    echo "=== 停止测试环境 ==="
    pkill newproxy 2>/dev/null || true
    docker compose down 2>/dev/null || true
    echo "已停止"
    exit 0
fi

# ─── env ───
if [ "$MODE" = "env" ]; then
    echo "=== NewProxy 测试环境 (手动模式) ==="
    echo ""

    echo "[1/3] 启动 MySQL 容器..."
    docker compose down --remove-orphans 2>/dev/null || true
    docker rm -f newproxy-test-mysql 2>/dev/null || true
    docker compose up -d --wait 2>&1
    echo "MySQL 就绪 (127.0.0.1:3306)"

    echo ""
    echo "[2/3] 编译 newproxy..."
    cd "$ROOT"
    cargo build --release 2>&1 | tail -3
    echo "编译完成"

    echo ""
    echo "[3/3] 启动代理..."
    pkill newproxy 2>/dev/null || true
    mkdir -p logs
    RUST_LOG=newproxy=debug target/release/newproxy tests/integration/newproxy-test.conf &
    PROXY_PID=$!
    sleep 3

    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "  测试环境已就绪"
    echo "  代理: 127.0.0.1:4051  管理: 127.0.0.1:9111"
    echo "  MySQL: 127.0.0.1:3306  (root/test_password)"
    echo "  日志: tail -f logs/newproxy.log"
    echo "  按 Ctrl+C 停止并清理"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

    cleanup() {
        echo ""
        echo "停止代理..."
        kill $PROXY_PID 2>/dev/null || true
        wait $PROXY_PID 2>/dev/null || true
        echo "停止 MySQL..."
        docker compose down 2>/dev/null || true
        echo "环境已清理"
    }
    trap cleanup EXIT

    wait $PROXY_PID
    exit 0
fi

# ─── 1. 启动 MySQL ───
echo "[1/4] 启动 MySQL 容器..."
docker compose down --remove-orphans 2>/dev/null || true
docker rm -f newproxy-test-mysql 2>/dev/null || true
docker compose up -d --wait 2>&1
echo "MySQL 就绪"

# ─── 2. 编译代理 ───
echo "[2/4] 编译 newproxy..."
cd "$ROOT"
cargo build --release 2>&1 | tail -3
echo "编译完成"

# ─── 3. 启动代理 ───
echo "[3/4] 启动代理 (port 4051)..."
pkill newproxy 2>/dev/null || true
mkdir -p logs
RUST_LOG=newproxy=debug target/release/newproxy tests/integration/newproxy-test.conf &
PROXY_PID=$!
sleep 3

cleanup() {
    echo ""
    echo "停止代理 (PID=$PROXY_PID)..."
    kill $PROXY_PID 2>/dev/null || true
    wait $PROXY_PID 2>/dev/null || true
    echo "logs/newproxy.log 内容:"
    cat logs/newproxy.log 2>/dev/null || true
}
trap cleanup EXIT

# ─── 4. 运行测试 ───
echo "[4/4] 运行测试..."
PASS=0
FAIL=0

run_test() {
    local name="$1"; shift
    echo -n "  $name ... "
    if "$@" >/dev/null 2>&1; then
        echo "PASS"
        PASS=$((PASS + 1))
    else
        echo "FAIL"
        echo "    命令: $*"
        FAIL=$((FAIL + 1))
    fi
}

run_test_neg() {
    local name="$1"; shift
    echo -n "  $name ... "
    if "$@" >/dev/null 2>&1; then
        echo "FAIL"
        echo "    命令(期望失败): $*"
        FAIL=$((FAIL + 1))
    else
        echo "PASS"
        PASS=$((PASS + 1))
    fi
}

MYSQL="mysql -h 127.0.0.1 -P 4051 -u root -ptest_password"

run_test "连接"       $MYSQL test -e "SELECT 1"
run_test "SELECT"     $MYSQL test -N -e "SELECT COUNT(*) FROM users"
run_test "INSERT"     $MYSQL test -e "INSERT INTO users(name) VALUES('itest')"
run_test "UPDATE"     $MYSQL test -e "UPDATE users SET name='updated' WHERE name='itest'"
run_test "DELETE"     $MYSQL test -e "DELETE FROM users WHERE name='updated'"
run_test "SHOW"       $MYSQL test -e "SHOW DATABASES"
run_test "管理命令"    $MYSQL test -e "checkproxy show status"
run_test_neg "认证拒绝" mysql -h 127.0.0.1 -P 4051 -u root -pwrong test -e "SELECT 1" 2>/dev/null

echo ""
echo "=== 结果: $PASS 通过, $FAIL 失败 ==="
[ "$FAIL" -eq 0 ] || exit 1
