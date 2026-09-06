#!/bin/bash
# ============================================================================
# NewProxy 测试框架 — 统一入口
# 用法: ./tests/integration/run.sh [command] [options]
#
# Commands:
#   smoke      快速冒烟测试（8 项，默认）
#   full       完整测试 + 10 并发压测
#   env        启动性能测试环境（MySQL+Proxy+sysbench 3容器，Ctrl+C 清理）
#   perf       性能测试 (prepare|baseline|proxy|run|all|cleanup)
#   2shard     2 分片容器场景测试 (up|check|test|all|cleanup) 与性能测试 (perf)
#   cov        代码覆盖率 (unit|e2e|all, 依赖 cargo-llvm-cov)
#   clippy     运行 cargo clippy 检查
#   stop       停止所有容器和代理
#   clean      深度清理：停止服务 + 删除日志/PID/构建缓存
#   logs       查看日志 (proxy | mysql | test | all) [-f]
#   help       显示此帮助
#
# Options（用于 smoke / full / env / perf）:
#   --debug    使用 debug 编译（默认 release）
#   --log      输出完整日志到 tools/itest_output.txt
#
# Examples:
#   ./tests/integration/run.sh                    # 默认 smoke
#   ./tests/integration/run.sh full               # 完整测试
#   ./tests/integration/run.sh env                 # 启动性能测试 3 容器环境
#   ./tests/integration/run.sh perf all           # 完整性能测试
#   ./tests/integration/run.sh 2shard all          # 2 分片场景测试(启动环境+功能检查)
#   ./tests/integration/run.sh 2shard check        # 功能检查(环境缺失时自动拉起,不重建)
#   ./tests/integration/run.sh 2shard test         # 纯测试(环境不在则报错,完全不碰 docker)
#   ./tests/integration/run.sh 2shard perf all     # 2 分片性能测试(准备+直连基线+代理+报告)
#   ./tests/integration/run.sh perf baseline      # 仅直连基线
#   ./tests/integration/run.sh perf proxy         # 仅代理测试
#   ./tests/integration/run.sh smoke --log --debug # debug + 日志
#   ./tests/integration/run.sh clippy             # 静态检查
#   ./tests/integration/run.sh stop               # 清理
#   ./tests/integration/run.sh logs proxy -f      # 实时跟踪代理日志
#   ./tests/integration/run.sh logs mysql         # 查看 MySQL 容器日志
#   ./tests/integration/run.sh logs all           # 列出所有日志文件
# ============================================================================

set -euo pipefail
THIS_SCRIPT="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
cd "$(dirname "$0")"
ROOT="$(cd ../.. && pwd)"

# ─── 默认值 ───
MODE="smoke"
BUILD_TYPE="release"
DO_LOG="false"
LOG_FOLLOW="false"
MODE_ARG=""

# ─── 解析参数 ───
for arg in "$@"; do
    case "$arg" in
        smoke|full|env|perf|2shard|cov|clippy|stop|clean|logs|help)
            # 第一个模式关键字决定 MODE;后续关键字(如 `2shard perf all` 的 perf)
            # 只作为子命令参数,不再覆盖 MODE
            if [ -z "$MODE_ARG" ]; then
                MODE="$arg"
                MODE_ARG="1"
            fi
            ;;
        prepare|baseline|proxy|run|all|cleanup|up|check|test|unit|e2e)
            # perf / 2shard / cov 子命令，透传不改变 MODE
            ;;
        --debug)
            BUILD_TYPE="debug"
            ;;
        --log)
            DO_LOG="true"
            ;;
        -f|--follow)
            LOG_FOLLOW="true"
            ;;
        *)
            echo "未知参数: $arg"
            echo "运行 '$0 help' 查看用法"
            exit 1
            ;;
    esac
done

# ─── 路径派生 ───
if [ "$BUILD_TYPE" = "debug" ]; then
    BIN="$ROOT/target/debug/newproxy"
    CARGO_BUILD="cargo build"
else
    BIN="$ROOT/target/release/newproxy"
    CARGO_BUILD="cargo build --release"
fi

LOG_FILE="$ROOT/tools/itest_output.txt"
CONF="$ROOT/tests/integration/newproxy-test.conf"

# 文件大小格式化辅助函数
format_size() {
    local bytes=$1
    if [ "$bytes" -lt 1024 ]; then
        echo "${bytes}B"
    elif [ "$bytes" -lt 1048576 ]; then
        echo "$((bytes / 1024))K"
    else
        echo "$((bytes / 1048576))M"
    fi
}

# ─── 日志设置 ───
if [ "$DO_LOG" = "true" ]; then
    mkdir -p "$(dirname "$LOG_FILE")"
    exec > >(tee "$LOG_FILE") 2>&1
fi

# ────────────────────────────────────────────────────────────────────────────
# help
# ────────────────────────────────────────────────────────────────────────────
if [ "$MODE" = "help" ]; then
    sed -n '2,34p' "$THIS_SCRIPT" | sed 's/^# //'
    exit 0
fi

# ────────────────────────────────────────────────────────────────────────────
# stop
# ────────────────────────────────────────────────────────────────────────────
if [ "$MODE" = "stop" ]; then
    echo "=== 停止测试环境 ==="
    pkill newproxy 2>/dev/null || true
    docker compose -f "$ROOT/tests/integration/docker-compose.yml" down 2>/dev/null || true
    docker compose -f "$ROOT/tests/perf/docker-compose.yml" down 2>/dev/null || true
    docker compose -f "$ROOT/tests/perf/docker-compose-2shard.yml" down 2>/dev/null || true
    echo "已停止"
    exit 0
fi

# ────────────────────────────────────────────────────────────────────────────
# clean — 深度清理
# ────────────────────────────────────────────────────────────────────────────
if [ "$MODE" = "clean" ]; then
    echo "=== 深度清理测试环境 ==="

    # 1. 停止代理进程
    echo -n "停止代理... "
    if pkill newproxy 2>/dev/null; then
        echo "已停止"
    else
        echo "无运行中的代理"
    fi

    # 2. 停止并删除所有测试容器（含卷）
    echo -n "停止集成测试容器... "
    docker compose -f "$ROOT/tests/integration/docker-compose.yml" down -v --remove-orphans 2>/dev/null || true
    echo "完成"

    echo -n "停止性能测试容器... "
    docker compose -f "$ROOT/tests/perf/docker-compose.yml" down -v --remove-orphans 2>/dev/null || true
    echo "完成"

    echo -n "停止 2 分片容器... "
    docker compose -f "$ROOT/tests/perf/docker-compose-2shard.yml" down -v --remove-orphans 2>/dev/null || true
    echo "完成"

    # 3. 强制删除残留容器（防御性）
    for c in newproxy-test-mysql \
             newproxy-perf-mysql newproxy-perf newproxy-perf-sysbench \
             newproxy-perf-2shard-mysql0 newproxy-perf-2shard-mysql1 \
             newproxy-perf-2shard newproxy-perf-2shard-sysbench; do
        if docker ps -a --format '{{.Names}}' 2>/dev/null | grep -qx "$c"; then
            echo -n "删除残留容器 $c... "
            docker rm -f "$c" 2>/dev/null || true
            echo "完成"
        fi
    done

    # 4. 清理日志文件
    echo -n "清理日志... "
    rm -f "$ROOT/logs"/*.log "$ROOT/logs"/*.pid 2>/dev/null || true
    rm -f "$ROOT/tools/itest_output.txt" 2>/dev/null || true
    rm -f "$ROOT/proxy-out.log" "$ROOT/itest.log" "$ROOT/mysql.log" 2>/dev/null || true
    echo "完成"

    # 5. 清理构建缓存（可选，通过 --deep 触发）
    for arg in "$@"; do
        if [ "$arg" = "--deep" ]; then
            echo -n "清理 Rust 构建缓存 (cargo clean)... "
            cd "$ROOT"
            cargo clean 2>/dev/null || true
            echo "完成"
            break
        fi
    done

    echo ""
    echo "=== 环境已深度清理 ==="
    exit 0
fi

# ────────────────────────────────────────────────────────────────────────────
# logs — 查看日志
# ────────────────────────────────────────────────────────────────────────────
if [ "$MODE" = "logs" ]; then
    # 从参数中提取日志目标（第一个非选项参数，排除 logs 自身和 -f/--follow）
    LOG_TARGET="all"
    for a in "$@"; do
        case "$a" in
            logs|-f|--follow) ;;
            proxy|mysql|test|all) LOG_TARGET="$a"; break ;;
        esac
    done

    TAIL_CMD="tail -n 50"
    if [ "$LOG_FOLLOW" = "true" ]; then
        TAIL_CMD="tail -f"
        echo "=== 实时跟踪日志 (Ctrl+C 退出) ==="
    fi

    case "$LOG_TARGET" in
        proxy)
            # 代理日志：查找最近的日志文件
            PROXY_LOG=""
            for f in "$ROOT/logs"/*.log "$ROOT/logs"/*.txt "$ROOT/logs"/*_stdout.txt; do
                [ -f "$f" ] && PROXY_LOG="$f" && break
            done
            # 兜底：检查是否有任何文件
            if [ -z "$PROXY_LOG" ]; then
                for f in "$ROOT/logs"/*; do
                    [ -f "$f" ] && PROXY_LOG="$f" && break
                done
            fi
            if [ -z "$PROXY_LOG" ]; then
                echo "代理日志目录为空 ($ROOT/logs/)"
                echo "提示: 使用 'RUST_LOG=newproxy=debug ./tests/integration/run.sh env' 启动代理以生成日志"
            else
                echo "--- 代理日志: $PROXY_LOG ---"
                $TAIL_CMD "$PROXY_LOG"
            fi
            ;;
        mysql)
            # MySQL 容器日志
            CONTAINER="newproxy-test-mysql"
            if docker ps -a --format '{{.Names}}' 2>/dev/null | grep -qx "$CONTAINER"; then
                echo "--- MySQL 容器日志 ($CONTAINER) ---"
                if [ "$LOG_FOLLOW" = "true" ]; then
                    docker logs -f "$CONTAINER" 2>&1
                else
                    docker logs --tail 50 "$CONTAINER" 2>&1
                fi
            else
                echo "MySQL 容器未运行 ($CONTAINER)"
                echo "提示: 使用 './tests/integration/run.sh env' 启动测试环境"
            fi
            ;;
        test)
            # 集成测试日志
            if [ -f "$LOG_FILE" ]; then
                echo "--- 测试日志: $LOG_FILE ---"
                $TAIL_CMD "$LOG_FILE"
            else
                echo "测试日志不存在 ($LOG_FILE)"
                echo "提示: 使用 './tests/integration/run.sh smoke --log' 生成测试日志"
            fi
            ;;
        all)
            # 列出所有日志文件
            echo "=== 日志文件概览 ==="
            echo ""

            echo "── 代理日志 (logs/) ──"
            HAS_FILES="false"
            if [ -d "$ROOT/logs" ]; then
                for f in "$ROOT/logs"/*; do
                    if [ -f "$f" ]; then
                        [ "$HAS_FILES" = "false" ] && HAS_FILES="true"
                        SIZE=$(wc -c < "$f" 2>/dev/null | tr -d ' ')
                        printf "  %-8s  %s\n" "$(format_size "$SIZE")" "$(basename "$f")"
                    fi
                done
            fi
            if [ "$HAS_FILES" = "false" ]; then
                echo "  (空)"
            fi

            echo ""
            echo "── 项目根目录日志 ──"
            for f in "$ROOT"/*.log; do
                if [ -f "$f" ]; then
                    SIZE=$(wc -c < "$f" 2>/dev/null | tr -d ' ')
                    printf "  %-8s  %s\n" "$(format_size "$SIZE")" "$(basename "$f")"
                fi
            done

            echo ""
            echo "── 测试日志 ──"
            if [ -f "$LOG_FILE" ]; then
                SIZE=$(wc -c < "$LOG_FILE" 2>/dev/null | tr -d ' ')
                printf "  %-8s  %s\n" "$(format_size "$SIZE")" "$LOG_FILE"
            else
                echo "  (不存在: $LOG_FILE)"
            fi

            echo ""
            echo "── MySQL 容器 ──"
            CONTAINER="newproxy-test-mysql"
            if docker ps -a --format '{{.Names}}' 2>/dev/null | grep -qx "$CONTAINER"; then
                echo "  $CONTAINER: 运行中"
                echo "  查看: $0 logs mysql"
            else
                echo "  $CONTAINER: 未运行"
            fi

            echo ""
            echo "用法:"
            echo "  $0 logs proxy    查看代理日志"
            echo "  $0 logs mysql    查看 MySQL 容器日志"
            echo "  $0 logs test     查看测试日志"
            echo "  $0 logs proxy -f 实时跟踪"
            ;;
    esac
    exit 0
fi

# ────────────────────────────────────────────────────────────────────────────
# clippy
# ────────────────────────────────────────────────────────────────────────────
if [ "$MODE" = "clippy" ]; then
    echo "=== cargo clippy ==="
    cd "$ROOT"
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    echo "clippy 通过"
    exit 0
fi

# ──────────────────────────────────────────────────────────────────
# 性能测试变量与函数（perf 模式）
# ──────────────────────────────────────────────────────────────────

PERF_SB_CONT="newproxy-perf-sysbench"
PERF_MY_CONT="newproxy-perf-mysql"
PERF_PX_CONT="newproxy-perf"
PERF_MY_HOST="mysql"
PERF_MY_PORT="3306"
PERF_PX_HOST="newproxy"
PERF_PX_PORT="4051"
PERF_MY_USER="root"
PERF_MY_PASS="test_password"
PERF_MY_DB="sbtest"
PERF_TABLE_SIZE="${TABLE_SIZE:-10000}"
PERF_THREADS="${THREADS:-8}"
PERF_TIME="${TEST_TIME:-30}"
PERF_RESULT="$ROOT/tests/perf/sysbench_result.txt"
PERF_REPORT="$ROOT/tests/perf/sysbench_report.md"

# 2 分片 perf 复用同一套 sysbench 函数:下面三个变量指向目标环境
# (单分片 perf 用默认值;2shard perf 覆盖为 2 分片容器 + 独立结果/报告文件)
SB_CONT="${SB_CONT:-$PERF_SB_CONT}"
SB_RESULT="${SB_RESULT:-$PERF_RESULT}"
SB_REPORT="${SB_REPORT:-$PERF_REPORT}"

PERF_TESTS=(
    "oltp_point_select:oltp_point_select.lua:点查询"
    "oltp_read_only:oltp_read_only.lua:只读"
    "oltp_read_write:oltp_read_write.lua:读写混合"
    "oltp_write_only:oltp_write_only.lua:只写"
    "oltp_update_index:oltp_update_index.lua:索引更新"
)

# 确保 3 个 perf 容器运行（如未运行则启动）
perf_ensure_containers() {
    local need_start="false"
    for c in "$PERF_MY_CONT" "$PERF_PX_CONT" "$PERF_SB_CONT"; do
        if ! docker ps --format '{{.Names}}' 2>/dev/null | grep -qx "$c"; then
            need_start="true"
            break
        fi
    done
    if [ "$need_start" = "true" ]; then
        echo "=== 启动性能测试容器（含 Docker 内编译）==="
        if ! docker compose -f "$ROOT/tests/perf/docker-compose.yml" up -d --build --wait 2>&1; then
            echo ""
            echo "ERROR: 容器启动失败，检查日志："
            docker compose -f "$ROOT/tests/perf/docker-compose.yml" logs --tail 30 2>&1 || true
            exit 1
        fi
        echo ""
    fi

    # 等待 MySQL 接受连接（healthcheck 通过不等于能连上）
    echo -n "等待 MySQL 就绪..."
    local mysql_ok="false"
    for i in $(seq 1 30); do
        if perf_mysql -e "SELECT 1" >/dev/null 2>&1; then
            mysql_ok="true"
            break
        fi
        sleep 2
    done
    if [ "$mysql_ok" != "true" ]; then
        echo ""
        echo "ERROR: MySQL ($PERF_MY_CONT) 在 60s 内未就绪"
        echo "容器状态:"
        docker ps -a --filter "name=newproxy-perf" --format 'table {{.Names}}\t{{.Status}}' 2>&1 || true
        echo ""
        echo "MySQL 日志:"
        docker logs --tail 20 "$PERF_MY_CONT" 2>&1 || true
        exit 1
    fi

    # 验证代理可用（mysql 客户端探测，不依赖测试数据）
    echo -n "，等待代理就绪..."
    local proxy_ok="false"
    for i in $(seq 1 15); do
        if perf_proxy_ready; then
            proxy_ok="true"
            break
        fi
        sleep 2
    done
    if [ "$proxy_ok" != "true" ]; then
        echo ""
        echo "ERROR: 代理 ($PERF_PX_CONT) 在 30s 内未就绪"
        echo "代理日志:"
        docker logs --tail 30 "$PERF_PX_CONT" 2>&1 || true
        exit 1
    fi

    echo " 就绪"
}

# 容器内 mysql / sysbench 快捷命令
perf_mysql() { docker exec "$PERF_MY_CONT" mysql -h localhost -u "$PERF_MY_USER" -p"$PERF_MY_PASS" "$@"; }
perf_sb()    { docker exec "$SB_CONT" sysbench "$@"; }

# 代理就绪探测：用 mysql 客户端连代理执行 SELECT 1。
# 不用 sysbench 压测探活——oltp_*_select.lua 在表为空时会卡死，
# 而 env 模式不 prepare 数据。mysql 客户端探测轻量且不依赖测试数据。
perf_proxy_ready() {
    docker exec "$PERF_SB_CONT" \
        mysql -h "$PERF_PX_HOST" -P "$PERF_PX_PORT" \
        -u "$PERF_MY_USER" -p"$PERF_MY_PASS" "$PERF_MY_DB" \
        -N -e "SELECT 1" >/dev/null 2>&1
}

# 建表 + 填数据
perf_prepare() {
    echo "=== 准备阶段（建表 + 填充 $PERF_TABLE_SIZE 行）==="
    echo "建表..."
    perf_mysql "$PERF_MY_DB" <<SQL
DROP TABLE IF EXISTS sbtest1;
CREATE TABLE sbtest1 (
    id  INT PRIMARY KEY AUTO_INCREMENT,
    k   INT NOT NULL DEFAULT 0,
    c   CHAR(120) NOT NULL DEFAULT '',
    pad CHAR(60) NOT NULL DEFAULT '',
    KEY idx_k (k)
) ENGINE=InnoDB;
SQL
    echo "填充 $PERF_TABLE_SIZE 行..."
    perf_mysql "$PERF_MY_DB" <<SQL
INSERT INTO sbtest1 (k, c, pad)
SELECT
    FLOOR(1 + RAND() * 100000),
    REPEAT('x', 120),
    REPEAT('y', 60)
FROM
    (SELECT 0 AS n UNION SELECT 1 UNION SELECT 2 UNION SELECT 3 UNION SELECT 4
     UNION SELECT 5 UNION SELECT 6 UNION SELECT 7 UNION SELECT 8 UNION SELECT 9) a,
    (SELECT 0 AS n UNION SELECT 1 UNION SELECT 2 UNION SELECT 3 UNION SELECT 4
     UNION SELECT 5 UNION SELECT 6 UNION SELECT 7 UNION SELECT 8 UNION SELECT 9) b,
    (SELECT 0 AS n UNION SELECT 1 UNION SELECT 2 UNION SELECT 3 UNION SELECT 4
     UNION SELECT 5 UNION SELECT 6 UNION SELECT 7 UNION SELECT 8 UNION SELECT 9) c,
    (SELECT 0 AS n UNION SELECT 1 UNION SELECT 2 UNION SELECT 3 UNION SELECT 4
     UNION SELECT 5 UNION SELECT 6 UNION SELECT 7 UNION SELECT 8 UNION SELECT 9) d
LIMIT $PERF_TABLE_SIZE;
SQL
    local actual
    actual=$(perf_mysql -N -e "SELECT COUNT(*) FROM $PERF_MY_DB.sbtest1" 2>/dev/null)
    echo "prepare 完成 (sbtest1: $actual 行)"
}

# 运行单组 sysbench（多进程模拟并发）
perf_run_sb_batch() {
    local mode="$1" host="$2" port="$3" desc="$4" script="$5" concurrency="$6"
    local mode_label
    [ "$mode" = "direct" ] && mode_label="直连" || mode_label="代理"
    echo "  $desc | $mode_label | concurrency=$concurrency"

    local total_out="" pids=()

    for _ in $(seq 1 "$concurrency"); do
        perf_sb \
            --mysql-host="$host" \
            --mysql-port="$port" \
            --mysql-user="$PERF_MY_USER" \
            --mysql-password="$PERF_MY_PASS" \
            --mysql-ssl=0 \
            --mysql-db="$PERF_MY_DB" \
            --tables=1 \
            --table-size="$PERF_TABLE_SIZE" \
            --threads=1 \
            --time="$PERF_TIME" \
            --report-interval=10 \
            --skip-trx=off \
            --rand-type=uniform \
            --db-ps-mode=disable \
            "/usr/local/share/sysbench/$script" run \
            >"/tmp/.perf_sb_${mode}_${concurrency}_$$_${RANDOM}.log" 2>&1 &
        pids+=($!)
    done

    for pid in "${pids[@]}"; do wait "$pid" 2>/dev/null || true; done

    for f in /tmp/.perf_sb_${mode}_${concurrency}_$$_*.log; do
        total_out+=$(cat "$f" 2>/dev/null)
        total_out+=$'\n'
        rm -f "$f"
    done

    echo -e "$mode\t$desc\t$concurrency\t$total_out" >> "$SB_RESULT"
    perf_parse_log "$total_out" "$desc" "$concurrency" "$mode"
}

# 解析 sysbench 输出
perf_parse_log() {
    local out="$1" desc="$2" threads="$3" mode="$4"
    local tps qps lat95
    tps=$(echo "$out" | grep -E 'transactions:' | grep -Eo '[0-9]+\.[0-9]+' | head -1 || echo "N/A")
    qps=$(echo "$out" | grep -E 'queries:' | grep -Eo '[0-9]+\.[0-9]+' | head -1 || echo "N/A")
    lat95=$(echo "$out" | grep -E '95th percentile:' | grep -Eo '[0-9]+\.[0-9]+' | head -1 || echo "N/A")
    printf "    TPS=%-10s QPS=%-10s P95=%-8sms\n" "$tps" "$qps" "$lat95"
}

# 直连 MySQL 基线测试
perf_baseline() {
    echo "=== 基线测试：直连 MySQL ($PERF_MY_HOST:$PERF_MY_PORT) ==="

    set +e
    for threads in $PERF_THREADS; do
        for test_def in "${PERF_TESTS[@]}"; do
            IFS=':' read -r label script desc <<< "$test_def"
            perf_run_sb_batch "direct" "$PERF_MY_HOST" "$PERF_MY_PORT" "$desc" "$script" "$threads"
        done
    done
    set -e
    echo "基线测试完成"
}

# 通过代理测试
perf_proxy() {
    echo "=== 代理测试：通过 NewProxy ($PERF_PX_HOST:$PERF_PX_PORT) ==="

    # 验证代理可用（mysql 客户端探测，最多重试 5 次）
    echo -n "检查代理可用性... "
    for i in $(seq 1 5); do
        if perf_proxy_ready; then
            echo "就绪"
            break
        fi
        if [ "$i" -eq 5 ]; then
            echo ""
            echo "ERROR: 代理 $PERF_PX_HOST:$PERF_PX_PORT 不可用"
            echo "代理日志:"
            docker logs --tail 20 "$PERF_PX_CONT" 2>&1 || true
            exit 1
        fi
        sleep 2
    done

    set +e
    for threads in $PERF_THREADS; do
        for test_def in "${PERF_TESTS[@]}"; do
            IFS=':' read -r label script desc <<< "$test_def"
            perf_run_sb_batch "proxy" "$PERF_PX_HOST" "$PERF_PX_PORT" "$desc" "$script" "$threads"
        done
    done
    set -e
    echo "代理测试完成"
}

# 生成 Markdown 报告(SB_REPORT/SB_RESULT 可被 2shard perf 覆盖为独立文件;
# REPORT_TITLE/REPORT_NOTE 可由调用方覆盖,标明 2 分片场景)
perf_generate_report() {
    echo ""
    echo "生成报告 → $SB_REPORT"
    {
        echo "# ${REPORT_TITLE:-NewProxy Proxy sysbench 性能报告}"
        echo ""
        echo "**时间**: $(date '+%Y-%m-%d %H:%M:%S')"
        echo "**表大小**: $PERF_TABLE_SIZE 行"
        echo "**测试时长**: ${PERF_TIME}s/场景"
        echo "**并发**: $PERF_THREADS"
        echo "**说明**: ${REPORT_NOTE:-单分片环境:直连 MySQL 与经代理对比}"
        echo ""
        echo "## 结果汇总"
        echo ""
        echo "| 场景 | 并发 | 直连 TPS | 代理 TPS | 直连 P95 | 代理 P95 |"
        echo "|------|------|----------|----------|----------|----------|"

        # 原始结果文件为多行记录:每条以 "mode\tdesc\tthreads\t" 开头,后跟整段 sysbench
        # 日志(含换行)。read 无法处理含换行字段,故先用 awk 按记录解析出
        # mode/desc/threads/transactions per-sec/95th percentile;
        # 再用数组按 "desc|threads" 配对直连/代理(场景名为中文,不能作变量名)。
        local keys=() kv
        declare -a K DTPS DP95 PTPS PP95
        while IFS=$'\t' read -r mode desc threads tps p95; do
            [ -n "$mode" ] || continue
            local key="${desc}|${threads}" idx=-1 k
            for ((k = 0; k < ${#K[@]}; k++)); do
                [ "${K[k]}" = "$key" ] && { idx=$k; break; }
            done
            if [ "$idx" -lt 0 ]; then
                idx=${#K[@]}
                K[idx]="$key"; DTPS[idx]=""; DP95[idx]=""; PTPS[idx]=""; PP95[idx]=""
            fi
            if [ "$mode" = "direct" ]; then
                DTPS[idx]="$tps"; DP95[idx]="$p95"
            else
                PTPS[idx]="$tps"; PP95[idx]="$p95"
            fi
        done < <(awk '
            BEGIN { FS = "\t" }
            /^(direct|proxy)\t/ {
                if (mode != "") emit()
                mode = $1; desc = $2; thr = $3; body = $4 "\n"
                next
            }
            { body = body $0 "\n" }
            END { if (mode != "") emit() }
            function emit() {
                tps = ""; p95 = ""
                n = split(body, L, "\n")
                for (i = 1; i <= n; i++) {
                    if (tps == "" && L[i] ~ /transactions:/) {
                        if (match(L[i], /[0-9]+\.[0-9]+/)) tps = substr(L[i], RSTART, RLENGTH)
                    } else if (p95 == "" && L[i] ~ /95th percentile:/) {
                        if (match(L[i], /[0-9]+\.[0-9]+/)) p95 = substr(L[i], RSTART, RLENGTH)
                    }
                }
                print mode "\t" desc "\t" thr "\t" (tps == "" ? "-" : tps) "\t" (p95 == "" ? "-" : p95)
            }
        ' "$SB_RESULT")
        for ((k = 0; k < ${#K[@]}; k++)); do
            local desc="${K[k]%|*}" thr="${K[k]#*|}"
            echo "| $desc | $thr | ${DTPS[k]:--} | ${PTPS[k]:--} | ${DP95[k]:--}ms | ${PP95[k]:--}ms |"
        done

        echo ""
        echo "## 关注点"
        echo ""
        echo "- **代理开销**: 对比直连 TPS，代理层引入的额外延迟"
        echo "- **扩展性**: 不同并发度下 TPS 的增长趋势"
        echo "- **P95 延迟**: 尾部延迟是否在可接受范围"
        echo ""
        echo "原始数据: \`$SB_RESULT\`"
    } > "$SB_REPORT"
    echo "报告已生成"
}

# 清理 sysbench 测试数据
perf_cleanup() {
    echo "=== 清理 sysbench 测试数据 ==="
    perf_mysql "$PERF_MY_DB" -e "DROP TABLE IF EXISTS sbtest1" 2>/dev/null || true
    echo "清理完成"
}

# ────────────────────────────────────────────────────────────────────────────
# perf — 性能测试（prepare | baseline | proxy | run | all | cleanup）
# ────────────────────────────────────────────────────────────────────────────
if [ "$MODE" = "perf" ]; then
    PERF_SUBCMD="all"
    for a in "$@"; do
        case "$a" in
            prepare|baseline|proxy|run|all|cleanup) PERF_SUBCMD="$a"; break ;;
        esac
    done

    # 确保 perf 容器运行
    perf_ensure_containers

    case "$PERF_SUBCMD" in
        prepare)  perf_prepare ;;
        baseline) > "$PERF_RESULT"; perf_baseline ;;
        proxy)    > "$PERF_RESULT"; perf_proxy ;;
        run)      > "$PERF_RESULT"; perf_baseline; perf_proxy; perf_generate_report;
                  echo ""; echo "✓ 性能测试完成"; echo "报告: $PERF_REPORT" ;;
        all)      perf_prepare; > "$PERF_RESULT"; perf_baseline; perf_proxy; perf_generate_report;
                  echo ""; echo "✓ sysbench 性能测试完成"; echo "报告: $PERF_REPORT" ;;
        cleanup)  perf_cleanup ;;
    esac
    exit 0
fi

# ────────────────────────────────────────────────────────────────────────────
# 2shard — 2 分片容器场景测试 (up | check | all | cleanup)
# 功能验证(不压测):2 个 MySQL 分片(行数 5/7 + 标记列可判别)+ 代理(2 tablet 配置)
# ────────────────────────────────────────────────────────────────────────────
P2_COMPOSE="$ROOT/tests/perf/docker-compose-2shard.yml"
P2_SB_CONT="newproxy-perf-2shard-sysbench"
P2_MY0_CONT="newproxy-perf-2shard-mysql0"
P2_MY1_CONT="newproxy-perf-2shard-mysql1"
P2_PX_CONT="newproxy-perf-2shard"
P2_S0_HOST="mysql-shard0"
P2_S1_HOST="mysql-shard1"
P2_PX_HOST="newproxy"
P2_PX_PORT=4051          # 容器内代理 MySQL 端口(宿主机映射 4052)
P2_MNG_PORT=9112         # 宿主机面板端口
P2_USER="root"
P2_PASS="test_password"
P2_DB="sbtest"
P2_CHECKS_PASS=0
P2_CHECKS_FAIL=0

# 经 sysbench 容器执行 SQL(可达两个分片与代理)
p2_sql() { docker exec "$P2_SB_CONT" mysql -h "$1" -P "${2:-3306}" -u "$P2_USER" -p"$P2_PASS" -N -e "$3" 2>/dev/null; }

p2_check_result() {
    local name="$1" ok="$2" detail="$3"
    if [ "$ok" = "1" ]; then
        P2_CHECKS_PASS=$((P2_CHECKS_PASS + 1))
        printf "  [PASS] %s — %s\n" "$name" "$detail"
    else
        P2_CHECKS_FAIL=$((P2_CHECKS_FAIL + 1))
        printf "  [FAIL] %s — %s\n" "$name" "$detail"
    fi
}

# 确保 4 个 2 分片容器运行(缺失则启动;镜像已存在时仅启动不重建,
# 重建请用 ./build.sh perf2 up 或 docker compose ... up -d --build)
p2_ensure_containers() {
    local need_start="false"
    for c in "$P2_MY0_CONT" "$P2_MY1_CONT" "$P2_PX_CONT" "$P2_SB_CONT"; do
        if ! docker ps --format '{{.Names}}' 2>/dev/null | grep -qx "$c"; then
            need_start="true"
            break
        fi
    done
    if [ "$need_start" = "true" ]; then
        if docker image inspect perf-2shard-newproxy >/dev/null 2>&1; then
            echo "=== 启动 2 分片容器 ==="
            docker compose -f "$P2_COMPOSE" up -d --wait 2>&1
        else
            echo "=== 构建并启动 2 分片容器(首次含 Docker 内编译约 4-5 分钟)==="
            DOCKER_BUILDKIT="${DOCKER_BUILDKIT:-0}" docker compose -f "$P2_COMPOSE" up -d --build --wait 2>&1
        fi
        if ! docker ps --format '{{.Names}}' 2>/dev/null | grep -qx "$P2_PX_CONT"; then
            echo ""
            echo "ERROR: 2 分片容器启动失败,检查日志:"
            docker compose -f "$P2_COMPOSE" logs --tail 30 2>&1 || true
            exit 1
        fi
    fi
    # 等待两个分片 + 代理均可连接(healthcheck 通过 ≠ 可连)
    echo -n "等待 2 分片环境就绪..."
    local ready="false"
    for i in $(seq 1 45); do
        local r0 r1 rp
        r0="$(p2_sql "$P2_S0_HOST" 3306 "SELECT 1" 2>/dev/null)"
        r1="$(p2_sql "$P2_S1_HOST" 3306 "SELECT 1" 2>/dev/null)"
        rp="$(p2_sql "$P2_PX_HOST" "$P2_PX_PORT" "SELECT 1" 2>/dev/null)"
        if [ "$r0" = "1" ] && [ "$r1" = "1" ] && [ "$rp" = "1" ]; then
            ready="true"
            break
        fi
        sleep 2
    done
    if [ "$ready" != "true" ]; then
        echo ""
        echo "ERROR: 2 分片环境 90s 内未就绪"
        docker ps -a --filter "name=newproxy-perf-2shard" --format 'table {{.Names}}\t{{.Status}}' 2>&1 || true
        exit 1
    fi
    echo " 就绪"
}

# 纯测试前置检查:环境必须已由 perf2 up 就绪。不执行任何 docker compose 操作,
# 环境不在(容器未运行/不可达)则报错退出,提示先创建环境。
p2_check_env_up() {
    for c in "$P2_MY0_CONT" "$P2_MY1_CONT" "$P2_PX_CONT" "$P2_SB_CONT"; do
        if ! docker ps --format '{{.Names}}' 2>/dev/null | grep -qx "$c"; then
            echo "ERROR: 2 分片容器 $c 未运行 — 请先执行 ./build.sh perf2 up 创建环境" >&2
            return 1
        fi
    done
    local r0 r1 rp
    r0="$(p2_sql "$P2_S0_HOST" 3306 "SELECT 1" 2>/dev/null)"
    r1="$(p2_sql "$P2_S1_HOST" 3306 "SELECT 1" 2>/dev/null)"
    rp="$(p2_sql "$P2_PX_HOST" "$P2_PX_PORT" "SELECT 1" 2>/dev/null)"
    if [ "$r0" != "1" ] || [ "$r1" != "1" ] || [ "$rp" != "1" ]; then
        echo "ERROR: 2 分片环境未就绪(分片0=$r0 分片1=$r1 代理=$rp) — 请先执行 ./build.sh perf2 up 创建环境" >&2
        return 1
    fi
    echo "环境就绪:分片0/分片1/代理均可连接(仅检查,未改动环境)"
}

# 功能检查(任一失败返回非 0)
P2_CONF="$ROOT/tests/perf/newproxy-2shard-docker.conf"
# 期望数据(host:期望行数 / host:期望标记列);未知 host 仅要求可查询
P2_EXPECT_ROWS="mysql-shard0:5 mysql-shard1:7"
P2_EXPECT_MARK="mysql-shard0:shard0 mysql-shard1:shard1"

# 从配置解析全部 Master_Host 后端 → "tablet host port" 行
# (cluster_tablet_name 在 port 之后,故在段结束/EOF 时输出)
p2_list_backends() {
    awk '
        /^\[Master_Host/ { if (inm && p != "") print ct, h, p; inm=1; ct=""; h=""; p=""; next }
        /^\[/ { if (inm && p != "") print ct, h, p; inm=0 }
        inm && /^[[:space:]]*cluster_tablet_name=/ { ct=$0; sub(/^[^=]*=[[:space:]]*/,"",ct) }
        inm && /^[[:space:]]*host=/ { h=$0; sub(/^[^=]*=[[:space:]]*/,"",h) }
        inm && /^[[:space:]]*port=/ { p=$0; sub(/^[^=]*=[[:space:]]*/,"",p) }
        END { if (inm && p != "") print ct, h, p }
    ' "$P2_CONF"
}

# 期望值查询:map 为 "k:v k2:v2",按 host 取期望;未命中返回非 0
p2_expect() {
    local map="$1" key="$2" kv
    for kv in $map; do
        case "$kv" in
            "$key":*) echo "${kv#*:}"; return 0 ;;
        esac
    done
    return 1
}

p2_check() {
    echo "=== 2 分片场景功能检查(遍历全部后端/分片/集群) ==="
    P2_CHECKS_PASS=0; P2_CHECKS_FAIL=0
    local token=""
    if command -v curl >/dev/null 2>&1; then
        token="$(curl -s -i -X POST "http://127.0.0.1:$P2_MNG_PORT/login" -d "user=admin&password=admin" | grep -i set-cookie | sed 's/.*newproxy_session=\([0-9a-f]*\).*/\1/')"
    fi

    # 1. 逐个分片:数据行数 + 标记列(解析配置全部 Master_Host,自动适配多分片/多集群)
    local n_be=0 tbl host port cnt mark exp_rows exp_mark
    while read -r tbl host port; do
        [ -n "$tbl" ] || continue
        n_be=$((n_be + 1))
        cnt="$(p2_sql "$host" "$port" "SELECT COUNT(*) FROM $P2_DB.sbtest1" 2>/dev/null)"
        exp_rows="$(p2_expect "$P2_EXPECT_ROWS" "$host")" || exp_rows=""
        if [ -n "$exp_rows" ]; then
            p2_check_result "分片 $tbl 数据" "$([ "$cnt" = "$exp_rows" ] && echo 1 || echo 0)" "$host:$port sbtest1=$cnt 行(期望 $exp_rows)"
        else
            p2_check_result "分片 $tbl 数据" "$([ -n "$cnt" ] && echo 1 || echo 0)" "$host:$port sbtest1=$cnt 行(无期望值,仅要求可查询)"
        fi
        mark="$(p2_sql "$host" "$port" "SELECT c FROM $P2_DB.sbtest1 LIMIT 1" 2>/dev/null)"
        exp_mark="$(p2_expect "$P2_EXPECT_MARK" "$host")" || exp_mark=""
        if [ -n "$exp_mark" ]; then
            p2_check_result "分片 $tbl 标记" "$([ "$mark" = "$exp_mark" ] && echo 1 || echo 0)" "c=$mark(期望 $exp_mark)"
        fi
    done < <(p2_list_backends)
    [ "$n_be" -ge 1 ] || p2_check_result "分片枚举" 0 "配置中未解析到任何 Master_Host 后端"

    # 2. 代理可查询:无尾号表 sbtest1 → 默认路由到第一个分片(分片 0,5 行)
    local rp
    rp="$(p2_sql "$P2_PX_HOST" "$P2_PX_PORT" "SELECT COUNT(*) FROM $P2_DB.sbtest1" 2>/dev/null)"
    p2_check_result "代理查询(无尾号默认分片0)" "$([ "$rp" = "5" ] && echo 1 || echo 0)" "经代理 sbtest1=$rp 行(期望 5:无尾号表默认第一个分片)"

    # 3. 分片数统计:API tablets 数 == 配置后端数,且 ≥2
    local n_clusters="?" n_tablets="?"
    if [ -n "$token" ]; then
        local resp2
        resp2="$(curl -s -H "Cookie: newproxy_session=$token" "http://127.0.0.1:$P2_MNG_PORT/api/config")"
        n_clusters="$(echo "$resp2" | grep -oE '"clusters"[[:space:]]*:[[:space:]]*[0-9]+' | grep -oE '[0-9]+$' || echo '?')"
        n_tablets="$(echo "$resp2" | grep -oE '"tablets"[[:space:]]*:[[:space:]]*[0-9]+' | grep -oE '[0-9]+$' || echo '?')"
    fi
    p2_check_result "分片数统计" "$([ "$n_tablets" = "$n_be" ] && [ "${n_tablets:-0}" -ge 2 ] && echo 1 || echo 0)" "clusters=$n_clusters tablets=$n_tablets 配置后端=$n_be(须一致且 ≥2)"

    # 4. 面板 /api/check:后端逐条可达(API 现在遍历全部分片)
    local api_ok="1" api_detail="宿主机无 curl,跳过"
    if command -v curl >/dev/null 2>&1; then
        api_ok="0"; api_detail=""
        if [ -n "$token" ]; then
            local resp be_total
            resp="$(curl -s -H "Cookie: newproxy_session=$token" "http://127.0.0.1:$P2_MNG_PORT/api/check")"
            be_total="$(echo "$resp" | grep -oE '"name":"后端"' | wc -l | tr -d ' ')"
            if echo "$resp" | grep -Eq '"overall"[[:space:]]*:[[:space:]]*"ok"' \
               && ! echo "$resp" | grep -q '不可达\|超时\|非 MySQL' \
               && [ "$be_total" = "$n_be" ]; then
                api_ok="1"
                api_detail="后端 $be_total/$n_be 全部可达:$(echo "$resp" | grep -oE '\[[^]]*\][^,]*可达' | tr '\n' ' ')"
            else
                api_detail="check 异常(后端条目 $be_total/$n_be): ${resp:0:150}"
            fi
        else
            api_detail="面板登录失败(:$P2_MNG_PORT)"
        fi
    fi
    p2_check_result "面板 /api/check(全后端)" "$api_ok" "$api_detail"

    # 5. 尾号路由:代理按表名尾号路由到对应分片(逐查询切换后端)
    #    sbtest_0(尾号 0 % 2 = 0)→ 分片 0(5 行,标记 shard0)
    #    sbtest_1(尾号 1 % 2 = 1)→ 分片 1(7 行,标记 shard1)
    #    表只存在于其归属分片,行数/标记列即路由落点;路由错误会查不到表或行数不对
    local r0c r0m r1c r1m
    r0c="$(p2_sql "$P2_PX_HOST" "$P2_PX_PORT" "SELECT COUNT(*) FROM $P2_DB.sbtest_0" 2>/dev/null)"
    r0m="$(p2_sql "$P2_PX_HOST" "$P2_PX_PORT" "SELECT c FROM $P2_DB.sbtest_0 LIMIT 1" 2>/dev/null)"
    r1c="$(p2_sql "$P2_PX_HOST" "$P2_PX_PORT" "SELECT COUNT(*) FROM $P2_DB.sbtest_1" 2>/dev/null)"
    r1m="$(p2_sql "$P2_PX_HOST" "$P2_PX_PORT" "SELECT c FROM $P2_DB.sbtest_1 LIMIT 1" 2>/dev/null)"
    p2_check_result "尾号路由 sbtest_0→分片0" "$([ "$r0c" = "5" ] && [ "$r0m" = "shard0" ] && echo 1 || echo 0)" "代理 sbtest_0=$r0c 行,标记=$r0m(期望 5/shard0)"
    p2_check_result "尾号路由 sbtest_1→分片1" "$([ "$r1c" = "7" ] && [ "$r1m" = "shard1" ] && echo 1 || echo 0)" "代理 sbtest_1=$r1c 行,标记=$r1m(期望 7/shard1)"

    # 6. 分片计数分布:两个分片都应有查询(逐查询路由生效,而非全部落第一个分片)
    #    注意 serde_json 按键排序输出,shards 数组元素形如 {"queries":N,"shard":"0.t0"}
    if [ -n "$token" ]; then
        local resp3 s0 s1
        resp3="$(curl -s -H "Cookie: newproxy_session=$token" "http://127.0.0.1:$P2_MNG_PORT/api/status")"
        s0="$(echo "$resp3" | grep -oE '"queries":[0-9]+,"shard":"0\.t0"' | grep -oE '[0-9]+' | head -1)"
        s1="$(echo "$resp3" | grep -oE '"queries":[0-9]+,"shard":"0\.t1"' | grep -oE '[0-9]+' | head -1)"
        if [ -n "$s0" ] && [ -n "$s1" ] && [ "$s0" -gt 0 ] && [ "$s1" -gt 0 ]; then
            p2_check_result "分片计数分布" 1 "0.t0=$s0 0.t1=$s1(均 >0,逐查询路由生效)"
        else
            p2_check_result "分片计数分布" 0 "0.t0=${s0:-?} 0.t1=${s1:-?}(须均 >0)"
        fi
    fi

    echo ""
    echo "检查结果: $P2_CHECKS_PASS 通过 / $P2_CHECKS_FAIL 失败"
    [ "$P2_CHECKS_FAIL" -eq 0 ]
}

p2_cleanup() {
    echo "=== 停止并删除 2 分片容器 ==="
    docker compose -f "$P2_COMPOSE" down 2>&1 | tail -2 || true
    echo "已清理"
}

# ────────────────────────────────────────────────────────────────────────────
# 2shard perf — 2 分片性能测试 (prepare | baseline | proxy | run | all | cleanup)
# 与单分片 perf 同参数对比:直连分片0(sbtest 全量数据) vs 经代理(2 分片分流)。
# 表名 perf_1 / perf_2:代理按表尾号路由 perf_1(尾号1%2=1)→分片1, perf_2(2%2=0)→分片0,
# 两个分片都被压到;两个分片各建两表(数据对称,直连基线在分片0即可跑完整 workload)。
# 容器内 sysbench 表名硬编码 sbtest%d,运行时用 sed 生成定制 lua(前缀 perf_)。
# 结果/报告写入独立文件,不与单分片 perf 混用。
# ────────────────────────────────────────────────────────────────────────────
P2_RESULT="$ROOT/tests/perf/sysbench_result_2shard.txt"
P2_REPORT="$ROOT/tests/perf/sysbench_report_2shard.md"
P2_TABLE_SIZE="${TABLE_SIZE:-10000}"
P2_THREADS="${THREADS:-8}"
P2_TIME="${TEST_TIME:-30}"

p2_sb() { docker exec "$P2_SB_CONT" sysbench "$@"; }

# 容器内生成定制 lua:表名前缀 sbtest → perf_(每次从原始文件重新生成,幂等)
p2_perf_lua_setup() {
    docker exec "$P2_SB_CONT" sh -c '
        mkdir -p /tmp/2s
        sed "s/sbtest/perf_/g" /usr/local/share/sysbench/oltp_common.lua > /tmp/2s/oltp_common.lua
        for f in oltp_point_select oltp_read_only oltp_read_write oltp_write_only oltp_update_index; do
            sed "s|require(\"oltp_common\")|dofile(\"/tmp/2s/oltp_common.lua\")|" /usr/local/share/sysbench/$f.lua > /tmp/2s/$f.lua
        done
    '
}

# 2 分片准备:两个分片各建 perf_1/perf_2(每表 $P2_TABLE_SIZE 行)
p2_perf_prepare() {
    p2_perf_lua_setup
    echo "=== 2 分片准备阶段(两分片各建 perf_1/perf_2, 每表 $P2_TABLE_SIZE 行)==="
    for s in "$P2_S0_HOST" "$P2_S1_HOST"; do
        p2_sql "$s" 3306 "DROP TABLE IF EXISTS $P2_DB.perf_1; DROP TABLE IF EXISTS $P2_DB.perf_2" 2>/dev/null || true
        echo "准备 $s ..."
        p2_sb --mysql-host="$s" --mysql-port=3306 --mysql-user="$P2_USER" --mysql-password="$P2_PASS" \
            --mysql-ssl=0 --mysql-db="$P2_DB" --tables=2 --table-size="$P2_TABLE_SIZE" \
            --db-ps-mode=disable "/tmp/2s/oltp_read_only.lua" prepare || { echo "prepare 失败($s)"; exit 1; }
    done
    echo "prepare 完成"
}

# 单组 sysbench 批跑(多进程模拟并发),结果写入 $SB_RESULT
p2_run_sb_batch() {
    local mode="$1" host="$2" port="$3" desc="$4" script="$5" concurrency="$6"
    local mode_label
    [ "$mode" = "direct" ] && mode_label="直连分片0" || mode_label="代理(2分片)"
    echo "  $desc | $mode_label | concurrency=$concurrency"

    local total_out="" pids=()

    for _ in $(seq 1 "$concurrency"); do
        p2_sb \
            --mysql-host="$host" \
            --mysql-port="$port" \
            --mysql-user="$P2_USER" \
            --mysql-password="$P2_PASS" \
            --mysql-ssl=0 \
            --mysql-db="$P2_DB" \
            --tables=2 \
            --table-size="$P2_TABLE_SIZE" \
            --threads=1 \
            --time="$P2_TIME" \
            --report-interval=10 \
            --skip-trx=off \
            --rand-type=uniform \
            --db-ps-mode=disable \
            "/tmp/2s/$script" run \
            >"/tmp/.p2_sb_${mode}_${concurrency}_$$_${RANDOM}.log" 2>&1 &
        pids+=($!)
    done

    for pid in "${pids[@]}"; do wait "$pid" 2>/dev/null || true; done

    for f in /tmp/.p2_sb_${mode}_${concurrency}_$$_*.log; do
        total_out+=$(cat "$f" 2>/dev/null)
        total_out+=$'\n'
        rm -f "$f"
    done

    echo -e "$mode\t$desc\t$concurrency\t$total_out" >> "$SB_RESULT"
    perf_parse_log "$total_out" "$desc" "$concurrency" "$mode"
}

# 直连基线:分片0(数据与代理 workload 完全一致:两表各 $P2_TABLE_SIZE 行)
p2_perf_baseline() {
    p2_perf_lua_setup
    echo "=== 2 分片基线测试:直连分片0 ($P2_S0_HOST:3306, perf_1/2 各 $P2_TABLE_SIZE 行)==="
    set +e
    for threads in $P2_THREADS; do
        for test_def in "${PERF_TESTS[@]}"; do
            IFS=':' read -r label script desc <<< "$test_def"
            p2_run_sb_batch "direct" "$P2_S0_HOST" "3306" "$desc" "$script" "$threads"
        done
    done
    set -e
    echo "2 分片基线测试完成"
}

# 代理测试:经 newproxy,perf_1→分片1, perf_2→分片0,两分片均匀受压
p2_perf_proxy() {
    p2_perf_lua_setup
    echo "=== 2 分片代理测试:经 newproxy ($P2_PX_HOST:$P2_PX_PORT), perf_1→分片1, perf_2→分片0 ==="
    echo -n "检查代理可用性... "
    local ok="false"
    for i in $(seq 1 5); do
        if [ "$(p2_sql "$P2_PX_HOST" "$P2_PX_PORT" "SELECT 1" 2>/dev/null)" = "1" ]; then ok="true"; break; fi
        sleep 2
    done
    if [ "$ok" != "true" ]; then
        echo ""
        echo "ERROR: 代理 $P2_PX_HOST:$P2_PX_PORT 不可用"
        docker logs --tail 20 "$P2_PX_CONT" 2>&1 || true
        exit 1
    fi
    echo "就绪"
    set +e
    for threads in $P2_THREADS; do
        for test_def in "${PERF_TESTS[@]}"; do
            IFS=':' read -r label script desc <<< "$test_def"
            p2_run_sb_batch "proxy" "$P2_PX_HOST" "$P2_PX_PORT" "$desc" "$script" "$threads"
        done
    done
    set -e
    echo "2 分片代理测试完成"
}

p2_perf_cleanup() {
    echo "=== 清理 2 分片 sysbench 数据 ==="
    for s in "$P2_S0_HOST" "$P2_S1_HOST"; do
        p2_sql "$s" 3306 "DROP TABLE IF EXISTS $P2_DB.perf_1; DROP TABLE IF EXISTS $P2_DB.perf_2" 2>/dev/null || true
    done
    echo "清理完成"
}

if [ "$MODE" = "2shard" ]; then
    P2_SUBCMD="all"
    for a in "$@"; do
        case "$a" in
            up|check|test|all|cleanup|perf) P2_SUBCMD="$a"; break ;;
        esac
    done
    rc=0
    case "$P2_SUBCMD" in
        up)      p2_ensure_containers ;;
        check)   p2_ensure_containers; p2_check || rc=1 ;;
        test)    p2_check_env_up || exit 1
                 if p2_check; then
                     echo ""; echo "✓ 2 分片场景测试完成(未改动环境)"
                 else
                     rc=1
                 fi ;;
        all)     p2_ensure_containers
                 if p2_check; then
                     echo ""; echo "✓ 2 分片场景测试完成"
                 else
                     rc=1
                 fi ;;
        perf)
            # 复用单分片 sysbench 批跑/解析/报告函数,切换到 2 分片环境
            SB_CONT="$P2_SB_CONT"
            SB_RESULT="$P2_RESULT"
            SB_REPORT="$P2_REPORT"
            REPORT_TITLE="NewProxy 2 分片 sysbench 性能报告"
            REPORT_NOTE="2 分片环境:直连分片0(perf_1/2 全量数据)与经代理对比;代理按表尾号路由 perf_1→分片1、perf_2→分片0"
            P2_PERF_SUB="all"
            for a in "$@"; do
                case "$a" in
                    prepare|baseline|proxy|run|all|cleanup) P2_PERF_SUB="$a"; break ;;
                esac
            done
            p2_ensure_containers
            case "$P2_PERF_SUB" in
                prepare)  p2_perf_prepare ;;
                baseline) > "$P2_RESULT"; p2_perf_baseline ;;
                proxy)    > "$P2_RESULT"; p2_perf_proxy ;;
                run)      > "$P2_RESULT"; p2_perf_baseline; p2_perf_proxy; perf_generate_report;
                          echo ""; echo "✓ 2 分片性能测试完成"; echo "报告: $P2_REPORT" ;;
                all)      p2_perf_prepare; > "$P2_RESULT"; p2_perf_baseline; p2_perf_proxy; perf_generate_report;
                          echo ""; echo "✓ 2 分片性能测试完成"; echo "报告: $P2_REPORT" ;;
                cleanup)  p2_perf_cleanup ;;
            esac
            exit 0 ;;
        cleanup) p2_cleanup ;;
    esac
    exit $rc
fi

# ────────────────────────────────────────────────────────────────────────────
# 通用函数
# ────────────────────────────────────────────────────────────────────────────

GREEN='\033[0;32m'
RED='\033[0;31m'
NC='\033[0m'

# 启动 MySQL 容器
start_mysql() {
    echo "[1/4] 启动 MySQL 容器..."
    docker compose -f "$ROOT/tests/integration/docker-compose.yml" down --remove-orphans 2>/dev/null || true
    docker rm -f newproxy-test-mysql 2>/dev/null || true
    if ! docker compose -f "$ROOT/tests/integration/docker-compose.yml" up -d --wait 2>&1; then
        echo "ERROR: 无法启动 MySQL 容器"
        exit 1
    fi
    echo "MySQL 就绪 (127.0.0.1:3306)"
}

# 编译代理
build_proxy() {
    echo "[2/4] 编译 newproxy ($BUILD_TYPE)..."
    cd "$ROOT"
    $CARGO_BUILD 2>&1 | tail -2
    echo "编译完成"
}

# 启动代理（返回 PID）
start_proxy() {
    echo "[3/4] 启动代理 (port 4051)..."
    pkill newproxy 2>/dev/null || true
    mkdir -p "$ROOT/logs"
    RUST_LOG=newproxy=info "$BIN" "$CONF" &
    PROXY_PID=$!
    sleep 2
    echo "代理就绪 (PID=$PROXY_PID)"
}

# 打印环境信息
print_env_info() {
    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "  测试环境已就绪 ($BUILD_TYPE build)"
    echo ""
    echo "  代理: 127.0.0.1:4051  管理: 127.0.0.1:9111"
    echo "  MySQL: 127.0.0.1:3306  (root / test_password)"
    echo "  数据库: test / sbtest"
    echo ""
    echo "  登录: mysql -h 127.0.0.1 -P 4051 -u root -ptest_password test"
    [ "$DO_LOG" = "true" ] && echo "  日志: tail -f $LOG_FILE"
    [ "$MODE" = "env" ] && echo "  按 Ctrl+C 停止并清理环境"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo ""
}

# 测试期望成功
run_test() {
    local name="$1"; shift
    echo -n "  $name ... "
    if "$@" >/dev/null 2>&1; then
        echo -e "${GREEN}PASS${NC}"
        PASS=$((PASS + 1))
    else
        echo -e "${RED}FAIL${NC}"
        FAIL=$((FAIL + 1))
    fi
}

# 测试期望失败（退出码非零 = PASS）
run_test_neg() {
    local name="$1"; shift
    echo -n "  $name ... "
    if "$@" >/dev/null 2>&1; then
        echo -e "${RED}FAIL${NC}"
        FAIL=$((FAIL + 1))
    else
        echo -e "${GREEN}PASS${NC}"
        PASS=$((PASS + 1))
    fi
}

# ────────────────────────────────────────────────────────────────────────────
# E2E 功能全量测试(full 与 cov e2e 复用)
# 前置:代理已在 4051(业务)/9111(管理)就绪,MySQL 容器已起。
# 覆盖:基础 SQL / DDL / 事务 / 预处理 / 错误处理 / 慢查询链路 /
#       checkproxy 管理命令 / 管理 HTTP API / 解析失败链路 / 并发
# ────────────────────────────────────────────────────────────────────────────
MYSQL="mysql -h 127.0.0.1 -P 4051 -u root -ptest_password"
MNG_BASE="http://127.0.0.1:9111"

# 管理 API 登录,输出会话 token
api_login() {
    curl -s -i -X POST "$MNG_BASE/login" -d "user=admin&password=admin" \
        | grep -i 'set-cookie' | sed 's/.*newproxy_session=\([0-9a-f]*\).*/\1/' | head -1
}

# mysql 查询输出(单值,无表头)等于期望
sql_eq() { [ "$($MYSQL -N -e "$1" 2>/dev/null)" = "$2" ]; }

# 管理 API GET:响应含子串(需先 TOKEN="$(api_login)")
api_has() { curl -s -H "Cookie: newproxy_session=$TOKEN" "$MNG_BASE$1" 2>/dev/null | grep -q "$2"; }
# 管理 API POST:响应含子串(需先 TOKEN)
api_post_has() { curl -s -X POST -H "Cookie: newproxy_session=$TOKEN" "$MNG_BASE$1" 2>/dev/null | grep -q "$2"; }
# 管理 API 免鉴权端点:响应含子串
api_open_has() { curl -s "$MNG_BASE$1" 2>/dev/null | grep -q "$2"; }

run_e2e_battery() {
    echo "=== E2E 功能全量测试 ==="
    # 清理历史残留数据,保证计数断言确定
    $MYSQL test -e "DELETE FROM users WHERE name LIKE 'e2e-%' OR name LIKE 'tx%'" >/dev/null 2>&1 || true

    # ─── 基础 SQL ───
    run_test "连接"                 $MYSQL test -e "SELECT 1"
    run_test "SELECT 计数"          sql_eq "SELECT COUNT(*) FROM users" "3"
    run_test "SELECT WHERE"         sql_eq "SELECT COUNT(*) FROM users WHERE name='Alice'" "1"
    run_test "SELECT JOIN"          sql_eq "SELECT COUNT(*) FROM users u JOIN orders o ON u.id=o.user_id" "3"
    run_test "SELECT 聚合"          sql_eq "SELECT COUNT(*) FROM orders" "3"
    run_test "SELECT 排序分页"      $MYSQL test -e "SELECT name FROM users ORDER BY id DESC LIMIT 2"
    run_test "INSERT"               $MYSQL test -e "INSERT INTO users(name) VALUES('e2e-ins')"
    run_test "INSERT 多行"          $MYSQL test -e "INSERT INTO users(name) VALUES('e2e-a'),('e2e-b')"
    run_test "UPDATE"               $MYSQL test -e "UPDATE users SET name='e2e-upd' WHERE name='e2e-ins'"
    run_test "DELETE"               $MYSQL test -e "DELETE FROM users WHERE name IN('e2e-upd','e2e-a','e2e-b')"
    run_test "REPLACE"              $MYSQL test -e "REPLACE INTO users(id,name) VALUES(1,'e2e-ralice')"
    run_test "REPLACE 还原"         $MYSQL test -e "REPLACE INTO users(id,name) VALUES(1,'Alice')"
    run_test "DDL 建删表"           $MYSQL test -e "CREATE TABLE IF NOT EXISTS e2e_tmp(id INT); DROP TABLE e2e_tmp"
    run_test "USE 切换库"           $MYSQL -e "USE test; SELECT COUNT(*) FROM users"
    run_test "SET 会话变量"         $MYSQL test -e "SET NAMES utf8mb4"
    run_test "SHOW DATABASES"       $MYSQL test -e "SHOW DATABASES"
    run_test "SHOW TABLES"          $MYSQL test -e "SHOW TABLES"
    run_test "DESC 表结构"          $MYSQL test -e "DESC users"
    run_test "SELECT DATABASE"      sql_eq "SELECT DATABASE()" "test"
    run_test "事务 COMMIT"          $MYSQL test -e "BEGIN; INSERT INTO users(name) VALUES('tx1'); COMMIT; SELECT 1"
    run_test "事务 ROLLBACK"        $MYSQL test -e "BEGIN; DELETE FROM users WHERE name='tx1'; ROLLBACK; SELECT 1"
    run_test "事务残留清理"         $MYSQL test -e "DELETE FROM users WHERE name='tx1'"
    run_test "文本 PREPARE/EXEC"    $MYSQL test -e "SET @x=41; PREPARE s FROM 'SELECT @x+1'; EXECUTE s; DEALLOCATE PREPARE s"

    # ─── CLI 边界(直接调用代理二进制;插桩运行时可提升 main.rs 覆盖率)───
    if [ -n "$PROXY_BIN" ] && [ -x "$PROXY_BIN" ]; then
        run_test "CLI --help"       "$PROXY_BIN" --help
        run_test "CLI --version"    "$PROXY_BIN" --version
        run_test_neg "CLI 无参数退出2" "$PROXY_BIN" 2>/dev/null
        run_test_neg "CLI 配置不存在" "$PROXY_BIN" /nonexistent/conf.ini 2>/dev/null
        run_test_neg "CLI 未知选项"  "$PROXY_BIN" --bogus 2>/dev/null
        run_test_neg "CLI 重复配置"  "$PROXY_BIN" -c a.conf -c b.conf 2>/dev/null

        # 变体启动(覆盖 main.rs run() 分支:热加载 watcher / 配置中心连接失败 / 无管理端口)
        # 变体配置:不同业务端口 + reload_interval>0 + ConfigCenter(连接必然失败,优雅降级)
        local vconf="$ROOT/logs/variant-boot.conf"
        cat > "$vconf" <<'VEOF'
[MySQL_Proxy_Layer]
port=14051
mng_port=0
reload_interval_secs=5
log_dir=logs
log_level=error
[ConfigCenter_0]
type=etcd
endpoints=http://127.0.0.1:1
root=/newproxy
[Cluster_0]
name=c
[CTablet_0_t0]
name=t0
[Master_Host_g0]
host=127.0.0.1
port=3306
cluster_tablet_name=t0
[DB_User_dbu]
db_username=root
db_password=x
cluster_name=c
[Product_User_pu]
username=u
password=p
db_username=root
cluster_name=c
VEOF
        "$PROXY_BIN" "$vconf" >"$ROOT/logs/variant-boot.out" 2>&1 &
        local vpid=$!
        sleep 1
        run_test "变体启动(无管理端口+热加载+配置中心)" kill -0 $vpid 2>/dev/null
        kill $vpid 2>/dev/null || true
        wait $vpid 2>/dev/null || true
        rm -f "$vconf"
    fi

    # ─── 错误处理(负向)───
    run_test_neg "语法错误 1064"    $MYSQL test -e "SELEC * FROM users" 2>/dev/null
    run_test_neg "未知表 1146"      $MYSQL test -e "SELECT * FROM no_such_tbl" 2>/dev/null
    run_test_neg "认证拒绝"         mysql -h 127.0.0.1 -P 4051 -u root -pwrong test -e "SELECT 1" 2>/dev/null
    run_test_neg "未知用户"         mysql -h 127.0.0.1 -P 4051 -u nobody -ptest test -e "SELECT 1" 2>/dev/null

    # ─── 慢查询链路:先触发 SLEEP 0.5s(> 阈值 200ms),记录断言放 API 段 ───
    $MYSQL test -e "SELECT SLEEP(0.5)" >/dev/null 2>&1 || true

    # ─── checkproxy 管理命令(业务端口)───
    run_test "checkproxy help"      $MYSQL test -e "checkproxy help"
    run_test "checkproxy status"    $MYSQL test -e "checkproxy show status"
    run_test "checkproxy connections" $MYSQL test -e "checkproxy show connections"
    run_test "checkproxy pool"      $MYSQL test -e "checkproxy show pool"
    run_test "checkproxy sql"       $MYSQL test -e "checkproxy show sql"
    run_test "checkproxy config"    $MYSQL test -e "checkproxy show config"
    run_test_neg "checkproxy 未知命令" $MYSQL test -e "checkproxy bogus" 2>/dev/null

    # ─── 管理 HTTP API ───
    if command -v curl >/dev/null 2>&1; then
        # 鉴权探测:配置了 mng_user 才验证 401/登录链路,否则跳过(免认证部署)
        if [ "$(curl -s -o /dev/null -w '%{http_code}' "$MNG_BASE/api/status")" = "401" ]; then
            run_test "API 未登录 401"   true
        else
            echo "  [SKIP] API 未登录 401 (本配置未启用管理鉴权)"
        fi
        TOKEN="$(api_login)"
        run_test "API 登录"         [ -n "$TOKEN" ]
        run_test "API /healthz"     test "$(curl -s "$MNG_BASE/healthz")" = "ok"
        run_test "API /readyz"      api_open_has "/readyz" "ok"
        run_test "API /api/status"  api_has "/api/status" '"connections_total"'
        run_test "API 分片阶段耗时" api_has "/api/status" '"shard_stages"'
        run_test "API /api/connections" api_has "/api/connections" '"connections"'
        run_test "API /api/sql"     api_has "/api/sql" '"top"'
        run_test "API 慢查询已记录" api_has "/api/slow" '"elapsed_us"'
        run_test "API 慢查询分片过滤" api_has "/api/slow?shard=0" '"slow"'
        run_test "API /api/recent"  api_has "/api/recent" '"recent"'
        run_test "API 最近查询库过滤" api_has "/api/recent?db=test" '"recent"'
        # 后端返回的 1064/1146 记录在 backend_errors(解析器本地错误才进 parsefailures)
        run_test "API 后端错误记录" api_has "/api/backenderrors" '"code"'
        run_test "API /api/pool"    api_has "/api/pool" '"buckets"'
        run_test "API /api/config"  api_has "/api/config" '"tablets"'
        run_test "API /api/check"   api_open_has "/api/check" '"overall":"ok"'
        run_test "API /metrics"     api_has "/metrics" "newproxy_queries_total"
        run_test "API 慢查询清除"   api_post_has "/api/slow/clear" '"ok":true'
        run_test "API 最近查询清除" api_post_has "/api/recent/clear" '"ok":true'
        # kill 真实连接:后台开一个 SLEEP 会话,从连接列表取 cid 后 kill
        $MYSQL test -e "SELECT SLEEP(5)" >/dev/null 2>&1 &
        local kpid=$!
        sleep 1
        local cid
        cid="$(curl -s -H "Cookie: newproxy_session=$TOKEN" "$MNG_BASE/api/connections" | grep -oE '"cid":[0-9]+' | grep -oE '[0-9]+' | head -1)"
        run_test "API 连接列表非空" [ -n "$cid" ]
        run_test "API /api/kill"    api_post_has "/api/kill?cid=$cid" '"ok":true'
        wait $kpid 2>/dev/null || true
    fi

    # ─── 并发(显式收集 pid 再 wait:裸 wait 会连代理服务器进程一起等,永不返回)───
    echo "--- 并发测试 (10 连接) ---"
    local -a cjobs=()
    local i
    for i in $(seq 1 10); do
        $MYSQL -e "SELECT SLEEP(0.1), $i" >/dev/null 2>&1 &
        cjobs+=($!)
    done
    wait "${cjobs[@]}"
    run_test "10并发" true
}

# ────────────────────────────────────────────────────────────────────────────
# env — 仅启动性能测试环境（3 容器：MySQL + Proxy + sysbench）
# ────────────────────────────────────────────────────────────────────────────
if [ "$MODE" = "env" ]; then
    echo "=== NewProxy 性能测试环境 (手动模式) ==="
    echo ""

    # 启动 3 容器（如未运行则自动 build + up）
    perf_ensure_containers

    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "  性能测试环境已就绪（3 容器）"
    echo ""
    echo "  MySQL:    mysql:3306  (宿主机 3307)"
    echo "  Proxy:    newproxy:4051 (宿主机 4051)"
    echo "  sysbench: 压测工具"
    echo "  账号:     root / test_password"
    echo "  数据库:   sbtest"
    echo ""
    echo "  登录代理: docker exec newproxy-perf-sysbench mysql -h newproxy -P 4051 -u root -ptest_password sbtest"
    echo "  登录 MySQL: docker exec newproxy-perf-sysbench mysql -h mysql -P 3306 -u root -ptest_password sbtest"
    echo "  压测:     ./tests/integration/run.sh perf all"
    echo "  容器日志: docker logs -f newproxy-perf"
    echo "  按 Ctrl+C 停止并清理环境"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo ""

    cleanup() {
        echo ""
        echo "停止性能测试容器..."
        docker compose -f "$ROOT/tests/perf/docker-compose.yml" down 2>/dev/null || true
        echo "环境已清理"
    }
    trap cleanup EXIT

    # 阻塞直到用户 Ctrl+C
    echo "按 Ctrl+C 退出..."
    while true; do sleep 60; done
fi

# ────────────────────────────────────────────────────────────────────────────
# cov — 代码覆盖率 (unit | e2e | all)
# 依赖:cargo-llvm-cov + rustup llvm-tools-preview(版本须匹配 rustc 内置 LLVM)
#   ./tests/integration/run.sh cov unit   # Rust 单测 + 解析测试覆盖率
#   ./tests/integration/run.sh cov e2e    # 插桩代理 + E2E 全量测试覆盖率
#   ./tests/integration/run.sh cov        # 两者都跑
# 报告:target/llvm-cov/html-unit|html-e2e/index.html
# 可选 COV_MIN_LINES=<百分比> 低于阈值时退出码非 0
# ────────────────────────────────────────────────────────────────────────────
cov_require() {
    if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
        echo "ERROR: 未安装 cargo-llvm-cov,请先执行: cargo install cargo-llvm-cov --locked" >&2
        exit 1
    fi
}

# 清除原始 profile，避免历史运行污染本次数字。
# cargo-llvm-cov report 会合并 target/llvm-cov-target 下全部 *.profraw，
# 因而这里必须显式建立干净基线。
cov_clear_profiles() {
    rm -f "$ROOT"/target/llvm-cov-target/*.profraw \
          "$ROOT"/target/llvm-cov-target/*.profdata
}

cov_unit() {
    cov_require
    cd "$ROOT"
    cov_clear_profiles
    echo "=== 代码覆盖率:单元/解析测试(独立干净口径) ==="
    cargo llvm-cov --workspace --lib --test mysql_syntax_tests --test skeleton --test front_error_paths \
        --html --output-dir "$ROOT/target/llvm-cov/html-unit" \
        --fail-under-lines "${COV_MIN_LINES:-0}"
    echo "HTML 报告: target/llvm-cov/html-unit/html/index.html"
}

# 在 e2e profile 的基础上追加 unit profile；仅 cov all 使用。
cov_unit_append() {
    cd "$ROOT"
    echo "=== 追加单元/解析 profile(合并口径) ==="
    cargo llvm-cov --no-clean --workspace --lib --test mysql_syntax_tests --test skeleton --test front_error_paths \
        --html --output-dir "$ROOT/target/llvm-cov/html-all" \
        --fail-under-lines "${COV_MIN_LINES:-0}"
}

cov_e2e() {
    cov_require
    cd "$ROOT"
    # e2e 独立口径:先清历史 profile；cov all 会先调用本函数，再追加 unit。
    cov_clear_profiles
    echo "=== 代码覆盖率:端到端(插桩代理 + E2E 全量测试，独立干净口径) ==="
    start_mysql
    echo ""
    # 编译插桩二进制并验证构建成功(使用 --help 正常退出)。
    # 不能组合 --no-report 与 --no-clean；此前命令失败被 || true 吞掉，
    # 会悄悄复用陈旧二进制，导致 Region 报告不可信。
    # cargo-llvm-cov 在独立 target 目录 target/llvm-cov-target 构建。
    echo "--- 编译插桩代理 ---"
    cargo llvm-cov run --no-report --bin newproxy -- --help >/dev/null
    local cov_bin="$ROOT/target/llvm-cov-target/debug/newproxy"
    [ -x "$cov_bin" ] || cov_bin="$ROOT/target/debug/newproxy"
    # 启动插桩代理。LLVM_PROFILE_FILE 必须落到 cargo-llvm-cov 扫描的
    # target/llvm-cov-target/ 目录(其 report 只合并该目录下的 *.profraw),
    # 且必须带 %m(无 %m 时 LLVM 运行时计数严重缺失,实测 168 vs 1888)。
    # 导出而非仅前缀:让 CLI/变体启动等子进程同样落盘 profraw。
    pkill -f "llvm-cov-target/debug/newproxy" 2>/dev/null || true
    mkdir -p "$ROOT/logs"
    export LLVM_PROFILE_FILE="$ROOT/target/llvm-cov-target/e2e-%p-%m.profraw"
    RUST_LOG=newproxy=info "$cov_bin" "$CONF" >"$ROOT/logs/cov-proxy.out" 2>&1 &
    local proxy_pid=$!
    # 等待代理就绪(插桩二进制首启较慢,最多 30s)
    local ready="false" i
    for i in $(seq 1 60); do
        if mysql -h 127.0.0.1 -P 4051 -u root -ptest_password test -e "SELECT 1" >/dev/null 2>&1; then
            ready="true"; break
        fi
        sleep 0.5
    done
    if [ "$ready" != "true" ]; then
        echo "ERROR: 插桩代理未就绪,日志尾部:" >&2
        tail -20 "$ROOT/logs/cov-proxy.out" >&2 || true
        kill $proxy_pid 2>/dev/null || true
        docker compose -f "$ROOT/tests/integration/docker-compose.yml" down --remove-orphans 2>/dev/null || true
        exit 1
    fi
    echo "插桩代理就绪 (PID=$proxy_pid)"
    echo ""
    # 跑 E2E 全量测试(复用 run_e2e_battery;PROXY_BIN 供 CLI 边界用例)
    PROXY_BIN="$cov_bin"
    PASS=0; FAIL=0
    run_e2e_battery
    echo ""
    echo "=== E2E 结果: $PASS 通过, $FAIL 失败 ==="
    # 优雅退出(SIGTERM → main 正常返回 → profraw 落盘);兜底:15s 未退出再强杀
    kill $proxy_pid 2>/dev/null || true
    local i
    for i in $(seq 1 30); do
        kill -0 $proxy_pid 2>/dev/null || break
        sleep 0.5
    done
    if kill -0 $proxy_pid 2>/dev/null; then
        echo "WARN: 插桩代理 15s 内未退出,强制终止(该次覆盖率数据可能不完整)" >&2
        kill -9 $proxy_pid 2>/dev/null || true
    fi
    wait $proxy_pid 2>/dev/null || true
    docker compose -f "$ROOT/tests/integration/docker-compose.yml" down --remove-orphans 2>/dev/null || true
    if [ "$FAIL" -ne 0 ]; then
        echo "E2E 有失败,跳过覆盖率报告生成(原始 profraw 保留在 target/llvm-cov-target)" >&2
        exit 1
    fi
    echo ""
    echo "--- 生成端到端覆盖率报告 ---"
    cargo llvm-cov report --html --output-dir "$ROOT/target/llvm-cov/html-e2e" \
        --fail-under-lines "${COV_MIN_LINES:-0}"
    echo "HTML 报告: target/llvm-cov/html-e2e/index.html"
}

if [ "$MODE" = "cov" ]; then
    COV_SUBCMD="all"
    for a in "$@"; do
        case "$a" in
            unit|e2e|all) COV_SUBCMD="$a"; break ;;
        esac
    done
    case "$COV_SUBCMD" in
        unit) cov_unit ;;
        e2e)  cov_e2e ;;
        # 合并口径:e2e 先建立干净基线，unit 用 --no-clean 追加 profile，
        # 最后才做报告。unit/e2e/all 三类报告互不混淆。
        all)  cov_e2e; cov_unit_append
              echo "--- 合并报告(排除外部依赖 zk/etcd/proc)---"
              # 同时输出可审计的文本 TOTAL 与 HTML 报告;--html 本身只打印保存路径。
              cargo llvm-cov report \
                  --ignore-filename-regex "config_center/(zk|etcd)\.rs|mgmt/proc\.rs" \
                  2>/dev/null | tail -2
              cargo llvm-cov report --html --output-dir "$ROOT/target/llvm-cov/html-all" \
                  --ignore-filename-regex "config_center/(zk|etcd)\.rs|mgmt/proc\.rs" \
                  --fail-under-lines "${COV_MIN_LINES:-0}" 2>/dev/null | tail -2
              echo "HTML 报告: target/llvm-cov/html-all/html/index.html"
              echo "--- 全量合并报告 ---"
              cargo llvm-cov report 2>/dev/null | tail -2 ;;
        *)    echo "未知 cov 子命令: $COV_SUBCMD (支持 unit|e2e|all)" >&2; exit 1 ;;
    esac
    exit 0
fi

# ────────────────────────────────────────────────────────────────────────────
# smoke / full — 运行测试
# ────────────────────────────────────────────────────────────────────────────
echo "=== NewProxy Rust 集成测试 ($MODE, $BUILD_TYPE) ==="
echo ""
start_mysql
echo ""
build_proxy
echo ""
start_proxy
print_env_info

cleanup() {
    echo ""
    echo "停止代理..."
    kill $PROXY_PID 2>/dev/null || true
    wait $PROXY_PID 2>/dev/null || true
    echo "停止 MySQL..."
    docker compose -f "$ROOT/tests/integration/docker-compose.yml" down --remove-orphans 2>/dev/null || true
}
trap cleanup EXIT

echo "[4/4] 运行测试..."
PASS=0
FAIL=0

if [ "$MODE" = "full" ]; then
    # 全量 E2E:基础 SQL/DDL/事务/预处理/错误处理/慢查询链路/
    # checkproxy 管理命令/管理 HTTP API/解析失败/CLI 边界/并发
    PROXY_BIN="$BIN"
    run_e2e_battery
else
    # 冒烟:快速 8 项
    run_test "连接"       $MYSQL test -e "SELECT 1"
    run_test "SELECT"     $MYSQL test -N -e "SELECT COUNT(*) FROM users"
    run_test "INSERT"     $MYSQL test -e "INSERT INTO users(name) VALUES('itest')"
    run_test "UPDATE"     $MYSQL test -e "UPDATE users SET name='updated' WHERE name='itest'"
    run_test "DELETE"     $MYSQL test -e "DELETE FROM users WHERE name='updated'"
    run_test "SHOW"       $MYSQL test -e "SHOW DATABASES"
    run_test "管理命令"    $MYSQL test -e "checkproxy show status"
    run_test_neg "认证拒绝" mysql -h 127.0.0.1 -P 4051 -u root -pwrong test -e "SELECT 1" 2>/dev/null
fi

echo ""
echo "=== 结果: $PASS 通过, $FAIL 失败 ==="
[ "$FAIL" -eq 0 ] || exit 1
