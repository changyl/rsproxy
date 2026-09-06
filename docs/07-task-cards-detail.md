# 任务卡(详细版)——P0 ~ P5 全量

> 本文档细化 [06-task-plan.md](./06-task-plan.md) 中 P0–P5 全部 35 张任务卡至"工程师拿起来就能干"的级别。
> 每张卡包含:目标 / 前置依赖 / 实施步骤(落到真实文件与 C 侧语义来源)/ 关键代码骨架 / 验收操作 / 常见坑。
> C 侧行号引用格式:`src/tr_xxx.c:NNNN`、`modules/sqlparser/include/sql_define.h:NNNN`;设计文档引用格式:`/docs/0X-xxx.md §N`。
> 术语与风格与 06-task-plan.md 保持一致。所有 C 侧行号均经 `grep -n` 核实。

> ⚠️ **勘误(相对 01–06 文档的修正)**:细化过程中核实源码发现以下事实与早期文档描述不符,以本文件为准:
> - **管理端口**:06 称"9111 管理命令",实际管理命令走**业务端口 4051,以 `checkproxy xxx` SQL 形式经 watchdog 用户接入**(`tr_packet.c:1615` 分发);`mng_port=9111` 仅是配置项,从不 bind socket。
> - **测试驱动**:`mysql_case` 用 **`mysqltest` 二进制**(非 JDBC),`newproxy_case` 用 **Python2 `nosetests` + `mysql.connector`**;`totalResult.sh` 是直连后端 5897 端口 record 期望结果的生成器。
> - **压测入口**:`tools/load.sh` 是 DJB supervise 启停脚本,非负载生成器;真正的 sysbench 压测在 `build.sh:run_sysbench()`,端口 4052。
> - **ASAN 用例**:`test/asan/` 仅剩 Makefile,C 源文件缺失,不可直接迁移,需 Rust 侧自写等价单测。
> - **metric 目标**:C 侧硬编码 `127.0.0.1:788`(`tr_metric.c:3-4`),非配置项;滚动计数器在 `tr_sql.c`,非 `tr_stat.c`。

---

## P0 —— 工程脚手架

### T0.1 Rust 工程骨架与构建

**目标**:在仓库根新建 Rust crate `newproxy`,产出可编译的空 binary(只含 `main` 打印版本号),并能链接 sqlparser 静态库 `libsqlparser.a`。

**前置依赖**:无(`/docs` 设计文档就绪)。

**实施步骤**:
1. 在仓库根新增 `Cargo.toml`:crate 名 `newproxy`,二进制名 `newproxy`(与 C 侧产物名一致,见根 `CMakeLists.txt:36` 的 `add_executable(newproxy ...)`),`edition = "2021"`。`[dependencies]` 先放占位:`tokio = { version = "1", features = ["full"] }`、`bytes = "1"`、`tracing = "0.1"`、`parking_lot = "0.12"`、`dashmap = "5"`、`sha1 = "0.10"`、`rand = "0.8"`。`[build-dependencies]` 放 `bindgen = "0.69"`(为 T0.2 预留)。
2. 新增目录骨架:`src/main.rs`(只 `#[tokio::main] async fn main() { println!("newproxy {}", env!("CARGO_PKG_VERSION")); }`)、`src/lib.rs`(声明 `pub mod ...`)、`src/proto/`、`src/pool/`、`src/conn/`、`src/config/`、`src/ffi/`、`build.rs`(空占位)、`tests/`。
3. 新增 `build.rs`:先只 `println!("cargo:rerun-if-changed=modules/sqlparser/include/sql_define.h");` + 链接指令 `cargo:rustc-link-search=native=modules/sqlparser/build` 与 `cargo:rustc-link-lib=static=sqlparser`(路径与 `modules/sqlparser/build_and_test.sh:8` 的 `BUILD_DIR` 一致;库名见 `modules/sqlparser/CMakeLists.txt:51` 的 `add_library(sqlparser ...)`)。同时声明 sqlparser 的 C 依赖:`cargo:rustc-link-lib=z`、`m`、`pthread`、`crypt`、`crypto`(对应 `modules/sqlparser/CMakeLists.txt:54` 的 `target_link_libraries(sqlparser z m pthread crypt crypto)`)。
4. 先手动构建一次 sqlparser 静态库(进入 `modules/sqlparser/` 跑 `sh build_and_test.sh build` 或 `cmake -B build && cmake --build build`),确认 `modules/sqlparser/build/libsqlparser.a` 存在。
5. 在 `src/main.rs` 加一个 `extern "C" { fn mp_init(size: u32) -> *mut std::ffi::c_void; }` 的最小 extern 声明,`main` 里 `unsafe { let _ = mp_init(1024*1024); }` 以验证链接成功(函数原型见 `sql_define.h:3004`)。这一步只为验证链接,真正的 bindgen 在 T0.2。
6. 配置 CI(GitHub Actions 或 GitLab CI,与团队一致):`cargo fmt --check` + `cargo clippy -- -D warnings` + `cargo build --release`。

**关键代码骨架/要点**:
```rust
// Cargo.toml 关键片段
[package]
name = "newproxy"
version = "5.0.1.6"          # 与 CMakeLists.txt:38 的 -DVERSION 一致
edition = "2021"

[[bin]]
name = "newproxy"
path = "src/main.rs"

[dependencies]
tokio = { version = "1", features = ["full"] }
bytes = "1"
tracing = "0.1"
parking_lot = "0.12"
dashmap = "5"
sha1 = "0.10"
rand = "0.8"

[build-dependencies]
bindgen = "0.69"
```
```rust
// build.rs(最小链接,bindgen 在 T0.2 扩展)
fn main() {
    println!("cargo:rerun-if-changed=modules/sqlparser/include/sql_define.h");
    println!("cargo:rustc-link-search=native=modules/sqlparser/build");
    println!("cargo:rustc-link-lib=static=sqlparser");
    for lib in ["z", "m", "pthread", "crypt", "crypto"] {
        println!("cargo:rustc-link-lib={lib}");
    }
}
```

**验收操作**:
- `cargo build --release` 成功,产出 `target/release/newproxy`。
- `./target/release/newproxy` 输出 `newproxy 5.0.1.6` 且不报 undefined symbol。
- `cargo fmt --check && cargo clippy -- -D warnings` 通过。
- `nm target/release/newproxy | grep mp_init` 能查到未定义符号引用(证明链接了 sqlparser)。

**常见坑/Rust 特有注意**:
- macOS 上 `crypto`/`crypt` 可能缺失:开发机若为 macOS,sqlparser 需在 Linux 容器里构建(`test/docker` 已有环境);CI 必须跑在 Linux。
- `bindgen` 需要 `libclang`:CI 镜像需装 `llvm`/`clang`。
- sqlparser 用 `-std=gnu89 -fPIC`(`modules/sqlparser/CMakeLists.txt:37`),与 Rust 默认 ABI 兼容,但注意 `parse_sql` 的 `char *sql` 参数是**可写**且需 NUL 结尾(见 T0.2)。
- 不要把 `modules/sqlparser/build/` 提交进 git;在 `.gitignore` 加 `modules/sqlparser/build/`。

---

### T0.2 sqlparser FFI bindgen

**目标**:用 `bindgen` 从 `modules/sqlparser/include/sql_define.h` 生成 Rust 绑定 `src/ffi/sqlparser_ffi.rs`,封装安全 API `ParserPool::acquire().parse(sql) -> Result<AstHandle>`,单元测试能解析 `SELECT 1` 返回 rc=0 且 `sql_cmd` 非空。

**前置依赖**:T0.1(工程骨架 + 链接通过)。

**实施步骤**:
1. 完善 `build.rs`:按 `/docs/05-parser-pool.md §8` 的配置,用 `bindgen::Builder` 指向 `modules/sqlparser/include/sql_define.h`。`allowlist_type("sql_.*|mem_pool_t|sql_parser_t")`、`allowlist_function("mp_.*|sql_parser_.*|parse_sql|sql_item_.*|sql_cmd_.*|sql_err_to_str|sql_syntax_err_str")`、`allowlist_var("CMD_.*|SQL_.*|MYSQL_.*")`、`derive_default(true)`。输出到 `OUT_DIR/sqlparser_ffi.rs`(`bindgen` 写文件,见 05 §8)。
2. 新增 `src/ffi/mod.rs`:`include!(concat!(env!("OUT_DIR"), "/sqlparser_ffi.rs"));` 引入生成绑定。确认以下符号存在(逐行核实于 `sql_define.h`):`mp_init`(3004)、`mp_clear`(3014)、`mp_free`(3019)、`sql_parser_init`(3026)、`sql_parser_clear`(3031)、`parse_sql`(3036)、`sql_parser_free`(3041);字段 `sql_parser_t.sql_cmd`(2972)、`sql_parser_t.err_no`(2969)、`sql_parser_t.sql_syntax_error_str`(2977)。
3. 在 `src/ffi/parser_pool.rs` 实现 `RawParser`、`ParserPool`、`ParserGuard<'a>`、`AstHandle<'g>`,严格按 `/docs/05-parser-pool.md §3-5`。`RawParser` 持 `*mut ffi::mem_pool_t` + `*mut ffi::sql_parser_t`,`unsafe impl Send for RawParser {}`(**绝不** `impl Sync`)。
4. `ParserPool::acquire()` 按 05 §4:锁 `free: Mutex<Vec<Box<RawParser>>>`,空且未满则 `RawParser::new()`(调 `mp_init(10*1024*1024)` + `sql_parser_init(pool)`),满则 `tokio::task::yield_now().await` 重试。
5. `ParserGuard::parse(&mut self, sql: &str)`:用 `CString::new(sql)`(防内部 NUL),`unsafe { ffi::parse_sql(c_sql.as_ptr() as *mut c_char, p.parser) }`;rc!=0 时用 `CStr::from_ptr((*p.parser).sql_syntax_error_str)` 取错误。返回 `AstHandle { cmd: (*p.parser).sql_cmd, _marker: PhantomData }`。
6. `Drop for ParserGuard`:调 `unsafe { ffi::sql_parser_clear(p.parser) }` + push 回池(05 §4,同步无 await)。
7. 写单元测试 `tests/ffi_parse.rs`:解析 `"SELECT 1"`、`"INSERT INTO t (a) VALUES (1)"`、`"SELECT * FROM t WHERE id=1"`(从 `modules/sqlparser/example/sample.c` 的用法与 `t/sp_basic_test_base.h` 的 FFI 驱动序列对齐)。断言 rc==0 且 `sql_cmd` 非空。再写 fuzz 测试:1000 条 `arbitrary` 生成的随机字节串,只断言不 segfault(05 §9)。
8. 对照 `modules/sqlparser/build_and_test.sh:91-100` 的回归用例:解析 `INSERT INTO t (a) VALUES ('McDonald''s');` 输出不含 `parsing error`。

**关键代码骨架/要点**:
```rust
// src/ffi/parser_pool.rs(核心,见 05-parser-pool.md §3-5)
use parking_lot::Mutex;
use std::sync::Arc;

pub struct RawParser {
    pool: *mut ffi::mem_pool_t,
    parser: *mut ffi::sql_parser_t,
}
unsafe impl Send for RawParser {}    // 只 Send 不 Sync

impl RawParser {
    fn new() -> Box<Self> {
        let pool = unsafe { ffi::mp_init(10 * 1024 * 1024) };
        let parser = unsafe { ffi::sql_parser_init(pool) };
        Box::new(RawParser { pool, parser })
    }
}

pub struct ParserPool {
    free: Mutex<Vec<Box<RawParser>>>,
    cap: usize,
}

pub struct ParserGuard<'a> {
    pool: &'a ParserPool,
    inner: Option<Box<RawParser>>,
}

pub struct AstHandle<'g> {
    pub(crate) cmd: *mut ffi::sql_command_t,
    _marker: std::marker::PhantomData<&'g mut ParserGuard<'g>>,
}

impl<'a> ParserGuard<'a> {
    pub fn parse(&mut self, sql: &str) -> Result<AstHandle<'_>, ParseError> {
        let p = self.inner.as_mut().unwrap();
        let c_sql = std::ffi::CString::new(sql).map_err(|_| ParseError::InteriorNul)?;
        let rc = unsafe { ffi::parse_sql(c_sql.as_ptr() as *mut i8, p.parser) };
        if rc != 0 {
            let err = unsafe { std::ffi::CStr::from_ptr((*p.parser).sql_syntax_error_str) }
                .to_string_lossy().into_owned();
            return Err(ParseError::Syntax(err));
        }
        Ok(AstHandle { cmd: unsafe { (*p.parser).sql_cmd }, _marker: std::marker::PhantomData })
    }
}
```
注意 `sql_command_t` 的 vtable(`sql_define.h:2080-2084` 的 `CMD_UNSET` 宏):第一字段 `cmd_type`,其后是三个函数指针 `to_string`/`to_stencil`/`clone`。若 P3 需要 unparse,需在 `AstHandle` 上加 `unsafe` 方法调 `(*self.cmd).to_string.expect(...)(buf, max, self.cmd)`。**不存在**顶层 `clone_command` 函数,clone 只能走 vtable 的 `clone` 字段(`sql_define.h:2084`),P3.7 prepared 缓存需深拷贝到独立 pool(05 §7)。

**验收操作**:
- `cargo build` 成功,`OUT_DIR/sqlparser_ffi.rs` 已生成。
- `cargo test --test ffi_parse` 通过:`SELECT 1` 解析 rc==0、`sql_cmd` 非空。
- `cargo test --test ffi_fuzz` 通过:1000 条随机 SQL 无 segfault。
- 手动:`cargo run --example parse_select "SELECT 1"`(可选 example)输出 `rc=0`。
- `valgrind ./target/debug/newproxy-xxx`(若装了 valgrind)无 definitely-lost。

**常见坑/Rust 特有注意**:
- `parse_sql` 第一个参数是 `char *`(可写),不能用 `sql.as_ptr()`(指向只读 `&str` 的 UTF8 buffer,且无 NUL);必须 `CString::new` 拷贝一份。
- `RawParser` 只 `Send` 不 `Sync`:clippy 加 `#![warn(clippy::non_send)]` 检查;`ParserGuard` 借用 `&'a ParserPool` 保证同一时刻一个 parser 只被一个 task 持有。
- `AstHandle<'g>` 的 `PhantomData<&'g mut ParserGuard<'g>>` 让 AST 指针不能逃逸出 guard 作用域——编译期杜绝 use-after-pool-clear。
- `sql_parser_clear` 只清状态保留 arena(05 §2);`mp_clear` 重置 arena。guard drop 调 `sql_parser_clear` 而非 `mp_clear`(arena 复用更快)。
- bindgen 可能生成大量未使用 warning:在 `src/ffi/mod.rs` 加 `#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code, deref_nullptr)]`。
- 若 bindgen 报 "layout test failed":`.layout_tests(false)` 关掉(FFI 结构体布局由 C 侧保证)。

---

### T0.3 配置解析(最小)

**目标**:`AppCtx::load(path)` 解析 `conf/newproxy.conf` 的核心字段(port / mng_port / max_threads / cluster / tablet / master/slave host / db_user / product_user / auth_ip),输出结构化 `Config`;字段缺失或非法时报错清晰。

**前置依赖**:T0.1。**不**依赖 T0.2。

**实施步骤**:
1. 用 `serde` + `rust-ini` crate 解析 GKeyFile INI 格式(格式见 `conf/newproxy.conf`,GKeyFile 即 `[section] key=value`,`#` 注释;加载入口对应 C 侧 `tr_config.c:2284` 的 `tr_read_config` + `tr_config.c:2300` 的 `g_key_file_load_from_file`)。
2. 在 `src/config/mod.rs` 定义 `Config` 结构,字段对齐 `tr_config.h:276-364` 的 `struct tr_config_s`。最小集:`port: u16`(278)、`mng_port: u16`(279)、`max_threads: usize`(280)、`log_dir`(287)、`log_level`(289)、`front_idle_timeout`(298)、`backend_idle_timeout`(299)、`conn_pool_socket_max_serve_client_times: u64`(302)、`max_query_size`(306)、`max_sql_size`(310)、`default_charset: u8`(311)、`stream_on: u8`(308)、`clusters: HashMap<ClusterId, Cluster>`(330)、`cluster_tablets`(331)、`db_user`(332)、`users`(334)、`auth_ips`(335)。
3. Section 分发按 C 侧 `tr_config.c:2313-2371` 的前缀匹配(非精确,前缀子串):`MySQL_Proxy_Layer`→`tr_init_mysql_proxy_layer`(2314)、`Cluster`→`tr_init_cluster`(2319)、`CTablet`→`tr_init_cluster_tablet`(2325)、`Master_Host`→`tr_init_database_by_group(...,0)`(2331)、`Slave_Host`→`tr_init_database_by_group(...,1)`(2338)、`DB_User`→`tr_init_db_user`(2344)、`Product_User`→`init_product_user`(2350)、`Auth_IP`→`init_auth_ip`(2357)、`Ignore_IP`→`init_ignore_ip`(2363)。Rust 里用 `section.starts_with("Cluster")` 等。
4. `[MySQL_Proxy_Layer]` key 解析对齐 `tr_config.c:1670` 的 `tr_init_mysql_proxy_layer`:`port`(1691,校验 `is_port_valid`)、`mng_port`(1699)、`max_threads`(1710,校验 `>0 && <=128`)、`log_level`(1817)、`client_timeout`→`front_idle_timeout`(1764)、`server_timeout`→`backend_idle_timeout`(1796)、`conn_pool_socket_max_serve_client_times`、`stream_transport_enable`→`stream_on`(1913)、`default_charset`(1871)、`max_sql_size`(1844)、`max_query_size`(1836)。
5. `Master_Host`/`Slave_Host` 段解析对齐 `tr_config.c:1500`:`host`(1540)、`port`(1532)、`max_conn_pool_size`(1549)、`max_connections`(1551)、`connect_timeout`(1579)、`weight`(1594)、`cluster_tablet_name`(1600)。`DB_User` 段对齐 `tr_config.c:1104`:`db_username`、`db_password`、`default_db`、`cluster_name`。`Product_User` 段对齐 `tr_config.c:336`:`username`、`password`、`db_username`、`max_connections`、`cluster_name`。
6. **前端密码预计算 scramble**:对齐 `tr_config.c:435-445`/`1170-1173`,加载 product_user 时用固定 20 字节 scramble `"\x2f\x55\x3e\x74\x50\x72\x6d\x4b\x56\x4c\x57\x54\x7c\x34\x2f\x2e\x37\x6b\x37\x6n"` 调 `scramble()`(T1.2 实现)存入 `user.scramble_password[1..21]`,`scramble_len=21`,`scramble_password[0]=0x14`。**最小版可暂存明文密码**,scramble 计算放 T1.2。
7. 默认值对齐 `tr_config.c:2095` 的 `tr_default_config`:`port=8888`(2141)、`mng_port=9111`(2142)、`max_threads=2`(2143)。注意 `newproxy.conf` 实际覆盖 port=4051、max_threads=1。
8. 错误处理:section 缺失关键字段(如 `Master_Host` 缺 `host`)返回 `Err(ConfigError::MissingKey { section, key })`,带行号提示。非法端口/线程数返回 `Err(ConfigError::Invalid(...))`。
9. `AppCtx::load` 返回 `Arc<AppCtx>`,其中 `config: Arc<ArcSwap<Config>>`(为 02 §9 reload 预留,首版不调用 store)。

**关键代码骨架/要点**:
```rust
// src/config/mod.rs
use serde::Deserialize;
use std::collections::HashMap;
use arc_swap::ArcSwap;

pub struct AppCtx {
    pub config: Arc<ArcSwap<Config>>,
    pub srv_pool: Arc<crate::pool::SrvPool>,   // T2.2 填充,先占位
    pub parser_pool: Arc<crate::ffi::ParserPool>, // T0.2 填充,先占位
}

#[derive(Clone, Deserialize)]
pub struct Config {
    pub port: u16,
    pub mng_port: u16,
    pub max_threads: usize,
    pub log_dir: String,
    pub log_level: u8,
    pub front_idle_timeout: u64,   // secs
    pub backend_idle_timeout: u64,
    pub conn_pool_socket_max_serve_client_times: u64,
    pub max_sql_size: usize,
    pub stream_on: u8,
    pub default_charset: u8,
    pub clusters: HashMap<String, Cluster>,
    pub cluster_tablets: HashMap<String, ClusterTablet>,
    pub db_users: HashMap<String, DbUser>,
    pub product_users: HashMap<String, ProductUser>,
    pub auth_ips: Vec<String>,
    // ... 其余字段
}

impl AppCtx {
    pub async fn load(path: &str) -> Result<Arc<Self>, ConfigError> {
        let cfg = parse_newproxy_conf(path)?;     // rust-ini + 前缀分发
        Ok(Arc::new(AppCtx {
            config: Arc::new(ArcSwap::from_pointee(cfg)),
            srv_pool: Arc::new(crate::pool::SrvPool::default()),
            parser_pool: Arc::new(crate::ffi::ParserPool::new(4)),
        }))
    }
}
```
新增依赖:`rust-ini = "0.20"`、`arc-swap = "1"`。

**验收操作**:
- `cargo test --test config_parse` 通过:加载 `conf/newproxy.conf`,断言 `port==4051`、`max_threads==1`、`clusters` 含 `DBA_C_demo`、`product_users` 含 `dba`、`auth_ips` 含 `127.0.0.1`。
- `cargo run --bin newproxy -- --check-conf conf/newproxy.conf` 打印解析后的结构化配置(或报错)。
- 故意删掉 `Master_Host_1` 的 `host` 行,运行报错 `MissingKey { section: "Master_Host_1", key: "host" }`。
- 对照 C 版:`./build/output/newproxy/bin/newproxy -c conf/newproxy.conf`(若 C 版有 dump 配置的开关)输出字段一致。

**常见坑/Rust 特有注意**:
- GKeyFile 的 section 是**前缀匹配**不是精确匹配:`[Cluster_1]` 和 `[Cluster_2]` 都匹配 `Cluster` 前缀;`[Master_Host_1]` 匹配 `Master_Host`。不要用精确 `==`。
- `ip` / `ip_0`..`ip_3` 都匹配 `ip` 前缀(C 侧 `init_auth_ip` `tr_config.c:134`)。
- 配置里有 key 但 C 侧**无解析分支**(如 `timeout_check_interval`、`socket_left_rate`、`up_limit_active`),C 侧静默忽略;Rust 版应同样忽略(不报错),用 `#[allow(dead_code)]` 或日志 warn。
- `route_*.conf` / `hash_*.conf` 是 JSON 不是 INI,由 `init_table_route_info`(`tr_config.c:1038`)/`init_table_hash_info`(`tr_config.c:957`)解析——P0 最小版**跳过**,留到 P3.3。
- C 侧 `tr_get_file_md5sum`(`tr_config.c:2454`)用 `popen("md5sum")`;Rust 版用 `md5` crate 算,不要 shell out。
- `ArcSwap` 读侧 `config.load()` 返回 `Arc<Config>` 无锁;首版不写 store,但读侧已为 reload 预留(02 §9)。

---

## P1 —— MySQL 协议层

> 参考 `/docs/04-mysql-protocol-statemachine.md`。所有协议事实已逐行核实,见各卡引用。

### T1.1 packet 帧编解码

**目标**:实现 `read_packet<R: AsyncRead>` / `write_packet<W: AsyncWrite>`,自动跨多次 read 拼帧、处理 partial read、维护 packet_id 序号;处理 16MB 分包语义(C 侧只做拒绝不做重组,需对齐)。

**前置依赖**:T0.1。

**实施步骤**:
1. 在 `src/proto/packet.rs` 定义常量(对齐 `tr_packet.h:4-6`):`HEADER_LEN: usize = 4`、`PACKET_LEN_MAX: usize = 0x00ffffff`、`PACKET_LEN_UNSET: u32 = 0xffffffff`。
2. 实现 `read_packet`:对应 C 侧 `tr_conn.c:971` 的 `read_packet` + `tr_conn.c:509` 的 `real_read`。用 `BytesMut` + `AsyncReadExt::read_buf` 自动处理 partial read(替代 C 侧 `header_read_len`/`packet_read_len` 两个偏移变量,见 `tr_conn.h:169-173`)。header 解析:`len = buf[0] as usize | (buf[1] as usize)<<8 | (buf[2] as usize)<<16`(对齐 `tr_conn.c:993`),`seq = buf[3]`(`tr_conn.c:997`)。
3. **16MB 分包语义**:核实发现 C 侧**不做** 0xFFFFFF 重组,只在 `tr_conn.c:998-1002` 拒绝 `packet_len > PACKET_LEN_MAX`。04 设计文档 §2 写的"payload==0xFFFFFF 递归合并"是理想化描述——首版**对齐 C 侧行为**:payload 恰为 0xFFFFFF 时不主动重组,逐帧透传(大结果集走 T1.5 流式,每帧独立转发)。在 `read_packet` 文档注释里写明这一行为差异,留 TODO 给后续按需重组。
4. 实现 `write_packet`:对应 C 侧 `network_queue_send_append`(`tr_packet.c:757`),编码 header:`header[0..3] = len.to_le_bytes()[0..3]`(实际是 `(len>>0)&0xff, (len>>8)&0xff, (len>>16)&0xff`,见 `tr_packet.c:769-772`)、`header[3] = seq`。用 `AsyncWriteExt::write_all`。
5. 实现 `PacketReader<R>` / `PacketWriter<W>` 封装,持 `stream: R` + `buf: BytesMut` + `seq: u8`(seq 按方向维护,见 `tr_conn.h:171` 的 `packet_id`)。`read_packet` 返回 `(seq, Bytes)`,`write_packet(payload, seq)` 自增 seq。
6. 实现 `read_lenenc_int` / `write_lenenc_int`:对应 `tr_packet.h:477` 的 `protocol_decode_len` 宏(`<251` 1 字节;251=NULL;252→2 字节;253→3 字节;254→8 字节)。实现 `read_null_string`(读到 0x00)。
7. 写单元测试 `tests/packet_io.rs`:用 `tokio::io::duplex` 构造 (writer, reader),写一个 payload 读回;测 partial read(把 payload 切成多个 chunk 喂给 reader);测 0xFFFFFF 边界(不重组,逐帧读);测 seq 递增。

**关键代码骨架/要点**:
```rust
// src/proto/packet.rs(对齐 04-mysql-protocol-statemachine.md §2)
use bytes::{BytesMut, Bytes, Buf, BufMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const HEADER_LEN: usize = 4;
pub const PACKET_LEN_MAX: usize = 0x00ff_ffff;

pub struct PacketReader<R> {
    r: R,
    buf: BytesMut,
}
pub struct PacketWriter<W> {
    w: W,
    seq: u8,   // 按方向维护,对应 tr_conn.h:171
}

impl<R: AsyncRead + Unpin> PacketReader<R> {
    pub async fn read_packet(&mut self) -> std::io::Result<(u8, Bytes)> {
        // 1. 读 4 字节 header(对齐 tr_conn.c:980-1003)
        while self.buf.len() < HEADER_LEN {
            let n = self.r.read_buf(&mut self.buf).await?;
            if n == 0 { return Err(std::io::ErrorKind::UnexpectedEof.into()); }
        }
        let len = (self.buf[0] as usize) | ((self.buf[1] as usize) << 8) | ((self.buf[2] as usize) << 16);
        let seq = self.buf[3];
        self.buf.advance(HEADER_LEN);
        if len > PACKET_LEN_MAX { return Err(...); }   // 对齐 tr_conn.c:998-1002
        // 2. 读 len 字节 payload
        self.buf.reserve(len);
        while self.buf.len() < len {
            let n = self.r.read_buf(&mut self.buf).await?;
            if n == 0 { return Err(std::io::ErrorKind::UnexpectedEof.into()); }
        }
        let payload = self.buf.split_to(len).freeze();
        Ok((seq, payload))
    }
}

impl<W: AsyncWrite + Unpin> PacketWriter<W> {
    pub async fn write_packet(&mut self, payload: &[u8], seq: u8) -> std::io::Result<()> {
        let mut hdr = [0u8; HEADER_LEN];
        hdr[0] = (payload.len() & 0xff) as u8;
        hdr[1] = ((payload.len() >> 8) & 0xff) as u8;
        hdr[2] = ((payload.len() >> 16) & 0xff) as u8;
        hdr[3] = seq;
        self.w.write_all(&hdr).await?;
        self.w.write_all(payload).await?;
        Ok(())
    }
}
```
对 >16MB 的 payload:MySQL 协议要求拆成多个 0xFFFFFF 帧 + 末尾帧;`write_packet` 若 `payload.len() > PACKET_LEN_MAX` 应循环拆分(首版可不实现,proxy 侧大结果集走 T1.5 逐帧流式,不会构造单帧 >16MB)。

**验收操作**:
- `cargo test --test packet_io` 通过:正常帧、partial read(切 1/3/7 字节 chunk)、seq 递增、lenenc int 编解码。
- `cargo test --test packet_io -- --nocapture` 看 hex dump 对照 MySQL 协议。
- (可选)写一个 echo server `examples/echo_packet.rs`,用 `mysql` CLI 连上发 `SELECT 1`,看能正确读出 COM_QUERY 帧。

**常见坑/Rust 特有注意**:
- `read_buf` 在 `BytesMut` 上会自动扩容,但需先 `reserve` 否则可能 0 长度读;C 侧靠 `tr_byte_array_t` realloc(`tr_byte_array.c`)。
- `buf.advance(HEADER_LEN)` 会移动内部指针,不释放底层内存;长期跑需周期 `buf.clear()` 或 `buf = BytesMut::new()` 防 RSS 增长。
- seq 是 `u8` 会回绕(0xFF→0x00),MySQL 协议允许;不要用 `u32`。
- **不要**在 `read_packet` 里跨 `.await` 持有 `&mut self.buf` 借用同时做别的——`read_buf` 的借用会在 await 点结束,没问题;但若 future 被挂起,buf 不能被别的逻辑碰。
- 16MB 分包:若后续要支持重组,递归 `read_packet` 合并 payload 即可,但注意深递归(>1 次分片极少见)。
- `Bytes` vs `BytesMut`:`read_packet` 返回 `Bytes`(freeze,不可变,零拷贝转发);不要返回 `BytesMut` 导致后续误改。

---

### T1.2 前端握手 + auth

**目标**:实现前端握手状态机——发 server handshake v10、解析 client auth response、校验 `mysql_native_password` scramble;前端从 `Accepted` 走到 `CommandLoop`。

**前置依赖**:T1.1。

**实施步骤**:
1. 在 `src/proto/com.rs` 定义 capability flag 常量(对齐 `tr_packet_com.h:37-61`):`CLIENT_LONG_PASSWORD=0x0001`、`CLIENT_FOUND_ROWS=0x0002`、`CLIENT_CONNECT_WITH_DB=0x0008`、`CLIENT_IGNORE_SPACE=0x0100`、`CLIENT_PROTOCOL_41=0x0200`、`CLIENT_TRANSACTIONS=0x2000`、`CLIENT_SECURE_CONNECTION=0x8000`、`CLIENT_MULTI_STATEMENTS=0x00010000`、`CLIENT_MULTI_RESULTS=0x00020000`、`CLIENT_PLUGIN_AUTH=0x00080000`、`CLIENT_DEPRECATE_EOF=0x01000000`(首版**不启用**,pre-8.0 EOF 风格)。
2. 在 `src/proto/handshake.rs` 实现 `build_handshake`:对应 C 侧 `sent_handshake`(`tr_front_auth.c:170`),handshake 结构对齐 `tr_conn.h:4-12` 的 `handshake_packet`(`thread_id`/`capability_flags`/`server_status`/`scramble[21]`/`charset`)。**关键发现**:C 侧 scramble 是**固定常量**不是随机的——`tr_front_auth.c:186-203` 的 `packet_handshake[]` 字面量里 part1=`\x2f\x55\x3e\x74\x50\x72\x6d\x4b`、part2=`\x56\x4c\x57\x54\x7c\x34\x2f\x2e\x37\x6b\x37\x6e`。首版**对齐 C 侧用固定 scramble**(便于与 C 版字节级对照);后续可改随机(T5.2 需重新算预期响应)。server capability lower=`\x0c\xa2`、upper=`\x08\x00`、charset=`0x1c`(gbk,可配置 `default_charset`)、status=`0x0002`(AUTOCOMMIT)。
3. 实现 `parse_client_auth`:对应 C 侧 `auth_read`(`tr_front_auth.c:244`)。解析 4 字节 capabilities(低 2 + 高 2,`tr_front_auth.c:266`)、4 字节 max_packet、1 字节 charset、23 字节 reserved、null-terminated username(`tr_front_auth.c:329` 的 `protocol_get_string`)、scramble(`CLIENT_SECURE_CONNECTION` 时读 1 字节 len + 20 字节,见 `tr_front_auth.c:517-528`)、可选 db(`CLIENT_CONNECT_WITH_DB`)、auth_plugin_name(校验 == `mysql_native_password`,`tr_front_auth.c:587-588`)。
4. 实现 scramble 校验:对应 `tr_front_auth.c:516-547`。C 侧**直接字节比较** client 发来的 20 字节 vs 配置预存的 `user.scramble_password[1..21]`(T0.3 步骤 6 预计算)。空密码路径:`scramble_len==0 && user.scramble_len==1`(`tr_front_auth.c:520`)。Rust 版用 `constant_time_eq` 防时序攻击(C 侧 `memcmp` 有时序泄漏,这是 Rust 改进点,但注意会改变行为,需 T5.2 记录)。
5. 实现 `scramble_password(password, scramble)`(供 T0.3 预计算 + T1.3 后端用):对应 C 侧 `tr_password.c:243` 的 `scramble()`。算法:`stage1=SHA1(pw)`、`stage2=SHA1(stage1)`、`token=SHA1(scramble||stage2) XOR stage1`。用 `sha1` crate 替代手写 `tr_password.c`/`tr_md5.c`。
6. 实现 `build_err_1045`:对应 `fill_auth_failed_packet`(`tr_packet.c:300`,声明 `tr_packet.h:471`)。ERR 包头 `\xff\x15\x04#28000`(`tr_packet.c:309`):`\xff`=ERR 标记、`\x15\x04`=LE errno 1045、`#28000`=SQLSTATE;消息按 `is_check_ip` 选(`tr_front_auth.c:52-92`):密码错 "Access denied for user..."、IP 错。seq=2(`tr_front_auth.c:232` 发 OK 时 `packet_id=2`,ERR 同)。
7. 在 `src/conn/front.rs` 的 `FrontConn::drive()` 里接入 `FrontState::Accepted`→发 handshake→`HandshakeSent`→读 auth→校验→发 OK(`\x00\x00\x00\x02\x00\x00\x00`,seq=2,见 `tr_front_auth.c:232` / `tr_packet.c:528` 的 `fill_ok_packet`)或 ERR→`CommandLoop`。状态枚举对齐 `tr_conn.h:38-55` 的 `enum connection_state`。
8. 写测试:起一个 `TcpListener`,用 `mysql` crate(或手写 raw client)连上,发正确/错误密码,断言握手包字节 + OK/ERR 响应。

**关键代码骨架/要点**:
```rust
// src/proto/handshake.rs
use bytes::{BytesMut, BufMut};

// 固定 scramble,对齐 tr_front_auth.c:189-202(C 侧非随机)
pub const FIXED_SCRAMBLE: [u8; 20] = [
    0x2f,0x55,0x3e,0x74,0x50,0x72,0x6d,0x4b, // part1
    0x56,0x4c,0x57,0x54,0x7c,0x34,0x2f,0x2e,0x37,0x6b,0x37,0x6e, // part2
];

pub fn build_handshake(thread_id: u32, charset: u8) -> Bytes {
    let mut b = BytesMut::new();
    b.put_u8(10);                          // protocol version
    b.put_slice(b"5.5.39b\0");             // server version (对齐 tr_front_auth.c:187)
    b.put_u32_le(thread_id);               // connection id
    b.put_slice(&FIXED_SCRAMBLE[0..8]);    // auth-plugin-data part1
    b.put_u8(0);                           // filler
    b.put_u16_le(0xa20c);                  // capability lower (对齐 \x0c\xa2)
    b.put_u8(charset);                     // 0x1c gbk 默认
    b.put_u16_le(0x0002);                  // server status AUTOCOMMIT
    b.put_u16_le(0x0008);                  // capability upper
    b.put_u8(21);                          // auth-plugin-data len
    b.put_slice(&[0u8; 10]);               // reserved
    b.put_slice(&FIXED_SCRAMBLE[8..20]);   // auth-plugin-data part2 (+NUL)
    b.put_u8(0);                           // part2 的 NUL 终止
    b.put_slice(b"mysql_native_password\0");
    b.freeze()
}

pub fn scramble_password(password: &str, scramble: &[u8; 20]) -> [u8; 20] {
    use sha1::{Sha1, Digest};
    let stage1 = Sha1::digest(password.as_bytes());
    let stage2 = Sha1::digest(&stage1);
    let mut h = Sha1::new();
    h.update(scramble);
    h.update(&stage2);
    let token = h.finalize();
    let mut out = [0u8; 20];
    for i in 0..20 { out[i] = stage1[i] ^ token[i]; }
    out
}
```

**验收操作**:
- `mysql -h 127.0.0.1 -P <port> -u <user> -p<正确密码>` 连上(代理后端未接,握手后命令会失败,但握手+auth 成功即可)。
- `mysql -u <user> -p<错误密码>` 返回 `ERROR 1045 (28000): Access denied`。
- `cargo test --test front_auth` 通过:正确密码得 OK(seq=2)、错误密码得 ERR 1045。
- 用 `tcpdump`/`wireshark` 抓包对照 C 版 handshake 字节(尤其 scramble 与 capability)。

**常见坑/Rust 特有注意**:
- **固定 scramble 的安全权衡**:C 侧固定 scramble 使所有连接用同一 challenge,易受重放;Rust 版若改随机 scramble,需在 `auth_read` 时实时算 `scramble_password(user.password, &random_scramble)` 而非用预存值——这会改变行为,T5.2 字节级对照时会与 C 版不同,需记录为"允许差异"。
- `CLIENT_SECURE_CONNECTION` 决定 scramble 编码方式(1 字节 len + 数据 vs 直接 20 字节);C 侧只支持 secure connection 路径(`tr_front_auth.c:517`),Rust 版同。
- auth_plugin_name 校验:C 侧要求 == `mysql_native_password`(`tr_front_auth.c:587`);若客户端发 `caching_sha2_password` 要拒绝(返回 ERR 或要求 auth_switch,但 C 侧无 auth_switch,直接拒)。
- `constant_time_eq` 改变时序行为:线上若有时序探测监控会报警;首版可用普通 `==`,T5.2 再决定。
- OK 包 `\x00\x00\x00\x02\x00\x00\x00` 是 7 字节 payload,header 是 4 字节,共 11 字节;不要漏 header。
- thread_id 要全局唯一递增(`ctx.next_thread_id()`,用 `AtomicU32`),对应 `tr_conn.h:7` 的 `thread_id`。

---

### T1.3 后端握手 + auth

**目标**:proxy 作为客户端连真实 MySQL 后端,收 server handshake、发 client auth response(含 `mysql_native_password` scramble),校验 auth 结果;后端从 `Connected` 走到 `CommandLoop`。

**前置依赖**:T1.1(用 T1.2 的 `scramble_password`)。

**实施步骤**:
1. 在 `src/conn/back.rs` 实现 `BackConn::connect`:对应 C 侧 `tr_handle_back_recurse_connect`(`tr_back_connect.c:58`)。用 `tokio::net::TcpStream::connect((host, port))` 替代 `connect()`(`tr_back_connect.c:152`);`connect_timeout` 用 `tokio::time::timeout` 包裹(对齐 `tr_back_connect.c:162` 的 `db->connect_timeout`)。
2. 实现后端握手接收 `read_server_handshake`:对应 C 侧 `handshake_read`(`tr_back_auth.c:157`)。解析 protocol_version、server_version(null-terminated)、thread_id、scramble part1(8 字节,`tr_back_auth.c:238` 的 `memcpy(handshake->scramble, packet+off, 8)`)、filler、capability lower、charset、server_status、capability upper、scramble_len、reserved(10 字节)、scramble part2(12 字节,`tr_back_auth.c:251` 的 `memcpy(handshake->scramble+8, packet+off, 12)`)、`handshake->scramble[20]='\0'`(`tr_back_auth.c:254`)。
3. 实现 `build_client_auth_response`:对应 C 侧 `auth_send`(`tr_back_auth.c:259`)。client flags=`{0x8d, 0xa2, 0x02, 0}`(`tr_back_auth.c:282`)、max_packet_size、charset、23 字节 filler、login_username(null-terminated)、scramble:空密码发单 `\x00`(`tr_back_auth.c:330`),否则发 `\x14` + 20 字节 `scramble_password(db_user.password, &server_handshake.scramble)`(`tr_back_auth.c:335-341`,调 T1.2 的 `scramble_password`)。seq=1(`tr_back_auth.c:365`)。
4. 实现 auth 结果读取 `read_auth_result`:对应 C 侧 `auth_result_read`(`tr_back_auth.c:379`)。首字节 `type`:==0x00 成功(OK 包);==0xff 失败(`tr_back_auth.c:399`),解析 errno + SQLSTATE + message,返回 `Err`。
5. 在 `BackConn::drive()` 里接入状态机(对齐 `tr_conn.h:47-54` 的 `STATE_BACKEND_*`):`Connected`→`read_server_handshake`→`AuthSend`→`build_client_auth_response`→`AuthResultRead`→`read_auth_result`→`CommandLoop`。对应 `tr_handle_back_auth`(`tr_back_auth.c:42`)。
6. 失败处理:对应 `tr_back_auth.c:124-136`,auth 失败日志 `"newproxy login db use username=%s failed"` + 关连接(C 侧 `server_free(c, 1)`)。Rust 版返回 `Err(BackAuthError)` 由上层关 stream。
7. 写测试:起一个本地 `mysqld`(或用 `mysql_async` 的 mock),配置 db_user 指向它,代理连上并 auth 成功;故意配错密码断言 auth 失败。

**关键代码骨架/要点**:
```rust
// src/conn/back.rs(对齐 tr_back_auth.c)
use crate::proto::handshake::scramble_password;

pub struct BackConn {
    pub reader: PacketReader<tokio::net::TcpStream>,
    pub writer: PacketWriter<tokio::net::TcpStream>,
    pub handshake: ServerHandshake,   // 对齐 tr_conn.h:213 的 handshake_packet
    pub state: BackState,
    pub db: Arc<Database>,
    pub ct: Arc<ClusterTablet>,
    pub ms: MasterSlave,
    pub in_pool: AtomicBool,          // T2.2 用
    pub served_times: AtomicU64,      // T2.2 用
    pub enqueue_pool_time: AtomicI64, // T2.2 用
}

struct ServerHandshake {
    thread_id: u32,
    capabilities: u32,
    charset: u8,
    server_status: u16,
    scramble: [u8; 21],   // 20 + NUL,对齐 tr_conn.h:10
}

impl BackConn {
    pub async fn connect(db: &Database, d_user: &DbUser) -> Result<Self> {
        let stream = tokio::time::timeout(
            Duration::from_secs(db.connect_timeout),
            tokio::net::TcpStream::connect((db.host.as_str(), db.port)),
        ).await??;
        // split into reader/writer...
        let mut back = BackConn { /* ... */ };
        back.read_server_handshake().await?;      // tr_back_auth.c:157
        back.send_auth_response(&d_user).await?;  // tr_back_auth.c:259
        back.read_auth_result().await?;           // tr_back_auth.c:379
        Ok(back)
    }

    async fn send_auth_response(&mut self, d_user: &DbUser) -> Result<()> {
        let mut b = BytesMut::new();
        b.put_u32_le(0x0002a28d);            // client flags 对齐 tr_back_auth.c:282
        b.put_u32_le(0x01000000);            // max packet size
        b.put_u8(self.handshake.charset);
        b.put_slice(&[0u8; 23]);             // filler
        b.put_slice(d_user.login_username.as_bytes());
        b.put_u8(0);                         // null-term
        if d_user.password.is_empty() {
            b.put_u8(0);                     // tr_back_auth.c:330
        } else {
            b.put_u8(0x14);                  // len=20
            let s = scramble_password(&d_user.password, &self.handshake.scramble[..20].try_into().unwrap());
            b.put_slice(&s);
        }
        self.writer.write_packet(&b, 1).await?;   // seq=1, tr_back_auth.c:365
        Ok(())
    }
}
```

**验收操作**:
- 单元测试:配本地 `mysqld` 的 dbuser,`cargo test --test back_auth` 断言连接+auth 成功(`state==CommandLoop`)。
- 错误密码:断言 `Err(BackAuthError { code: 1045, .. })`。
- `mysql -h <proxy> -u <product_user> -p<pw> -e "SELECT 1"`(代理后端已通)握手阶段不报错(此时命令透传还未实现,会卡在 CommandLoop,但 auth 链路通)。

**常见坑/Rust 特有注意**:
- 后端 scramble 是**动态的**(每个 MySQL server 握手发不同 challenge),与前端固定 scramble 不同;必须用后端 handshake 里的 scramble 算响应。
- `TcpStream::connect` 默认阻塞?不——tokio 的 `TcpStream::connect` 是 async;但 `connect_timeout` 必须用 `tokio::time::timeout` 包,否则后端宕机会挂死。
- client flags `0x0002a28d`:对照 C 侧 `{0x8d, 0xa2, 0x02, 0}`(LE),即 `0x0002a28d`。注意高低字节。
- `read_server_handshake` 解析 server_version 是 null-terminated 字符串,长度可变;不要写死偏移,要用 `read_null_string`。
- MySQL 8.0 默认 `caching_sha2_password`:后端若用 8.0 默认账号会拒绝 `mysql_native_password`。需在 mysqld 配 `default_authentication_plugin=mysql_native_password` 或建专用账号。
- `Drop for BackConn` 不能 await:stream 关闭用 `tokio::net::TcpStream` 的 Drop(同步关 fd)即可;异步 reset/归还放 T2.3 的 `finalize_conn`。

---

### T1.7 caching_sha2_password + AuthSwitch(官方规范,必做)

**目标**:实现 MySQL 8.0+ 默认认证插件 `caching_sha2_password` 的 SHA256 scramble + fast-auth 三路径,以及 `AuthSwitchRequest` 插件协商。这是连接 MySQL 8 默认账号的前提——C 版未实现,直接连默认账号会失败。

**前置依赖**:T1.2、T1.3(mysql_native_password 链路跑通,作为对照基线)。

**实施步骤**:
1. 在 `src/proto/auth/` 新增 `caching_sha2.rs`:实现 `sha2_scramble(password, nonce) -> [u8;32]`,算法为 `XOR(SHA256(pw), SHA256(SHA256(SHA256(pw)) | nonce))`(对应官方 `caching_sha2_password` 定义)。用 `sha2` crate,非手写。
2. 扩展 `FrontState`/`BackState`:在 `AuthRead` 后增加 `AuthMoreData` 处理分支——server 发 `AuthMoreData`(首字节 0x01)时,读首字节 fast-auth 标识:0x03=缓存命中(直接等 OK),0x04=需完整认证。
3. 实现完整认证的两条子路径:(a) 若 client/backend 已 TLS → 明文密码走加密通道(首版无 TLS 时跳过);(b) 否则用 server RSA 公钥(`AuthMoreData` 0x01 携带)RSA-OAEP 加密明文密码。`rsa` crate,首版可只做 (a) 的占位 + (b) 的 RSA。
4. 在 `src/proto/auth/auth_switch.rs` 实现 `handle_auth_switch(pkt)`:解析 `AuthSwitchRequest`(插件名 + 新 nonce),按插件名分发到 `scramble_password`(native)或 `sha2_scramble`(sha2),重发 response。
5. 能力位协商:handshake 阶段若 client 宣告 `CLIENT_PLUGIN_AUTH`,server(handshake)按账号真实插件返回;若两端插件不匹配,走 AuthSwitch。
6. 单测:`sha2_scramble` 对照已知向量(官方文档/MySQL 客户端抓包);`handle_auth_switch` 用模拟 AuthSwitchRequest 包验证插件切换。

**关键代码骨架/要点**:
```rust
// src/proto/auth/caching_sha2.rs
use sha2::{Sha256, Digest};
pub fn sha2_scramble(password: &str, nonce: &[u8; 20]) -> [u8; 32] {
    let s1 = Sha256::digest(password.as_bytes());          // SHA256(pw)
    let s2 = Sha256::digest(&s1);                          // SHA256(SHA256(pw))
    let mut h = Sha256::new(); h.update(&s2); h.update(nonce);
    let s3 = h.finalize();                                 // SHA256(s2 | nonce)
    let mut out = [0u8; 32];
    for i in 0..32 { out[i] = s1[i] ^ s3[i]; }
    out
}

// fast-auth 状态分支
enum Sha2FastAuth { CacheHit /* 0x03 */, Full /* 0x04 */ }
// AuthMoreData 首字节:0x01=server RSA 公钥,0x03=fast ok,0x04=full auth
```

**验收操作**:
- 起一个 MySQL 8 实例,**不**把账号改回 `mysql_native_password`(保持默认 `caching_sha2_password`)。
- 用 `mysql -u <sha2账号> -p` 连 Rust 版代理,验证 fast-auth 缓存命中路径(第二次连接起 server 缓存命中,0x03)通过。
- 首次登录(无缓存,0x04)+ 无 TLS:验证 RSA 路径(若实现)或明确报"需 TLS/RSA"增量项。
- 构造插件不匹配(client 宣告 native、server 要求 sha2):验证 `handle_auth_switch` 切换成功。
- 抓包对照官方 `caching_sha2_password` 字节流(wireshark 解析 MySQL 协议)。

**常见坑/Rust 特有注意**:
- **nonce 长度**:caching_sha2 用 20 字节 nonce(与 native 同),但 scramble 是 32 字节(SHA256),别和 native 的 20 字节混淆。
- **fast-auth 状态机**:AuthMoreData 是多包交互,状态机要能处理"发了 scramble 后先收 AuthMoreData 再收 OK"的序列,不能假设一收一发。
- **RSA-OAEP**:MySQL 用的是 RSA-OAEP(SHA1 padding 部分有特定实现),`rsa` crate 的 OAEP 默认参数需对照 MySQL 源码 `sha2_password.cc` 调整,否则后端解密失败。
- **AuthSwitch 后 seq 对齐**:切换插件后 packet seq 重置,别沿用原 seq 导致 server 拒包。
- **首版范围**:若 RSA 路径成本高,首版可只实现"缓存命中 + TLS 明文"两条,无 TLS 首登场景明确报错并列入增量;但绝不能让默认账号完全连不上。

---

### T1.4 命令分发 + 透传

**目标**:**M1 里程碑**——实现 `Command` enum + `dispatch`,COM_QUERY/PING/QUIT/INIT_DB 透传到后端并回传结果,端到端 `SELECT 1` 经代理返回正确结果。

**前置依赖**:T1.2、T1.3。

**实施步骤**:
1. 在 `src/proto/command.rs` 定义 `Command` enum(`#[repr(u8)]`),对齐 `tr_packet_com.h:4-35` 的 `COM_*` 定义:`Quit=0x01`、`InitDb=0x02`、`Query=0x03`、`FieldList=0x04`、`Ping=0x0e`、`StmtPrepare=0x16`、`StmtExecute=0x17`、`StmtSendLongData=0x18`、`StmtClose=0x19`、`StmtReset=0x1a`、`SetOption=0x1b`、`StmtFetch=0x1c`。实现 `Command::from_u8(b: u8) -> Option<Command>`,白名单对齐 `is_valid_command`(`tr_packet.c:2593-2599`)——`Sleep/Connect/Time/DelayedInsert/BinlogDump/ResetConnection` 等返回 None(拒绝)。
2. 实现 `FrontConn::read_command`:读一个 packet,首字节是 command byte,payload[1..] 是参数。对应 C 侧 `query_read`(`tr_front_cmd.c:409`),command 提取在 `tr_front_cmd.c:474`,`args_len = packet_len - 1`(`tr_front_cmd.c:524`)。
3. 实现 `FrontConn::dispatch(cmd, payload)`:对应 `tr_handle_front_command`(`tr_front_cmd.c:42`)的外层 + `query_read` 的本地处理分支:
   - `Command::Ping` → 本地 `send_ok()`(对齐 `tr_front_cmd.c:514` 的 `fill_ok_packet`)。
   - `Command::Quit` → 关前端连接(对齐 `tr_front_cmd.c:493` 的 `RET_COMMAND_SHUTDOWN`)。
   - `Command::SetOption` → 本地处理 multi-query 开关(对齐 `tr_front_cmd.c:497`,改 packet 字节)。
   - `Command::InitDb` → 更新 `current_db` 并透传(对齐 `tr_front_cmd.c:724-746`)。
   - `Command::Query`/`StmtPrepare` → **P1 阶段先透传不解析**(对齐 `tr_front_cmd.c:820` 的解析门,但 P1 不接 parser,P3 再加解析+路由);透传 = 把原 packet 发后端。
   - 其他 → `forward_raw`(透传)。
4. 实现透传:`acquire_backend`(P1 阶段临时直接 `BackConn::connect`,P2 换池)、`backend.writer.write_packet(raw_packet, seq)`、读后端结果转发回前端。对应 `tr_handle_backend_command`(`tr_back_cmd.c:28`)+ `query_result_read`(`tr_back_cmd.c:262`)+ `query_result_send`(`tr_front_cmd.c:193`)。seq 管理:发后端时 `server.packet_id = client.packet_id`(`tr_back_cmd.c:2028`),后端结果透传时 seq 原样转发。
5. 实现结果转发最小版:读后端首包,判断 `type`:0x00=OK(直接转发)、0xff=ERR(转发)、0xFE=EOF(转发)、其他=结果集(走 T1.5 流式)。首版可先全量缓冲读再发(简化),T1.5 换流式。
6. 在 `conn_task`(`src/conn/mod.rs`)里串起来:`accept`→`front.drive()` 握手→`CommandLoop`→`read_command`→`dispatch`→透传→回结果。对应 `conn_task` 骨架(02 §3)。
7. 本地起 `mysqld` + 配 `newproxy.conf` 指向它,跑 `mysql -h <proxy> -e "SELECT 1"`、`SELECT * FROM t`、`SET @a=1`、`PING`。

**关键代码骨架/要点**:
```rust
// src/proto/command.rs
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Quit = 0x01, InitDb = 0x02, Query = 0x03, FieldList = 0x04,
    Ping = 0x0e,
    StmtPrepare = 0x16, StmtExecute = 0x17, StmtSendLongData = 0x18,
    StmtClose = 0x19, StmtReset = 0x1a, SetOption = 0x1b, StmtFetch = 0x1c,
}
impl Command {
    pub fn from_u8(b: u8) -> Option<Self> {  // 对齐 is_valid_command tr_packet.c:2593
        use Command::*;
        Some(match b {
            0x01 => Quit, 0x02 => InitDb, 0x03 => Query, 0x04 => FieldList,
            0x0e => Ping,
            0x16 => StmtPrepare, 0x17 => StmtExecute, 0x18 => StmtSendLongData,
            0x19 => StmtClose, 0x1a => StmtReset, 0x1b => SetOption, 0x1c => StmtFetch,
            _ => return None,
        })
    }
}
```
```rust
// src/conn/front.rs dispatch(对齐 tr_front_cmd.c:42 + query_read)
impl FrontConn {
    async fn dispatch(&mut self, cmd_byte: u8, payload: &[u8]) -> Result<()> {
        let cmd = Command::from_u8(cmd_byte).ok_or_else(|| {
            // 对齐 tr_front_cmd.c:484 "denied command"
            Error::DeniedCommand(cmd_byte)
        })?;
        match cmd {
            Command::Ping => { self.send_ok(0,0,0x0002).await?; }   // tr_front_cmd.c:514
            Command::Quit => { return Err(Error::ClientQuit); }
            Command::SetOption => { /* 本地改字节 + 透传 */ }
            Command::Query | Command::StmtPrepare => {
                // P1 透传不解析;P3 在此插入 parser_pool.parse + 路由 + 改写
                self.forward_to_backend(payload).await?;
            }
            _ => { self.forward_to_backend(payload).await?; }
        }
        Ok(())
    }

    async fn forward_to_backend(&mut self, payload: &[u8]) -> Result<()> {
        let mut back = self.acquire_backend().await?;   // P1 直接 connect,P2 换池
        // seq 对齐 tr_back_cmd.c:2028
        back.writer.write_packet(payload, self.client_seq).await?;
        self.proxy_result(&mut back).await?;   // T1.5 流式
        self.release_backend(back).await?;     // P1 直接 drop,P2 归还池
        Ok(())
    }
}
```
注意 `acquire_backend`/`release_backend` 在 P1 是占位(每次新建/关闭),P2.3 换成池。`conn_task` 退出前调 `finalize_conn`(02 §6)归还——P1 可先省略。

**验收操作**:
- **M1**:`mysql -h 127.0.0.1 -P 4051 -u dba -p<pw> -e "SELECT 1"` 经代理返回 `1`。
- `mysql ... -e "SELECT * FROM test LIMIT 5"` 返回正确行。
- `mysql ... -e "SET @a=1; SELECT @a"` 返回 1。
- `mysqladmin -h ... ping` 返回 `mysqld is alive`。
- `mysql_case/totalTest.sh` 的 select 子集(简单 SELECT)通过(对照 C 版)。
- `cargo test --test e2e_select` 通过(用 `mysql` crate 驱动)。

**常见坑/Rust 特有注意**:
- **seq 对齐**:client→proxy 读到的 seq 要原样发给后端(`tr_back_cmd.c:2028` `server->packet_id = client->packet_id`);后端→client 的结果 seq 原样透传。proxy 自己构造的包(OK/ERR)才自己管 seq。seq 不对会导致客户端 "Packets out of order"。
- COM_QUERY 的 SQL 字符串在 `payload[1..]`,**不**含 NUL 终止(MySQL 协议 payload 就是剩余字节);不要误加 `\0`。
- `forward_raw` 要把整个原 packet(含 command byte)发给后端,不要只发 payload[1..]。
- P1 透传不解析:不要在此阶段调 parser;P3 在 `Command::Query` 分支插入解析。
- `Command::from_u8` 返回 None 时拒绝:C 侧发 "denied command -_-||"(`tr_front_cmd.c:484`),Rust 版发 ERR 1045 或类似。
- `acquire_backend` 每次新建连接会很慢(P1 可接受);P2 换池后必须复用,否则 QPS 极低。

---

### T1.5 结果集流式转发

**目标**:实现 `proxy_result_set` 流式转发——从后端读一行发一行,不缓冲全量;正确处理 column count / 列定义 / 列 EOF / 行 / 行 EOF(0xFE)或 ERR(0xFF)终止。

**前置依赖**:T1.4。

**实施步骤**:
1. 在 `src/conn/front.rs` 实现 `proxy_result_set(&mut self, back: &mut BackConn)`:对应 `query_result_read`(`tr_back_cmd.c:262`)的 `STATE_READ_RESULT_*` 序列 + 04 §6.2 的流式骨架。
2. 步骤:(a) 读首包 = column count(lenenc int,首字节,对齐 `tr_back_cmd.c:318` 的 `STATE_READ_RESULT_BEGIN`),转发;(b) 读 N 个列定义包并转发(对齐 `STATE_READ_RESULT_HEADER/FIELDS`,`tr_back_cmd.c:720/755`);(c) 读列 EOF(0xFE)并转发(对齐 `STATE_READ_RESULT_FIELDS_EOF`,`tr_back_cmd.c:789`);(d) 循环读行包并转发,直到首字节 == 0xFE(EOF,`tr_back_cmd.c:811`)或 0xFF(ERR,`check_error_packet` `tr_packet.c:329`)。
3. 流式语义:每读一个 packet 立即 `self.writer.write_packet` 转发,不缓冲全量。对应 04 §6.2。C 侧靠 `stream_transport_enable`(`tr_config.c:1913`→`config->stream_on`,`tr_config.h:308`)配置 + `is_during_read_result`(`tr_back_cmd.c:841`)分块 flush;Rust async 天然流式,无需该配置。
4. ERR 检测:每个包检查首字节 `==0xff`,调 `check_error_packet`(`tr_packet.c:329` / `tr_packet.c:347`)语义,解析 errno + SQLSTATE + message,转发 ERR 包给客户端后终止。
5. multi-result 处理:`SERVER_MORE_RESULTS_EXISTS`(`tr_back_cmd.c:903`)——status flag 含此位时继续读下一个结果集。首版可先不处理(CLI 罕见),记 TODO。
6. 写压测:用 `SELECT * FROM big_table`(100w 行)经代理,监控 RSS 恒定(不随行数增长)。用 `cargo test --test streaming` + 一个本地大表。

**关键代码骨架/要点**:
```rust
// src/conn/front.rs(对齐 tr_back_cmd.c:262 + 04-mysql-protocol-statemachine.md §6.2)
impl FrontConn {
    pub async fn proxy_result_set(&mut self, back: &mut BackConn) -> Result<()> {
        // 首包:column count
        let (seq, header) = back.reader.read_packet().await?;
        match header.first() {
            Some(&0x00) => { self.writer.write_packet(&header, seq).await?; return Ok(()); } // OK 包
            Some(&0xff) => { self.writer.write_packet(&header, seq).await?; return Ok(()); } // ERR 包,转发
            Some(&0xfe) => { self.writer.write_packet(&header, seq).await?; return Ok(()); } // EOF(空结果集)
            _ => {}
        }
        let n_cols = read_lenenc_int(&header)? as usize;
        self.writer.write_packet(&header, seq).await?;
        // 列定义 + 列 EOF
        for _ in 0..n_cols {
            let (s, p) = back.reader.read_packet().await?;
            self.writer.write_packet(&p, s).await?;
        }
        let (s, eof) = back.reader.read_packet().await?;   // 列 EOF (0xFE)
        self.writer.write_packet(&eof, s).await?;
        // 行流式
        loop {
            let (s, row) = back.reader.read_packet().await?;
            let is_end = matches!(row.first(), Some(&0xfe) | Some(&0xff));
            self.writer.write_packet(&row, s).await?;
            if is_end { break; }
        }
        Ok(())
    }
}
```

**验收操作**:
- `mysql -e "SELECT * FROM one_million_rows"` 经代理返回全部行,RSS 恒定(用 `top -p <pid>` 观察,不随结果集大小线性增长)。
- `mysql -e "SELECT * FROM no_such_table"` 返回 ERR 1146。
- `mysql -e "SELECT 1"` 返回单行。
- 大结果集 >16MB:多帧 0xFFFFFF 不重组,逐帧转发正确(`SELECT REPEAT('x', 20*1024*1024)`)。
- `cargo test --test streaming` 通过(用 mock backend 喂分片结果集)。

**常见坑/Rust 特有注意**:
- **背压**:若客户端慢、后端快,`write_packet` 会 await(写缓冲满);这天然背压,`back.reader.read_packet` 不会无限读——但要注意 `self.writer` 与 `back.reader` 不能同时借用 `self`,需拆分 borrow(用 `&mut self.writer` 与 `&mut back.reader` 分开,或把 reader/writer 拆成独立字段)。
- 行 EOF(0xFE)与 ERR(0xFF)的判定:MySQL 协议里 0xFE 在 payload<9 字节时是 EOF,否则是行数据(行首字节也可能是 0xFE?实际行首 lenenc int <251 不会是 0xFE,所以安全)。C 侧 `tr_back_cmd.c:811` 直接判 `packet[off]==0xfe`。
- 16MB 分包:大行(如 BLOB)可能跨多个 0xFFFFFF 帧;流式逐帧转发即可,不重组(对齐 C 侧)。
- `SERVER_MORE_RESULTS_EXISTS`:若后端 status 含此位,EOF 包后还有结果集;首版忽略会导致多结果集客户端只读到第一个。记 TODO。
- 不要缓冲全量:`Vec::new()` 收集所有行再发会 OOM(100w 行场景);必须读一行发一行。
- 流式时 seq 管理:后端发来的 seq 原样转发给客户端,不要重编号(透传场景);proxy 自己构造的合并结果才重编号(T3.5)。

---

### T1.6 OK/ERR/EOF 包构造

**目标**:实现 `build_ok`/`build_error`/`build_eof`,字节级对照 C 版输出(用于 proxy 本地构造响应,如 PING/SET/错误)。

**前置依赖**:T1.1。

**实施步骤**:
1. 在 `src/proto/packet_build.rs` 实现 `build_ok(affected, last_id, status, warnings, msg, seq) -> Bytes`:对应 `make_ok_packet`(`tr_packet.c:571`)。布局:`0x00` + lenenc affected_rows + lenenc last_insert_id + 2 字节 LE server_status + 2 字节 LE warning_count + 可选 msg。lenenc 编码用 T1.1 的 `write_lenenc_int`。注意 C 侧固定 OK `\x00\x00\x00\x02\x00\x00\x00`(`tr_packet.c:528` 的 `fill_ok_packet`)是 affected=0/last_id=0/status=2/warnings=0/msg 空 的特例。
2. 实现 `build_error(code: u16, sql_state: &[u8;5], msg: &str, seq) -> Bytes`:对应 `fill_auth_failed_packet`(`tr_packet.c:300`)+ `check_error_packet`(`tr_packet.c:329`)。布局:`0xff` + 2 字节 LE errno + `#` + 5 字节 SQLSTATE + msg。对照 `tr_packet.c:309` 的 `\xff\x15\x04#28000`(errno 1045 = `0x0415` LE = `\x15\x04`,SQLSTATE `28000`)。
3. 实现 `build_eof(warnings: u16, status: u16, seq) -> Bytes`:对应 `make_eof_packet`(`tr_packet.c:814`)。布局:`0xfe` + 2 字节 LE warnings + 2 字节 LE status。对照 `tr_packet.c:814` 的 `\xfe\x00\x00\x02\x00`(warnings=0,status=2)。另有 `make_merge_eof_packet`(`tr_packet.c:840`)带自定义 status,合并场景用。
4. 实现 `build_result_set_header(n_cols: usize, seq) -> Bytes`:对应 `make_result_set_header_packet`(`tr_packet.c:786`),布局:lenenc n_cols。
5. 实现 `build_field_packet(field: &FieldDef, seq) -> Bytes`:对应 `make_full_field_packet`(`tr_packet.c:638`),列定义包(catalog/db/table/org_table/name/org_name/charset/len/type/flags/decimals)。
6. 写字节级对照测试:用 C 版构造同样的 OK/ERR/EOF,hexdump 对比。`cargo test --test packet_build` 断言字节相等。

**关键代码骨架/要点**:
```rust
// src/proto/packet_build.rs(对齐 tr_packet.c:528/571/300/329/814/840/786/638)
use bytes::{BytesMut, BufMut};

pub fn build_ok(affected: u64, last_id: u64, status: u16, warnings: u16, msg: &str, seq: u8) -> Bytes {
    let mut p = BytesMut::new();
    p.put_u8(0x00);
    write_lenenc_int(&mut p, affected);   // tr_packet.h:477 协议
    write_lenenc_int(&mut p, last_id);
    p.put_u16_le(status);                  // 对齐 make_ok_packet tr_packet.c:571
    p.put_u16_le(warnings);
    if !msg.is_empty() { p.put_slice(msg.as_bytes()); }
    // 外层 header 由 write_packet 加
    p.freeze()
}

pub fn build_error(code: u16, sql_state: &[u8; 5], msg: &str, seq: u8) -> Bytes {
    let mut p = BytesMut::new();
    p.put_u8(0xff);
    p.put_u16_le(code);                    // 对齐 tr_packet.c:309 errno LE
    p.put_u8(b'#');
    p.put_slice(sql_state);                // 5 字节,如 b"28000"
    p.put_slice(msg.as_bytes());
    p.freeze()
}

pub fn build_eof(warnings: u16, status: u16, seq: u8) -> Bytes {
    let mut p = BytesMut::new();
    p.put_u8(0xfe);                        // 对齐 make_eof_packet tr_packet.c:814
    p.put_u16_le(warnings);
    p.put_u16_le(status);
    p.freeze()
}
```

**验收操作**:
- `cargo test --test packet_build`:对照 C 版 hexdump。
  - `build_ok(0,0,2,0,"")` == `00 00 00 02 00 00 00`(对照 `tr_packet.c:528`)。
  - `build_error(1045, b"28000", "Access denied", 2)` == `ff 15 04 23 32 38 30 30 30 Access denied`(对照 `tr_packet.c:309`)。
  - `build_eof(0, 2, N)` == `fe 00 00 02 00`(对照 `tr_packet.c:814`)。
- 用 mysql CLI 触发:`mysql -e "SELECT 1"` 抓 OK 包字节;`mysql -e "SELECT * FROM no_such_table"` 抓 ERR 包字节,对照。

**常见坑/Rust 特有注意**:
- errno 是 **LE** 编码:1045 = `0x0415` → 字节 `\x15\x04`(C 侧 `tr_packet.c:309` 字面量 `\xff\x15\x04`)。不要写成 BE。
- `#` + SQLSTATE 是 `CLIENT_PROTOCOL_41` 风格;newproxy 总是启用(`tr_packet_com.h:46`)。
- lenenc int:0-250 用 1 字节;251 表示 NULL(不用于 affected_rows);252-65535 用 3 字节(0xfc + 2 LE);更大用 9 字节(0xfe + 8 LE)。affected_rows=0 是 `\x00`。
- OK 包的 msg 在 `CLIENT_PROTOCOL_41` 下可选;newproxy 多数场景 msg 为空。
- `build_ok` 返回 payload;外层 4 字节 header 由 `write_packet(payload, seq)` 加。测试时要么测 payload,要么套 header。
- EOF 包 payload 固定 5 字节(1+2+2);header 4 字节;共 9 字节。
- **结束包按对端能力位**(官方 `CLIENT_DEPRECATE_EOF` 规范,见 [04](./04-mysql-protocol-statemachine.md) §6):client 宣告 `CLIENT_DEPRECATE_EOF` 时,结果集列定义结束与行流结束用 **OK 包**(0x00,且 payload<0xFFFFFF)而非 EOF(0xFE)。封装 `build_end(peer_caps, EndPacket, seq)` 按能力位选 OK/EOF——别像 C 版固定发 EOF,否则 MySQL 8 客户端(默认宣告 DEPRECATE_EOF)会解析异常。

---

## P2 —— 并发核心 + 后端池

> 参考 `/docs/02-concurrency-model.md`、`/docs/03-backend-pool-optimization.md`。C 侧池语义已逐行核实,见各卡引用。

### T2.1 连接 task 模型 + 状态机骨架

**目标**:把 P1 的透传逻辑迁到 `conn_task` + `FrontConn` task 模型,连接状态是 task 局部变量;`drive()` 状态机替代 C 侧 `core_driver_machine` + `conn_handler_pt`。

**前置依赖**:T1.4。

**实施步骤**:
1. 在 `src/conn/front.rs` 定义 `FrontConn` 结构(对齐 02 §3):`stream` 拆成 `reader: PacketReader<TcpStream>` + `writer: PacketWriter<TcpStream>`、`state: FrontState`、`user: Option<Arc<ProductUser>>`、`db: Option<String>`、`ctx: Arc<AppCtx>`、`backend: Option<Arc<BackConn>>`(P2.3 用)。
2. 定义 `FrontState` enum(对齐 `tr_conn.h:38-55`):`Accepted`/`HandshakeSent`/`AuthRead`/`AuthResultSent`/`CommandLoop`(精简 C 侧 `STATE_FRONTEND_*`,合并 `WaitBackend`/`UpstreamReturned` 进 CommandLoop 的子状态)。
3. 实现 `FrontConn::drive(&mut self) -> Result<DriveOutcome>`(对齐 02 §3):`match self.state` 分派到 T1.2 握手 / T1.4 命令循环。`DriveOutcome::Continue`/`ShutDown`。
4. 在 `src/conn/mod.rs` 实现 `conn_task(stream, peer, ctx)`(对齐 02 §3):`loop { match conn.drive().await { Continue=>{}, ShutDown|Err=>break } }` + `finalize_conn(conn).await`(02 §6,归还 backend)。`main.rs` 的 accept loop `tokio::spawn(conn_task(...))`(02 §12)。
5. 实现 `finalize_conn`(02 §6):task 退出前显式异步归还 backend(P1 占位为 drop,P2.3 换 `release_backend`)。
6. 配置 tokio runtime:`#[tokio::main(flavor = "multi_thread", worker_threads = cfg.max_threads)]`(对齐 02 §2,worker_threads = CPU 核数或 `config.max_threads`,`tr_config.h:280`)。
7. 迁移 P1 的透传逻辑进 `dispatch`,确保行为不变。

**关键代码骨架/要点**:
```rust
// src/conn/front.rs(对齐 02-concurrency-model.md §3)
pub enum FrontState { Accepted, HandshakeSent, AuthRead, CommandLoop }
pub enum DriveOutcome { Continue, ShutDown }

pub struct FrontConn {
    pub reader: PacketReader<tokio::net::TcpStream>,
    pub writer: PacketWriter<tokio::net::TcpStream>,
    pub state: FrontState,
    pub user: Option<Arc<ProductUser>>,
    pub current_db: Option<String>,
    pub ctx: Arc<AppCtx>,
    pub backend: Option<Arc<BackConn>>,   // T2.3 归还用
    pub client_seq: u8,
}

impl FrontConn {
    pub async fn drive(&mut self) -> Result<DriveOutcome> {
        match self.state {
            FrontState::Accepted => { /* T1.2 build_handshake + send */ self.state = FrontState::HandshakeSent; }
            FrontState::HandshakeSent => { /* T1.2 parse_client_auth + 校验 */ self.state = FrontState::AuthRead; }
            FrontState::AuthRead => { /* T1.2 send_ok */ self.state = FrontState::CommandLoop; }
            FrontState::CommandLoop => { /* T1.4 read_command + dispatch */ }
        }
        Ok(DriveOutcome::Continue)
    }
}

// src/conn/mod.rs
pub async fn conn_task(stream: TcpStream, peer: SocketAddr, ctx: Arc<AppCtx>) {
    let (r, w) = stream.into_split();
    let mut conn = FrontConn {
        reader: PacketReader::new(r), writer: PacketWriter::new(w),
        state: FrontState::Accepted, user: None, current_db: None,
        ctx: ctx.clone(), backend: None, client_seq: 0,
    };
    loop {
        match conn.drive().await {
            Ok(DriveOutcome::Continue) => {}
            Ok(DriveOutcome::ShutDown) | Err(_) => break,
        }
    }
    let _ = finalize_conn(conn).await;   // 02 §6 显式异步归还
}
```

**验收操作**:
- 单连接透传行为与 P1 一致:`mysql -e "SELECT 1"` 经代理返回 1。
- `cargo test --test conn_task` 通过(模拟 client 连上、握手、查询、退出)。
- task 退出后 backend 连接被 drop(P1 阶段)或归还(P2.3 后);用 `lsof -p <pid> | grep ESTABLISHED` 观察后端连接数。
- `tokio-console` 连上观察 task 数量随连接数增减(可选,需启用 `tokio-console` feature)。

**常见坑/Rust 特有注意**:
- `Drop` 不能 `.await`:backend 归还是 async,必须在 `finalize_conn` 里显式调,**不能**放 `Drop for FrontConn`。`Drop for FrontConn` 只做同步清理(关 stream 由 `TcpStream` 的 Drop 完成)。
- `reader`/`writer` 拆成独立字段:避免 `&mut self` 同时借 reader 和 writer 的借用冲突(`proxy_result_set` 读 backend 写 client 时需要同时用两个 borrow)。
- `conn_task` 里 `conn` 是 task 局部,天然单线程占有,无需 `Arc<Mutex>`——这是 02 §3 的核心收益。
- `Err(_)` 要 break:命令处理出错(如后端断开)要关闭前端连接,不能死循环。
- `worker_threads`:C 侧 `max_threads` 默认 2(`tr_config.c:2143`),Rust 版建议默认 CPU 核数,`max_threads` 配置覆盖。
- task panic:tokio 默认 spawn 的 task panic 不会崩溃进程,但连接会泄漏——`finalize_conn` 用 `BackendGuard`(T2.3)RAII 兜底。

---

### T2.2 后端连接池数据结构

**目标**:实现 `SrvPool` / `AttrBucket`(三层 DashMap 嵌套 + parking_lot Mutex),对齐 C 侧 4 层 GHashTable 池结构;`BackConn` 带 `in_pool`/`served_times`/`enqueue_pool_time` 原子字段。

**前置依赖**:T2.1。

**实施步骤**:
1. 在 `src/pool/mod.rs` 定义 `SrvPool`(对齐 03 §2 + C 侧 `tr_srv_pool.h:9-15` 的 `tr_srv_pool_s`):`map: DashMap<ClusterId, DashMap<TabletId, DashMap<UserId, Arc<AttrBucket>>>>`。C 侧是 cluster→tablet→username→属性桶(`$found_rows$ignore_space$multi_query`,见 T2.1 调查)4 层;Rust 版属性桶合并成 `AttrBucket` 的 `cfg` 字段(首版不分 found_rows/ignore_space)。
2. 定义 `AttrBucket`(对齐 03 §2 + C 侧 `tr_srv_pool.h:4-7` 的 `tr_srv_pool_entry_t`):`write: Mutex<VecDeque<Arc<BackConn>>>`(原 `w_queue`,主)、`read: Mutex<VecDeque<Arc<BackConn>>>`(原 `r_queue`,从)、`cfg: PoolCfg`。用 `parking_lot::Mutex`(03 §2 说明:无竞争时纯用户态,不跨 await 持有)。
3. 定义 `PoolCfg`(对齐 03 §2):`max_serve_times: u64`(对应 `conn_pool_socket_max_serve_client_times`,`tr_config.h:302`)、`max_idle_secs: u64`(`backend_idle_timeout`,`tr_config.h:299`)、`max_size: usize`(`max_conn_pool_size`)。
4. 完善 `BackConn` 字段(对齐 03 §2 + C 侧 `tr_conn.h:250/180/153`):`in_pool: AtomicBool`(对应 `is_in_svr_pool`,`tr_conn.h:250`)、`served_times: AtomicU64`(对应 `served_client_times`,`tr_conn.h:180`)、`enqueue_pool_time: AtomicI64`(对应 `enqueue_pool_time`,`tr_conn.h:153`)、`last_active_ts: AtomicI64`(`tr_conn.h:154`)。注意 C 侧 idle 淘汰用 `enqueue_pool_time`(`tr_srv_pool.c:257`)不是 `last_active_ts`。
5. 实现 `SrvPool::bucket_for(&self, cluster, tablet, user, ms) -> Option<Arc<AttrBucket>>`:三层 DashMap 查找。
6. 实现 `SrvPool::all_buckets()` 用于 T2.5 空闲回收遍历。
7. 写单元测试:构造池,插入/查找桶,CAS 契约验证(`in_pool` 1↔0)。

**关键代码骨架/要点**:
```rust
// src/pool/mod.rs(对齐 03-backend-pool-optimization.md §2 + tr_srv_pool.h:4-15)
use dashmap::DashMap;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicI64};

pub struct SrvPool {
    /// cluster -> tablet -> user -> AttrBucket
    pub map: DashMap<String, DashMap<String, DashMap<String, Arc<AttrBucket>>>>,
}

pub struct AttrBucket {
    pub write: Mutex<VecDeque<Arc<BackConn>>>,   // 原 w_queue (主)
    pub read:  Mutex<VecDeque<Arc<BackConn>>>,   // 原 r_queue (从)
    pub cfg: PoolCfg,
    pub db: Arc<Database>,
    pub ct: Arc<ClusterTablet>,
}

pub struct PoolCfg {
    pub max_serve_times: u64,    // conn_pool_socket_max_serve_client_times
    pub max_idle_secs: u64,      // backend_idle_timeout
    pub max_size: usize,         // max_conn_pool_size
}

pub struct BackConn {
    pub reader: PacketReader<tokio::net::TcpStream>,
    pub writer: PacketWriter<tokio::net::TcpStream>,
    pub db: Arc<Database>,
    pub ct: Arc<ClusterTablet>,
    pub ms: MasterSlave,
    pub in_pool: AtomicBool,          // tr_conn.h:250 is_in_svr_pool
    pub served_times: AtomicU64,      // tr_conn.h:180 served_client_times
    pub enqueue_pool_time: AtomicI64, // tr_conn.h:153
    pub last_active_ts: AtomicI64,    // tr_conn.h:154
}

impl AttrBucket {
    pub fn queue_for(&self, ms: MasterSlave) -> &Mutex<VecDeque<Arc<BackConn>>> {
        match ms { MasterSlave::Master => &self.write, MasterSlave::Slave => &self.read }
    }
}
```

**验收操作**:
- `cargo test --test srv_pool_struct`:插入桶、查找、`in_pool` CAS `true→false` 成功、`false→true` 成功、二次 `false→true` 失败(double-put 防护,对齐 03 §4)。
- `cargo test --test srv_pool_struct`:write/read 队列独立锁。
- 内存:`Arc<AttrBucket>` 引用计数正确,无泄漏(`drop` 后 `Arc::strong_count==0`)。

**常见坑/Rust 特有注意**:
- `parking_lot::Mutex` 而非 `std::sync::Mutex`:03 §2 说明——热路径无竞争时 std 也要 syscall,parking_lot 纯用户态。且**不能跨 `.await` 持有**(03 §5),acquire/release 是同步函数。
- `DashMap` 嵌套三层:读路径分片并发,避免顶层单锁;但嵌套 `DashMap` 的 `get` 返回 `Ref` 借用,不能跨 await 持有——在 acquire 里先 `clone` 出 `Arc<AttrBucket>` 再释放 `Ref`。
- `AttrBucket` 用 `Arc` 而非直接存值:03 §2 说明——reload 换桶时旧桶可能被在跑 task 持有,`Arc` 让旧桶引用释放后才回收,无需手写排空。
- `in_pool` 用 `AtomicBool` + `compare_exchange`:对齐 C 侧 `tr_atomic_compare_and_swap(&is_in_svr_pool, 1, 0)`(`tr_srv_pool.c:31`)。CAS 语义保证不可能 double-put(03 §4)。
- C 侧 `tr_conn_pool_t`(`tr_conn_pool.h:4-8`)是单队列 + `pthread_mutex_t`(line 7,`PTHREAD_MUTEX_RECURSIVE`);Rust 版把 w_queue/r_queue 提到 `AttrBucket`,每队列独立锁(03 §5.2 阶梯 1)。
- `BackConn` 持 `PacketReader/Writer<TcpStream>`:不能 `Clone`,`Arc<BackConn>` 共享;但 reader/writer 的 `&mut` 需要独占——用 `Mutex` 包裹或保证同一时刻一个 task 持有(池语义保证)。

---

### T2.3 acquire/release + 异步归还

**目标**:实现 `acquire_or_connect` / `release_backend`;`BackendGuard` RAII 兜底防泄漏;复用上限(`served_client_times`)达上限不复用;task panic 不泄漏连接。

**前置依赖**:T2.2。

**实施步骤**:
1. 在 `src/pool/acquire.rs` 实现 `AttrBucket::acquire(&self, ms) -> Option<Arc<BackConn>>`(对齐 03 §3 + C 侧 `get_network_socket_from_queue` `tr_srv_pool.c:21` / `tr_srv_pool_get_by_client` `tr_srv_pool.c:199`):锁队列 `pop_front`,CAS `in_pool` `true→false`(`tr_srv_pool.c:31`),失败(被并发抢)继续 pop;成功后检查 `is_expired`(复用上限 + 空闲超时),过期则关连接继续 pop。**同步函数,持锁窗口内不做 IO**(03 §3)。
2. 实现 `BackConn::is_expired(&self, cfg: &PoolCfg) -> bool`(对齐 03 §3 + C 侧 `tr_srv_pool.c:257/320`):`served_times >= cfg.max_serve_times`(对齐 `tr_srv_pool.c:320`)或 `now - enqueue_pool_time > cfg.max_idle_secs * 1_000_000`(对齐 `tr_srv_pool.c:257` 的 idle 淘汰,用 `enqueue_pool_time` 不是 `last_active_ts`)。
3. 实现 `acquire_or_connect(pool, ct, ms) -> Result<Arc<BackConn>>`(对齐 03 §7):先 `bucket.acquire(ms)`,命中返回;池空调 `load_balance` + `connect_with_failover`(T2.4)。
4. 在 `src/pool/release.rs` 实现 `release_backend(c: Arc<BackConn>, pool: &SrvPool)`(对齐 03 §4 + C 侧 `tr_srv_pool_add` `tr_srv_pool.c:309`):CAS `in_pool` `false→true`(对齐 `tr_srv_pool.c:377`),失败说明已销毁直接返回(03 §4 double-put 防护);`served_times.fetch_add(1)` 后检查 `>= max_serve_times`(对齐 `tr_srv_pool.c:320`),超限则 `in_pool.store(false)` 不入池(等 Drop 关 fd);否则 `enqueue_pool_time = now`(`tr_srv_pool.c:381`)、push 回队列(`tr_srv_pool.c:385`)。
5. 实现 `BackendGuard` RAII(对齐 03 §9):`struct BackendGuard { conn: Option<Arc<BackConn>>, pool: Arc<SrvPool> }`,`Drop` 里 `tokio::spawn(async move { release_backend(c, &pool).await })`(03 §9,同步 Drop 不能 await,投递后台 task)。
6. 在 `FrontConn::acquire_backend` 用 `acquire_or_connect` 替换 P1 的直接 connect;`finalize_conn` 用 `release_backend` 替换 P1 的 drop(02 §6)。
7. 写测试:复用计数(同一连接 `served_times` 递增)、达上限后不复用(新建)、task panic 后 guard Drop 归还(用 `std::panic::catch_unwind` 模拟)。

**关键代码骨架/要点**:
```rust
// src/pool/acquire.rs(对齐 03 §3 + tr_srv_pool.c:21-32)
impl AttrBucket {
    pub fn acquire(&self, ms: MasterSlave) -> Option<Arc<BackConn>> {
        let q = self.queue_for(ms);
        let mut q = q.lock();   // parking_lot,无 await
        while let Some(c) = q.pop_front() {
            // CAS 1->0,对齐 tr_srv_pool.c:31
            if c.in_pool.compare_exchange(true, false, AcqRel, Relaxed).is_ok() {
                if c.is_expired(&self.cfg) {
                    drop(q);   // 先释放锁
                    let _ = close_backend(&c).await;   // async,在锁外
                    continue;
                }
                return Some(c);
            }
        }
        None
    }
}

impl BackConn {
    pub fn is_expired(&self, cfg: &PoolCfg) -> bool {
        if self.served_times.load(Relaxed) >= cfg.max_serve_times { return true; }  // tr_srv_pool.c:320
        let now = now_micros();
        let enq = self.enqueue_pool_time.load(Relaxed);
        now - enq > (cfg.max_idle_secs as i64) * 1_000_000   // tr_srv_pool.c:257
    }
}
```
```rust
// src/pool/release.rs(对齐 03 §4 + tr_srv_pool.c:309-387)
pub async fn release_backend(c: Arc<BackConn>, pool: &SrvPool) {
    // CAS 0->1,对齐 tr_srv_pool.c:377;失败=已销毁
    if c.in_pool.compare_exchange(false, true, AcqRel, Relaxed).is_err() {
        return;
    }
    let bucket = c.ct.bucket_for(&c.ms);
    // 复用上限检查,对齐 tr_srv_pool.c:320
    if c.served_times.fetch_add(1, Relaxed) + 1 >= bucket.cfg.max_serve_times {
        c.in_pool.store(false, Release);   // 不入池,等 Drop 关 fd
        return;
    }
    c.enqueue_pool_time.store(now_micros(), Release);   // tr_srv_pool.c:381
    bucket.queue_for(c.ms).lock().push_back(c);          // tr_srv_pool.c:385
}

// RAII 兜底,对齐 03 §9
pub struct BackendGuard {
    pub conn: Option<Arc<BackConn>>,
    pool: Arc<SrvPool>,
}
impl Drop for BackendGuard {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            let pool = self.pool.clone();
            tokio::spawn(async move { release_backend(c, &pool).await; });
        }
    }
}
```

**验收操作**:
- `cargo test --test pool_acquire_release`:acquire 命中池、release 入池、`served_times` 递增、达 `max_serve_times` 后不入池(新建连接)。
- `cargo test --test pool_guard_panic`:task panic 后 `BackendGuard::drop` 触发归还,后端连接不泄漏(池大小恢复)。
- 端到端:连续 100 次 `SELECT 1`,后端实际新建连接数 << 100(复用);用 `SHOW PROCESSLIST` 在后端观察连接数稳定。
- `lsof -p <pid> | grep ESTABLISHED | wc -l` 在压测期间稳定(不泄漏)。

**常见坑/Rust 特有注意**:
- **Drop 不能 await**:`release_backend` 是 async,不能放 `Drop for BackConn`;`BackendGuard::drop` 用 `tokio::spawn` 投递(03 §9)。注意 spawn 的 task 若 runtime 已关闭会丢——`finalize_conn` 里显式 `release_backend().await` 是主路径,guard Drop 只是 panic 兜底。
- `acquire` 是同步函数,持锁窗口内不做 IO(03 §3):关过期连接的 `close_backend` 是 async,必须 `drop(q)` 释放锁后再 await。
- `compare_exchange` 的 Ordering:`AcqRel` for success,`Relaxed` for failure(03 §3);不要全用 `SeqCst`(过度同步)。
- `served_times` 在 `fetch_add` 后判断:`fetch_add` 返回旧值,所以 `+1 >= max` 判断(03 §4)。
- `enqueue_pool_time` 用 `enqueue_pool_time` 不是 `last_active_ts` 做 idle 判定(对齐 C 侧 `tr_srv_pool.c:257`)。
- 新建连接风暴:池空时多个 task 同时 `connect_with_failover` 会风暴;03 §9 建议桶级 `Semaphore` 限制并发建连数——首版可不加,压测后按需。
- `BackendGuard` 持 `Arc<SrvPool>`:避免 guard 比 pool 先死导致悬垂;`Arc` 保证 pool 至少与 guard 同寿。

---

### T2.4 负载均衡 + failover

**目标**:实现 power-of-two-choices 选 DB + 重试 bitmap failover;多后端时连接分布均匀,单后端宕机自动 failover。

**前置依赖**:T2.3。

**实施步骤**:
1. 在 `src/pool/lb.rs` 实现 `load_balance(dbs: &[Arc<Database>], ms: MasterSlave) -> Option<Arc<Database>>`(对齐 03 §7 + C 侧 `tr_back_lb.c:3` 的 `load_balance`):过滤可用 db(`network_database_available`,C 侧 `tr_route.c:94`),抽两个随机 `rand()`(对齐 `tr_back_lb.c:63/65` 的 `rand() % av_dbs->len`),选 `current_conn_num/weight` 比值小的(对齐 `tr_back_lb.c:88-94`)。weight==0 时选 `current_conn_num` 小的(`tr_back_lb.c:82-87`)。
2. 实现可用性检查 `database_available(db, ms, cfg) -> bool`(对齐 `tr_route.c:94` 的 `network_database_available`):`weight>0`、`current_conn_num < max_connections`(`tr_route.c:111`)、`time_reconnect_interval` 内不重试刚失败的(`tr_route.c:117-119`)。
3. 实现 `connect_with_failover(bucket, ms) -> Result<Arc<BackConn>>`(对齐 03 §7 + C 侧 `server_connection_failover` `tr_back_connect.c:775`):循环 `load_balance` → `BackConn::connect` → 失败则标记 tried bitmap(对齐 `tr_back_lb.c:16/98-101` 的 `tried` 位图)→ 重试,直到成功或全部 tried。
4. 重试 bitmap:用 `u64` 或 `Vec<u64>`(C 侧 `tried` 是 `uintptr_t[]`,`tr_back_lb.c:14-16`);Rust 版首版用 `u64`(支持 ≤64 个后端,足够)。
5. `Database` 结构加 `active_conns: AtomicU64`(当前连接数,`load_balance` 用)、`last_fail_time: AtomicI64`、`weight: u32`、`max_connections: u32`(对齐 `tr_config.c:1551/1594`)。
6. 写测试:多后端(3 个 mysqld)压测,统计连接分布均匀(±10%);杀一个后端,后续请求 failover 到其他后端,`last_fail_time` 内不重试。

**关键代码骨架/要点**:
```rust
// src/pool/lb.rs(对齐 03 §7 + tr_back_lb.c:3-101)
use rand::seq::SliceRandom;

pub fn load_balance(dbs: &[Arc<Database>], ms: MasterSlave) -> Option<Arc<Database>> {
    let avail: Vec<_> = dbs.iter().filter(|d| d.available(ms)).cloned().collect();
    if avail.is_empty() { return None; }
    if avail.len() == 1 { return Some(avail[0].clone()); }   // tr_back_lb.c:58
    let mut rng = rand::thread_rng();
    let a = avail.choose(&mut rng)?;
    let b = avail.choose(&mut rng)?;
    // 对齐 tr_back_lb.c:88-94:比较 (current_conn+1)/weight
    let ra = (a.active_conns.load(Relaxed) + 1) as f64 / a.weight as f64;
    let rb = (b.active_conns.load(Relaxed) + 1) as f64 / b.weight as f64;
    Some(if ra <= rb { a.clone() } else { b.clone() })
}

pub async fn connect_with_failover(bucket: &AttrBucket, ms: MasterSlave) -> Result<Arc<BackConn>> {
    let mut tried: u64 = 0;
    let n = bucket.dbs.len();
    loop {
        let avail: Vec<_> = bucket.dbs.iter().enumerate()
            .filter(|(i, d)| tried & (1u64 << i) == 0 && d.available(ms))
            .collect();
        if avail.is_empty() { return Err(Error::NoBackend); }
        let db = load_balance(&avail.iter().map(|(_, d)| d.clone()).collect::<Vec<_>>(), ms)
            .ok_or(Error::NoBackend)?;
        let idx = bucket.dbs.iter().position(|d| Arc::ptr_eq(&d, &db)).unwrap();
        match BackConn::connect(&db, &bucket.db_user).await {
            Ok(c) => { db.active_conns.fetch_add(1, Relaxed); return Ok(Arc::new(c)); }
            Err(_) => {
                tried |= 1u64 << idx;              // tr_back_lb.c:98
                db.last_fail_time.store(now_micros(), Release);
                continue;
            }
        }
    }
}
```

**验收操作**:
- 3 个后端 mysqld,`sysbench --threads=50 --time=60 ./select.lua` 经代理,各后端 PROCESSLIST 连接数均匀(±10%)。
- `kill -9` 一个后端,代理后续请求 failover 到其余 2 个,无 5xx 返回;`last_fail_time` 内(`time_reconnect_interval`)不重试死掉的后端。
- 全部后端宕机:返回 `NoBackend` 错误给客户端(ERR 2003 或类似)。
- `cargo test --test lb_failover` 通过。

**常见坑/Rust 特有注意**:
- `rand::thread_rng()` 是 async 安全的(非阻塞);不要用 `Math.random`(C 侧 `rand()` 是全局且非线程安全,Rust 改进)。
- `choose` 两次可能相同(对齐 C 侧 `pos1==pos2` 分支 `tr_back_lb.c:67-70`),选那个即可。
- `active_conns` 必须在 `release_backend` 时递减(连接归还或关闭时);否则 `load_balance` 比例失真。注意 release 路径里 `fetch_sub`。
- `time_reconnect_interval`:C 侧 `tr_config.c:1585`,失败后多久才重试该后端;Rust 版用 `last_fail_time + interval > now` 判定。
- weight==0 的后端:不参与 `load_balance` 的权重比较,但可用性检查里 weight==0 直接不可用(`tr_route.c:107`)。
- failover 循环要有上限(`n` 次后放弃),否则全部不可用时死循环。
- `tried` bitmap 用 `u64` 只支持 64 后端;实际场景足够。若需更多,改 `Vec<u64>`(对齐 C 侧 `tried[]`)。

---

### T2.5 空闲回收 + 预热

**目标**:实现 `idle_reaper` task 周期扫描各桶关闭超 `max_idle_secs` 的空闲连接;可选 `warmup_pool` 预建 min_idle 条连接消除冷启动毛刺。

**前置依赖**:T2.3。

**实施步骤**:
1. 在 `src/pool/reaper.rs` 实现 `idle_reaper(pool: Arc<SrvPool>, tick: Duration)`(对齐 03 §6.2 + C 侧 `tr_srv_pool.c:379` 的 idle timeout event):`tokio::time::interval(tick)` 周期遍历 `pool.all_buckets()`,对每个桶 `retain` 队列:超 `max_idle_secs` 且 CAS `in_pool true→false` 成功的收集到 `to_close`,从队列移除;锁外 `close_backend(c).await`。
2. `retain` + CAS 语义(对齐 03 §6.2):正在被 acquire 抢走的连接(`in_pool` 已 `false`)不会被回收(CAS 抢不到说明在用,跳过)。
3. 实现 `warmup_pool(pool: &SrvPool, cfg: &Config)`(对齐 03 §6.1,可选):启动时为每个桶预建 `min_idle` 条连接,`in_pool.store(true)` + push 回队列。
4. 在 `main.rs` 启动时 `tokio::spawn(idle_reaper(ctx.srv_pool.clone(), Duration::from_secs(10)))`(02 §2 旁路 task)。
5. `PoolCfg` 加 `min_idle: usize`(预热用,首版可设 0)。
6. 写测试:插入 N 个空闲连接,等 `max_idle_secs+tick` 后全被回收;在用连接(in_pool=false)不被误杀。

**关键代码骨架/要点**:
```rust
// src/pool/reaper.rs(对齐 03 §6.2)
pub async fn idle_reaper(pool: Arc<SrvPool>, tick: Duration) {
    let mut interval = tokio::time::interval(tick);
    loop {
        interval.tick().await;
        for bucket in pool.all_buckets() {
            for ms in [MasterSlave::Master, MasterSlave::Slave] {
                let mut to_close = Vec::new();
                {
                    let mut q = bucket.queue_for(ms).lock();
                    q.retain(|c| {
                        if c.is_expired(&bucket.cfg)
                           && c.in_pool.compare_exchange(true, false, AcqRel, Relaxed).is_ok() {
                            to_close.push(c.clone());
                            false   // 从队列移除
                        } else { true }
                    });
                }
                for c in to_close { let _ = close_backend(&c).await; }
            }
        }
    }
}
```

**验收操作**:
- 插入 10 个空闲连接,`max_idle_secs=1`,`tick=1s`,2 秒后池为空。
- 压测结束后空闲连接在 `max_idle_secs` 后被回收,`PROCESSLIST` 连接数下降。
- 在用连接(acquire 后 in_pool=false)不被回收:`cargo test --test idle_reaper`。
- (若启用预热)冷启动 `SELECT 1` 延迟 < 未预热的;首版可跳过预热。

**常见坑/Rust 特有注意**:
- `retain` 闭包内不能 await:`close_backend` 是 async,必须收集到 `to_close` 后锁外 await(03 §6.2)。
- `retain` + CAS:`in_pool` 已 `false`(被 acquire 抢走)的连接 CAS 失败,`retain` 返回 true 保留在队列——但它其实已经不在队列了(acquire pop 出去了)。实际上 acquire 是 `pop_front` 移除的,队列里只剩 `in_pool=true` 的;reaper 扫到的都是 `in_pool=true`,CAS 必成功。但并发下 acquire 可能 pop 了还没改 in_pool?不会——acquire 是 pop 后立即 CAS。retain 看到的是队列内剩余,都是 `in_pool=true`。
- `tick` 默认 10s,可配;不要太短(频繁扫锁影响性能)。
- `close_backend` 关 fd:tokio `TcpStream` drop 自动关;但若要发 `COM_QUIT` 给后端优雅关,需 async。首版直接 drop fd。
- 预热 `warmup_pool` 是可选(06-task-plan.md §5 列为可后置);首版可不实现,留 `min_idle=0`。

---

### T2.6 多连接并发压测

**目标**:**M2 里程碑**——100 并发连接稳定透传,无错;`tokio-console` 无锁热点告警。

**前置依赖**:T2.3、T2.4。

**实施步骤**:
1. 准备压测环境:本地起 3 个 `mysqld`(或 1 个),`conf/newproxy.conf` 配多 master/slave;`run_env/script/select.lua` 是 `db_query("select 1")`(已核实)。
2. 用 `sysbench` 或 `mysql-connector-j` 跑 100 并发:`sysbench --threads=100 --time=60 --mysql-host=127.0.0.1 --mysql-port=4051 ./run_env/script/select.lua run`。或用 `tools/load.sh`(C 侧压测脚本,`tools/load.sh` 已核实)。
3. 监控:QPS、p99 延迟、错误率;`top -p <pid>` 看 RSS/CPU;`lsof -p <pid> | grep ESTABLISHED | wc -l` 看连接数稳定(前后端)。
4. 启用 `tokio-console`(需 `tokio` 的 `tracing` feature):`tokio-console 127.0.0.1:6669`,观察 task 数量、是否有 task 长时间阻塞(锁热点)、waker 唤醒频率。
5. 长跑 10 分钟,确认无 crash、RSS 无上涨趋势、连接数稳定。
6. 对照 C 版:同压测条件下 QPS/p99 对比(Rust 版首版可能低于 C 版,T5.3 再优化)。
7. 记录热点:若 `tokio-console` 显示 `AttrBucket::write` 锁等待高,按 03 §5.3 决策树升级(阶梯 2 桶内分片 / 阶梯 3 Treiber 栈)——但首版先保持阶梯 0,T5.4 按需优化。

**关键代码骨架/要点**:
```rust
// main.rs 启动(对齐 02-concurrency-model.md §12)
#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let ctx = Arc::new(AppCtx::load("conf/newproxy.conf").await?);
    let listener = TcpListener::bind(("0.0.0.0", ctx.config.load().port)).await?;
    // 旁路 task
    tokio::spawn(crate::pool::idle_reaper(ctx.srv_pool.clone(), Duration::from_secs(10)));
    // tokio::spawn(crate::pool::warmup_pool(...));   // 可选
    loop {
        let (stream, peer) = listener.accept().await?;
        let ctx = ctx.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::conn::conn_task(stream, peer, ctx).await {
                tracing::warn!(%peer, error=%e, "conn task ended");
            }
        });
    }
}
```
`Cargo.toml` 加 `tokio = { version = "1", features = ["full", "tracing"] }` 用于 console。

**验收操作**:
- **M2**:`sysbench --threads=100 --time=60 ... select.lua run` 无错(errors=0),QPS 记录对照 C 版。
- `tokio-console` 无 task 长时间阻塞(>1ms 在锁上)、无 task 泄漏(连接断开后 task 消失)。
- 10 分钟长跑:无 crash,RSS 稳定(±10%),前后端 ESTABLISHED 连接数稳定。
- `mysql_case/totalTest.sh` select/transaction 子集通过(对照 C 版)。
- 压测后 `SHOW PROCESSLIST` 后端无残留连接(空闲回收生效)。

**常见坑/Rust 特有注意**:
- **task 数量**:100 并发 = 100+ 个 conn_task + 后端查询子 task;tokio 能轻松扛万级 task,但要确认没在 task 里持有大 buffer(每个 `BytesMut` 预分配别太大)。
- `tokio-console` feature 开启后有性能开销(约 5-10%);压测对照 C 版时要关掉 console feature。
- 锁热点定位:`tokio-console` 看 task 的 `busy` 时间;若 `AttrBucket::write.lock()` 是热点,用 `parking_lot::Mutex` 的 `try_lock_for` 或换分片(03 §5.2)。
- 连接泄漏:压测中途 `kill -9` 一个 client,确认对应 task 退出、backend 归还(`BackendGuard::drop`);用 `lsof` 观察连接数回降。
- `worker_threads` 设为 CPU 核数;太少(如 2)会导致 task 调度排队,p99 升高。
- 后端 mysqld 的 `max_connections` 要够大(100+),否则代理建连失败被误判为 bug。
- `sysbench` 的 `select.lua` 是 `db_query("select 1")`(已核实 `run_env/script/select.lua`),极轻量,主要压代理协议栈+池;若要压合并(T3.5)需换分片 SQL。
- 若 p99 远高于 C 版:先查是否每次建连(P2.3 池复用是否生效)、是否锁竞争(03 §5.3)、是否 buffer 拷贝过多(`Bytes` 应零拷贝);T5.3/T5.4 系统优化。

---

## P3 — 分片/改写/合并核心

### T3.1 parser 对象池

**目标**:封装 sqlparser FFI,提供 `ParserPool`/`ParserGuard`/`AstHandle`,把非线程安全的 C 解析器池化,编译期杜绝 AST 指针逃逸。

**前置依赖**:T0.2(bindgen 生成 `sqlparser_ffi.rs`,能调 `mp_init`/`sql_parser_init`/`parse_sql`)。

**实施步骤**:
1. 在 `src/ffi/mod.rs` 确认 T0.2 生成的绑定暴露 `mp_init`/`mp_clear`/`mp_free`/`sql_parser_init`/`sql_parser_clear`/`parse_sql`/`sql_parser_free`(对应 `modules/sqlparser/include/sql_define.h:3004-3041`)。若 `sql_parser_clear` 未在 allowlist,补到 `build.rs` 的 `allowlist_function`。
2. 实现 `RawParser`(持 `*mut mem_pool_t` + `*mut sql_parser_t`),`new()` 调 `mp_init(ARENA_SIZE)`(建议 ARENA_SIZE=1<<20)再 `sql_parser_init`。声明 `unsafe impl Send for RawParser {}`,**绝不** impl `Sync`。
3. 实现 `ParserPool { free: parking_lot::Mutex<Vec<Box<RawParser>>>, cap: usize }`,`cap` 默认 = tokio worker 线程数。`acquire()` 按 [05-parser-pool.md §4](./05-parser-pool.md) 的 Guard 模式:pop 空闲 → 池未满则新建 → 池满则 `yield_now().await`。
4. 实现 `ParserGuard<'a>` 与 `AstHandle<'g>`(`cmd: *mut sql_command_t` + `PhantomData<&'g mut ParserGuard<'g>>`),`parse(&mut self, sql: &str)` 用 `CString` 拷贝后调 `parse_sql`,rc!=0 时从 `(*parser).sql_syntax_error_str` 取错误。对照 `tr_sql.c:1864 tr_parse_sql` 的语义。
5. `ParserGuard::Drop` 调 `sql_parser_clear`(对应 `sql_define.h:3031`,清状态保留 arena)+ push 回池。**Drop 内禁止 `.await`**。
6. 写 `tests/parser_pool.rs`:并发 16 task 各解析 `SELECT * FROM t WHERE id=1` 1000 次不 crash;fuzz 10000 条 `arbitrary` 随机 SQL 仅验证不 segfault(对照 [05 §9](./05-parser-pool.md) Fuzz)。

**关键代码骨架/要点**:
```rust
pub struct AstHandle<'g> { cmd: *mut ffi::sql_command_t, _m: PhantomData<&'g mut ParserGuard<'g>> }
impl<'g> AstHandle<'g> {
    pub fn cmd(&self) -> *mut ffi::sql_command_t { self.cmd }   // 下游 unsafe 遍历入口
}
// ParserGuard::parse 签名
pub fn parse<'a>(&'a mut self, sql: &str) -> Result<AstHandle<'a>, ParseError>;
```
crate:`parking_lot = "0.12"`(池 Mutex)、`tokio = { features=["full"] }`(`yield_now`)、`arbitrary = "1"`(fuzz)。

**验收操作**:`cargo test -p newproxy-ffi parser_pool`;对照 C 侧 `modules/sqlparser/test/` 用例,同一条 SQL 的 `sql_cmd` 非空且 rc=0;`MIRIFLAGS="-Zmiri-tag-raw" cargo +nightly miri test` 跑小用例验证裸指针无 invalid deref。

**常见坑/Rust 特有注意**:`AstHandle` 生命周期必须绑定 `&mut ParserGuard`——这是把"use-after-pool-clear"在编译期杜绝的唯一手段,下游 decomposer 必须在 guard 存活期间完成 AST 遍历(见 [05 §6](./05-parser-pool.md));guard 作用域内**禁止 `.await`**(否则拉长池占用且 `&mut` 借用跨 await 易出生命周期错误);GBK 5C 转义由 C 侧 `sql_define.h` 的 `GBK_5C_RECORDS` 处理,FFI 透传 charset 即可,不要在 Rust 侧重造。

---

### T3.2 SQL 分类 + 路由 hint 解析

**目标**:对每条入站 SQL 判定命令类型(SELECT/INSERT/UPDATE/DELETE/REPLACE/USE/OTHER)并解析 `/*{...}*/` 注释 hint(router/tbl/pid/force_tbl/trace_id/is_tx/mode),产出 `QueryMeta` 供路由使用。

**前置依赖**:T3.1(需 `AstHandle` 取 `sql_command_t`);T1.4(已有 COM_QUERY payload)。

**实施步骤**:
1. 在 `src/sql/classify.rs` 实现 `classify(cmd: &Command, sql_first_byte: u8, ast: &AstHandle) -> SqlType`。COM_* 直接映射(对照 `tr_sql.c:31-60 get_query_type` 的 switch:COM_INIT_DB→Use、COM_FIELD_LIST→FieldList、COM_PING→Ping…),COM_QUERY 则按 SQL 首关键字 + AST `sql_cmd->command` 判定。SQL 类型枚举对照 `tr_packet.h:102`(SQL_SELECT_NUM=1)与 `:203`(SQL_WRITE_NUM=255)。
2. 在 `src/sql/hint.rs` 实现 `parse_hints(sql: &str) -> QueryHints`。C 侧逻辑在 `tr_sql.c:570-665`:抽取 `/*{...}*/` JSON 注释,用 `serde_json` 解析(C 侧用 jansson)。字段映射:`router`→`comment_router_type`(MS_MASTER=1/MS_SLAVE=2,见 `tr_const.h:132-133`、`tr_packet.h:8-9` COMMENT_MASTER="m"/COMMENT_SLAVE="s");`tbl`→`logic_table_name`+`parse_has_fix_tbl=2`;`pid`→`partition_id`+MD5 摘要前 8 字节(`parse_has_fix_pid=1`);`force_tbl`→`force_tablet_name`+`parse_has_force_tablet=1`(`tr_packet.h:419`、`tr_conn.h:322-323`);`mode=shadow`→`parse_is_test_sql=1`;`is_tx`→`parse_check_is_tx`;`trace_id`→`parse_trace_id`。
3. `QueryHints` 结构体聚合上述字段,`QueryMeta { sql_type, hints, ast }` 作为 T3.3/T3.4 的输入。
4. 单元测试对照 C 版逐 hint 字段:`/*{router:m}*/SELECT...`→master;`/*{force_tbl:DBA_C_demo_2}*/...`;`/*{pid:12345}*/...` 的 MD5 摘要用 `md5` crate 对照 C 侧 `MD5_DIGEST` 结果。

**关键代码骨架/要点**:
```rust
pub struct QueryHints {
    pub router: Option<MasterSlave>,        // MS_MASTER / MS_SLAVE
    pub fix_table: Option<String>,          // logic_table_name
    pub partition_id: Option<(i64, u64)>,   // (pid, pid_md5sum)
    pub force_tablet: Option<String>,
    pub is_test_sql: bool,                  // shadow
    pub is_tx: Option<bool>,
    pub trace_id: Option<String>,
}
pub fn parse_hints(sql: &str) -> QueryHints;   // 纯函数,无 FFI
```
crate:`serde_json = "1"`(解析注释 JSON)、`md-5 = "0.10"`(pid 摘要)。

**验收操作**:`cargo test -p newproxy-sql hint_`;手工 `mysql -h127.0.0.1 -P4051 -e "/*{router:s}*/SELECT 1"` 抓包确认走 slave(待 T3.6 联动);对照 `newproxy_case/case/` 下 shadow/hint 用例。

**常见坑/Rust 特有注意**:hint 解析是**纯字符串/JSON 处理,不应触碰 AST**,放在 guard 作用域外做,避免拉长池占用;`my_strncasestr` 是大小写不敏感匹配,Rust 用 `eq_ignore_ascii_case`;`erase_backticks` 逻辑(`tr_sql.h:35-44`)对 `tbl`/`mode` 字段去反引号,Rust 侧要复刻;JSON 注释可能不是合法 JSON(C 侧 `json_loads` 失败仅 debug 日志不中断),`serde_json::from_str` 失败要降级为"无 hint"而非报错。

---

### T3.3 分片路由(表→tablet)

**目标**:逻辑表名 → 物理分片(cluster_tablet),支持 LIST/HASH_MOD/MD5_HASH_MOD/RANGE 四种分片策略 + PCRE 路由规则 + force_tbl 覆盖。

**前置依赖**:T3.2(`QueryMeta.hints` 提供 force_tbl/fix_table/partition_id);T4.1(配置加载完 `tr_route_info_t`/`tr_hash_info_t`,但本卡可用最小配置先行)。

**实施步骤**:
1. 在 `src/route/mod.rs` 实现 `route_table(logic_table: &str, db_user: &DbUser, hints: &QueryHints) -> Result<Arc<ClusterTablet>>`。对照 `tr_route.c:3 find_cluster_tablet` 与 `:56 route_cluster_tablet`。
2. 路由规则解析(C 侧 `tr_route.c:13-50`):先查 `route_mapping`(精确匹配 HashMap);未命中则遍历 `route_pcre`(`sec_rule_node` 数组,`tr_config.h:146-151`),用 `regex` crate 替代 C 侧 `spreg_search_q`;仍未命中查 `"default"`。命中后调 `route_mapping_add` 缓存(对应 `tr_config.h:376`)。路由配置文件格式见 `conf/route_DBA_C_demo_db_user.conf`(`route_type`/`route_key`/`route_body` JSON)。
3. 分片策略在 `src/route/partition.rs`。对照 `tr_sql_partition.h:4-9 partition_by_t` 枚举(LIST/HASH_MOD/MD5_HASH_MOD/RANGE)与 `:55-64 tr_partition_info_s`(partition_key、partition_key_type、modulus、shift_bit、partition_map)。实现 `get_table_for_distributed_table(partition, value) -> PartitionItem`,对照 `tr_sql_partition.c:669`。
   - HASH_MOD:`generate_key_for_hash_mod`(`tr_sql_partition.c:562`),int 直接 `% modulus`,string 先转 hash。
   - MD5_HASH_MOD:对 value 做 MD5 取前 8 字节再 `% modulus`(对照 `tr_sql.c:625-633` 的 pid_md5sum 逻辑)。
   - RANGE:`tr_parse_range`(`:81`)+ `tr_range_compare`(`:79`)+ `GTree` range_map → Rust 用 `BTreeMap`。
   - LIST:`generate_key_for_list`(`:529`)+ `GHashTable item_list` → `HashMap<String, PartitionItem>`。
4. `force_tbl` hint 命中时直接跳过分片,用 `force_tablet_name` 查 `cluster_tablets`(`tr_conn.h:322-323` 语义)。
5. 子分片(`is_sub_partition`,`tr_sql_partition.h:51`)递归调用 `get_table_for_distributed_table`(对照 `:737`)。

**关键代码骨架/要点**:
```rust
pub enum PartitionBy { List, HashMod, Md5HashMod, Range }
pub struct PartitionInfo {
    pub partition_key: String,
    pub key_type: KeyType,            // Int/Float/Datetime/String
    pub by: PartitionBy,
    pub modulus: i32, pub shift_bit: i32,
    pub items: HashMap<String, Arc<PartitionItem>>,  // LIST
    pub range_map: BTreeMap<Range, Arc<PartitionItem>>, // RANGE
    pub default: Option<Arc<PartitionItem>>,
}
pub fn route_table(logic_table: &str, user: &DbUser, hints: &QueryHints) -> Result<Arc<ClusterTablet>>;
```
crate:`regex = "1"`(PCRE 路由规则,不兼容时换 `pcre2`)。

**验收操作**:`cargo test -p newproxy-route`;对照 C 版 `tr_sql_partition.c` 单测:hash_mod(modulus=4)对 id=0..7 命中 4 个分片;range 边界属于 `belongingness_t`(`tr_sql_partition.h:66-73` LEFT_OUT/LEFT/INNER/RIGHT/RIGHT_OUT);跑 `mysql_case/select/` 分片用例。

**常见坑/Rust 特有注意**:PCRE → Rust `regex` 语法不完全兼容(PCRE 支持反向引用、命名组等),先用 `regex` crate,若有用例不兼容换 `pcre2` crate(见 [01 §4](./01-rewrite-cost-assessment.md) 依赖映射);`route_mapping` 缓存带 `pthread_mutex_t`(`tr_route_info_s`),Rust 用 `DashMap` 或 `parking_lot::Mutex<HashMap>`;range 比较的 `belongingness` 枚举要完整复刻 5 种开闭区间语义,边界 off-by-one 是高频 bug。

---

### T3.4 SQL 改写(decomposer)

**目标**:将一条逻辑 SQL 拆解为 N 条物理子请求(每分片一条),WHERE 条件按分片裁剪,通过 AST vtable `to_string` 生成改写后 SQL。

**前置依赖**:T3.1(`AstHandle`);T3.3(分片命中结果 `PartitionItem`);T3.2(`QueryMeta`)。

**实施步骤**:
1. 在 `src/decomposer/mod.rs` 实现 `decompose(ast: &AstHandle, partition: &PartitionInfo, current_db: &str, hash_info: &HashInfo) -> Result<Vec<SubRequest>>`。入口对照 `tr_sql_decomposer.c:1903 sql_decomposer_decompose`,按 `sql_cmd->command` 分派到 insert/delete/update/select。
2. 各命令分解器对照 C 侧:
   - `decompose_insert_cmd`(`tr_sql_decomposer.c:1072`):INSERT 多值按分片拆行,`to_string` 生成子 SQL(`:1246`)。
   - `decompose_delete_cmd`(`:1446`):WHERE 条件交集判定(调 T3.3 的 `check_intersection`),每分片生成 DELETE。
   - `decompose_update_cmd`(`:1592`):同 delete,WHERE 裁剪 + `to_string`(`:1692`)。
   - `decompose_select_cmd`(`:1880`):单表走 `decompose_select_cmd_single_table`(`:1752`);WHERE 拆解到 `tablet_exprs_array_mappings`(`tr_sql_decomposer.h:41`),每分片生成 SELECT。
3. WHERE 拆解核心:遍历 `partition_sql_exprs` + `atomic_exprs`,对每个原子条件调 `check_intersection_by_hash_mod`/`check_intersection_by_range`(`tr_sql_partition.h:103-113`)判定与哪些分片相交,build `tablet_exprs_array_mappings`(`tr_sql_decomposer.h:64 build_tablet_exprs_array_mappings_recursively`)。
4. `to_string` 调用:AST 节点的 vtable,FFI 侧 `(*sql_cmd).to_string(buf, size, cmd)`(对照 `tr_sql_decomposer.c:1246/1531/1692/1828`),需在 FFI 封装层暴露该方法。子请求结构 `SubRequest { tablet: Arc<ClusterTablet>, sql: String }`(对照 `tr_sql_decomposer.h:25-28 sql_decomposer_elem_t`)。
5. AVG 改写:SELECT 里的 AVG 在分片阶段改写为 SUM+COUNT(对照 `tr_sql_cmd.h:6 rewrite_field_avg_to_sum_count_in_sql_cmd`),合并阶段再算回 AVG——这是合并正确性的前提。
6. `tr_get_sub_requests`(`tr_sql_decomposer.c:2043`)是顶层编排,串起 parse→decompose→to_string;Rust 侧由 `handle_query` 在 guard 作用域内调用。

**关键代码骨架/要点**:
```rust
pub struct SubRequest { pub tablet: Arc<ClusterTablet>, pub sql: String, pub merge_attr: MergeAttributes }
pub fn decompose<'g>(ast: &AstHandle<'g>, pinfo: &PartitionInfo, db: &str, hinfo: &HashInfo)
    -> Result<Vec<SubRequest>>;
// FFI 暴露 to_string:
pub unsafe fn ast_to_string(cmd: *mut sql_command_t, buf: &mut [u8]) -> Result<usize>;
```

**验收操作**:`cargo test -p newproxy-decomposer`;对照 C 版逐 SQL 类型:INSERT 多值拆分、UPDATE 带 id 分片、SELECT 无分片键走全分片(`switch_query_with_pk=0` 时);跑 `mysql_case/update/`、`mysql_case/select/`、`mysql_case/replace/`。

**常见坑/Rust 特有注意**:整个 decompose **必须在 `ParserGuard` 存活期间完成**(AST 指针指向 pool),且作用域内禁 `.await`([05 §6](./05-parser-pool.md));`to_string` 的 buffer 要足够大(C 侧 `MAX_SQL_REWRITE__SIZE`),Rust 用 `Vec<u8>` 动态扩容,返回实际长度;AVG→SUM/COUNT 改写若漏掉,合并阶段 AVG 会算错(必现);`is_prepare` 标志(`tr_sql_decomposer.h:33`)影响 prepared 场景,与 T3.7 联动。

---

### T3.5 scatter/gather + 结果合并

**目标**:并发 fan-out 子请求到多 backend,合并结果集(min-heap 归并排序 + GROUP BY 聚合 + AVG/COUNT DISTINCT + LIMIT 下推)。

**前置依赖**:T3.4(`Vec<SubRequest>`);T2.3(`acquire_or_connect`/`release_backend`)。

**实施步骤**:
1. 在 `src/exec/distributed.rs` 实现 `exec_distributed(sub_reqs, ctx) -> Result<MergedResult>`,按 [02 §5](./02-concurrency-model.md) 用 `futures::future::try_join_all` 并发。每子请求 `acquire_backend` → `run_backend_query` → 收集 `result_packet_t`。
2. 合并核心在 `src/merge/mod.rs`,对照 `tr_result.c`。入口 `merge_query_results`(`tr_result.h:49`),分两类:`merge_without_result_set`(`tr_result.h:47`,OK 结果合并 affected_rows)与有结果集合并。
3. min-heap 归并:对照 `tr_result.c:1255 min_heap`,数据结构 `heap_item_t`(`tr_result.h:4-7`:row_ptr + sub_req_number)+ `min_heap_param_t`(`:9-24`:order_fields、limit_param、send_buf)。Rust 用 `BinaryHeap<HeapItem>`(小顶堆需 `Reverse` 或自定义 `Ord`),按 `order_fields` 比较行。
4. 聚合合并:
   - SUM/COUNT/MIN/MAX:`merge_grouped_calculate_rows_pop`(`tr_result.c:1761`),GROUP BY 同组累加。
   - AVG:`merge_avg_field_in_result_set`(`:1590`),用 T3.4 改写出的 SUM/COUNT 列算回 AVG(`:1633` 合并 index/index+1)。
   - COUNT DISTINCT:`merge_count_distinct_rows_pop`(`:1681`)+ `merge_count_distinct_rows_finish`(`:1700`),受 `switch_count_distinct` 配置控制。
5. LIMIT 下推:`min_heap_param_t.limit_offset/limit_rows`(`tr_result.h:14-15`),每分片下推 `LIMIT offset+rows`,合并后裁剪。受 `max_limit_of_select` 配置限制。
6. 合并后流式回前端([04 §6.2](./04-mysql-protocol-statemachine.md)),列定义来自第一个分片的结果集 header。

**关键代码骨架/要点**:
```rust
pub async fn exec_distributed(reqs: Vec<SubRequest>, ctx: &Arc<AppCtx>) -> Result<MergedResult>;
pub fn merge_query_results(sub: Vec<SubResult>, attr: &MergeAttributes) -> Result<MergedResult>;
// 纯函数合并,无并发
struct HeapItem<'a> { row: &'a [Bytes], sub_idx: usize, order: &'a [FieldIndex] }
impl Ord for HeapItem<'_> { ... }   // 按 order_fields 比较
```
crate:`futures = "0.3"`(`try_join_all`)、`bytes = "1"`(行数据)。

**验收操作**:**M3 里程碑**——`mysql_case/totalTest.sh` 分片子集通过;重点验证 `AVG` 合并(`newproxy_case` 有专例)、`COUNT(DISTINCT)`、`ORDER BY ... LIMIT` 跨分片、`GROUP BY` 聚合;对照 C 版 `tr_result.c` 行为。

**常见坑/Rust 特有注意**:`merge_query_results` **必须是同步纯函数**([02 §5](./02-concurrency-model.md)),并发在 `exec_distributed` 层,合并层不持锁不 await;min-heap 比较函数要处理 NULL 排序(MySQL NULL 最小);AVG 的 SUM/COUNT 列定位靠 `avg_fields[i].ref_sum_field_index`(`tr_result.c:647`),字段索引映射错位是典型 bug;合并需全量缓冲(不能流式,业务语义决定,见 [04 §6.2](./04-mysql-protocol-statemachine.md));`max_sub_request_in_process` 配置限制并发子请求数,超出要排队或报错。

---

### T3.6 读写分离

**目标**:SELECT 走 slave、写走 master;支持 `write_time_interval` 内读强制走主、表级读写分离白名单、`/*{router:m}*/` hint 覆盖。

**前置依赖**:T3.3(路由产出 `ClusterTablet`,含 master_dbs/slave_dbs);T3.2(hint 的 `comment_router_type`)。

**实施步骤**:
1. 在 `src/route/rw_split.rs` 实现 `pick_master_slave(sql_type, hints, session, ct, cfg) -> MasterSlave`。对照 `tr_const.h:132-133`(MS_MASTER=1/MS_SLAVE=2)。
2. 规则优先级(从高到低):
   - `hints.router == Some(MS_MASTER)` → master(`tr_sql.c:586-587`);`Some(MS_SLAVE)` → slave(`:588-589`)。
   - 写操作(INSERT/UPDATE/DELETE/REPLACE/DDL)→ master(SQL_WRITE_NUM,`tr_packet.h:203`)。
   - 事务中(`parse_check_is_tx` 或 session 在事务)→ master。
   - `write_time_interval` 内:session 最后一次写时间 + `write_time_interval`(us,配置项)未过期 → 读走 master(对照 `conf/newproxy.conf:69` write_time_interval=200000)。
   - 表级读写分离:`switch_table_read_write_split=1` 时,查 `table_read_white_ht`(`tr_multi_site.c:7 query_check`),白名单内的表才走 slave。
   - 其余 SELECT → slave。
3. 只读集群拒绝写:`cluster_is_readonly=1`(`conf/newproxy.conf:152`)时 master 不接查询,写操作报错(对照 `tr_multi_site.c:18-23` 返回 RET_ERROR)。
4. slave 不可用时降级到 master:调 T2.4 的 `network_database_available`(`tr_route.c:94`)判定。
5. 与 T2.3 联动:`acquire_or_connect(pool, ct, ms)` 的 `ms` 参数由本卡决定。

**关键代码骨架/要点**:
```rust
pub fn pick_ms(sql_type: SqlType, hints: &QueryHints, sess: &Session, ct: &ClusterTablet, cfg: &Config) -> MasterSlave;
// session 状态:
pub struct Session { last_write_at: Option<Instant>, in_txn: bool, ... }
```

**验收操作**:`mysql -e "SELECT /*{router:s}*/ ..."` 抓包确认走 slave;`mysql -e "BEGIN; SELECT ..."` 确认走 master;`switch_table_read_write_split=1` 配置下白名单外表走 master;对照 `newproxy_case/case/` 读写分离用例。

**常见坑/Rust 特有注意**:`write_time_interval` 用 `Instant` 记录最后写时间,跨 task 迁移时 `Session` 是 task 局部变量无需锁;`query_check` 的 `parse_has_dts`(DTS 同步工具标记,`tr_multi_site.c:18`)影响写白名单判定,需从 hint 透传;slave 全部不可用时降级 master 要记 metric(`period_read_miss_num`,`tr_multi_site.c:14`)。

---

### T3.7 预编译语句(prepare/execute)

**目标**:COM_STMT_PREPARE 缓存解析后 AST(独立 mem_pool 深拷贝),COM_STMT_EXECUTE 绑定 binary 参数后 re-exec 跳过解析,正确处理 MYSQL_TYPE_NULL 边界。

**前置依赖**:T3.4(decompose 复用);T3.1(`AstHandle` + 独立 pool clone)。

**实施步骤**:
1. 在 `src/stmt/mod.rs` 实现 `PreparedStmt`,对照 `tr_stmt_prepare.h:52-71 stmt_ctx_t`。字段:`stmt_id`、`sql`、`own_pool`/`own_parser`(独立 mem_pool)、`ast`(`*mut sql_command_t`,深拷贝自 `mem_pool_copy`)、`param_count`、`param_list`、`fields_array`、`res_array`。
2. AST 深拷贝:对照 `tr_stmt_prepare.c:1175 generate_stmt_ctx_for_prepare_sql` + `:1206 mem_pool_copy(stmt->mempool, parser->pool)`。Rust 侧 FFI 调 `mp_init` 新建 pool + `mem_pool_copy`(`tr_sql.h:85`)复制 AST。**前提**:需 C 侧暴露 `clone_command` 或确认 `mem_pool_copy` 足够([05 §7](./05-parser-pool.md) 已标注此协调点)。
3. `handle_prepare(sql)`:解析 → 深拷贝 AST 到独立 pool → `find_sql_markers_in_sql_cmd`(`tr_sql_cmd.h:4`)定位 `?` 参数 → 缓存到 `PreparedRegistry`(task 局部 `HashMap<u32, PreparedStmt>`,对照 `stmt_ctx_array`,`tr_stmt_prepare.h:73-75`,MAX_STAT_PER_SESSION=64)→ 发 COM_STMT_PREPARE_OK。
4. `handle_execute(payload)`:解析 binary protocol 参数,对照 `tr_packet.c:2963 get_prepare_params` + `:2799 fill_prepare_param`。null bitmap(`is_param_null`,`tr_stmt_prepare.h:101`)+ type(`get_param_type`,`:107`)+ value(`get_param_from_buffer`,`:125`)。参数绑定到 AST marker(`replace_sql_cmd_with_stmt`,`tr_sql_cmd.h:18`)→ 走 T3.4 decompose + T3.5 exec。
5. `MYSQL_TYPE_NULL` 边界:这正是 `test/asan/` 试图复现的 1 字节堆溢出(C 侧 `get_param_from_buffer` 对齐问题,导致 jemalloc free-time coredump)。Rust 用 `Buf` trait 的边界检查 + `get_param_from_buffer` 严格 `buf_len` 校验,从根上杜绝。注意:`test/asan/` 目录当前仅有 Makefile,其 C 源文件(`stmt_param_oob_test.c` 等)缺失,需按 Makefile 意图自行补写驱动,或直接在 Rust 侧写等价单测验证边界安全。
6. `handle_stmt_close`:`free_stmt_prepare_cache`(`tr_stmt_prepare.h:147`),Drop `PreparedStmt` 调 `sql_parser_free` + `mp_free`。
7. `gen_stmt_id`(`tr_stmt_prepare.h:77`)对照实现;`MAX_STMT_ID=65536`(`:5`)。

**关键代码骨架/要点**:
```rust
pub struct PreparedStmt {
    stmt_id: u32, sql: String,
    own_pool: Own<MemPool>,           // Drop 时 mp_free
    ast: *mut sql_command_t,
    param_types: Vec<ParamType>, num_params: u16, num_columns: u16,
}
impl Drop for PreparedStmt { fn drop(&mut self) { unsafe { ffi::sql_parser_free(self.own_parser); ffi::mp_free(self.own_pool); } } }
pub fn parse_execute_params(payload: &[u8], types: &[ParamType]) -> Result<Vec<ParamValue>>;   // 严格边界
```

**验收操作**:在 Rust 侧写 `MYSQL_TYPE_NULL` 参数解析单测,`RUSTFLAGS="-Zsanitizer=address" cargo +nightly test` 验证无越界(`test/asan/` 的 C 源当前缺失,不可直接迁移);`mysql_case` prepare/exec 用例;对照 C 版 `tr_stmt_prepare.c` 行为;`MAX_STAT_PER_SESSION=64` 超限报错。

**常见坑/Rust 特有注意**:`mem_pool_copy` 是深拷贝整个 arena,AST 指针在新 pool 内有效——这是 prepared 能脱离原 guard 存活的关键([05 §7](./05-parser-pool.md));若 C 侧无 `clone_command` 入口,降级为 re-parse(性能略损,风险登记已列);binary protocol 的 null bitmap 字节序与位数计算(MySQL 协议:每 8 参数 1 字节,bit = param_index % 8)易错;`Drop` 里 FFI 调用是 unsafe,需确保 pool 未被重复释放(`own_pool` 用 `Option` 包裹 take 模式)。

---

## P4 — 配置/metric/管理/日志

### T4.1 完整配置解析

**目标**:解析 `conf/newproxy.conf` 全部字段 + 路由/哈希配置文件,产出强类型 `Config` 结构,覆盖所有 section 与 switch_/enable_ 开关。

**前置依赖**:T0.3(最小配置骨架);T3.3(需了解 `PartitionInfo`/`RouteInfo` 形状以决定配置加载边界)。

**实施步骤**:
1. 在 `src/config/mod.rs` 定义 `Config` 结构,对照 `tr_config.h:276-383 tr_config_s`。顶层字段:port、mng_port(`:279`)、max_threads、log_*、timeout_*、conn_pool_socket_max_serve_client_times(`:299`)、write_time_interval(`:300`)、stream_on、default_charset 等。
2. switch_/enable_ 字段共约 13 个(已核实 `conf/newproxy.conf`):switch_status_check、switch_table_read_write_split、switch_set_autocommit、switch_status_forbid、switch_same_db_username、switch_select_1_status、switch_detect_status、switch_query_with_pk、switch_count_distinct、switch_result_resort、switch_tablet_restrict、skip_grant_enable、stream_transport_enable。映射到 `tr_config.h` 的 `status_check_enable`/`table_read_write_split_enable`/`set_autocommit_enable`/`status_forbid_enable`/`same_db_username_enable`/`switch_select_1_status`/`switch_detect_status`/`switch_query_with_pk`/`count_distinct_enable`/`result_resort_enable` 等。注意:配置文件的 `switch_*` key 名与结构体字段名(`*_enable`)常有差异(如 `switch_status_check`→`status_check_enable`)。
3. Section 解析(对照 `conf/newproxy.conf`):`[MySQL_Proxy_Layer]`、`[Cluster_N]`(`tr_config.h:202-207` cluster_name + cluster_tablets + is_single_cluster)、`[CTablet_N_M]`(`:191-200` cluster_tablet_name + master_dbs + slave_dbs + master_weight/slave_weight)、`[Master_Host_N]`/`[Slave_Host_N]`(`:172-175` host/port/max_connections/max_conn_pool_size/connect_timeout/weight/local_level)、`[DB_User_N]`(`:223-235` db_username/db_password/default_db/table_route_file/table_hash_file/cluster_name)、`[Product_User_N]`(username/password/db_username/max_connections/up_limit_*/work_log_threshold)、`[Auth_IP_*]`/`[Ignore_IP]`。C 侧解析入口 `tr_config.c:2284 tr_read_config`,用 GLib `GKeyFile` 按前缀分派到 `tr_init_mysql_proxy_layer`(`:1670`)/`tr_init_cluster`(`:1417`)/`tr_init_cluster_tablet`(`:1305`)/`tr_init_database_by_group`(`:1500`)/`tr_init_db_user`(`:1104`)/`init_product_user`(`:336`)/`init_auth_ip`(`:134`)/`init_ignore_ip`(`:72`)。
4. 路由配置文件:解析 `table_route_file`(JSON,格式见 `conf/route_DBA_C_demo_db_user.conf`:`route_type` list/pcre、`route_key`、`route_body` 映射表)→ `RouteInfo`(`tr_config.h:210-217`:route_type/route_key/route_pcre/route_mapping)。解析 `table_hash_file`(JSON,格式见 `conf/hash_DBA_C_demo_db_user.conf`:`tablename`→{hashtype,hashnum})→ `HashInfo`(`tr_config.h:219-221` hash_mapping)。
5. 用 `serde` + 自定义 section 解析(GLib `GKeyFile` 风格的 INI,section `[X]` + `key = value`)。C 侧 `tr_config.c:1700` 逐行 `strtoul`,布尔字段用 `if (config->field != 0) config->field = 1;` 钳制到 0/1。
6. 非法配置报错:port 范围校验(对照 `tr_config.c:1701 is_port_valid`)、必填字段缺失、cluster/tablet 引用完整性。默认值对照 `tr_config.c:2095 tr_default_config`(如 `mng_port=9111`@`:2142`)。
7. `Config::load(path)` 返回 `Result<Config>`,支持 `ArcSwap<Config>` 发布([02 §9](./02-concurrency-model.md))。

**关键代码骨架/要点**:
```rust
pub struct Config {
    pub port: u16, pub mng_port: u16, pub max_threads: usize,
    pub log: LogConfig, pub timeout: TimeoutConfig, pub pool: PoolConfig,
    pub switches: Switches,   // 13 个 bool
    pub clusters: HashMap<String, Cluster>, pub tablets: HashMap<String, ClusterTablet>,
    pub db_users: HashMap<String, DbUser>, pub product_users: HashMap<String, ProductUser>,
    pub auth_ips: Vec<IpAddr>, pub ignore_ips: Vec<IpAddr>,
}
pub fn load(path: &str) -> Result<Arc<Config>>;
```
crate:`serde = { features=["derive"] }`、`serde_json = "1"`(路由/哈希文件)、`arc-swap = "1"`(发布)。

**验收操作**:`cargo test -p newproxy-config` 用 `conf/newproxy.conf` + `conf/local.newproxy.conf` 双 fixture;字段缺失时错误信息清晰(对照 C 侧 `printf("MPL:...")`);全字段对照 `conf/newproxy.conf` 逐项 diff。

**常见坑/Rust 特有注意**:`GKeyFile` 允许重复 key(后者覆盖)、section 顺序无关、注释 `#` 与 `;`;`local.newproxy.conf` 字段比 `newproxy.conf` 少(如无 mng_port),要兼容——每个字段给 `Default`;路由文件 JSON 的 `default` 键是特殊路由;`same_db_username_enable` 影响 `DbUser.username` 拼接规则(`tr_config.h:223 username = cluster+login`),不要简化;`switch_tablet_restrict` 在文件中位于 `[Product_User_1]` 段下(`conf/newproxy.conf:228`),但逻辑上属 product user 属性,解析时注意归属。

---

### T4.2 metric 上报

**目标**:周期上报 proxy 维度 metric(QPS/延迟/连接数/错误计数/池命中)到 statsd,用 `AtomicU64` + `DashMap` 无锁累加。

**前置依赖**:T2.1(`AppCtx` 可注入 metric handle);T4.1(配置读 ns_newproxy)。

**实施步骤**:
1. 在 `src/metric/mod.rs` 定义 metric 结构,对照 `tr_metric.h:36-74 tr_product_user_metric_s`。字段:`current_conn_num`、`period_conn_qps`、`period_delay_num`、`period_frond_conn_error_num`、`period_backend_conn_error_num`、`period_front_auth_error_num`、`period_router_error_num`、`avg_sql_parser_time`、`avg_query_total_time`、`period_read/write_hit/miss_num`、`limit_qps_num`、`grey_list`、`global_list`、`mysql_error`(HashMap<errno,count>)。
2. 实例维度 metric 对照 `tr_metric.h:15-27 tr_db_metric_s`:`current_conn_num`、`conn_num_in_pool`、`conn_create`、`conn_destory`、`db_traffic`、`last_fail_time`。key 格式 `ct_name->host->port->m/s`(`tr_metric.h:116-135 tr_db_metric_key_`)。
3. 累加用 `AtomicU64`([02 §8](./02-concurrency-model.md)),热路径 `fetch_add(Relaxed)` 无锁。复杂统计按 user/db 聚合用 `DashMap<String, AtomicCounters>`。**statsd 目标在 C 侧是硬编码的 `127.0.0.1:788`**(`tr_metric.c:3-4` 的 `METRIC_STATSD_IP`/`METRIC_STATSD_PORT`),非配置项——首版保持硬编码以行为等价。
4. Reporter task:独立 `tokio::spawn`,周期 tick(对照 `tr_metric.c:378 tr_metric_main`,间隔 `TR_METRIC_REPORT_INTERVAL_SEC`)。snapshot 各原子计数 → reset → 上报。每周期末把所有 `period_*` 计数器清零(`tr_metric.c:444-466`);每约 300s 调一次 `sql_statistic_flush()` + `ip_statistic_flush()`(`tr_metric.c:513-516`)。
5. 上报通道对照 `tr_metric.h:6-13 tr_odin_metric_s`:statsd UDP(`statsd_fd`)。C 侧 `tr_odin_metric_report`(`tr_metric.c:114`)用 `snprintf` 拼 statsd 格式串。Rust 用 `cadence` crate 或手写 UDP。上报字段对照 `tr_metric.c:128-296` 的注释清单(conn_dispatch_error/front_auth_error/backend_auth_error/parsing_pkg_error/qps/delay/router_error/avg_parser_time 等)。
6. **SQL/IP 统计不在 `tr_stat.c`**:滚动计数器(`sql_statistic_list`/`ip_statistic_list`)在 `tr_sql.c`(`sql_statistic_flush`@`:2001`、`ip_statistic_flush`@`:2083`),由 instance 级 hashtable + 自旋锁支撑,结构对照 `tr_sql.h:11-20 tr_sql_statistic_info_s`(count/total_time_cost/max/min)。`tr_stat.c` 是**按需快照**模块,为 `checkproxy status`/`processlist` 等命令构建瞬时状态(如 `get_proxy_status`@`:245`、`get_client_conns_info`@`:55`),不是周期计数器。
7. 上报输出两条路径:(a) UDP statsd 数据报到 `127.0.0.1:788`;(b) `log_status(...)` 行写入 `.status` 日志文件。**无 HTTP、无管理端口暴露。**
8. `tr_metric_init`(`tr_metric.h:223`)对照:初始化 `db_user_metrics`/`prod_user_metrics`/`db_metrics` 三个 HashMap。

**关键代码骨架/要点**:
```rust
pub struct ProdUserMetric {
    pub current_conn: AtomicU64, pub period_qps: AtomicU64, pub period_delay_us: AtomicU64,
    pub front_auth_err: AtomicU64, pub backend_auth_err: AtomicU64, pub router_err: AtomicU64,
    pub avg_parser_us: AtomicU64, pub avg_total_us: AtomicU64,
    pub read_hit: AtomicU64, pub read_miss: AtomicU64, pub mysql_error: DashMap<i32, AtomicU64>,
}
pub async fn spawn_metric_reporter(ctx: Arc<AppCtx>, interval: Duration);
fn statsd_line(name: &str, val: i64, ns: &str, user: &str) -> String;
```
crate:`cadence = "0.29"`(statsd 客户端)、`dashmap = "5"`(聚合)。

**验收操作**:`cargo test -p newproxy-metric`;起 statsd mock(`nc -ul 788`)抓上报包,对照 C 版字段名;周期上报后计数 reset 正确(原子 swap);对照 `tr_metric.c:128-296` 逐 metric 名 diff。

**常见坑/Rust 特有注意**:reporter 的 snapshot+reset 必须用 `swap(0, Relaxed)` 而非 `load`+`store`(否则丢增量);statsd UDP 是 fire-and-forget,不要因上报失败影响主路径;`ALIGN64`(`tr_metric.h:22`)是 C 侧缓存行对齐避免 false sharing,Rust 可用 `#[repr(C)]` + `#[align(64)`](或 `core::mem::align_to`),热计数器建议分 cache line;`mysql_error` 的 errno key(如 1062)受 `filter_errno_log` 配置影响;avg 时间用累计 sum/count 算,不是存最近值。

---

### T4.3 管理端口(mng_port)

**目标**:实现管理命令(show stats/processlist/reload/switch_master/delay),show 类返回正确数据,reload/switch_master 返回"未实现请重启"。

**前置依赖**:T4.1(配置含 mng_port、watchdog user);T4.2(show stats 读 metric)。

**实施步骤**:
1. 在 `src/mng/mod.rs` 实现管理命令分发。**关键事实**(已核实):C 侧管理命令**通过业务端口(4051)以 `checkproxy xxx` SQL 形式接入**,由 watchdog 用户(`is_watchdog_user`,`tr_config.h:261`)执行,**而非独立 mng_port 监听**。`mng_port=9111` 仅是配置项,C 侧从未对其 bind socket(仅在 `tr_reload.c:634-636` 比较"不可 reload,建议 restart")。命令解析入口 `is_proxy_statue_cmd`(`tr_sql.c:370-556`),分发在 `make_proxy_status_result_packet`(`tr_packet.c:1615`),枚举 `CHECK_TYPE_*`(`tr_packet.h:59-85`)。
2. 命令清单对照 `tr_packet.h:59-85`:VERSION(0)、PROCESSLIST(1)/PROCESSLIST_ALL(2)、STATUS(3)、CONNECTS(4)、REMOTE_SLAVE(5)、CLUSTER(6)、RELOAD(7)、AUTHIP(8)、SHUTDOWN(9)、ROUTE(10)、HASH(11)、DELAY(12)/DELAY_ALL(16)、RELOAD_AUTHIP(13)、REMOTE_MASTER(14)、SWITCH_MASTER_WORK(15)、PRODUCT_USER(17)、RING_BUFFER_STATUS(18)、ADD/DEL_BLACK/WHITE(20-23)、BLACKLIST(24)/GREYLIST(25)。
3. 实现的命令:
   - **show 类**(STATUS/PROCESSLIST/CONNECTS/CLUSTER/ROUTE/HASH/VERSION/PRODUCT_USER):对照 `tr_packet.c:1615+` 各 case,返回 MySQL 结果集。PROCESSLIST 对照 `tr_stat.c:55 get_client_conns_info`(遍历连接状态 `STATE_FRONTEND_*`/`STATE_BACKEND_*`,`tr_stat.c:77-138`)。STATUS 对照 `tr_stat.c:245 get_proxy_status`(pool size、pid、uptime、`getrusage` vmsize)。响应协议是标准 MySQL 结果集包(`make_result_set_header_packet`/`make_field_packet`/`make_eof_packet`)。
   - **reload 类**(RELOAD/RELOAD_AUTHIP):C 侧调 `tr_do_reload`(`tr_mng.c:3`)/`tr_do_reload_auth_ip`(`:25`)。Rust 侧**返回结果集但内容为"未实现,请重启生效"**,对应 `tr_reload.c:634-636` 的 "Cann't reload mng_port, use restart" 语义。
   - **switch_master**(SWITCH_MASTER_WORK):C 侧 `tr_do_switch_master`(`tr_mng.c:105`)只翻转内存 `switch_master` 标志(不写配置、不强制重连)。Rust 侧返回"未实现,请重启"。
   - **SHUTDOWN**:C 侧 `checkproxy shutdown`(`tr_packet.c:2290`)**仅构造响应包,不直接触发关闭**——真正的优雅关闭走 SIGTERM(T4.5)。Rust 侧可同样仅返回响应,或直接触发 T4.5。
   - **DELAY/DELAY_ALL**:`tr_set_delay`(`tr_mng.c:51`,注入 `inject_delay_time`)。首版可返回"未实现"。
   - **monitor**:`write_monitor_info`(`tr_mng.c:136`)返回 JSON 监控信息(对照 `:152-153` 的字段:stream_transport_enable/switch_status_check 等),通过 MySQL 连接回写。
4. 命令识别:`is_proxy_statue_cmd`(`tr_sql.h:101`/`tr_sql.c:370`)解析 `checkproxy <subcmd>` 字符串 → `CHECK_TYPE_*`。watchdog 用户校验:非 watchdog 执行 reload 返回错误(对照 `tr_packet.c:2266 fill_checkproxy_reload_error_packet`)。前端命令路径入口 `tr_front_cmd.c:554`/`:776`(`SQL_PROXY_STATUS_NUM`)。
5. mng_port(9111)配置项保留(`tr_config.h:279`),但首版不单独监听——管理走业务端口 + watchdog 鉴权(与 C 版一致)。

**关键代码骨架/要点**:
```rust
pub enum CheckType { Version, Processlist, Status, Reload, ReloadAuthIp, Shutdown, SwitchMaster, Delay, ... }
pub fn handle_checkproxy(cmd: CheckType, ctx: &Arc<AppCtx>, is_watchdog: bool) -> MngResult;
// MngResult::ResultSet(Vec<Row>)  -- show 类
// MngResult::NotImplemented(&'static str)  -- reload/switch_master
```

**验收操作**:`mysql -h127.0.0.1 -P4051 -u watchdog -e "checkproxy status"` 返回正确统计;`checkproxy processlist` 列出连接;`checkproxy reload` 返回"未实现,请重启";对照 C 版 `tr_packet.c:1615+` 各 case 输出格式。

**常见坑/Rust 特有注意**:reload/switch_master **明确不实现**([06 §5](./06-task-plan.md) 推迟项),返回友好提示即可,不要细化 reload 逻辑;watchdog 鉴权必须在命令分发前,否则任意用户可触发 reload;`checkproxy` 命令是 SQL 文本解析,走正常 COM_QUERY 路径,在 `dispatch` 里特殊判定(对照 `is_proxy_statue_cmd`);PROCESSLIST 遍历连接需访问共享状态,用 `DashMap` 快照而非持锁遍历;`checkproxy shutdown` 不等于 SIGTERM,C 侧仅返回响应包。

---

### T4.4 日志(tracing)

**目标**:用 `tracing` crate 实现结构化日志,支持多级别、文件滚动、按类型分文件(work/wf/load/status),兼容 C 版日志格式。

**前置依赖**:T0.1(工程骨架);T4.1(读 log_dir/log_filename/log_level/log_maxsize 配置)。

**实施步骤**:
1. 在 `src/log/mod.rs` 初始化 `tracing_subscriber`。配置对照 `tr_log.h:4-9` 级别:NONE=0/ERROR=1/WARNING=2/WORK=4/LOAD=8/DEBUG=16,`log_level` 是位掩码(如 15=ERROR|WARNING|WORK|LOAD)。Rust 侧映射到 `tracing::Level`(ERROR/WARN/INFO/DEBUG),WORK→INFO、LOAD→INFO 单独 layer。
2. 文件分桶对照 `tr_log.h:110`:`["", ".wf", ".load", ".status"]` 四类(MAX_LOG_FILE_TYPE=4,`:11`)。WORK 日志进 `.log`,ERROR/WARNING 进 `.log.wf`,LOAD 进 `.log.load`,STATUS 进 `.log.status`。用 `tracing_appender::rolling` 或 `tracing_subscriber::fmt::layer` + 多 writer。
3. 滚动对照 `tr_log.c:210 if (st.st_size >= logger->maxsize)`:`log_maxsize`(MB,配置 `log_maxsize=1800` 即 1800MB)超限时关闭 + 带时间戳重命名(`tr_log.c:214 tr_format_filename`)+ reopen(`tr_log.c:194 my_fopen`)。检查频率最多每 10s 一次(`tr_log.c:255`)。Rust 用 `rolling::RollingFileAppender` 或自实现 size-based rotation。
4. 日志格式对照 `tr_log.h:96-119` 宏:`[E]`/`[W]`/`[D]`/`[L]`/`[S]`/`[M]`/`[ACCESS]` 前缀 + `log_template`(含 tid/时间/trace_id)。C 侧 `log_write`(`tr_log.h:76`/`tr_log.c:225`)是 `printf` 风格,级别过滤 `(level_num & config->log_level) == 0`(`tr_log.c:243`)。Rust 用 `tracing` 的 span/event + 自定义 formatter 复刻前缀。
5. trace_id/span_id:T3.2 已从 hint 解析 `trace_id`(`tr_sql.c:665+`),注入 span context。`tracing::span!` 携带 trace_id,与分布式追踪对接。C 侧 `logger->log_info.trace_id`(`tr_log.h:47`)在每条 `log_template`/`log_work` 行输出(`[%ld]`,`tr_log.h:96`),由 `log_info_update`(`tr_log.c:590`)从 `front->trace_id` 设置。
6. syslog 选项:`syslog_enable=1` 时同时输出到 syslog(`tr_log.c:22-28 openlog`,LOCAL3 facility)。Rust 用 `syslog` crate。
7. `log_query_min_time`:仅 query 耗时超过此阈值(us)才记 work 日志(对照 `conf/newproxy.conf:33`)。`max_log_per_info_size`(`:42`):单条 SQL 截断长度。`filter_errno_log=0`(`:45`):屏蔽 1062(duplicate entry)错误日志。

**关键代码骨架/要点**:
```rust
pub fn init_log(cfg: &LogConfig) -> Result<WorkerGuard> {
    let level = parse_level_mask(cfg.log_level);   // 位掩码 → LevelFilter
    let work_layer = fmt::layer().with_writer(FileWriter::new("newproxy.log")).with_filter(level_filter);
    let wf_layer = fmt::layer().with_writer(FileWriter::new("newproxy.log.wf")).with_filter(|m| m.level <= WARN);
    tracing_subscriber::registry().with(work_layer).with(wf_layer).init();
}
// 热路径:tracing::info!(target: "work", tid=%tid, sql=%sql, "query");
```
crate:`tracing = "0.1"`、`tracing-subscriber = "0.3"`、`tracing-appender = "0.2"`、`syslog = "6"`(可选)。

**验收操作**:跑代理后检查 `log/newproxy.log{,.wf,.load,.status}` 四文件生成;级别过滤:`log_level=1` 仅 ERROR;对照 C 版同操作日志格式 diff(前缀/时间/tid);`log_query_min_time` 阈值生效。

**常见坑/Rust 特有注意**:`tracing` 的 `Span` 进出有运行时开销,热路径(query 处理)用 `tracing::instrument` 要标注 `level` 避免无条件 enter;日志 writer 要异步(`tracing_appender` 的 non_blocking)避免 IO 阻塞 worker;C 侧 `log_buf` 是单线程 TLS 缓冲(`tr_log.c:100`),Rust 用 `tracing` 无需手写缓冲;`log_maxsize` 单位是 MB(C 侧 `*1048576`,`tr_log.c:83`),不要搞错量级;rotation 时文件名要保持(C 侧不改名直接 truncate reopen,`tr_log.c:194`),与运维采集脚本兼容。

---

### T4.5 优雅关闭

**目标**:SIGTERM → 停止 accept → drain 现有连接(等待或超时强制关)→ 关闭 backend 池 → 干净退出。

**前置依赖**:T2.1(conn_task 模型);T2.3(backend 归还);T4.3(SHUTDOWN 管理命令)。

**实施步骤**:
1. 在 `src/shutdown.rs` 注册信号处理。对照 `tr_signal.c:25-33 tr_init_signal`:SIGTERM 触发关闭,SIGPIPE/SIGINT/SIGHUP/SIGUSR1/SIGUSR2 忽略,SIGXFSZ(`:3`)打日志(磁盘满)。Rust 用 `tokio::signal::unix::signal(SignalKind::terminate())`。
2. 关闭流程对照 `tr_core.c:511 tr_core_shutdown` + `tr_signal.c:10-22 tr_sigterm_handler`:
   - 关 listen fd(`tr_signal.c:13 close(instance->main_cycle->id)`)→ Rust 侧 drop `TcpListener`。
   - `tr_core_graceful_shutdown`(`tr_reload.c:1130`)向每个 worker 的 notify pipe 写 `TR_SHUT_DOWN_SIGNAL`,CAS 设置 instance 状态为 `INSTANCE_STATUS_WAIT_DOWN`(shutdown)或 `INSTANCE_STATUS_WAIT_DESTROY`(reload)。
   - `force_close_connect`(`tr_core.c:485`)强制关在途前端连接;Rust 侧用 `CancellationToken` 广播给所有 conn_task。
   - `tr_ac_destroy`/`tr_ac_remove`(`tr_core.c:522-525`)清理前后端连接链表。
   - `alive_work_count` 减到 0(`tr_core.c:533`)→ `tr_instance_shutdown`(`:536`)→ `tr_is_exit=1` 通知主线程退出(`:541-545`)。
3. Rust 实现:`AppCtx` 持 `CancellationToken`;SIGTERM 触发 `cancel()`;accept loop `select!` 监听 cancel;conn_task 在 `.await` 点 `select!` 检查 cancel。drain 超时用 `graceful_shutdown_timeout`(`tr_config.h:355`)。C 侧 drain 逻辑在 `tr_cycle.c:187-226`:收到 `TR_SHUT_DOWN_SIGNAL` 后释放早期握手状态的连接,若 `active_front_conns.conn_num <= 0` 立即关闭,否则起 `graceful_shutdown_timeout` 定时器(`tr_core_timeout_shutdown`,`tr_core.c:479`);shutdown 期间新连接直接丢弃(`tr_cycle.c:236-240`)。
4. backend 归还:conn_task 退出前调 T2.3 的 `finalize_conn`([03 §4](./03-backend-pool-optimization.md))异步归还 backend,或 drain 超时后强制关。
5. `checkproxy shutdown` 管理命令(T4.3 的 CHECK_TYPE_SHUTDOWN)在 C 侧**仅构造响应包,不直接触发关闭**——真正的关闭走 SIGTERM。Rust 版可选择同样仅返回响应,或直接触发 `CancellationToken::cancel()`。
6. 进程退出:`tokio::runtime` 的 `shutdown_timeout` 确保所有 task 完成。

**关键代码骨架/要点**:
```rust
pub async fn run(ctx: Arc<AppCtx>, listener: TcpListener) -> Result<()> {
    let shutdown = ctx.shutdown_token.clone();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => { tracing::log!(LOAD, "draining..."); break; }
            acc = listener.accept() => { let (s,a) = acc?; tokio::spawn(conn_task(s,a,ctx.clone())); }
        }
    }
    drain(ctx.clone(), ctx.graceful_timeout).await;
    Ok(())
}
```
crate:`tokio-util = "0.7"`(`CancellationToken`)。

**验收操作**:`kill -TERM <pid>` 后无 "connection reset" 报错日志;`checkproxy shutdown` 行为对照 C 版(仅响应 vs 触发关闭);drain 期间新连接被拒;超时后强制退出;进程 exit code 0;对照 C 版 `tr_signal.c` 行为。

**常见坑/Rust 特有注意**:`tokio::signal::unix` 必须在 tokio runtime 内 spawn;`CancellationToken` 是 clone-cheap 的,每 conn_task 持一份;drain 不能无限等待(配置 `graceful_shutdown_timeout`),超时要强制关 backend(可能丢未提交事务,但优于挂死);`Drop` 不能 `.await`([03 §4](./03-backend-pool-optimization.md)),backend 归还必须在 task 退出前显式 `await`;SIGXFSZ(磁盘满)要处理,否则日志写失败 panic;C 侧 `force_close_connect` 会跳过管理员用户 `7R9ALrUkB%@*()`(`tr_core.c:485`),Rust 版若有类似特殊用户需保留。

---

## P5 — 集成/测试迁移/性能对齐

### T5.1 测试用例迁移

**目标**:让 `mysql_case/totalTest.sh`(662 用例)+ `newproxy_case/run.sh`(59 用例)驱动 Rust 版代理,建立通过率基线。

**前置依赖**:T3.5(M3 分片核心就绪);T4.1(配置可加载);T1.4(协议透传)。

**实施步骤**:
1. 阅读测试驱动脚本,确认接入方式(已核实):
   - `mysql_case/totalTest.sh` 是编排脚本,逐子目录调本地 `test.sh`,后者用 **`mysqltest` 二进制**(MySQL 官方 mysql-test-run 工具,非 JDBC),连接 `127.0.0.1 -P4051 -uprod_user -pprod_pass`。用例布局是标准 mysqltest 的 `t/newproxy_<name>.test`(输入)+ `r/newproxy_<name>.result`(期望输出),失败写 `.reject`。子目录顺序:hashfunction、settimezone、shadow、transaction、update、replace、select/common、select/tablet(delete、subquery 已注释禁用)。
   - `newproxy_case/run.sh` 是 **Python 2 `nosetests`** + `mysql.connector`(纯 Python MySQL 客户端,非 JDBC),产 HTML 报告。`lib/newproxy_case.py` 是 Python 2(注意 `print` 语句语法)。框架自带代理生命周期管理(start/stop `bin/newproxy conf/newproxy.conf`)。配置硬编码:`127.0.0.1:4051`、`prod_user`/`prod_pass`、`prod_user2`、特殊字符用户 `7R9ALrUkB%@*()`、db `test`、charset `utf8`。
   - `mysql_case/totalResult.sh` **不是结果聚合**,而是**期望结果生成器**:用 `mysqltest -r`(record 模式)直连后端 MySQL(端口 5897,db_user)重新生成 `.result` 金标准文件。
2. 准备 Rust 版运行环境:`cargo build --release` 产出 binary,用 `conf/local.newproxy.conf` 启动(后端指向测试 MySQL)。注意 `local.newproxy.conf` 的 `port=6666`、后端 `127.0.0.1:3306`、charset `gbk`;而运行时配置 `run_env/newproxy/conf/newproxy.conf` 用 `port=4051`、后端 `3307/3308`(master)/`5897/5898`(slave)。功能测试目标端口 4051。
3. 适配测试脚本:`mysql_case` 各 `test.sh` 硬编码 `mysqltest` 参数(host/port/user),若 Rust 版端口不同需改脚本或用环境变量。`newproxy_case` 框架管理代理生命周期(`bin/newproxy`),需把 `TRIBBLE_BIN` 指向 Rust 版 binary。`lib.conf` 的 `start_cmd=0`(0=正常启动,1=valgrind)。
4. 分类跑用例:`mysql_case/` 下 `select/common`(97 子用例)、`select/tablet`、`update/`、`transaction/`、`replace/`、`hashfunction/`、`settimezone/`、`shadow/`;`newproxy_case/case/` 下约 50 个 Python 测试模块(分片 `test_sql_partition_*.py` 最大 ~47KB、prepare、事务、shadow、读写分离、reload、charset、master switch、trace_id、whitelist 等)。逐子集跑,记录通过率。
5. 失败用例分析:按失败类型分类(协议字节差异/分片路由错误/合并结果错误/配置不支持)。逐个建 issue,关联到 T3.x/T4.x 修复。
6. 建立基线看板:用例总数 / 通过数 / 失败数 / 跳过数(不支持特性)。`build.sh` 的 `check_result()` grep `test.log` 中的 `not ok` 字符串做判定。

**关键代码骨架/要点**:无代码,主要是测试脚本适配 + 失败用例追踪表。

**验收操作**:`mysql_case/totalTest.sh` 与 `newproxy_case/run.sh` 全量跑通,Rust 版通过率对照 C 版基线(目标:首版 ≥ 95%,M3 达 100%);`totalResult.sh` 重新生成金标准可用;失败用例有归因。

**常见坑/Rust 特有注意**:测试依赖外部工具:MySQL `mysqltest` 二进制、Python 2 + `nose` + `mysql.connector`、sysbench、valgrind——仓库内**无 CI 配置**(无 `.gitlab-ci.yml`/`.github/`),`build.sh` 是事实上的本地构建+测试入口;`mysqltest` 对协议细节敏感(packet_id 连续性、EOF vs OK 包、charset handshake);`settimezone` 用例验证时区处理;`shadow` 用例验证 `parse_is_test_sql`;`hashfunction` 验证分片哈希一致性;测试用例可能依赖特定后端数据,需先 init;`newproxy_case` 是 Python 2,若环境无 Python 2 需移植到 Python 3 或用 Rust 测试替代;`newproxy_case` 框架自带 conf 修改/备份(`bak_conf/newproxy.conf.<name>`),Rust 版配置需兼容这些变体。

---

### T5.2 行为等价对照

**目标**:Rust 版与 C 版对同一用例的输出字节级 diff(packet_id/payload),除允许差异(时间戳/trace_id)外完全一致。

**前置依赖**:T5.1(测试可跑);T1.6(OK/ERR/EOF 包构造)。

**实施步骤**:
1. 搭建对照环境:C 版与 Rust 版并行跑,后端指向同一 MySQL(或两个相同数据的 MySQL)。用 `mysqltest` 对两边跑同一 `.test` 文件,diff `.result` 输出。
2. 抓包对照:用 `tcpdump`/`mitmproxy` 抓 C 版与 Rust 版对同一 SQL 的响应包。重点 diff:握手包(`build_handshake` 的 server_version/scramble)、OK/ERR/EOF 包(字段顺序、status flag)、结果集(列定义、行编码、lenenc 编码)。
3. packet_id 对照:C 侧 packet_id 按方向递增(`tr_packet.c:461`),Rust 侧 `seq` 维护要一致([04 §2](./04-mysql-protocol-statemachine.md))。握手后从 1 起,每包 +1。
4. 允许差异清单:server_version 字符串(若 Rust 版改了版本号)、scramble(随机值,但校验逻辑一致)、时间戳类结果(如 `NOW()`)、trace_id(若注入)。其余必须一致。
5. 分片查询对照:同一条分片 SQL,C 版与 Rust 版的子请求数、各子请求 SQL 文本(decomposer 输出)、合并后结果集行顺序都要一致。
6. prepare/execute 对照:COM_STMT_PREPARE_OK 的 stmt_id 分配策略、binary protocol 参数编码、re-exec 结果。**注意**:`test/asan/` 目录当前仅有 Makefile,C 源文件缺失,无法直接迁移 ASAN 用例——需在 Rust 侧自行写等价单测验证 `MYSQL_TYPE_NULL` 参数解析的边界安全。
7. 自动化:写 diff 脚本,批量跑用例,输出差异 packet 的 hex dump。

**关键代码骨架/要点**:无代码,主要是抓包 + diff 工具脚本。

**验收操作**:`mysql_case` + `newproxy_case` 全量字节级 diff,差异仅限允许清单;`MYSQL_TYPE_NULL` 参数解析 Rust 侧单测通过(Rust 边界检查从根上杜绝 C 侧的 1 字节堆溢出);分片合并结果(行顺序/聚合值)一致。

**常见坑/Rust 特有注意**:packet_id 不连续是高频 bug(Rust 侧某个分支漏 +1);EOF 风格(pre-8.0)必须发 EOF 包,不能偷懒发 OK([04 §1](./04-mysql-protocol-statemachine.md));lenenc 编码(<251 直接、<65536 两字节+0xFC、<16MB 三字节+0xFD、更大 8 字节+0xFE)要严格对照;浮点/decimal 的字符串表示可能因 Rust float 格式化与 C `sprintf` 差异(如 `1.0` vs `1`);NULL 值的行编码(0xFB)与空字符串区别;合并结果的行顺序:有 ORDER BY 时严格按 order_fields,无 ORDER BY 时 C 版的"min-heap 自然顺序"要与 Rust `BinaryHeap` 一致(注意 Rust `BinaryHeap` 是大顶堆,需 `Reverse`)。

---

### T5.3 性能压测对齐

**目标**:sysbench 压测 Rust 版,QPS ≥ C 版 80%、p99 不劣化、`tokio-console` 无锁热点。

**前置依赖**:T2.6(M2 100 并发稳定);T3.5(M3 分片就绪);T5.1(用例通过)。

**实施步骤**:
1. 确认压测入口(已核实):**`tools/load.sh` 和 `run_env/newproxy/load.sh` 不是负载生成器**,是 DJB-style supervise 的启停包装脚本。真正的 sysbench 压测在 `build.sh` 的 `run_sysbench()` 函数:
   ```
   taskset -c 8-47 sysbench run_env/script/select.lua \
     --mysql-host=127.0.0.1 --mysql-port=4052 \
     --mysql-user=prod_user --mysql-password=prod_pass --mysql-db=test \
     --db-driver=mysql --tables=10 --table-size=1000000 \
     --report-interval=10 --threads=$1 --time=$2 run
   ```
   通过 `build.sh pft` → `press_test 50 300`(50 线程、300 秒)调用。**注意压测端口是 4052**(与功能测试 4051 不同)。`run_env/script/select.lua` 极简,每个 event 只执行 `db_query("select 1")`。若需更丰富 workload(分片 SELECT/INSERT),自写 lua 脚本。
2. `press_test()` 还用 `top -p <pid>` + `mpstat -P 4,5,6,7` 采样 CPU/内存,写 `performance_stats.txt`(每核 CPU + avg mem)。
3. 压测参数:对照 `conf/local.newproxy.conf` 的 `max_connections=20`(product user),压测并发 16/32/64/100/200,时长 60-120s。后端 MySQL 配置与 C 版压测时一致。
4. 基线采集:先压 C 版,记录 QPS、p50/p95/p99、CPU、RSS。再压 Rust 版同参数。对比。
5. `tokio-console` 接入:Rust 版编译时启用 `tokio-console` subscriber(`tokio::task::Instrument`),运行时 `tokio-console <pid>` 观察任务调度、锁等待、唤醒频率。
6. 按 [03 §5.3](./03-backend-pool-optimization.md) 决策树定位瓶颈:若 p99 因池等待升高 → `tokio-console` 看是否 `AttrBucket` 桶锁热点 → 决定是否触发 T5.4。
7. 分场景压测:透传(单分片 `select 1`)、scatter/gather(多分片 fan-out)、prepare/execute、纯 SELECT(读分离走 slave)。当前 `select.lua` 仅覆盖 `select 1`,需补充场景。
8. 内存监控:压测期间 RSS 采样,确认无增长趋势(对照连接池复用)。

**关键代码骨架/要点**:无代码,压测脚本 + tokio-console 配置。

**验收操作**:Rust QPS ≥ C 版 80%(目标 100%);p99 不劣化(≤ C 版 ×1.2);`tokio-console` 无 task 长时间挂起、无锁等待告警;100 并发持续 120s 无错;RSS 稳定。

**常见坑/Rust 特有注意**:tokio runtime 线程数默认 = CPU 核数,若 `max_threads=1`(配置项)要映射到 `worker_threads=1`(对照 C 侧单 worker);`parking_lot::Mutex` 在无竞争时优于 std,但竞争激烈时仍可能成热点;async 的 task 迁移破坏 cache 局部性([02 §11](./02-concurrency-model.md)),热路径用 `BytesMut`/零拷贝;压测前确认后端 MySQL 不是瓶颈(后端 QPS 上限);`stream_transport_enable=1` 流式 vs 缓冲全量对内存/QPS 影响大,要对照 C 版配置;统计 `fetch_add` 在极高 QPS 下 cache line 弹跳,必要时分片计数(见 T4.2 ALIGN64);C 版 `taskset -c 8-47` 绑核,Rust 版若不绑核可能因调度差异影响对比,建议同样绑核。

---

### T5.4 锁竞争优化(按需)

**目标**:仅当 T5.3 发现桶锁热点时,按 [03 §5.2](./03-backend-pool-optimization.md) 优化阶梯升级,改善 p99。

**前置依赖**:T5.3(压测定位到桶锁热点)。

**实施步骤**:
1. 复核 T5.3 的 `tokio-console` 数据:确认 p99 升高由 `AttrBucket.write`/`read` 的 `Mutex` 等待导致(而非 IO/协议)。
2. 测量单桶并发 QPS,按 [03 §5.3](./03-backend-pool-optimization.md) 决策树:
   - < 5k → 阶梯 0(parking_lot)已足够,问题在别处。
   - 5k~20k → 阶梯 2:桶内分片 `Vec<Mutex<VecDeque>>`,hash 连接 id 取分片(如 `conn_id % N`)。
   - \> 20k → 阶梯 3:`crossbeam::TreiberStack` 无锁 LIFO。
3. 阶梯 2 实现:`AttrBucket` 内 `write: [Mutex<VecDeque>; N]` + `read: [Mutex<VecDeque>; N]`,acquire 时 `hash(conn) % N` 选分片,统计需聚合 N 个分片。
4. 阶梯 3 实现:`crossbeam::epoch` + `TreiberStack<Arc<BackConn>>`,LIFO 语义(最近归还优先,cache 更热,[03 §5.2](./03-backend-pool-optimization.md) 说明 LIFO 比 FIFO 更贴合)。ABA 由 crossbeam epoch 回收处理。
5. 优化后重跑 T5.3 压测,对比 p99 改善。

**关键代码骨架/要点**:
```rust
// 阶梯 2
struct AttrBucket { write: [Mutex<VecDeque<Arc<BackConn>>>; SHARDS], ... }
fn shard_idx(conn: &BackConn) -> usize { (Arc::as_ptr(conn) as usize) % SHARDS }
// 阶梯 3
struct AttrBucket { write: Atomic<TreiberStack<Arc<BackConn>>>, ... }
```
crate:`crossbeam = "0.8"`(epoch + TreiberStack)。

**验收操作**:压测后 p99 改善(目标:热点场景 p99 降幅 ≥ 20%);`tokio-console` 锁等待消失;功能回归(T5.1 用例不退化);无 double-put/泄漏(`compare_exchange` 契约不变)。

**常见坑/Rust 特有注意**:`crossbeam::TreiberStack` 的 `pop` 返回 `Option`,ABA 靠 epoch guard——在 async 上下文用 `crossbeam::epoch::pin()` 要小心跨 `.await`(pin 不能跨 await,应在同步临界区内完成 pop/push);阶梯 2 分片数 N 太小无效果、太大浪费内存,建议 = CPU 核数 × 2;阶梯 3 的 LIFO 语义改变连接复用顺序,可能影响后端 session 状态分布,需压测验证;优化是**按需**触发,不要过度设计([06 §5](./06-task-plan.md) 明确 T5.4 仅在压测热点时)。

---

### T5.5 稳定性长跑

**目标**:72h 持续压测无 crash、RSS 无泄漏趋势、连接数稳定、无连接泄漏。

**前置依赖**:T5.3(压测达标)。

**实施步骤**:
1. 搭建长跑环境:Rust 版代理 + 后端 MySQL(或 mock),sysbench 持续压测(混合 SELECT/INSERT/UPDATE,并发 50-100),持续 72h。用 `build.sh pft` 的 sysbench 模式或自写持续压测脚本。
2. 监控指标:
   - RSS:每分钟采样 `ps -o rss= -p <pid>`,绘图看趋势(对照 [05 §9](./05-parser-pool.md) pool 内存泄漏风险)。
   - 连接数:前端连接数、backend 池连接数(in_pool/in-use)、`served_times` 分布。
   - 错误率:5xx/error packet/连接 reset 计数。
   - QPS/p99:持续记录,看是否衰减(衰减提示内存泄漏或资源耗尽)。
3. parser 池监控([05 §9](./05-parser-pool.md)):`ParserPool` 的 `free.len()` 与 `cap` 关系,确认 guard 归还正常(pool 不耗尽)。
4. 周期注入异常:长跑中途 kill 一个 backend(MySQL),验证 failover(T2.4)与连接重建;backend 恢复后池自动补充。
5. 长跑结束:coredump 检查(无 crash)、ASAN 末段跑(若 Rust 版有 unsafe)、metric 累计值合理性。
6. 内存泄漏判定:RSS 上升趋势线斜率(允许 GC/分配器波动,但 72h 累计增长 < 10%)。

**关键代码骨架/要点**:无代码,监控脚本 + 判定标准。

**验收操作**:72h 无 crash;RSS 72h 增长 < 10%(对照 [06 §6](./06-task-plan.md));backend kill/恢复后无连接泄漏;`tokio-console` 末段无 task 堆积;metric 计数累计正确。

**常见坑/Rust 特有注意**:Rust 的 `Arc` 循环引用会导致内存泄漏(前端持 backend `Arc`,backend 绝不能持前端 `Arc`,[02 §4](./02-concurrency-model.md));`Drop` 里 spawn 的归还 task(`BackendGuard::drop`,[03 §9](./03-backend-pool-optimization.md))若 task 泄漏会累积;`CString` 在 FFI 边界每次解析都分配,确认 parser guard drop 后释放;`DashMap` 的 shard 在极高并发下可能不均衡;长跑中 `tokio` runtime 的 task 积压(若某 `.await` 不让出)需 `tokio-console` 末段抽查;backend `served_times` 到上限后不复用([03 §1](./03-backend-pool-optimization.md)),长跑会频繁建连,确认池 replenish 正常。

---

### T5.6 灰度准备

**目标**:部署脚本 + 回滚方案 + 监控告警接入,可在测试环境灰度,回滚 < 5min。

**前置依赖**:T5.5(长跑通过);T4.2(metric 上报);T4.3(管理端口);T4.5(优雅关闭)。

**实施步骤**:
1. 部署脚本:对照 C 版 `run_env/newproxy/` 目录结构与 `tools/supervise.newproxy`(DJB supervise 二进制)。Rust 版产出单一 binary(`cargo build --release` → `newproxy`),部署只需 binary + `conf/newproxy.conf` + 路由/哈希配置文件。写 `deploy.sh`:停旧 → 换 binary → 起 → 健康检查。C 版 `run_env/newproxy/load.sh` 是 supervise 启停脚本(写 `u`/`d`/`k`/`x` 到 `status/newproxy/control` FIFO),可复用或换 systemd。
2. 进程托管:C 版用 supervise(`tools/supervise.newproxy`,编译二进制)。Rust 版可复用 supervise 或换 systemd/supervisord,配置 `Restart=on-failure`、日志路径、`LimitNOFILE`。
3. 回滚方案:保留 C 版 binary(`newproxy.c.bak`),`rollback.sh` 5min 内切回:停 Rust → 换 C binary → 起。配置文件向后兼容(C 版能读 Rust 版用过的 conf)。
4. 监控告警接入:
   - T4.2 的 metric 上报到 statsd(`127.0.0.1:788`)+ `.status` 日志文件,接入现有看板(QPS/p99/error/连接数)。
   - 告警规则:QPS 突降、error rate > 阈值、连接数饱和、RSS 超限。
   - 日志采集:`log/newproxy.log{,.wf}` 接入现有日志系统。
5. 灰度策略:
   - 阶段 1:测试环境 100% 流量,观察 1 周。
   - 阶段 2:单集群小流量灰度(5% → 25% → 50% → 100%),每阶段观察 1-2 天。
   - 灰度切流靠上层负载均衡(Nginx/LVS)权重调整,不依赖 reload。
6. 健康检查:`checkproxy status`(T4.3)作为健康探针,或 TCP 端口探活。优雅关闭(T4.5)确保摘流后 drain。
7. 应急预案:crash 时 supervise 自动重启;连续 crash 告警 + 自动回滚。

**关键代码骨架/要点**:无代码,运维脚本 + 配置。

**验收操作**:测试环境灰度 1 周无异常;`rollback.sh` 实测 < 5min;监控看板数据正确;告警规则触发验证(人为 kill 触发);`kill -TERM` 优雅关闭无连接报错;`checkproxy status` 健康探针可用。

**常见坑/Rust 特有注意**:Rust binary 的 glibc 版本兼容性(编译机与部署机 glibc 版本对齐,或用 musl 静态链接 `x86_64-unknown-linux-musl`);配置文件兼容性:Rust 版若新增字段,C 版读到要忽略(向后兼容),反之亦然;日志格式与 C 版差异可能导致采集脚本解析失败(运维依赖 `.wf` 文件名与 `[E]`/`[W]` 前缀);灰度期间 C 与 Rust 并存时,backend 池共享同一 MySQL,注意连接数上限(`max_connections`);`mng_port` 9111 若被监控探针使用,Rust 版要保留(即使首版不单独监听,业务端口的 `checkproxy` 要可用);回滚后 metric 断点要可接受(不同版本 metric 标签区分)。

