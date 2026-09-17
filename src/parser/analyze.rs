// 结构化 SQL 分析器(token 流之上)
//
// 替代旧 classify.rs 的字符串子串扫描:
//   - 语句分类(首关键字 + WITH/EXPLAIN/UNION/前导注释/括号查询)
//   - 表引用收集:主语句级表(own)在前、括号内子查询/派生表/CTE 体内表(nested)在后,
//     均按文本序;CTE 名/别名不作为物理表;免疫字符串/注释/反引号污染
//   - 分片键候选:仅**最外层**查询的顶层 WHERE 的 AND 等值条件
//   - tokenizer 畸形输入(未闭合引号/注释)→ uncertain,调用方保守降级
//
// 表名排序契约(与 front.rs 按"首个带尾号表"路由配合):
//   own(语句主查询体,含顶层 UNION 各支)先,nested(任意括号内子查询/派生表/CTE 体)后。

use crate::parser::ast::{ShardKeyValue, StatementType};
use crate::parser::lex::{self, Token, TokenKind};

/// 分析结果(对外)
#[derive(Debug, Clone)]
pub struct Analysis {
    pub stmt_type: StatementType,
    /// 物理表名(小写、去重保序;反引号按原文保留)
    pub table_names: Vec<String>,
    /// 最外层顶层 AND 等值条件中的候选分片键
    pub shard_keys: Vec<ShardKeyValue>,
    /// 词法畸形(未闭合引号/注释等)→ true 表示结果不可靠
    pub uncertain: bool,
}

/// 全量分析(语句类型 + 表名 + 分片键)
pub fn analyze_full(sql: &str) -> Analysis {
    analyze_impl(sql, true)
}

/// 轻量分析:只要语句类型 + 表名(热路径,跳过 shard 解析)
pub fn analyze_lite(sql: &str) -> (StatementType, Vec<String>) {
    let a = analyze_impl(sql, false);
    (a.stmt_type, a.table_names)
}

/// 快速判断语句是否可能携带可路由表(DML / EXPLAIN / WITH / 括号查询)。
/// 只扫前几个 token,供路由热路径对非 DML(SHOW/SET/USE/BEGIN…)提前返回空表。
pub fn maybe_routable(sql: &str) -> bool {
    let mut lexer = lex::Lexer::new(sql);
    loop {
        match lexer.next_token() {
            None => return false,
            Some(t) if t.kind == TokenKind::Comment => continue,
            Some(t) => {
                if t.kind == TokenKind::Punct {
                    return t.text(sql) == "(";
                }
                if t.kind == TokenKind::Word {
                    return matches!(
                        t.text(sql).to_ascii_lowercase().as_str(),
                        "select"
                            | "insert"
                            | "update"
                            | "delete"
                            | "replace"
                            | "explain"
                            | "with"
                    );
                }
                return false;
            }
        }
    }
}

/// SELECT 只读可分流的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadClass {
    /// 可安全分流到 follower(还需会话状态机与一致性档位判定)
    Eligible,
    /// 必须走 leader;携带原因(指标/日志用)
    LeaderRequired(&'static str),
}

/// 判定一条 SQL 是否属于"可分流纯读"(供读写分离决策)。
///
/// 命中以下任一 → LeaderRequired,任何一致性档位都不绕过:
/// - 非纯 SELECT / 多语句(顶层 `;`);
/// - `FOR UPDATE` / `FOR SHARE` / `LOCK IN SHARE MODE` / `INTO` / `PROCEDURE`;
/// - 用户/系统变量 `@x`/`@@…`;
/// - `SQL_CALC_FOUND_ROWS`(其后必然在同连接调 FOUND_ROWS());
/// - 依赖会话状态/主库时钟/锁的函数:LAST_INSERT_ID / FOUND_ROWS / GET_LOCK /
///   RELEASE_LOCK / IS_FREE_LOCK / IS_USED_LOCK / MASTER_POS_WAIT / SLEEP。
///
/// 词法级保守扫描(任意深度出现即拦截):宁可少分流,不错分流。
pub fn classify_read_only(sql: &str) -> ReadClass {
    let (toks, _) = lex::tokenize(sql);

    // 首关键字必须 SELECT(跳过注释);多语句(顶层 `;`)直接拦截
    let mut end = toks.len();
    {
        let mut depth = 0usize;
        for (i, t) in toks.iter().enumerate() {
            if t.kind != TokenKind::Punct {
                continue;
            }
            match t.text(sql) {
                "(" => depth += 1,
                ")" => depth = depth.saturating_sub(1),
                ";" if depth == 0 => {
                    end = i;
                    break;
                }
                _ => {}
            }
        }
    }
    let Some(first) = (0..end).find(|&i| toks[i].kind != TokenKind::Comment) else {
        return ReadClass::LeaderRequired("no-statement");
    };
    if toks[first].kind != TokenKind::Word
        || !lex::eq_ignore_ascii_case(toks[first].text(sql), "select")
    {
        return ReadClass::LeaderRequired("not-select");
    }
    if end < toks.len() {
        return ReadClass::LeaderRequired("multi-statement");
    }

    const FUNCS: &[&str] = &[
        "last_insert_id",
        "found_rows",
        "get_lock",
        "release_lock",
        "is_free_lock",
        "is_used_lock",
        "master_pos_wait",
        "sleep",
    ];

    let mut lowers: Vec<String> = Vec::with_capacity(toks.len());
    for t in &toks {
        if t.kind == TokenKind::Punct {
            if t.text(sql) == "@" {
                return ReadClass::LeaderRequired("user-variable");
            }
            continue;
        }
        if t.kind == TokenKind::Word {
            lowers.push(t.text(sql).to_ascii_lowercase());
        }
    }
    for w in &lowers {
        match w.as_str() {
            "into" => return ReadClass::LeaderRequired("select-into"),
            "procedure" => return ReadClass::LeaderRequired("procedure-analyse"),
            "sql_calc_found_rows" => return ReadClass::LeaderRequired("found-rows-prefix"),
            _ if FUNCS.contains(&w.as_str()) => return ReadClass::LeaderRequired("session-function"),
            _ => {}
        }
    }
    for win in lowers.windows(2) {
        if win[0] == "for" && (win[1] == "update" || win[1] == "share") {
            return ReadClass::LeaderRequired("for-update-share");
        }
    }
    for win in lowers.windows(3) {
        if win[0] == "lock" && win[1] == "in" && win[2] == "share" {
            return ReadClass::LeaderRequired("lock-in-share");
        }
    }
    ReadClass::Eligible
}

// ─── 关键字集合 ───

const JOIN_PREFIX: &[&str] = &[
    "inner", "left", "right", "cross", "natural", "straight_join", "join",
];

/// FROM 列表内出现即代表表列表结束的顶层词(不再解析为表因子)
const FROM_LIST_STOPS: &[&str] = &[
    "where", "group", "having", "order", "limit", "union", "for", "into", "procedure", "set",
    "values", "using",
];

/// SELECT 查询块的子句边界词(WHERE 区域在第一个此类词处结束)
const SELECT_CLAUSE_STOPS: &[&str] = &[
    "where", "group", "having", "order", "limit", "union", "for", "into", "procedure",
];

/// 表因子之后不允许作为裸别名出现(否则会被当作别名吃掉)的词
fn is_reserved_after_factor(w: &str) -> bool {
    FROM_LIST_STOPS.contains(&w)
        || matches!(
            w,
            "select"
                | "insert"
                | "update"
                | "delete"
                | "replace"
                | "from"
                | "on"
                | "as"
                | "inner"
                | "left"
                | "right"
                | "cross"
                | "natural"
                | "straight_join"
                | "join"
                | "use"
                | "force"
                | "ignore"
                | "index"
                | "partition"
                | "lock"
                | "offset"
                | "duplicate"
                | "key"
                | "returning"
        )
}

// ─── 内部扫描上下文 ───

struct Ctx<'a> {
    sql: &'a str,
    toks: Vec<Token>,
    /// 主语句级物理表(文本序)
    own: Vec<String>,
    /// 括号内子查询/派生表/CTE 体内的物理表(文本序)
    nested: Vec<String>,
    /// CTE 名字作用域栈(每层一个 WITH 列表)
    cte: Vec<Vec<String>>,
    /// 词法畸形
    uncertain: bool,
    /// 最外层查询 WHERE 词 token 索引(仅第一个)
    where_start: Option<usize>,
}

impl<'a> Ctx<'a> {
    fn word(&self, i: usize) -> Option<&'a str> {
        self.toks.get(i).and_then(|t| {
            if t.kind == TokenKind::Word {
                Some(t.text(self.sql))
            } else {
                None
            }
        })
    }

    fn is_kw_at(&self, i: usize, kw: &str) -> bool {
        self.toks
            .get(i)
            .map(|t| lex::is_word(self.sql, *t, kw))
            .unwrap_or(false)
    }

    fn punct(&self, i: usize) -> Option<char> {
        let t = *self.toks.get(i)?;
        if t.kind == TokenKind::Punct {
            let s = t.text(self.sql);
            if s.len() == 1 {
                return s.chars().next();
            }
        }
        None
    }

    fn next_meaningful(&self, from: usize) -> Option<usize> {
        lex::next_meaningful(&self.toks, from)
    }

    fn cte_visible(&self, name: &str) -> bool {
        self.cte.iter().flatten().any(|n| n == name)
    }

    /// 记录最外层 WHERE(allow_shard 与首次条件由调用方保证)
    fn add_own_table(&mut self, name: &str) {
        if !self.cte_visible(name) {
            self.own.push(name.to_string());
        }
    }

    fn add_nested_table(&mut self, name: &str) {
        if !self.cte_visible(name) {
            self.nested.push(name.to_string());
        }
    }
}

// ─── 入口 ───

fn analyze_impl(sql: &str, want_shards: bool) -> Analysis {
    let (toks, lex_err) = lex::tokenize(sql);
    let n = toks.len();
    // 语句边界:首个深度 0 的 `;`(多语句包只分析第一句)
    let stmt_end = {
        let mut depth = 0usize;
        let mut end = n;
        for (i, t) in toks.iter().enumerate() {
            if t.kind != TokenKind::Punct {
                continue;
            }
            match t.text(sql) {
                "(" => depth += 1,
                ")" => depth = depth.saturating_sub(1),
                ";" if depth == 0 => {
                    end = i;
                    break;
                }
                _ => {}
            }
        }
        end
    };

    let i0 = match (0..stmt_end).find(|&i| toks[i].kind != TokenKind::Comment) {
        Some(i) => i,
        None => {
            return Analysis {
                stmt_type: StatementType::Unknown,
                table_names: Vec::new(),
                shard_keys: Vec::new(),
                uncertain: lex_err,
            };
        }
    };

    let mut ctx = Ctx {
        sql,
        toks,
        own: Vec::new(),
        nested: Vec::new(),
        cte: Vec::new(),
        uncertain: lex_err,
        where_start: None,
    };

    let stmt_type = classify_dispatch(&mut ctx, i0, stmt_end, want_shards);

    // 由最外层 WHERE 区域解析分片键
    let shard_keys = if want_shards {
        if let Some(ws) = ctx.where_start {
            let e = find_clause_stop(&ctx, ws + 1, stmt_end);
            parse_shard_region(&ctx, ws + 1, e)
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    let table_names = finalize_tables(&ctx);
    Analysis {
        stmt_type,
        table_names,
        shard_keys,
        uncertain: ctx.uncertain,
    }
}

fn finalize_tables(ctx: &Ctx) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(ctx.own.len() + ctx.nested.len());
    for name in ctx.own.iter().chain(ctx.nested.iter()) {
        if !out.iter().any(|o| o == name) {
            out.push(name.clone());
        }
    }
    out
}

// ─── 语句分类与分发 ───

fn classify_dispatch(ctx: &mut Ctx, i0: usize, end: usize, want_shards: bool) -> StatementType {
    // 非 Word 首 token:仅支持 `(SELECT…)` 括号查询
    if ctx.toks[i0].kind != TokenKind::Word {
        if ctx.punct(i0) == Some('(') && paren_is_query(ctx, i0) {
            collect_query_expression(ctx, i0, end, false, want_shards);
            return StatementType::Select;
        }
        return StatementType::Unknown;
    }
    let w0 = ctx.word(i0).unwrap().to_ascii_lowercase();
    match w0.as_str() {
        "select" => {
            collect_query_expression(ctx, i0, end, false, want_shards);
            StatementType::Select
        }
        "with" => {
            // WITH ... 后跟 SELECT/UPDATE/DELETE
            match parse_cte_list(ctx, i0, end) {
                Some((kw_i, main)) => {
                    let ty = statement_type_of_main(&main);
                    match main.as_str() {
                        "select" => {
                            collect_query_expression(ctx, kw_i, end, false, want_shards);
                        }
                        "update" => scan_update(ctx, kw_i, end, want_shards),
                        "delete" => scan_delete(ctx, kw_i, end, want_shards),
                        _ => {}
                    }
                    ty
                }
                None => StatementType::Unknown,
            }
        }
        "insert" | "replace" => {
            scan_insert_replace(ctx, i0, end);
            if w0 == "insert" {
                StatementType::Insert
            } else {
                StatementType::Replace
            }
        }
        "update" => {
            scan_update(ctx, i0, end, want_shards);
            StatementType::Update
        }
        "delete" => {
            scan_delete(ctx, i0, end, want_shards);
            StatementType::Delete
        }
        "explain" => {
            scan_explain(ctx, i0, end);
            StatementType::Explain
        }
        "begin" | "start" => StatementType::Begin,
        "commit" => StatementType::Commit,
        "rollback" => StatementType::Rollback,
        "use" => StatementType::Use,
        "set" => StatementType::Set,
        "show" => StatementType::Show,
        "describe" | "desc" => StatementType::Describe,
        "create" => StatementType::Create,
        "drop" => StatementType::Drop,
        "alter" => StatementType::Alter,
        "truncate" => StatementType::Truncate,
        "call" => StatementType::Call,
        "prepare" => StatementType::Prepare,
        "execute" => StatementType::Execute,
        "deallocate" => StatementType::Deallocate,
        "kill" => StatementType::Kill,
        _ => StatementType::Unknown,
    }
}

fn statement_type_of_main(w: &str) -> StatementType {
    match w {
        "select" => StatementType::Select,
        "insert" => StatementType::Insert,
        "replace" => StatementType::Replace,
        "update" => StatementType::Update,
        "delete" => StatementType::Delete,
        _ => StatementType::Unknown,
    }
}

/// `(` 位置后跟 SELECT/WITH(跳过注释)→ 是查询
fn paren_is_query(ctx: &Ctx, open: usize) -> bool {
    match ctx.next_meaningful(open + 1) {
        Some(i) => ctx
            .word(i)
            .map(|w| {
                let l = w.to_ascii_lowercase();
                l == "select" || l == "with"
            })
            .unwrap_or(false),
        None => false,
    }
}

/// 解析 WITH 开头的 CTE 列表;返回 (主体 DML 关键字 token 索引, 小写词)。
/// CTE 体内表收集进 nested;解析主体前把名字压入作用域。
fn parse_cte_list(ctx: &mut Ctx, i0: usize, end: usize) -> Option<(usize, String)> {
    let mut i = i0 + 1;
    if ctx.is_kw_at(i, "recursive") {
        i += 1;
    }
    let mut all_names: Vec<String> = Vec::new();
    loop {
        let name_i = ctx.next_meaningful(i)?;
        let name = ctx.word(name_i)?.to_ascii_lowercase();
        i = name_i + 1;
        // 可选列清单 (col,...)
        if ctx.punct(i) == Some('(') {
            i = skip_balanced_group(ctx, i).unwrap_or(end);
        }
        if !ctx.is_kw_at(i, "as") {
            return None;
        }
        i += 1;
        if ctx.punct(i) != Some('(') {
            return None;
        }
        let close = lex::matching_paren(ctx.sql, &ctx.toks, i)?;
        // body 解析:名字先可见(递归 CTE 自引用、先前 CTE 引用)
        all_names.push(name);
        ctx.cte.push(all_names.clone());
        collect_query_expression(ctx, i + 1, close, true, false);
        ctx.cte.pop();
        i = close + 1;
        if ctx.punct(i) == Some(',') {
            i += 1;
            continue;
        }
        break;
    }
    // 列表结束:所有名字对本查询表达式主体可见
    ctx.cte.push(all_names);
    let kw_i = ctx.next_meaningful(i)?;
    let w = ctx.word(kw_i)?.to_ascii_lowercase();
    if matches!(w.as_str(), "select" | "update" | "delete") {
        Some((kw_i, w))
    } else {
        None
    }
}

// ─── 查询表达式(SELECT 家族:arm + UNION / 括号 arm / WITH 前缀)───
//
// into_nested: 该表达式产出的"本层表"归入 nested(派生表/子查询/CTE 体)还是 own(语句/UNION 顶层)。
// allow_shard:  是否可记录最外层 WHERE(仅最外层语句为 true;nested 一律 false)。

fn collect_query_expression(
    ctx: &mut Ctx,
    start: usize,
    end: usize,
    into_nested: bool,
    allow_shard: bool,
) -> usize {
    let mut i = start;
    loop {
        if i >= end {
            break;
        }
        let j = match ctx.next_meaningful(i) {
            Some(j) if j < end => j,
            _ => break,
        };
        if ctx.is_kw_at(j, "with") {
            match parse_cte_list(ctx, j, end) {
                Some((kw_i, _)) => {
                    i = kw_i; // 主循环接着处理 DML arm
                    continue;
                }
                None => {
                    ctx.uncertain = true;
                    return end;
                }
            }
        }
        if ctx.is_kw_at(j, "select") {
            i = parse_select_arm(ctx, j, end, into_nested, allow_shard);
            continue;
        }
        if ctx.punct(j) == Some('(') {
            // 括号 arm
            let close = match lex::matching_paren(ctx.sql, &ctx.toks, j) {
                Some(c) => c,
                None => {
                    ctx.uncertain = true;
                    return end;
                }
            };
            collect_query_expression(ctx, j + 1, close, into_nested, allow_shard);
            i = close + 1;
            continue;
        }
        if ctx.is_kw_at(j, "union") {
            i = j + 1;
            if ctx.is_kw_at(i, "all") || ctx.is_kw_at(i, "distinct") {
                i += 1;
            }
            continue;
        }
        // 理论到不了这里:跳过
        i = j + 1;
    }
    end
}

/// 解析单个 SELECT 查询块(从 SELECT 词开始,到本 arm 结束处)。
fn parse_select_arm(
    ctx: &mut Ctx,
    p: usize,
    end: usize,
    into_nested: bool,
    allow_shard: bool,
) -> usize {
    let mut i = p + 1;
    let mut depth = 0usize;
    while i < end {
        let t = ctx.toks[i];
        match t.kind {
            TokenKind::Word => {
                let w = t.text(ctx.sql).to_ascii_lowercase();
                if depth == 0 {
                    match w.as_str() {
                        "from" => {
                            i = collect_from_list(ctx, i + 1, end, into_nested);
                            continue;
                        }
                        "where" => {
                            if allow_shard {
                                ctx.where_start.get_or_insert(i);
                            }
                            i += 1; // 继续走,括号子查询在后续循环中递归收集
                            continue;
                        }
                        "union" | "for" | "procedure" | "into" => {
                            return i; // 本 arm 结束
                        }
                        _ => {}
                    }
                }
                i += 1;
            }
            TokenKind::Punct => match ctx.punct(i) {
                Some('(') => {
                    if paren_is_query(ctx, i) {
                        let close = match lex::matching_paren(ctx.sql, &ctx.toks, i) {
                            Some(c) => c,
                            None => {
                                ctx.uncertain = true;
                                return end;
                            }
                        };
                        // 子查询:表归 nested,不捕获 shard
                        collect_query_expression(ctx, i + 1, close, true, false);
                        i = close + 1;
                    } else {
                        depth += 1;
                        i += 1;
                    }
                }
                Some(')') => {
                    if depth == 0 {
                        return i; // 防御:多余闭括号
                    }
                    depth -= 1;
                    i += 1;
                }
                Some(';') => return i,
                _ => i += 1,
            },
            TokenKind::Comment => i += 1,
            _ => i += 1,
        }
    }
    end
}

/// 从 from_i 开始找顶层子句边界词(返回该词 token 索引)。
fn find_clause_stop(ctx: &Ctx, from_i: usize, end: usize) -> usize {
    let mut depth = 0usize;
    let mut i = from_i;
    while i < end {
        let t = ctx.toks[i];
        match t.kind {
            TokenKind::Word => {
                if depth == 0 {
                    let w = t.text(ctx.sql).to_ascii_lowercase();
                    if SELECT_CLAUSE_STOPS.contains(&w.as_str()) {
                        return i;
                    }
                }
            }
            TokenKind::Punct => match ctx.punct(i) {
                Some('(') => depth += 1,
                Some(')') => depth = depth.saturating_sub(1),
                _ => {}
            },
            _ => {}
        }
        i += 1;
    }
    end
}

// ─── FROM 列表(表因子 + JOIN + 派生表)───

/// 收集 from 列表(own 级因子 → own;`(...)` 派生/子查询 → nested)。
/// 返回停止词/`)`/末尾所在 token 索引(不消费该 token)。
fn collect_from_list(ctx: &mut Ctx, mut i: usize, end: usize, into_nested: bool) -> usize {
    loop {
        let j = match ctx.next_meaningful(i) {
            Some(j) if j < end => j,
            _ => return end,
        };
        let t = ctx.toks[j];
        if t.kind == TokenKind::Word {
            let w = t.text(ctx.sql).to_ascii_lowercase();
            // 表列表边界词
            if FROM_LIST_STOPS.contains(&w.as_str()) {
                return j;
            }
            // JOIN 前缀
            if JOIN_PREFIX.contains(&w.as_str()) {
                i = j + 1; // 下一个应是表因子
                continue;
            }
            i = parse_table_factor(ctx, j, end, into_nested);
            // 因子后允许 USING(...) / ON(表达式) 兜底消费
            loop {
                let k = ctx.next_meaningful(i).unwrap_or(end);
                if k >= end {
                    return end;
                }
                let tw = ctx.word(k).map(|w| w.to_ascii_lowercase());
                match tw.as_deref() {
                    Some("using") => {
                        let l = ctx.next_meaningful(k + 1).unwrap_or(end);
                        if ctx.punct(l) == Some('(') {
                            i = skip_balanced_group(ctx, l).unwrap_or(end);
                        } else {
                            i = k + 1;
                        }
                    }
                    Some("on") => {
                        i = skip_on_expr(ctx, k + 1, end);
                    }
                    _ => break,
                }
            }
            continue;
        }
        match ctx.punct(j) {
            Some(',') => i = j + 1,
            Some(')') => return j,
            Some('(') => {
                // 派生表 / VALUES 构造 / 因子组
                if paren_is_query(ctx, j) {
                    let close = match lex::matching_paren(ctx.sql, &ctx.toks, j) {
                        Some(c) => c,
                        None => {
                            ctx.uncertain = true;
                            return end;
                        }
                    };
                    collect_query_expression(ctx, j + 1, close, true, false);
                    i = close + 1;
                } else {
                    // 非查询括号:整体跳过(VALUES 行/因子组等)
                    i = skip_balanced_group(ctx, j).unwrap_or(end);
                }
                // 派生表别名
                i = skip_alias(ctx, i, end);
                continue;
            }
            _ => return j, // 无法识别:结束(保守)
        }
    }
}

/// 解析单个表因子 `[schema.]table`,含反引号/别名/索引提示。
fn parse_table_factor(ctx: &mut Ctx, j: usize, end: usize, into_nested: bool) -> usize {
    let name_start = j;
    let mut name_end = j + 1;
    if let Some(dot) = ctx.next_meaningful(name_end) {
        if ctx.punct(dot) == Some('.') {
            if let Some(part2) = ctx.next_meaningful(dot + 1) {
                if part2 < end && ctx.toks[part2].kind == TokenKind::Word {
                    name_end = part2 + 1;
                }
            }
        }
    }
    let mut name = String::new();
    for k in name_start..name_end {
        let tk = &ctx.toks[k];
        if tk.kind == TokenKind::Punct {
            name.push('.');
        } else if tk.kind == TokenKind::Word {
            name.push_str(tk.text(ctx.sql));
        }
    }
    if !name.is_empty() {
        let lower = name.to_lowercase();
        if into_nested {
            ctx.add_nested_table(&lower);
        } else {
            ctx.add_own_table(&lower);
        }
    }
    let mut i = skip_alias(ctx, name_end, end);
    // 索引提示:USE/FORCE/IGNORE INDEX [(...)]
    loop {
        let k = match ctx.next_meaningful(i) {
            Some(k) if k < end => k,
            _ => break,
        };
        let hint_ok = matches!(
            ctx.word(k).map(|w| w.to_ascii_lowercase()).as_deref(),
            Some("use") | Some("force") | Some("ignore")
        ) && ctx
            .next_meaningful(k + 1)
            .map(|l| ctx.is_kw_at(l, "index"))
            .unwrap_or(false);
        if !hint_ok {
            break;
        }
        i = ctx.next_meaningful(k + 1).unwrap_or(k + 1) + 1;
        if ctx.punct(i) == Some('(') {
            i = skip_balanced_group(ctx, i).unwrap_or(end);
        }
    }
    i
}

/// 跳过 [AS] alias(仅在下一个 Word 非保留字时当作别名)。
fn skip_alias(ctx: &Ctx, mut i: usize, end: usize) -> usize {
    loop {
        let j = match ctx.next_meaningful(i) {
            Some(j) if j < end => j,
            _ => return end,
        };
        let t = ctx.toks[j];
        if t.kind == TokenKind::Word {
            let w = t.text(ctx.sql).to_ascii_lowercase();
            if w == "as" {
                if let Some(k) = ctx.next_meaningful(j + 1) {
                    if k < end && ctx.toks[k].kind == TokenKind::Word {
                        i = k + 1;
                        continue;
                    }
                }
                return j;
            }
            if is_reserved_after_factor(&w) {
                return j;
            }
            i = j + 1; // 裸别名
            continue;
        }
        return j;
    }
}

/// 跳过 ON 后的连接条件表达式(到下一 JOIN / 边界词或 end;内部子查询递归收集)。
fn skip_on_expr(ctx: &mut Ctx, mut i: usize, end: usize) -> usize {
    let mut depth = 0usize;
    while i < end {
        let t = ctx.toks[i];
        match t.kind {
            TokenKind::Word => {
                if depth == 0 {
                    let w = t.text(ctx.sql).to_ascii_lowercase();
                    if JOIN_PREFIX.contains(&w.as_str())
                        || FROM_LIST_STOPS.contains(&w.as_str())
                    {
                        return i;
                    }
                }
                i += 1;
            }
            TokenKind::Punct => match ctx.punct(i) {
                Some('(') => {
                    if paren_is_query(ctx, i) {
                        let close = match lex::matching_paren(ctx.sql, &ctx.toks, i) {
                            Some(c) => c,
                            None => {
                                ctx.uncertain = true;
                                return end;
                            }
                        };
                        collect_query_expression(ctx, i + 1, close, true, false);
                        i = close + 1;
                    } else {
                        depth += 1;
                        i += 1;
                    }
                }
                Some(')') => {
                    if depth == 0 {
                        return i;
                    }
                    depth -= 1;
                    i += 1;
                }
                _ => i += 1,
            },
            _ => i += 1,
        }
    }
    end
}

/// 跳过平衡括号组 `(...)`(内容忽略;用于列清单/VALUES 行/函数实参等)。
fn skip_balanced_group(ctx: &Ctx, open: usize) -> Option<usize> {
    lex::matching_paren(ctx.sql, &ctx.toks, open).map(|c| c + 1)
}

/// 通用表达式跳过:处理 `(...)` 子查询递归、深度 0 遇到 stop 词返回。
/// 用于 SET 赋值区等(update/insert)。
fn skip_expr_region(ctx: &mut Ctx, mut i: usize, end: usize, stops: &[&str]) -> usize {
    let mut depth = 0usize;
    while i < end {
        let t = ctx.toks[i];
        match t.kind {
            TokenKind::Word => {
                if depth == 0 {
                    let w = t.text(ctx.sql).to_ascii_lowercase();
                    if stops.contains(&w.as_str()) {
                        return i;
                    }
                }
                i += 1;
            }
            TokenKind::Punct => match ctx.punct(i) {
                Some('(') => {
                    if paren_is_query(ctx, i) {
                        let close = match lex::matching_paren(ctx.sql, &ctx.toks, i) {
                            Some(c) => c,
                            None => {
                                ctx.uncertain = true;
                                return end;
                            }
                        };
                        collect_query_expression(ctx, i + 1, close, true, false);
                        i = close + 1;
                    } else {
                        depth += 1;
                        i += 1;
                    }
                }
                Some(')') => {
                    if depth == 0 {
                        return i;
                    }
                    depth -= 1;
                    i += 1;
                }
                _ => i += 1,
            },
            _ => i += 1,
        }
    }
    end
}

// ─── UPDATE / DELETE / INSERT …SELECT / EXPLAIN ───

fn scan_update(ctx: &mut Ctx, i0: usize, end: usize, allow_shard: bool) {
    let mut i = i0 + 1;
    // 可选项
    loop {
        let j = match ctx.next_meaningful(i) {
            Some(j) if j < end => j,
            _ => return,
        };
        let w = ctx.word(j).map(|w| w.to_ascii_lowercase());
        if matches!(w.as_deref(), Some("low_priority") | Some("ignore")) {
            i = j + 1;
        } else {
            break;
        }
    }
    // 目标表 + JOIN …(到 SET 停止)
    i = collect_from_list(ctx, i, end, false);
    // SET 赋值 → WHERE / 结尾(赋值区括号内子查询递归收集)
    let mut depth = 0usize;
    while i < end {
        let t = ctx.toks[i];
        match t.kind {
            TokenKind::Word => {
                let w = t.text(ctx.sql).to_ascii_lowercase();
                if depth == 0 && w == "where" {
                    if allow_shard {
                        ctx.where_start.get_or_insert(i);
                    }
                    i += 1;
                    continue;
                }
                i += 1;
            }
            TokenKind::Punct => match ctx.punct(i) {
                Some('(') => {
                    if paren_is_query(ctx, i) {
                        let close = match lex::matching_paren(ctx.sql, &ctx.toks, i) {
                            Some(c) => c,
                            None => {
                                ctx.uncertain = true;
                                return;
                            }
                        };
                        collect_query_expression(ctx, i + 1, close, true, false);
                        i = close + 1;
                    } else {
                        depth += 1;
                        i += 1;
                    }
                }
                Some(')') => {
                    depth = depth.saturating_sub(1);
                    i += 1;
                }
                _ => i += 1,
            },
            _ => i += 1,
        }
    }
}

fn scan_delete(ctx: &mut Ctx, i0: usize, end: usize, allow_shard: bool) {
    let mut i = i0 + 1;
    // 可选项
    loop {
        let j = match ctx.next_meaningful(i) {
            Some(j) if j < end => j,
            _ => return,
        };
        let w = ctx.word(j).map(|w| w.to_ascii_lowercase());
        if matches!(w.as_deref(), Some("low_priority") | Some("quick") | Some("ignore")) {
            i = j + 1;
        } else {
            break;
        }
    }
    // 多表形式 DELETE t1, t2 FROM ...:目标名是别名,物理表在 FROM/USING 区;跳过目标名
    if ctx.word(i).is_some() && !ctx.is_kw_at(i, "from") {
        let mut j = i;
        while j < end {
            let w = ctx.word(j).map(|w| w.to_ascii_lowercase());
            if w.as_deref() == Some("from") {
                i = j;
                break;
            }
            j += 1;
        }
        if j >= end {
            return;
        }
    }
    if ctx.is_kw_at(i, "from") {
        i += 1;
        i = collect_from_list(ctx, i, end, false);
    }
    if ctx.is_kw_at(i, "using") {
        i += 1;
        i = collect_from_list(ctx, i, end, false);
    }
    if ctx.is_kw_at(i, "where") && allow_shard {
        ctx.where_start.get_or_insert(i);
    }
}

fn scan_insert_replace(ctx: &mut Ctx, i0: usize, end: usize) {
    let mut i = i0 + 1;
    loop {
        let j = match ctx.next_meaningful(i) {
            Some(j) if j < end => j,
            _ => return,
        };
        let w = ctx.word(j).map(|w| w.to_ascii_lowercase());
        if matches!(
            w.as_deref(),
            Some("low_priority") | Some("delayed") | Some("high_priority") | Some("ignore")
        ) {
            i = j + 1;
        } else {
            break;
        }
    }
    if ctx.is_kw_at(i, "into") {
        i += 1;
    }
    // 目标表
    let j = match ctx.next_meaningful(i) {
        Some(j) if j < end => j,
        _ => return,
    };
    if ctx.toks[j].kind != TokenKind::Word {
        return;
    }
    i = parse_table_factor(ctx, j, end, false);
    // 列清单
    if ctx.punct(i) == Some('(') {
        i = skip_balanced_group(ctx, i).unwrap_or(end);
    }
    // 数据源
    loop {
        let k = match ctx.next_meaningful(i) {
            Some(k) if k < end => k,
            _ => break,
        };
        let w = ctx.word(k).map(|w| w.to_ascii_lowercase());
        match w.as_deref() {
            Some("select") => {
                collect_query_expression(ctx, k, end, false, false);
                break;
            }
            Some("with") => {
                if let Some((kw_i, _)) = parse_cte_list(ctx, k, end) {
                    collect_query_expression(ctx, kw_i, end, false, false);
                }
                break;
            }
            Some("values") => {
                // 跳过行序列(到 on/select/set/末尾)
                i = k + 1;
                while i < end {
                    let m = ctx.next_meaningful(i).unwrap_or(end);
                    if m >= end {
                        break;
                    }
                    if ctx.punct(m) == Some('(') {
                        i = skip_balanced_group(ctx, m).unwrap_or(end);
                        continue;
                    }
                    let mw = ctx.word(m).map(|w| w.to_ascii_lowercase());
                    if matches!(mw.as_deref(), Some("on") | Some("select") | Some("set")) {
                        break;
                    }
                    i = m + 1;
                }
            }
            Some("set") | Some("on") => {
                // INSERT SET / ON DUPLICATE KEY UPDATE 赋值区:扫描到结尾(含子查询)
                let _ = skip_expr_region(ctx, k + 1, end, &[]);
                break;
            }
            _ => i = k + 1,
        }
    }
}

fn scan_explain(ctx: &mut Ctx, i0: usize, end: usize) {
    let mut i = i0 + 1;
    loop {
        let j = match ctx.next_meaningful(i) {
            Some(j) if j < end => j,
            _ => return,
        };
        let w = match ctx.word(j) {
            Some(w) => w.to_ascii_lowercase(),
            None => return,
        };
        match w.as_str() {
            "analyze" | "extended" | "partitions" => i = j + 1,
            "format" => {
                i = j + 1;
                if ctx.punct(i) == Some('=') {
                    i += 1;
                }
                if let Some(k) = ctx.next_meaningful(i) {
                    i = k + 1;
                }
            }
            "select" => {
                collect_query_expression(ctx, j, end, false, false);
                return;
            }
            "update" => {
                scan_update(ctx, j, end, false);
                return;
            }
            "delete" => {
                scan_delete(ctx, j, end, false);
                return;
            }
            "insert" | "replace" => {
                scan_insert_replace(ctx, j, end);
                return;
            }
            "with" => {
                if let Some((kw_i, main)) = parse_cte_list(ctx, j, end) {
                    match main.as_str() {
                        "select" => {
                            collect_query_expression(ctx, kw_i, end, false, false);
                        }
                        "update" => scan_update(ctx, kw_i, end, false),
                        "delete" => scan_delete(ctx, kw_i, end, false),
                        _ => {}
                    }
                }
                return;
            }
            _ => return, // EXPLAIN table 等:无内层语句
        }
    }
}

// ─── 分片键提取(最外层 WHERE 顶层 AND 等值)───

fn parse_shard_region(ctx: &Ctx, s: usize, e: usize) -> Vec<ShardKeyValue> {
    if s >= e {
        return Vec::new();
    }
    // 顶层 OR → 整体歧义,保守空
    let mut depth = 0usize;
    let mut and_positions: Vec<usize> = Vec::new();
    let mut i = s;
    while i < e {
        let t = ctx.toks[i];
        if t.kind == TokenKind::Punct {
            match ctx.punct(i) {
                Some('(') => depth += 1,
                Some(')') => depth = depth.saturating_sub(1),
                _ => {}
            }
        } else if t.kind == TokenKind::Word && depth == 0 {
            let w = t.text(ctx.sql).to_ascii_lowercase();
            if w == "or" {
                return Vec::new();
            }
            if w == "and" {
                and_positions.push(i);
            }
        }
        i += 1;
    }

    let mut keys: Vec<ShardKeyValue> = Vec::new();
    let mut seg_start = s;
    for &ap in and_positions.iter().chain(std::iter::once(&e)) {
        if ap > seg_start {
            if let Some(kv) = parse_conjunct(ctx, seg_start, ap) {
                keys.push(kv);
            }
        }
        seg_start = ap + 1;
    }
    keys
}

/// 解析单个等值条件 `col = value` / `col IN (单值)`;返回 (列, 值)。
fn parse_conjunct(ctx: &Ctx, s: usize, e: usize) -> Option<ShardKeyValue> {
    // 去除整段包裹括号
    let (mut a, mut b) = (s, e);
    loop {
        if ctx.punct(a) != Some('(') {
            break;
        }
        let close = lex::matching_paren(ctx.sql, &ctx.toks, a)?;
        if close + 1 == b {
            a += 1;
            b = close;
        } else {
            break;
        }
    }
    if a >= b {
        return None;
    }

    let mut depth = 0usize;
    let mut i = a;
    while i < b {
        let t = ctx.toks[i];
        match t.kind {
            TokenKind::Punct => match ctx.punct(i) {
                Some('(') => depth += 1,
                Some(')') => depth = depth.saturating_sub(1),
                Some('=') if depth == 0 => {
                    return parse_eq_value(ctx, a, i, b);
                }
                _ => {}
            },
            TokenKind::Word if depth == 0 => {
                if ctx.is_kw_at(i, "in") {
                    let l = ctx.next_meaningful(i + 1)?;
                    if ctx.punct(l) == Some('(') {
                        let close = lex::matching_paren(ctx.sql, &ctx.toks, l)?;
                        return parse_in_value(ctx, a, i, l + 1, close, b);
                    }
                }
                if ctx.is_kw_at(i, "is") || ctx.is_kw_at(i, "between") {
                    return None; // IS / BETWEEN → 不可静态路由
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn parse_eq_value(ctx: &Ctx, a: usize, eq: usize, b: usize) -> Option<ShardKeyValue> {
    let col = left_column(ctx, a, eq)?;
    let val = literal_value(ctx, eq + 1, b)?;
    Some(ShardKeyValue {
        column: col,
        value: val,
    })
}

/// `col IN (…)`:仅当恰好一个字面量时可路由。
fn parse_in_value(
    ctx: &Ctx,
    a: usize,
    in_i: usize,
    inner_s: usize,
    inner_e: usize,
    b: usize,
) -> Option<ShardKeyValue> {
    // 括号内容可能是子查询/多值 → 一律按整段拒绝,除非单字面量
    let single = literal_value(ctx, inner_s, inner_e)?;
    let col = left_column(ctx, a, in_i)?;
    // 检查 IN 之后无额外内容
    if ctx.next_meaningful(inner_e + 1).map(|k| k < b) == Some(true) {
        return None;
    }
    Some(ShardKeyValue {
        column: col,
        value: single,
    })
}

/// 左部列名:`col` 或 `t.col`;拒绝函数/括号/数字。
fn left_column(ctx: &Ctx, a: usize, b: usize) -> Option<String> {
    if a >= b {
        return None;
    }
    let mut last_word: Option<&str> = None;
    for i in a..b {
        let t = &ctx.toks[i];
        match t.kind {
            TokenKind::Word => {
                let w = t.text(ctx.sql);
                let l = w.to_lowercase();
                // 列名本身不应是子句关键字(保留字必须加引号,引号后文本含引号,不匹配)
                if SELECT_CLAUSE_STOPS.contains(&l.as_str()) || FROM_LIST_STOPS.contains(&l.as_str()) {
                    return None;
                }
                last_word = Some(w);
            }
            TokenKind::Punct if ctx.punct(i) == Some('.') => {}
            _ => return None, // 函数/括号/操作符 → 非简单列
        }
    }
    let col = strip_ident_quotes(last_word?);
    if col.is_empty() {
        return None;
    }
    Some(col)
}

/// 区域 [s,e) 恰好是单字面量(Num/Str/'-' Num)→ 规范化值;否则 None。
fn literal_value(ctx: &Ctx, s: usize, e: usize) -> Option<String> {
    let i = ctx.next_meaningful(s)?;
    if i >= e {
        return None;
    }
    // 值后不允许再有 token(注释/空白除外)
    if let Some(k) = ctx.next_meaningful(i + 1) {
        if k < e {
            return None;
        }
    }
    let t = ctx.toks[i];
    match t.kind {
        TokenKind::Num => Some(t.text(ctx.sql).to_lowercase()),
        TokenKind::Str => {
            let raw = t.text(ctx.sql);
            let inner = raw
                .strip_prefix('\'')
                .or_else(|| raw.strip_prefix('"'))
                .and_then(|r| r.strip_suffix('\'').or_else(|| r.strip_suffix('"')))
                .unwrap_or(raw);
            Some(inner.to_lowercase())
        }
        TokenKind::Punct if t.text(ctx.sql) == "-" => {
            let j = ctx.next_meaningful(i + 1)?;
            if j >= e {
                return None;
            }
            let n = ctx.toks[j];
            if n.kind != TokenKind::Num {
                return None;
            }
            if let Some(k) = ctx.next_meaningful(j + 1) {
                if k < e {
                    return None;
                }
            }
            Some(format!("-{}", n.text(ctx.sql)))
        }
        _ => None,
    }
}

fn strip_ident_quotes(s: &str) -> String {
    let t = s.trim();
    if t.len() >= 2
        && ((t.starts_with('`') && t.ends_with('`')) || (t.starts_with('"') && t.ends_with('"')))
    {
        t[1..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tables(sql: &str) -> Vec<String> {
        analyze_full(sql).table_names
    }

    fn shards(sql: &str) -> Vec<(String, String)> {
        analyze_full(sql)
            .shard_keys
            .into_iter()
            .map(|k| (k.column, k.value))
            .collect()
    }

    fn typ(sql: &str) -> StatementType {
        analyze_full(sql).stmt_type
    }

    #[test]
    fn basic_statement_tables() {
        assert_eq!(tables("SELECT * FROM users WHERE id=1"), vec!["users"]);
        assert_eq!(tables("INSERT INTO orders (id) VALUES (1)"), vec!["orders"]);
        assert_eq!(tables("UPDATE products SET a=1 WHERE id=2"), vec!["products"]);
        assert_eq!(tables("DELETE FROM sessions WHERE x=1"), vec!["sessions"]);
        assert_eq!(tables("REPLACE INTO cache (k) VALUES ('a')"), vec!["cache"]);
    }

    #[test]
    fn complex_queries_not_fooled_by_strings() {
        // 字符串中的 FROM 不再污染
        assert_eq!(tables("SELECT 'a FROM b' AS s, x FROM t1"), vec!["t1"]);
        // 标量子查询在 select-list(先于主 FROM)
        assert_eq!(tables("SELECT (SELECT MAX(x) FROM t2) FROM t1"), vec!["t1", "t2"]);
        // 派生表(own 级主表在前,nested 在后)
        assert_eq!(
            tables("SELECT * FROM (SELECT id FROM t2) d JOIN t3 ON d.id=t3.id"),
            vec!["t3", "t2"]
        );
        // JOIN 家族
        assert_eq!(
            tables("SELECT * FROM a LEFT JOIN b ON a.x=b.x RIGHT JOIN c ON b.y=c.y"),
            vec!["a", "b", "c"]
        );
        // UNION 两支都收集
        assert_eq!(
            tables("SELECT * FROM t1 UNION SELECT * FROM t2 UNION ALL SELECT 1"),
            vec!["t1", "t2"]
        );
        // INSERT ... SELECT:目标在前、源在后,无垃圾词
        assert_eq!(tables("INSERT INTO t1 SELECT * FROM t2"), vec!["t1", "t2"]);
        // WITH:CTE 名不出现,CTE 体表收集,主表在前
        assert_eq!(
            tables("WITH c AS (SELECT id FROM t2) SELECT * FROM sbtest_1 JOIN c ON c.id=sbtest_1.id"),
            vec!["sbtest_1", "t2"]
        );
        // 多表 UPDATE
        assert_eq!(
            tables("UPDATE t1 JOIN t2 ON t1.id=t2.id SET t1.a=1 WHERE t2.b=2"),
            vec!["t1", "t2"]
        );
        // 多表 DELETE
        assert_eq!(
            tables("DELETE t1 FROM t1 JOIN t2 ON t1.id=t2.id WHERE t2.x=1"),
            vec!["t1", "t2"]
        );
        // 注释/字符串里的关键字不干扰
        assert_eq!(
            tables("SELECT /* FROM x JOIN y */ * FROM real_t WHERE c='-- and more'"),
            vec!["real_t"]
        );
    }

    #[test]
    fn nested_subqueries_collected() {
        // IN (SELECT …) 里的表在 WHERE 内
        assert_eq!(
            tables("SELECT * FROM t WHERE id IN (SELECT id FROM t2)"),
            vec!["t", "t2"]
        );
        // EXISTS / ON 表达式内子查询
        assert_eq!(
            tables("SELECT * FROM a JOIN b ON a.x = (SELECT MAX(y) FROM c) WHERE EXISTS (SELECT 1 FROM d WHERE d.a=a.x)"),
            vec!["a", "b", "c", "d"]
        );
    }

    #[test]
    fn classification_edge_types() {
        assert_eq!(typ("WITH c AS (SELECT 1) SELECT * FROM t"), StatementType::Select);
        assert_eq!(typ("EXPLAIN SELECT * FROM t"), StatementType::Explain);
        assert_eq!(typ("EXPLAIN FORMAT=JSON SELECT 1"), StatementType::Explain);
        assert_eq!(typ("/* hint */ INSERT INTO t VALUES (1)"), StatementType::Insert);
        assert_eq!(typ("-- c\nUPDATE t SET a=1"), StatementType::Update);
        assert_eq!(typ("(SELECT 1) UNION (SELECT 2)"), StatementType::Select);
        assert_eq!(typ("WITH c AS (SELECT 1) DELETE FROM t WHERE a=1"), StatementType::Delete);
        assert_eq!(typ("SHOW TABLES"), StatementType::Show);
        assert_eq!(typ("BEGIN"), StatementType::Begin);
    }

    #[test]
    fn shard_keys_top_level_and_only() {
        assert_eq!(
            shards("SELECT * FROM t WHERE id = 42 AND name = 'test'"),
            vec![("id".into(), "42".into()), ("name".into(), "test".into())]
        );
        // 只取最外层
        assert_eq!(
            shards("SELECT * FROM (SELECT * FROM t2 WHERE b = 1) d WHERE a = 2"),
            vec![("a".into(), "2".into())]
        );
        // 顶层 OR → 保守空
        assert!(shards("SELECT * FROM t WHERE a = 1 OR b = 2").is_empty());
        // 多值 IN / 子查询 / 函数列 → 不产键
        assert!(shards("SELECT * FROM t WHERE id IN (1, 2, 3)").is_empty());
        assert!(shards("SELECT * FROM t WHERE id IN (SELECT id FROM t2)").is_empty());
        assert!(shards("SELECT * FROM t WHERE LOWER(name) = 'x'").is_empty());
        // 单值 IN 可路由
        assert_eq!(shards("SELECT * FROM t WHERE id IN (42)"), vec![("id".into(), "42".into())]);
        // UPDATE/DELETE 的 WHERE
        assert_eq!(shards("UPDATE t SET a=1 WHERE user_id=7"), vec![("user_id".into(), "7".into())]);
        assert_eq!(shards("DELETE FROM t WHERE id='abc'"), vec![("id".into(), "abc".into())]);
        // WHERE 区域到 ORDER BY 截断
        assert_eq!(
            shards("SELECT * FROM t WHERE id=1 ORDER BY id LIMIT 10"),
            vec![("id".into(), "1".into())]
        );
    }

    #[test]
    fn quoted_and_dotted_names() {
        assert_eq!(
            tables("SELECT * FROM `order` JOIN db.`t_1` ON 1=1"),
            vec!["`order`", "db.`t_1`"]
        );
        assert_eq!(tables("SELECT * FROM sbtest.sbtest_1"), vec!["sbtest.sbtest_1"]);
        assert_eq!(tables("SELECT * FROM users u WHERE u.id=1"), vec!["users"]);
        assert_eq!(
            tables("SELECT * FROM users AS u JOIN orders o ON u.id=o.uid"),
            vec!["users", "orders"]
        );
    }

    #[test]
    fn aliases_and_index_hints() {
        assert_eq!(
            tables("SELECT * FROM t1 USE INDEX (idx_a) JOIN t2 FORCE INDEX (idx_b) ON t1.id=t2.id"),
            vec!["t1", "t2"]
        );
        assert_eq!(tables("SELECT * FROM (SELECT * FROM t2) AS d WHERE d.x=1"), vec!["t2"]);
    }

    #[test]
    fn routing_lite_matches_full() {
        // classify 与热路径抽取必须一致
        let cases = [
            "SELECT * FROM sbtest_1 WHERE id=1",
            "SELECT (SELECT MAX(x) FROM sbtest_2) FROM sbtest_1",
            "INSERT INTO sbtest_1 SELECT * FROM sbtest_2",
            "WITH c AS (SELECT id FROM sbtest_2) SELECT * FROM sbtest_1 JOIN c ON 1=1",
            "SHOW TABLES",
            "BEGIN",
            "SELECT 1",
        ];
        for sql in cases {
            let full = analyze_full(sql);
            let (_, lite) = analyze_lite(sql);
            assert_eq!(lite, full.table_names, "mismatch for {sql}");
        }
    }

    #[test]
    fn multi_statement_only_first() {
        assert_eq!(tables("SELECT 1; SELECT * FROM sbtest_2"), Vec::<String>::new());
    }

    #[test]
    fn explain_collects_inner_tables() {
        assert_eq!(tables("EXPLAIN SELECT * FROM sbtest_1 WHERE id=1"), vec!["sbtest_1"]);
        assert_eq!(
            tables("EXPLAIN FORMAT=JSON UPDATE sbtest_2 SET a=1 WHERE id=2"),
            vec!["sbtest_2"]
        );
        assert!(tables("EXPLAIN SELECT 1").is_empty());
    }

    #[test]
    fn garbage_no_panic() {
        for s in [
            "'abc", "/* x", "SELECT * FROM (SELECT", "UPDATE t SET a=", "WITH c AS (SELECT 1",
            "SELECT * FROM t WHERE a = ", ")))(((", "SELECT '' FROM t WHERE x='' AND y=2",
            "SELECT * FROM t1, ", "INSERT INTO (SELECT", "DELETE FROM", "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<5) SELECT * FROM c",
        ] {
            let a = analyze_full(s);
            let _ = (a.stmt_type, a.table_names, a.shard_keys, a.uncertain);
        }
    }

    #[test]
    fn read_only_classification() {
        use crate::parser::analyze::ReadClass;
        // 可分流
        assert_eq!(classify_read_only("SELECT * FROM sbtest_1 WHERE id=1"), ReadClass::Eligible);
        assert_eq!(classify_read_only("select a, b from t where x=1 AND y='for update'"), ReadClass::Eligible);
        assert_eq!(classify_read_only("SELECT 1"), ReadClass::Eligible);
        // 强制 leader
        assert_eq!(classify_read_only("SELECT * FROM t WHERE a=1 FOR UPDATE"), ReadClass::LeaderRequired("for-update-share"));
        assert_eq!(classify_read_only("SELECT * FROM t FOR SHARE"), ReadClass::LeaderRequired("for-update-share"));
        assert_eq!(classify_read_only("SELECT * FROM t LOCK IN SHARE MODE"), ReadClass::LeaderRequired("lock-in-share"));
        assert_eq!(classify_read_only("SELECT @x"), ReadClass::LeaderRequired("user-variable"));
        assert_eq!(classify_read_only("SELECT @@session.autocommit"), ReadClass::LeaderRequired("user-variable"));
        assert_eq!(classify_read_only("SELECT LAST_INSERT_ID()"), ReadClass::LeaderRequired("session-function"));
        assert_eq!(classify_read_only("SELECT FOUND_ROWS()"), ReadClass::LeaderRequired("session-function"));
        assert_eq!(classify_read_only("SELECT SQL_CALC_FOUND_ROWS * FROM t"), ReadClass::LeaderRequired("found-rows-prefix"));
        assert_eq!(classify_read_only("SELECT * FROM t INTO OUTFILE '/tmp/x'"), ReadClass::LeaderRequired("select-into"));
        assert_eq!(classify_read_only("INSERT INTO t SELECT * FROM t2"), ReadClass::LeaderRequired("not-select"));
        assert_eq!(classify_read_only("SELECT 1; SELECT 2"), ReadClass::LeaderRequired("multi-statement"));
        // 字符串/注释里的危险词不误伤
        assert_eq!(classify_read_only("SELECT 'for update' AS s, '@x' FROM t"), ReadClass::Eligible);
        assert_eq!(classify_read_only("SELECT /* for update */ * FROM t"), ReadClass::Eligible);
        // 标识符内含关键字不误伤
        assert_eq!(classify_read_only("SELECT update_x, id FROM t WHERE update_x=1"), ReadClass::Eligible);
    }
}
