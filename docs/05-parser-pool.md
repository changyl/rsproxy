# SQL 解析器对象池

> 承接 [02-并发模型](./02-concurrency-model.md) §7。newproxy 的 SQL 解析器(`modules/sqlparser`,~29.6K LOC)是自研 bison/flex C 解析器,**非线程安全**(C 侧靠每线程一份 + TLS)。本文档定义 Rust 版通过 FFI 复用该解析器、并以对象池串行化访问的方案。

## 1. 决策:FFI 保留 vs 替换

| 路径 | 成本 | 风险 |
|---|---|---|
| **FFI 保留解析器(采用)** | bindgen 封装 + 池化,~1–2 人月 | unsafe 指针遍历;但解析器是纯 C + 干净入口,可控 |
| 换 sqlparser-rs | +4–6 人月(重实现 MySQL 方言、DDL 扩展、unparse/clone) | newproxy 深度依赖 `to_string`/`to_stencil`/`clone` vtable,sqlparser-rs 的 `Display` 不等价 |

**采用 FFI 保留**。理由:30K LOC 语法是项目最大资产,newproxy 的分片/改写/合并核心(`tr_sql_decomposer.c`/`tr_sql_partition.c`/`tr_result.c`)直接遍历其 AST 并调用 vtable,替换等于重写 ~10K LOC 业务逻辑。

## 2. 解析器 API(已核实)

`modules/sqlparser/include/sql_define.h` 暴露的 C API(均在 `extern "C"` 块内,FFI 友好):

```c
// 内存池(arena):AST 节点全部 pool 分配
mem_pool_t *mp_init(unsigned int size);     // :3004
void       mp_clear(mem_pool_t *pool);      // :3014  重置(复用)
void       mp_free(mem_pool_t *pool);       // :3019

// 解析器
sql_parser_t *sql_parser_init(mem_pool_t *pool);  // :3026
void          sql_parser_clear(sql_parser_t *p);  // :3031  清状态(复用)
int           parse_sql(char *sql, sql_parser_t *p);  // :3036  唯一解析入口
void          sql_parser_free(sql_parser_t *p);   // :3041

// 解析结果落在 p->sql_cmd(sql_command_t *,AST 根)
// 错误落在 p->err_no / p->sql_syntax_error_str
```

**生命周期**:一个 `mem_pool_t` 可关联一个 `sql_parser_t`;解析后 `sql_parser_clear` 清状态但保留 pool(arena 复用),`mp_clear` 重置 arena。典型用法是**长期持有 pool + parser,每次解析前 clear**。

## 3. 非线程安全 → 对象池 + Mutex

C 侧用 `__thread` TLS 每线程一份 parser(`tr_cycle.h:22` 注释明确"非线程安全")。work-stealing 下 task 会迁移,`thread_local!` 持可变状态跨 `.await` 会 panic。

**方案**:全局 parser 对象池,`Mutex` 串行化每个 parser 的使用:

```rust
use std::sync::Arc;
use parking_lot::Mutex;

struct ParserPool {
    /// 空闲 parser 栈。池大小 ≈ 并发解析数,通常 N(线程数)足够
    free: Mutex<Vec<Box<RawParser>>>,
    /// 池上限(防止无界增长)
    cap: usize,
}

struct RawParser {
    pool: *mut ffi::mem_pool_t,     // ffi = bindgen 生成
    parser: *mut ffi::sql_parser_t,
}
unsafe impl Send for RawParser {}    // parser 不跨线程并发用(池保证),Send 安全
```

**为什么 `Send` 安全**:parser 本身非线程安全,但池保证**同一时刻一个 parser 只被一个 task 持有**(`Mutex` 串行 acquire/release),不存在并发访问。`Send` 表示可在线程间转移所有权——这正是对象池要做的(空闲 parser 从池线程转移到 task 线程)。声明 `unsafe impl Send` 是合理的,但**绝不能 `Sync`**(那是允许多线程同时访问,会出事)。

## 4. acquire / release(Guard 模式)

```rust
/// RAII guard:取用时锁池 pop,释放时 push 回池 —— 即便 task panic 也归还
pub struct ParserGuard<'a> {
    pool: &'a ParserPool,
    inner: Option<Box<RawParser>>,
}

impl ParserPool {
    pub async fn acquire(&self) -> ParserGuard<'_> {
        loop {
            {
                let mut free = self.free.lock();
                if let Some(p) = free.pop() {
                    return ParserGuard { pool: self, inner: Some(p) };
                }
                if free.len() < self.cap {
                    // 池未满且空:新建一个
                    let p = RawParser::new();
                    return ParserGuard { pool: self, inner: Some(Box::new(p)) };
                }
            }
            // 池满且空:等释放(轻量 yield,不持锁)
            tokio::task::yield_now().await;
        }
    }
}

impl<'a> Drop for ParserGuard<'a> {
    fn drop(&mut self) {
        if let Some(p) = self.inner.take() {
            // clear 状态以便复用(arena 保留)
            unsafe { ffi::sql_parser_clear(p.parser); }
            let mut free = self.pool.free.lock();
            free.push(p);
        }
    }
}
```

**关键:Guard 的 `Drop` 是同步的**,但只做 `sql_parser_clear`(纯 C 同步调用)+ push,无 `.await`,符合 `Drop` 约束。panic 安全靠 `Drop` 保证——parser 不会泄漏。

## 5. 解析封装:把 unsafe 关在边界内

```rust
impl<'a> ParserGuard<'a> {
    /// 解析 SQL,返回 AST 句柄。unsafe 限定在此方法内,不外泄
    pub fn parse(&mut self, sql: &str) -> Result<AstHandle, ParseError> {
        let p = self.inner.as_mut().unwrap();
        // C 要求可写、NUL 结尾的 char*。用 CString 拷贝(sql 归 Rust 所有)
        let c_sql = CString::new(sql).map_err(|_| ParseError::InteriorNul)?;
        let rc = unsafe { ffi::parse_sql(c_sql.as_ptr() as *mut c_char, p.parser) };
        if rc != 0 {
            let err = unsafe { CStr::from_ptr((*p.parser).sql_syntax_error_str) }
                .to_string_lossy().into_owned();
            return Err(ParseError::Syntax(err));
        }
        // 返回 AST 根的裸指针句柄 —— 生命周期绑定到 parser(pool)
        let cmd = unsafe { (*p.parser).sql_cmd };
        Ok(AstHandle { cmd, _marker: PhantomData })  // 借用 guard,见下
    }
}

/// AST 句柄:借用 ParserGuard,确保 AST(pool 内存活)不超过 parser 生命周期
pub struct AstHandle<'g> {
    cmd: *mut ffi::sql_command_t,
    _marker: PhantomData<&'g mut ParserGuard<'g>>,
}
```

**生命周期约束**:`AstHandle<'g>` 借用 `ParserGuard`,编译期保证 AST 指针不会逃逸出 guard 的作用域——这是 Rust 把"use-after-pool-clear"在编译期杜绝的机制。下游(decomposer/partition/merge)在 guard 存活期间遍历 AST,guard 释放时 AST 随 pool clear 失效。

## 6. 使用模式:短临界区

解析 + AST 遍历必须**在同一个 guard 作用域内**完成,因为 AST 指针指向 pool:

```rust
impl FrontConn {
    async fn handle_query(&mut self, sql: &str) -> Result<()> {
        // 1. 取 parser(可能短暂等池)
        let mut guard = self.ctx.parser_pool.acquire().await;
        // 2. 解析
        let ast = guard.parse(sql)?;
        // 3. 在 guard 存活期间完成 AST 遍历 + 改写 + unparse
        //    (tr_sql_decomposer / tr_sql_partition / tr_route 的逻辑)
        let sub_reqs = decompose(&ast, &self.ctx.config.load())?;
        let rewritten: Vec<String> = sub_reqs.iter()
            .map(|r| ast_to_string(&r.ast_node))   // 调 cmd->to_string vtable
            .collect();
        // 4. guard 在此 drop,parser 归还池,AST 失效
        drop(guard);
        // 5. 用 rewritten SQL 走 backend(此时已不持 parser)
        self.exec_distributed(sub_reqs, rewritten).await
    }
}
```

**注意**:步骤 3 的 AST 遍历是同步的(纯 CPU),持 guard 期间不 `.await`——这天然让 parser 临界区短(解析+遍历通常 <1ms)。如果遍历里混入 await(如查配置),会让 guard 跨 await 持有,拉长池占用——**应避免,把 await 移到 guard 作用域外**。

## 7. prepared 语句的 AST 缓存(特殊场景)

C 侧 prepared 语句**克隆 AST 到独立 pool** 以便 re-exec 复用(`tr_stmt_prepare.c:1201-1206`,`mem_pool_copy`)。这要求 AST 脱离原 parser 生命周期存活——与 §5 的"AST 不逃逸 guard"冲突。

**解法**:prepared 缓存时,给 AST **分配独立 mem_pool 并深拷贝**:

```rust
struct PreparedStmt {
    // 独立 pool + parser,AST 生命周期 = stmt 生命周期
    own_pool: *mut ffi::mem_pool_t,
    own_parser: *mut ffi::sql_parser_t,
    ast: *mut ffi::sql_command_t,   // 来自 mem_pool_copy
    // ...
}

impl PreparedStmt {
    /// 从一个解析结果深拷贝 AST 到独立 pool(对应 C mem_pool_copy)
    fn clone_ast(src: &AstHandle) -> PreparedStmt {
        let own_pool = unsafe { ffi::mp_init(ARENA_SIZE) };
        let own_parser = unsafe { ffi::sql_parser_init(own_pool) };
        let ast = unsafe { ffi::clone_command(src.cmd, own_pool) };  // 需要 C 侧暴露 clone 入口
        PreparedStmt { own_pool, own_parser, ast }
    }
}

impl Drop for PreparedStmt {
    fn drop(&mut self) {
        unsafe {
            ffi::sql_parser_free(self.own_parser);
            ffi::mp_free(self.own_pool);   // AST 随 pool 释放
        }
    }
}
```

**前提**:需要 C 侧暴露 `clone_command(cmd, pool)` 入口(或复用现有 `to_string` 后重新 parse,但损失 AST 标注)。这是 FFI 封装阶段需与 sqlparser 维护者协调的一点——若 C 侧无现成 clone 入口,可作为阶段 1 的小改动提交。

## 8. bindgen 配置

```toml
# Cargo.toml
[build-dependencies]
bindgen = "0.69"

[dependencies]
parking_lot = "0.12"
tokio = { version = "1", features = ["full"] }
```

```rust
// build.rs —— 生成 sql_define.h 的绑定
fn main() {
    println!("cargo:rerun-if-changed=modules/sqlparser/include/sql_define.h");
    bindgen::Builder::default()
        .header("modules/sqlparser/include/sql_define.h")
        .allowlist_type("sql_.*|mem_pool_t|sql_parser_t")
        .allowlist_function("mp_.*|sql_parser_.*|parse_sql|sql_item_.*")
        .allowlist_var("CMD_.*|SQL_.*")
        .derive_default(true)
        .generate()
        .expect("bindgen failed")
        .write_to_file(Path::new(&out_dir).join("sqlparser_ffi.rs"))
        .unwrap();
    // 链接 sqlparser 静态库
    println!("cargo:rustc-link-search=native=modules/sqlparser/build");
    println!("cargo:rustc-link-lib=static=sqlparser");
}
```

## 9. 风险与验证

| 风险 | 缓解 |
|---|---|
| AST 裸指针遍历的 unsafe 面 | `AstHandle<'g>` 借用 guard,编译期防逃逸;遍历封装在安全 API 后 |
| parser 非线程安全被误用 | `RawParser` 只 `Send` 不 `Sync`;池保证独占;clippy 检查 `unsafe` 边界 |
| pool 内存泄漏(clear 不彻底) | parser guard drop 调 `sql_parser_clear`;长期跑压测监控 RSS |
| 池满时 task 等待 | `cap` 设为并发解析上限(≈线程数);压测调优;池空时新建而非死等 |
| prepared 的 clone 入口缺失 | 阶段 1 与 sqlparser 维护者协调暴露 `clone_command`,或用 re-parse 降级 |
| FFI 跨语言 panic | C 侧不会 panic,但 `parse_sql` 可能 segfault——CI 加 fuzz 测试随机 SQL |
| gbk/charset 处理(`sql_gbk*` 字段) | FFI 透传 charset 配置,C 侧已处理 GBK 5C 转义(`sql_define.h` GBK_5C_RECORDS) |

### Fuzz 验证
```rust
// 用 arbitrary 生成随机 SQL,确保 FFI 不 crash
#[cfg(test)]
fn fuzz_parse(parser_pool: &ParserPool) {
    for _ in 0..10000 {
        let sql = gen_random_sql();   // arbitrary crate
        let mut g = parser_pool.acquire().await;
        let _ = g.parse(&sql);        // 不关心结果,只验证不 segfault
    }
}
```
对照 C 侧已有的 sqlparser 测试套件(`modules/sqlparser/test/`),确保 Rust FFI 解析结果与 C 一致。
