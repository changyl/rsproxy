# NewProxy Proxy — 性能测试用 Docker 镜像（多阶段编译）
#
# 构建: docker compose -f tests/perf/docker-compose.yml build newproxy
#
# 基础镜像源 ARG(网络受限环境直连 Docker Hub 超时,默认经 docker-compose.yml
# 指向镜像加速器 docker.m.daocloud.io;直连环境可用环境变量覆盖回官方源):
#   BASE_RUST=rust:1-bookworm   BASE_DEBIAN=debian:bookworm-slim

# ─── 基础镜像源 ARG(文件顶部声明,供所有 FROM 使用)───
ARG BASE_RUST=rust:1-bookworm
ARG BASE_DEBIAN=debian:bookworm-slim

# ─── 编译阶段 ───
FROM ${BASE_RUST} AS builder
WORKDIR /app

# 国内镜像源：apt 换清华源，cargo 换 rsproxy（绕开 colima 访问官方源慢/失败）
RUN sed -i 's|deb.debian.org|mirrors.tuna.tsinghua.edu.cn|g; s|security.debian.org|mirrors.tuna.tsinghua.edu.cn|g' /etc/apt/sources.list.d/debian.sources
# protoc:etcd-client 的 build.rs 需要编译 proto 文件(本地开发机有,容器镜像没有,
# 缺失会直接导致构建失败)
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler && rm -rf /var/lib/apt/lists/*
RUN mkdir -p /usr/local/cargo && printf '[source.crates-io]\nreplace-with = "rsproxy-sparse"\n\n[source.rsproxy-sparse]\nregistry = "sparse+https://rsproxy.cn/index/"\n\n[registries.rsproxy]\nindex = "sparse+https://rsproxy.cn/index/"\n\n[net]\ngit-fetch-with-cli = true\n' > /usr/local/cargo/config.toml

# 先复制依赖清单（利用 Docker 缓存层）
COPY Cargo.toml Cargo.lock ./
COPY build.rs ./

# 容器内编译资源受限(colima VM 默认 ~6GB/4 核):release 用 thin LTO 替代
# Cargo.toml 的 fat LTO(链接期内存峰值过高,rustc 易被 OOM SIGKILL),
# 并限制并行度避免多 rustc 同时编译。thin 相对 fat 的产物性能差异
# 对测试容器可忽略。
ENV CARGO_PROFILE_RELEASE_LTO=thin \
    CARGO_BUILD_JOBS=4

# 创建占位 src，让 cargo 只下载并编译依赖
# 注意：必须保留 build.rs 要求的最小 lib 结构，否则依赖编译会失败
RUN mkdir src && echo 'fn main() {}' > src/main.rs && echo '' > src/lib.rs
RUN cargo build --release 2>&1 | tail -5 || true

# 复制真实源码并强制重新编译 newproxy bin
# （touch src/main.rs 确保 cargo 重新链接 bin，避免占位 stub 残留）
RUN rm -rf src
COPY src ./src
RUN touch src/main.rs src/lib.rs && cargo build --release

# 校验：二进制必须大于占位 stub（~330KB），否则编译未真正生效
RUN test "$(stat -c%s /app/target/release/newproxy)" -gt 1000000 \
    && echo "OK: newproxy binary size=$(stat -c%s /app/target/release/newproxy)" \
    || { echo "ERROR: newproxy binary too small, build failed"; exit 1; }

# ─── 运行阶段 ───
FROM ${BASE_DEBIAN}

# apt 换清华源
RUN sed -i 's|deb.debian.org|mirrors.tuna.tsinghua.edu.cn|g; s|security.debian.org|mirrors.tuna.tsinghua.edu.cn|g' /etc/apt/sources.list.d/debian.sources
# bash: build.sh 解释器;procps: build.sh status 的 ps
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates bash procps \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/newproxy /usr/local/bin/newproxy
# 服务管理脚本(容器入口):提供 start/stop/restart/status
#   docker exec newproxy-perf-newproxy build.sh restart|stop|status
COPY build.sh /usr/local/bin/build.sh
RUN chmod +x /usr/local/bin/build.sh && mkdir -p /app/logs

EXPOSE 4051 9111

# 容器入口 = build.sh start:后台启动 newproxy 并挂住,容器生命周期与 newproxy 一致;
# stop 后容器退出,由 compose 的 restart: unless-stopped 策略自动恢复。
ENTRYPOINT ["/usr/local/bin/build.sh"]
CMD ["start"]
