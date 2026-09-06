# sysbench — arm64 原生镜像（避免 severalnines/sysbench 仅 amd64 在 QEMU 下 LuaJIT 段错误）
#
# 构建: docker compose -f tests/perf/docker-compose.yml build sysbench
#
# 用 Debian 仓库的 sysbench 1.0.20，原生 arm64，压测数据准确。
#
# 基础镜像源 ARG:网络受限环境经 docker-compose.yml 指向镜像加速器,
# 直连环境可用 BASE_DEBIAN 环境变量覆盖回官方源。

ARG BASE_DEBIAN=debian:bookworm-slim
FROM ${BASE_DEBIAN}

# apt 换清华源(与 dbproxy Dockerfile 一致,实测稳定;阿里云源在本机
# 下载 14MB 依赖包时多次 unexpected EOF 中断,导致 --build 失败)
RUN sed -i 's|deb.debian.org|mirrors.tuna.tsinghua.edu.cn|g; s|security.debian.org|mirrors.tuna.tsinghua.edu.cn|g' /etc/apt/sources.list.d/debian.sources

# Acquire::Retries:网络抖动时 apt 自动重试下载,避免构建半途失败
RUN apt-get -o Acquire::Retries=5 update \
    && apt-get -o Acquire::Retries=5 install -y --no-install-recommends sysbench default-mysql-client \
    && rm -rf /var/lib/apt/lists/*

# Debian 包的 lua 脚本在 /usr/share/sysbench，建软链接兼容 severalnines 镜像的
# /usr/local/share/sysbench 路径（run.sh 脚本硬编码了该路径）
RUN ln -s /usr/share/sysbench /usr/local/share/sysbench

# 校验：sysbench 必须可执行（QEMU 模拟下 LuaJIT 会段错误，原生 arm64 不会）
RUN sysbench --version && test -f /usr/local/share/sysbench/oltp_point_select.lua
