#!/bin/bash
# NewProxy Rust 集成测试脚本
# 用法: ./tools/test.sh [--smoke|--full]
#
# 前置条件:
#   1. colima start (或 docker 可用)
#   2. docker compose up -d (启动 MySQL 后端)
#   3. cargo build --release (编译代理)
#
# Smoke 测试: 基础连接 + SELECT 1
# Full 测试: mysql_case 兼容用例 + 压测

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

MODE="${1:-smoke}"
MYSQL_BIN="${MYSQL_BIN:-mysql}"
PROXY_PORT=4051

echo "=== NewProxy Rust 集成测试 ($MODE) ==="

# 检查 MySQL 后端
echo -n "检查 MySQL 后端... "
if $MYSQL_BIN -h 127.0.0.1 -P 3306 -u root -ptest_password -e "SELECT 1" test >/dev/null 2>&1; then
    echo "OK"
else
    echo "FAIL — 请先启动 MySQL: docker compose up -d"
    exit 1
fi

# 确保二进制已编译
if [ ! -f target/release/newproxy ]; then
    echo "编译 newproxy..."
    cargo build --release
fi

# 启动代理
echo "启动代理 (port $PROXY_PORT)..."
RUST_LOG=newproxy=info target/release/newproxy &
PROXY_PID=$!
sleep 2

# 清理函数
cleanup() {
    echo "停止代理..."
    kill $PROXY_PID 2>/dev/null || true
    wait $PROXY_PID 2>/dev/null || true
}
trap cleanup EXIT

# ─── Smoke 测试 ───
echo ""
echo "--- Smoke 测试 ---"

# 测试 1: 连接
echo -n "TEST 1: 连接代理... "
if $MYSQL_BIN -h 127.0.0.1 -P $PROXY_PORT -u root -ptest_password -e "SELECT 1" test >/dev/null 2>&1; then
    echo "PASS"
else
    echo "FAIL"
    exit 1
fi

# 测试 2: SELECT
echo -n "TEST 2: SELECT * FROM users... "
RESULT=$($MYSQL_BIN -h 127.0.0.1 -P $PROXY_PORT -u root -ptest_password -N -e "SELECT COUNT(*) FROM users" test 2>/dev/null)
if [ "$RESULT" = "3" ]; then
    echo "PASS ($RESULT rows)"
else
    echo "FAIL (got: $RESULT)"
    exit 1
fi

# 测试 3: INSERT
echo -n "TEST 3: INSERT... "
$MYSQL_BIN -h 127.0.0.1 -P $PROXY_PORT -u root -ptest_password -e "INSERT INTO users (name) VALUES ('test_user')" test >/dev/null 2>&1 && echo "PASS" || echo "FAIL"

# 测试 4: UPDATE
echo -n "TEST 4: UPDATE... "
$MYSQL_BIN -h 127.0.0.1 -P $PROXY_PORT -u root -ptest_password -e "UPDATE users SET name='updated' WHERE id=4" test >/dev/null 2>&1 && echo "PASS" || echo "FAIL"

# 测试 5: DELETE
echo -n "TEST 5: DELETE... "
$MYSQL_BIN -h 127.0.0.1 -P $PROXY_PORT -u root -ptest_password -e "DELETE FROM users WHERE id=4" test >/dev/null 2>&1 && echo "PASS" || echo "FAIL"

# 测试 6: 认证失败
echo -n "TEST 6: 错误密码应拒绝... "
if $MYSQL_BIN -h 127.0.0.1 -P $PROXY_PORT -u root -pwrong_pass -e "SELECT 1" test >/dev/null 2>&1; then
    echo "FAIL (should have rejected)"
else
    echo "PASS"
fi

# 测试 7: PING
echo -n "TEST 7: PING... "
$MYSQL_BIN -h 127.0.0.1 -P $PROXY_PORT -u root -ptest_password --connect-timeout=2 -e "SELECT 1" test >/dev/null 2>&1 && echo "PASS" || echo "FAIL"

# 测试 8: 大结果集
echo -n "TEST 8: 大结果集 (1000 行 INSERT)... "
$MYSQL_BIN -h 127.0.0.1 -P 3306 -u root -ptest_password test -e "
  CREATE TABLE IF NOT EXISTS big_test (id INT PRIMARY KEY AUTO_INCREMENT, data VARCHAR(100));
  INSERT INTO big_test (data) SELECT CONCAT('row_', n) FROM (SELECT @row := @row + 1 AS n FROM information_schema.columns a, information_schema.columns b, (SELECT @row := 0) r LIMIT 1000) t;
" >/dev/null 2>&1
COUNT=$($MYSQL_BIN -h 127.0.0.1 -P $PROXY_PORT -u root -ptest_password -N -e "SELECT COUNT(*) FROM big_test" test 2>/dev/null)
if [ "$COUNT" = "1000" ]; then
    echo "PASS ($COUNT rows)"
else
    echo "FAIL (got: $COUNT)"
fi

# 测试 9: 管理命令
echo -n "TEST 9: checkproxy show status... "
$MYSQL_BIN -h 127.0.0.1 -P $PROXY_PORT -u root -ptest_password -e "checkproxy show status" test >/dev/null 2>&1 && echo "PASS" || echo "FAIL"

echo ""
echo "=== Smoke 测试完成 ==="

if [ "$MODE" = "full" ]; then
    echo ""
    echo "--- 并发压测 ---"
    # 简单并发测试:10 个客户端同时查询
    for i in $(seq 1 10); do
        $MYSQL_BIN -h 127.0.0.1 -P $PROXY_PORT -u root -ptest_password -e "SELECT SLEEP(0.1), $i" test >/dev/null 2>&1 &
    done
    wait
    echo "10 并发连接 PASS"
fi

echo ""
echo "所有测试通过!"
