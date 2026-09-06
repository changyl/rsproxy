#!/bin/bash
# =============================================================================
# NewProxy (newproxy) — Rust 编译脚本
#
# 用法:
#   ./build.sh [命令]
#
# 命令:
#   release  编译 release 二进制 (默认, 生产用 target/release/newproxy)
#   debug    编译 debug 二进制 (target/debug/newproxy, 开发调试)
#   check    仅类型检查 (cargo check, 不产生二进制)
#   test     编译 + 全部测试 (cargo test --all-targets)
#   clippy   静态检查 (cargo clippy --workspace --all-targets -- -D warnings)
#   ci       等同 CI: fmt --check + clippy + build release + test
#   clean    清理 target/
#   install  编译 release 并安装到 PREFIX/bin (默认 /usr/local/bin)
#   start    以后台服务方式启动 newproxy (二进制缺失时先编译 release)
#   stop     停止服务 (SIGTERM, 10s 未退出则强杀)
#   restart  停止后重新启动
#   status   查看服务运行状态 (pid/端口/配置)
#   login    登录测试容器 (默认 newproxy,可指定 sysbench|mysql)
#   mysql    以 mysql 客户端直接登录中间件 (自动读测试配置端口/凭据)
#   logs     查看 newproxy 日志 (业务滚动日志; -f 跟踪, --out 看 stdout, --host 强制宿主机)
#   itest    单独运行测试,透传 tests/integration/run.sh (默认 smoke)
#   perf2    2 分片容器测试场景 (up|down|status|login|mysql|perf, 默认 up)
#   help     打印本帮助
#
# 容器模式(测试容器 tests/perf, Dockerfile 以 build.sh 为入口):
#   容器内自动检测(/.dockerenv),或设置 NEWPROXY_CONTAINER=1 强制。
#   - 路径切换为容器布局: /usr/local/bin/newproxy、/app/conf、/app/logs
#   - 容器内不编译(二进制缺失直接报错)
#   - 容器入口(PID 1)执行 start 时挂住,容器生命周期与 newproxy 一致:
#       启动容器      → build.sh start (ENTRYPOINT)
#       停止服务      → docker exec newproxy-perf build.sh stop
#       重启服务      → docker exec newproxy-perf build.sh restart
#       查看状态      → docker exec newproxy-perf build.sh status
#   - 2 分片场景独立 compose 项目(perf-2shard),与单分片并存:
#       ./build.sh perf2 up      # 构建并启动 2 分片容器(4052/9112/3308/3309)
#       ./build.sh perf2 status  # 容器状态 + 分片行数校验(5/7)
#
# 环境变量:
#   CARGO_ARGS    追加到所有 cargo 构建命令的参数 (如 --features xxx)
#   PREFIX        安装前缀 (仅 install 生效, 默认 /usr/local)
#   RUST_LOG      运行时日志级别, 与编译无关 (见 docs/11-usage-guide.md)
#   NEWPROXY_BIN   服务二进制路径 (默认 $ROOT/target/release/newproxy)
#   NEWPROXY_CONFIG 服务配置文件 (默认 $ROOT/conf/newproxy.conf)
#   NEWPROXY_PIDFILE PID 文件 (默认 $ROOT/logs/newproxy.pid)
#   NEWPROXY_OUTLOG 服务 stdout/stderr 重定向文件 (默认 $ROOT/logs/newproxy.out)
#   NEWPROXY_CONTAINER=1  强制容器模式(本机模拟容器路径用)
#
# 产物:
#   target/release/newproxy  (release)
#   target/debug/newproxy    (debug)
# =============================================================================

set -eu
# pipefail 是 bash/ksh 扩展,dash 不支持。支持时启用(管道任一环节失败即整体失败),
# 不支持时静默降级——保证 `sh build.sh`(Debian/Ubuntu 的 sh 是 dash)也能运行。
(set -o pipefail) 2>/dev/null && set -o pipefail || true

ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT"

CMD="${1:-release}"
# ${SECONDS:-0}:SECONDS 是 bash 专属,用 `sh` 执行时(如 `sh build.sh`)不定义,
# 配合 set -u 会直接报错;默认值保证两种解释器下都不中断。
START_TIME=${SECONDS:-0}

# ─── 输出辅助 ───
color() { printf "\033[%sm%s\033[0m\n" "$1" "$2"; }
step()  { printf "\n── %s ──\n" "$1"; }
ok()    { color "32" "✓ $1"; }
warn()  { color "33" "⚠ $1"; }
fail()  { color "31" "✗ $1"; }

# ─── 工具链检查 ───
require_cargo() {
    if ! command -v cargo >/dev/null 2>&1; then
        fail "未找到 cargo。请先安装 Rust 工具链: https://rustup.rs"
        exit 1
    fi
    # 注意:cargo 是 rustup 的 shim 时,`cargo --version` 可能因未配置默认工具链
    # 而失败(输出到 stderr)。此处必须容错——否则 set -e + pipefail 会在这一行
    # 静默杀死脚本,后续看不到任何报错。版本取不到只降级显示,真正的失败会
    # 由后续 cargo build 以明确错误暴露。
    local ver
    ver="$(cargo --version 2>/dev/null | awk '{print $2}')" || ver="?"
    [ -n "$ver" ] || ver="?"
    step "工具链"
    echo "cargo ${ver:-?} ($(rustc --version 2>/dev/null | awk '{print $2}' || echo '?'))"
    if [ "$ver" = "?" ]; then
        warn "cargo --version 无输出。若为 rustup 环境,请先执行: rustup default stable"
    fi
}

# ─── 产物校验 ───
verify_binary() {
    local bin="$1"
    step "产物"
    if [ -f "$bin" ]; then
        local size ver
        size="$(du -h "$bin" | cut -f1)"
        ver="$("$bin" -V 2>/dev/null || echo "?")"
        ok "$bin ($size, $ver)"
    else
        fail "未找到产物 $bin"
        exit 1
    fi
}

# ─── 各命令实现 ───
do_debug() {
    require_cargo
    step "编译 (debug)"
    cargo build ${CARGO_ARGS:-}
    verify_binary "target/debug/newproxy"
}

do_release() {
    require_cargo
    step "编译 (release, LTO + strip)"
    cargo build --release ${CARGO_ARGS:-}
    verify_binary "target/release/newproxy"
}

do_check() {
    require_cargo
    step "类型检查 (cargo check)"
    cargo check --all-targets ${CARGO_ARGS:-}
    echo ""
    ok "check 通过 (无二进制产物)"
}

do_test() {
    require_cargo
    step "编译 (release)"
    cargo build --release ${CARGO_ARGS:-}
    step "测试 (cargo test --all-targets)"
    cargo test --all-targets ${CARGO_ARGS:-}
    verify_binary "target/release/newproxy"
}

do_clippy() {
    require_cargo
    step "clippy (-D warnings)"
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    echo ""
    ok "clippy 通过"
}

do_ci() {
    require_cargo
    export CARGO_TERM_COLOR=always

    step "fmt --check"
    cargo fmt --check
    ok "fmt 通过"

    step "clippy (-D warnings)"
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    ok "clippy 通过"

    step "build --release"
    cargo build --release ${CARGO_ARGS:-}

    step "test --all-targets"
    cargo test --all-targets ${CARGO_ARGS:-}

    verify_binary "target/release/newproxy"
    echo ""
    color "32" "=== CI 全部通过 ==="
}

do_clean() {
    step "清理 target/"
    cargo clean
    echo ""
    ok "清理完成"
}

do_install() {
    do_release
    local prefix="${PREFIX:-/usr/local}"
    local bindir="$prefix/bin"
    step "安装"
    mkdir -p "$bindir"
    cp target/release/newproxy "$bindir/newproxy"
    ok "已安装到 $bindir/newproxy"
    echo "运行: newproxy -c <config>"
}

# ─── 服务管理 (start/stop/restart/status) ───
# newproxy 本身是前台进程,此处以 nohup 后台方式托管,通过 PID 文件管理生命周期。
# 路径均可通过环境变量覆盖 (见文件头注释)。
#
# 容器模式:检测到容器环境(/.dockerenv)或 NEWPROXY_CONTAINER=1 时,
# 默认路径切换为容器布局(/usr/local/bin/newproxy、/app/conf、/app/logs),
# 且不尝试容器内编译(镜像已内置二进制)。容器入口(PID 1)执行 start 时
# 挂住等待 newproxy 退出,使容器生命周期与 newproxy 一致:
#   - 启动容器            → build.sh start (ENTRYPOINT)
#   - docker exec ... stop → 停 newproxy,容器随之退出(restart 策略接管)
#   - docker exec ... restart → 容器内重启服务

IN_CONTAINER=0
if [ -f /.dockerenv ] || [ "${NEWPROXY_CONTAINER:-0}" = "1" ]; then
    IN_CONTAINER=1
fi

if [ "$IN_CONTAINER" = "1" ]; then
    : "${NEWPROXY_BIN:=/usr/local/bin/newproxy}"
    : "${NEWPROXY_CONFIG:=/app/conf/newproxy.conf}"
    : "${NEWPROXY_PIDFILE:=/app/logs/newproxy.pid}"
    : "${NEWPROXY_OUTLOG:=/app/logs/newproxy.out}"
else
    : "${NEWPROXY_BIN:=$ROOT/target/release/newproxy}"
    : "${NEWPROXY_CONFIG:=$ROOT/conf/newproxy.conf}"
    : "${NEWPROXY_PIDFILE:=$ROOT/logs/newproxy.pid}"
    : "${NEWPROXY_OUTLOG:=$ROOT/logs/newproxy.out}"
fi

# 进程是否存活(依据 PID 文件)
service_is_running() {
    [ -f "$NEWPROXY_PIDFILE" ] || return 1
    local pid
    pid="$(cat "$NEWPROXY_PIDFILE" 2>/dev/null || true)"
    [ -n "$pid" ] || return 1
    kill -0 "$pid" 2>/dev/null
}

service_pid() {
    service_is_running && cat "$NEWPROXY_PIDFILE" 2>/dev/null
}

# 从配置的 [MySQL_Proxy_Layer] 段提取主端口
service_port() {
    awk '
        /^\[/ { in_layer = ($0 ~ /^\[MySQL_Proxy_Layer\]/) }
        in_layer && /^[[:space:]]*port[[:space:]]*=/ {
            sub(/^[[:space:]]*port[[:space:]]*=[[:space:]]*/, "")
            print
            exit
        }
    ' "$NEWPROXY_CONFIG" 2>/dev/null
}

port_open() {
    local port="$1"
    if command -v nc >/dev/null 2>&1; then
        nc -z -w 1 127.0.0.1 "$port" >/dev/null 2>&1
    else
        true # 无 nc 时退化为"进程存活即就绪"
    fi
}

do_start() {
    if service_is_running; then
        fail "newproxy 已在运行 (pid $(service_pid), 配置 $NEWPROXY_CONFIG)"
        exit 1
    fi

    local port
    port="$(service_port)"

    # 预检端口:已被占用说明可能已有别的 newproxy 实例在跑(如手动启动),
    # 此时再起一个必然 bind 失败,直接拒绝并提示。
    # 注意:无 nc 时无法探测,跳过预检(否则会误判"端口被占用"而拒绝启动,
    # 容器镜像默认不含 nc,start 将永远失败)。
    if [ -n "$port" ] && command -v nc >/dev/null 2>&1 && port_open "$port"; then
        fail "端口 $port 已被占用 (可能已有 newproxy 实例在运行), 拒绝启动"
        exit 1
    fi

    if [ ! -f "$NEWPROXY_BIN" ]; then
        if [ "$IN_CONTAINER" = "1" ]; then
            fail "容器内未找到二进制 $NEWPROXY_BIN"
            echo "  请重新构建镜像: docker compose -f tests/perf/docker-compose.yml build newproxy"
            exit 1
        fi
        step "未找到二进制 $NEWPROXY_BIN,先编译 release"
        do_release
    fi

    if [ ! -f "$NEWPROXY_CONFIG" ]; then
        fail "配置文件不存在: $NEWPROXY_CONFIG (可用 NEWPROXY_CONFIG 覆盖)"
        exit 1
    fi

    mkdir -p "$(dirname "$NEWPROXY_PIDFILE")" "$(dirname "$NEWPROXY_OUTLOG")"

    step "启动 newproxy"
    echo "  binary : $NEWPROXY_BIN"
    echo "  config : $NEWPROXY_CONFIG"
    echo "  port   : ${port:-?}"
    echo "  outlog : $NEWPROXY_OUTLOG  (业务日志见配置 log_dir 的滚动文件)"

    nohup "$NEWPROXY_BIN" "$NEWPROXY_CONFIG" >>"$NEWPROXY_OUTLOG" 2>&1 &
    local pid=$!
    echo "$pid" >"$NEWPROXY_PIDFILE"

    # 先等进程稳定 (快速失败如 bind 冲突会在此时退出),再判断存活
    sleep 0.5
    if ! kill -0 "$pid" 2>/dev/null; then
        fail "进程启动后即退出, 最近日志:"
        tail -20 "$NEWPROXY_OUTLOG" 2>/dev/null || true
        rm -f "$NEWPROXY_PIDFILE"
        exit 1
    fi

    # 端口就绪探测(此时端口若可连即为本进程),最多 5s
    local ready=0
    local i
    for i in $(seq 1 25); do
        if [ -n "$port" ] && port_open "$port"; then
            ready=1
            break
        fi
        if ! kill -0 "$pid" 2>/dev/null; then
            break
        fi
        sleep 0.2
    done

    if [ "$ready" -eq 1 ]; then
        ok "已启动 (pid $pid, 端口 $port)"
    else
        if kill -0 "$pid" 2>/dev/null; then
            fail "进程已启动但端口 $port 未就绪, 详见 $NEWPROXY_OUTLOG"
        else
            fail "进程启动后即退出, 最近日志:"
            tail -20 "$NEWPROXY_OUTLOG" 2>/dev/null || true
        fi
        rm -f "$NEWPROXY_PIDFILE"
        exit 1
    fi

    # 容器入口(PID 1)场景:进入监督循环,使容器生命周期与 newproxy 一致。
    #   - newproxy 存活 → 容器保持运行;
    #   - `docker exec ... build.sh stop` 清除 pidfile → 容器干净退出(exit 0),
    #     由 compose restart 策略恢复;
    #   - 进程异常退出(pidfile 残留)→ 容器退出(exit 1),restart 策略恢复;
    #   - `docker exec ... build.sh restart` 换新实例 → 循环跟随新 pid。
    # docker exec 方式执行 start(PID != 1)时不进入循环,直接返回。
    if [ "$IN_CONTAINER" = "1" ] && [ "$$" -eq 1 ]; then
        echo "  (容器入口模式:监督 newproxy,stop/restart 可用 docker exec <容器> build.sh 触发)"
        local prev=""
        while true; do
            if [ -f "$NEWPROXY_PIDFILE" ]; then
                local cur
                cur="$(cat "$NEWPROXY_PIDFILE" 2>/dev/null || true)"
                if [ -n "$cur" ] && kill -0 "$cur" 2>/dev/null; then
                    prev="$cur"
                    sleep 1
                    continue
                fi
                if [ "$cur" != "$prev" ]; then
                    # restart 正在换实例(新 pidfile 已写入但进程尚未可 kill):观察一轮
                    prev="$cur"
                    sleep 1
                    continue
                fi
                echo "newproxy (pid $cur) exited unexpectedly, container exiting"
                exit 1
            fi
            if [ -n "$prev" ]; then
                echo "newproxy stopped, container exiting"
                exit 0
            fi
            sleep 1
        done
    fi
}

do_stop() {
    if ! service_is_running; then
        warn "newproxy 未在运行"
        rm -f "$NEWPROXY_PIDFILE"
        return 0
    fi

    local pid
    pid="$(cat "$NEWPROXY_PIDFILE")"
    step "停止 newproxy (pid $pid)"
    kill "$pid" 2>/dev/null || true

    # 等待优雅退出,最多 10s,超时强杀
    local i
    for i in $(seq 1 50); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.2
    done
    if kill -0 "$pid" 2>/dev/null; then
        warn "10s 内未退出, 强制 kill -9"
        kill -9 "$pid" 2>/dev/null || true
        sleep 0.3
    fi

    rm -f "$NEWPROXY_PIDFILE"
    ok "已停止"
}

do_restart() {
    do_stop
    do_start
}

do_status() {
    if service_is_running; then
        local pid
        pid="$(cat "$NEWPROXY_PIDFILE")"
        ok "newproxy 运行中 (pid $pid)"
        ps -p "$pid" -o pid,etime,command 2>/dev/null | tail -1 || true
        local port
        port="$(service_port)"
        [ -n "$port" ] && echo "  端口: $port"
        echo "  配置: $NEWPROXY_CONFIG"
        echo "  二进制: $NEWPROXY_BIN"
    else
        echo "newproxy 未运行"
        [ -f "$NEWPROXY_PIDFILE" ] && echo "  (存在残留 PID 文件 $NEWPROXY_PIDFILE, 可执行 stop 清理)"
        exit 1
    fi
}

# ─── 登录测试容器 (login) ───
# 直接登录中间件(newproxy-perf)或指定容器 shell(docker exec -it)。
# 容器未运行时自动拉起再登录,一步到位。
# 用法: ./build.sh login [newproxy|sysbench|mysql]
do_login() {
    local container="${1:-newproxy-perf}"
    if ! command -v docker >/dev/null 2>&1; then
        fail "未找到 docker(需 Docker / Colima 运行中)"
        exit 1
    fi
    # 容器别名 → 完整名
    case "$container" in
        newproxy|proxy) container="newproxy-perf" ;;
        sysbench)      container="newproxy-perf-sysbench" ;;
        mysql)         container="newproxy-perf-mysql" ;;
    esac

    # 容器未运行 → 自动启动(compose 按依赖拉起),再登录
    if ! docker ps --format '{{.Names}}' | grep -qx "$container"; then
        warn "容器 $container 未运行,自动启动..."
        local svc
        case "$container" in
            newproxy-perf)  svc="newproxy" ;;
            newproxy-perf-sysbench) svc="sysbench" ;;
            newproxy-perf-mysql)    svc="mysql" ;;
            *)                     svc="newproxy" ;;
        esac
        docker compose -f "$ROOT/tests/perf/docker-compose.yml" up -d "$svc" 2>&1 | tail -3
        # 等待容器运行(最多 60s)
        local i
        for i in $(seq 1 30); do
            if docker ps --format '{{.Names}}' | grep -qx "$container"; then
                break
            fi
            sleep 2
        done
        if ! docker ps --format '{{.Names}}' | grep -qx "$container"; then
            fail "容器 $container 启动失败,请手动检查: docker compose -f tests/perf/docker-compose.yml up -d"
            exit 1
        fi
    fi

    step "登录测试容器 $container"
    docker exec -it "$container" bash
}

# ─── 以 mysql 客户端直接登录中间件 (mysql) ───
# 从测试配置自动读取端口与产品用户凭据,宿主机 mysql 客户端连接中间件
# (等价于 mysql -h 127.0.0.1 -P <port> -u <user> -p<pass>)。
# 用法:
#   ./build.sh mysql                    # 交互式登录中间件
#   ./build.sh mysql sbtest             # 指定默认库后交互
#   ./build.sh mysql sbtest -e "SELECT 1"   # 执行 SQL(注意用 -e,SQL 不能作位置参数)
do_mysql() {
    local conf="${NEWPROXY_MYSQL_CONF:-$ROOT/tests/perf/newproxy-perf-docker.conf}"
    # 解析配置:主端口 + 第一个产品用户
    local port user pass
    port="$(awk '/^\[MySQL_Proxy_Layer\]/{f=1;next} /^\[/{f=0} f && /^[[:space:]]*port[[:space:]]*=/{sub(/^[^=]*=[[:space:]]*/,"");print;exit}' "$conf")"
    user="$(awk '/^\[Product_User/{f=1;next} /^\[/{f=0} f && /^username=/{sub(/^[^=]*=/,"");print;exit}' "$conf")"
    pass="$(awk '/^\[Product_User/{f=1;next} /^\[/{f=0} f && /^password=/{sub(/^[^=]*=/,"");print;exit}' "$conf")"
    [ -n "$port" ] || port=4051
    [ -n "$user" ] || { warn "配置 $conf 未找到产品用户,回退 root"; user=root; }
    [ -n "$pass" ] || pass=""

    if ! command -v mysql >/dev/null 2>&1; then
        fail "宿主机未安装 mysql 客户端"
        echo "  可用容器: ./build.sh login sysbench  然后执行: mysql -h newproxy -P 4051 -u $user ${pass:+-p$pass}"
        exit 1
    fi

    step "登录中间件 127.0.0.1:$port (user=$user)"
    echo "  (配置: $conf)"
    # 所有参数透传给 mysql(数据库名 / SQL / -e 等);无参数则交互模式
    mysql -h 127.0.0.1 -P "$port" -u "$user" ${pass:+-p"$pass"} "$@"
}

# ─── 查看日志 (logs) ───
# 用法:
#   ./build.sh logs              # 最近 50 行业务日志(慢查询/错误/阶段耗时)
#   ./build.sh logs -f           # 实时跟踪
#   ./build.sh logs --out        # stdout/stderr 输出(进程启动/崩溃日志)
#   ./build.sh logs --host       # 强制宿主机日志(容器在跑时默认看容器日志)
do_logs() {
    local follow="" show_out=0 host_mode=0
    for a in "$@"; do
        case "$a" in
            -f|--follow) follow="-f" ;;
            --out) show_out=1 ;;
            --host) host_mode=1 ;;
            *) warn "忽略未知参数: $a (支持 -f/--follow, --out, --host)" ;;
        esac
    done

    # 容器优先:容器在跑(且未强制宿主机)或脚本本身在容器内
    local container="newproxy-perf"
    local in_container=0
    if [ "$IN_CONTAINER" = "1" ]; then
        in_container=1
    elif [ "$host_mode" = "0" ] && command -v docker >/dev/null 2>&1 \
        && docker ps --format '{{.Names}}' 2>/dev/null | grep -qx "$container"; then
        in_container=1
    fi

    if [ "$in_container" = "1" ]; then
        step "查看容器日志 $container (/app/logs/newproxy.out)"
        # 容器内 newproxy 由 build.sh 托管,stdout/stderr 全部进入 newproxy.out
        # (含 info/warn 与 slow query 阶段耗时);滚动文件不一定落盘。
        docker exec "$container" tail ${follow:-} -n 50 /app/logs/newproxy.out 2>&1 || true
        return 0
    fi

    # 宿主机模式:业务日志在配置 log_dir 的滚动文件
    local ldir
    ldir="$(awk '/^\[MySQL_Proxy_Layer\]/{f=1;next} /^\[/{f=0} f && /^[[:space:]]*log_dir[[:space:]]*=/{sub(/^[^=]*=[[:space:]]*/,"");print;exit}' "$NEWPROXY_CONFIG")"
    [ -n "$ldir" ] || ldir="$ROOT/logs"
    case "$ldir" in
        /*) ;;
        *) ldir="$ROOT/$ldir" ;;
    esac
    step "查看日志: $ldir/newproxy.*.log"
    if [ "$show_out" = "1" ]; then
        tail ${follow:-} -n 50 "$NEWPROXY_OUTLOG" 2>&1 || true
    else
        tail ${follow:-} -n 50 "$ldir"/newproxy.*.log 2>&1 || true
    fi
}

# ─── 2 分片容器测试场景 (perf2) ───
# 独立 compose 项目(name: perf-2shard),与单分片场景(tests/perf/docker-compose.yml)
# 并存:容器名/端口/项目名均不冲突,可同时运行。
#   - 2 个 MySQL 分片: mysql-shard0(宿主机 3308)/mysql-shard1(3309),
#     sbtest1 表行数不同(5/7),查询结果可判别落在哪个分片
#   - newproxy-2shard: 4052(MySQL 协议)/9112(管理面板)
# 用法:
#   ./build.sh perf2                      # 构建 + 启动全部 2 分片容器(等价 perf2 up)
#   ./build.sh perf2 up                   # 一键启动(构建镜像 + 起全部容器)
#   ./build.sh perf2 down                 # 停止并删除 2 分片容器(数据随 init_2shard_*.sql 重建)
#   ./build.sh perf2 status               # 容器状态 + 分片行数校验(期望 5/7)
#   ./build.sh perf2 login [newproxy|mysql0|mysql1|sysbench]  # 登录容器(未运行自动拉起)
#   ./build.sh perf2 mysql [proxy|shard0|shard1]              # mysql 客户端连接(默认 proxy)
#   ./build.sh perf2 perf all                                 # 2 分片性能测试(等价 itest 2shard perf all)
PERF2_COMPOSE="$ROOT/tests/perf/docker-compose-2shard.yml"
PERF2_CONF="$ROOT/tests/perf/newproxy-2shard-docker.conf"
PERF2_PREFIX="newproxy-perf-2shard"
PERF2_PX_PORT=4052
PERF2_MNG_PORT=9112
PERF2_S0_PORT=3308
PERF2_S1_PORT=3309

perf2_require_docker() {
    if [ "${IN_CONTAINER:-0}" = "1" ]; then
        fail "perf2 是宿主机命令(需 docker/compose),容器内不可用"
        exit 1
    fi
    if ! command -v docker >/dev/null 2>&1; then
        fail "未找到 docker(需 Docker / Colima 运行中)"
        exit 1
    fi
}

perf2_status() {
    perf2_require_docker
    step "2 分片场景容器状态"
    docker ps -a --filter name="$PERF2_PREFIX" --format '  {{.Names}}\t{{.Status}}' | sort || true
    echo ""
    if command -v mysql >/dev/null 2>&1; then
        # 遍历配置中全部 Master_Host 后端(host → 宿主机端口映射),逐个统计行数。
        # 新容器首次初始化中行数查询可能瞬时失败,重试 3 次(间隔 2s)。
        local be_map="mysql-shard0:$PERF2_S0_PORT mysql-shard1:$PERF2_S1_PORT"
        local tbl host port hport rows i kv all_ok="true"
        while read -r tbl host port; do
            [ -n "$tbl" ] || continue
            hport=""
            for kv in $be_map; do
                case "$kv" in
                    "$host":*) hport="${kv#*:}" ;;
                esac
            done
            if [ -z "$hport" ]; then
                echo "  (分片 $tbl $host: 未配置宿主机端口映射,跳过行数统计)"
                continue
            fi
            rows="不可达"
            for i in 1 2 3; do
                [ "$rows" = "不可达" ] && rows="$(mysql -h127.0.0.1 -P"$hport" -uroot -ptest_password -N -e "select count(*) from sbtest.sbtest1" 2>/dev/null || echo '不可达')"
                [ "$rows" != "不可达" ] && break
                sleep 2
            done
            echo "  分片 $tbl ($host :$hport) sbtest1 行数 = $rows (期望 5/7)"
        done < <(awk '
            /^\[Master_Host/ { if (inm && p != "") print ct, h, p; inm=1; ct=""; h=""; p=""; next }
            /^\[/ { if (inm && p != "") print ct, h, p; inm=0 }
            inm && /^[[:space:]]*cluster_tablet_name=/ { ct=$0; sub(/^[^=]*=[[:space:]]*/,"",ct) }
            inm && /^[[:space:]]*host=/ { h=$0; sub(/^[^=]*=[[:space:]]*/,"",h) }
            inm && /^[[:space:]]*port=/ { p=$0; sub(/^[^=]*=[[:space:]]*/,"",p) }
            END { if (inm && p != "") print ct, h, p }
        ' "$PERF2_CONF")
        echo "  代理   (:$PERF2_PX_PORT, 面板 :$PERF2_MNG_PORT)"
    else
        echo "  代理端口: :$PERF2_PX_PORT (MySQL) / :$PERF2_MNG_PORT (面板, admin/admin)"
        echo "  分片端口: :$PERF2_S0_PORT / :$PERF2_S1_PORT (宿主机无 mysql 客户端,跳过行数校验)"
    fi
}

perf2_up() {
    perf2_require_docker
    step "2 分片场景:构建并启动 ($PERF2_COMPOSE)"
    # 沙箱/受限环境默认禁用 BuildKit(buildx 目录不可写),可用 DOCKER_BUILDKIT=1 覆盖
    DOCKER_BUILDKIT="${DOCKER_BUILDKIT:-0}" docker compose -f "$PERF2_COMPOSE" up -d --build 2>&1 | tail -6
    # 等待代理容器运行
    local i
    for i in $(seq 1 45); do
        docker ps --format '{{.Names}}' | grep -qx "$PERF2_PREFIX" && break
        sleep 2
    done
    if ! docker ps --format '{{.Names}}' | grep -qx "$PERF2_PREFIX"; then
        fail "2 分片代理容器启动失败,请手动检查: docker compose -f $PERF2_COMPOSE logs --tail 30"
        exit 1
    fi
    # 等待两个分片 healthcheck 通过(新容器首次初始化约 20-60s,期间行数查询会失败)
    for i in $(seq 1 60); do
        local all=1 st
        for c in "$PERF2_PREFIX-mysql0" "$PERF2_PREFIX-mysql1"; do
            st="$(docker inspect -f '{{.State.Health.Status}}' "$c" 2>/dev/null || echo missing)"
            [ "$st" = "healthy" ] || all=0
        done
        [ "$all" = "1" ] && break
        sleep 2
    done
    ok "2 分片场景已启动"
    perf2_status
}

perf2_down() {
    perf2_require_docker
    step "停止并删除 2 分片容器"
    docker compose -f "$PERF2_COMPOSE" down 2>&1 | tail -3
    ok "已清理 (下次 perf2 up 会重建并重新初始化分片数据)"
}

perf2_login() {
    perf2_require_docker
    local container="${1:-newproxy}"
    case "$container" in
        newproxy|proxy) container="newproxy-perf-2shard" ;;
        mysql0|shard0)  container="newproxy-perf-2shard-mysql0" ;;
        mysql1|shard1)  container="newproxy-perf-2shard-mysql1" ;;
        sysbench)       container="newproxy-perf-2shard-sysbench" ;;
        *)              container="newproxy-perf-2shard" ;;
    esac
    if ! docker ps --format '{{.Names}}' | grep -qx "$container"; then
        warn "容器 $container 未运行,自动启动 2 分片场景..."
        perf2_up
    fi
    step "登录容器 $container"
    docker exec -it "$container" bash
}

perf2_mysql() {
    perf2_require_docker
    local target="${1:-proxy}"
    local port="$PERF2_PX_PORT"
    case "$target" in
        proxy)         port="$PERF2_PX_PORT" ;;
        shard0|mysql0) port="$PERF2_S0_PORT" ;;
        shard1|mysql1) port="$PERF2_S1_PORT" ;;
        *)
            echo "未知连接目标: $target" >&2
            echo "支持: proxy | shard0 | shard1 (默认 proxy);其余参数透传 mysql,如: ./build.sh perf2 mysql proxy sbtest -e \"select 1\"" >&2
            exit 1
            ;;
    esac
    if [ "$#" -gt 0 ]; then shift; fi   # 去掉目标,其余参数透传 mysql(库名/-e 等)
    if ! command -v mysql >/dev/null 2>&1; then
        fail "宿主机未安装 mysql 客户端"
        echo "  可用容器: ./build.sh perf2 login sysbench  然后执行: mysql -h newproxy -P 4051 ..."
        exit 1
    fi
    local user pass
    user="$(awk '/^\[Product_User/{f=1;next} /^\[/{f=0} f && /^username=/{sub(/^[^=]*=/,"");print;exit}' "$PERF2_CONF")"
    pass="$(awk '/^\[Product_User/{f=1;next} /^\[/{f=0} f && /^password=/{sub(/^[^=]*=/,"");print;exit}' "$PERF2_CONF")"
    [ -n "$user" ] || user=root
    step "连接 $target (127.0.0.1:$port, user=$user)"
    mysql -h 127.0.0.1 -P "$port" -u "$user" ${pass:+-p"$pass"} "$@"
}

do_perf2() {
    local sub="${1:-up}"
    case "$sub" in
        up|start|"")    perf2_up ;;
        down|stop)      perf2_down ;;
        status|ps)      perf2_status ;;
        login)          shift; perf2_login "${1:-newproxy}" ;;
        mysql)          shift; perf2_mysql "$@" ;;
        perf)           shift; "$ROOT/tests/integration/run.sh" 2shard perf "$@" ;;
        *)
            echo "未知 perf2 子命令: $sub" >&2
            echo "支持: up | down | status | login [newproxy|mysql0|mysql1|sysbench] | mysql [proxy|shard0|shard1] | perf [prepare|baseline|proxy|run|all|cleanup]" >&2
            exit 1
            ;;
    esac
}

# ─── 单独运行测试 (itest) ───
# 透传 tests/integration/run.sh,支持 smoke/full/env/perf 等全部模式。
# 用法: ./build.sh itest [smoke|full|perf all|...]
do_itest() {
    if [ "$#" -eq 0 ]; then
        set -- smoke
    fi
    if [ ! -f "$ROOT/tests/integration/run.sh" ]; then
        fail "未找到 tests/integration/run.sh"
        exit 1
    fi
    step "运行测试: run.sh $*"
    "$ROOT/tests/integration/run.sh" "$@"
}

print_help() {
    cat <<'EOF'
NewProxy (newproxy) — Rust 编译 & 服务管理脚本

用法:
    ./build.sh [命令]

命令:
    release  编译 release 二进制 (默认, 生产用 target/release/newproxy)
    debug    编译 debug 二进制 (target/debug/newproxy, 开发调试)
    check    仅类型检查 (cargo check, 不产生二进制)
    test     编译 + 全部测试 (cargo test --all-targets)
    clippy   静态检查 (cargo clippy --workspace --all-targets -- -D warnings)
    ci       等同 CI: fmt --check + clippy + build release + test
    clean    清理 target/
    install  编译 release 并安装到 PREFIX/bin (默认 /usr/local/bin)
    start    以后台服务方式启动 newproxy (二进制缺失时先编译 release)
    stop     停止服务 (SIGTERM, 10s 未退出则强杀)
    restart  停止后重新启动
    status   查看服务运行状态 (pid/端口/配置)
    login    直接登录中间件/测试容器 (默认 newproxy,未运行自动拉起;可指定 sysbench|mysql)
    mysql    以 mysql 客户端直接登录中间件 (自动读测试配置端口/凭据)
    logs     查看 newproxy 日志 (-f 跟踪; --out stdout; --host 强制宿主机)
    itest    单独运行测试,透传 tests/integration/run.sh (默认 smoke)
    cov      代码覆盖率 (unit|e2e|all; 依赖 cargo-llvm-cov,报告在 target/llvm-cov)
    perf2    2 分片容器测试场景 (up|down|status|login|mysql|perf, 默认 up)
    help     打印本帮助

环境变量:
    CARGO_ARGS    追加到所有 cargo 构建命令的参数 (如 --features xxx)
    PREFIX        安装前缀 (仅 install 生效, 默认 /usr/local)
    NEWPROXY_BIN      服务二进制路径 (默认 target/release/newproxy)
    NEWPROXY_CONFIG   服务配置文件 (默认 conf/newproxy.conf)
    NEWPROXY_PIDFILE  PID 文件 (默认 logs/newproxy.pid)
    NEWPROXY_OUTLOG   服务 stdout/stderr 重定向文件 (默认 logs/newproxy.out)
    NEWPROXY_CONTAINER=1  强制容器模式(路径切换为 /usr/local/bin、/app 布局)

容器模式(测试容器 tests/perf):
    docker compose -f tests/perf/docker-compose.yml up -d --build   # 启动(入口 build.sh start)
    docker exec newproxy-perf build.sh status
    docker exec newproxy-perf build.sh restart
    docker exec newproxy-perf build.sh stop
    # stop 后容器随 newproxy 退出,由 restart: unless-stopped 策略自动恢复
    ./build.sh perf2 up        # 2 分片场景:构建并启动 (4052/9112/3308/3309, 独立项目 perf-2shard)
    ./build.sh perf2 status    # 容器状态 + 分片行数校验(期望 5/7)
    ./build.sh perf2 down      # 停止并删除 2 分片容器

示例:
    ./build.sh release            # 编译 release
    ./build.sh start              # 启动服务 (conf/newproxy.conf)
    NEWPROXY_CONFIG=tests/integration/newproxy-test.conf ./build.sh start
    ./build.sh status             # 查看运行状态
    ./build.sh restart            # 重启服务
    ./build.sh stop               # 停止服务
    ./build.sh login              # 登录测试容器 (newproxy-perf)
    ./build.sh login sysbench     # 登录压测容器
    ./build.sh logs -f            # 实时跟踪代理日志
    ./build.sh mysql              # 交互式登录中间件 (mysql 客户端)
    ./build.sh mysql sbtest -e "SELECT 1"  # 直接执行 SQL
    ./build.sh itest              # 运行冒烟测试 (run.sh smoke)
    ./build.sh itest full         # 完整集成测试
    ./build.sh itest perf all     # 完整性能测试
    ./build.sh itest 2shard all   # 2 分片场景测试(环境+功能检查)
    ./build.sh itest 2shard check # 2 分片功能检查(环境缺失时自动拉起,不重建)
    ./build.sh itest 2shard test  # 2 分片纯测试(需先 perf2 up,不碰环境)
    ./build.sh itest 2shard perf all  # 2 分片性能测试(准备+直连基线+代理+报告)
    ./build.sh cov unit           # 代码覆盖率:单元/解析测试 (target/llvm-cov/html-unit)
    ./build.sh cov e2e            # 代码覆盖率:端到端全量测试 (target/llvm-cov/html-e2e)
    ./build.sh cov                # 单元 + 端到端覆盖率(COV_MIN_LINES=60 可设阈值)
    ./build.sh perf2              # 构建并启动 2 分片容器测试场景
    ./build.sh perf2 mysql shard0 # 直连分片 0 验证数据 (应 5 行)
    ./build.sh perf2 status       # 查看状态与分片行数

产物:
    target/release/newproxy  (release)
    target/debug/newproxy    (debug)
EOF
}

# ─── 分发 ───
case "$CMD" in
    release) do_release ;;
    debug)   do_debug ;;
    check)   do_check ;;
    test)    do_test ;;
    clippy)  do_clippy ;;
    ci)      do_ci ;;
    clean)   do_clean ;;
    install) do_install ;;
    start)   do_start ;;
    stop)    do_stop ;;
    restart) do_restart ;;
    status)  do_status ;;
    login)   do_login "${2:-}" ;;
    mysql)   shift; do_mysql "$@" ;;
    logs)    shift; do_logs "$@" ;;
    itest)   shift; do_itest "$@" ;;
    cov)     shift; do_itest cov "$@" ;;
    perf2)   shift; do_perf2 "$@" ;;
    help|-h|--help) print_help ;;
    *)
        echo "未知命令: $CMD" >&2
        echo "" >&2
        print_help >&2
        exit 1
        ;;
esac

ELAPSED=$(( ${SECONDS:-0} - START_TIME ))
echo ""
echo "=== 完成 (${ELAPSED}s) ==="
