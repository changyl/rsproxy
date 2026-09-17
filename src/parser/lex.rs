// SQL 词法器(零拷贝,基于字节扫描的迭代式 tokenizer)
//
// 背景:parser 旧实现是对整条 SQL 做字符串遍历 + `find(" FROM ")` 类子串搜索,
// 字符串/注释/反引号内的关键字会污染结果,复杂 SQL(子查询/CTE/UNION/派生表)
// 无法正确处理。本模块提供一个单遍词法器:
//
// - token 只记录 kind + [start,end) 字节区间,文本直接从源串切片,**零拷贝**;
// - 正确处理:单/双引号字符串(含 `\'`、`''` 转义)、三种注释、反引号标识符
//   (含 ``` `` ``` 转义)、数字(十进制/科学计数/0x/0b)、操作符、`?` 占位符;
// - 关键字不做上卷拷贝,需要判断时用 ASCII 大小写不敏感逐字节比较;
// - 未闭合字符串/注释等畸形输入不 panic,置 `err` 标记后停止产出 token,
//   调用方按"不确定"保守降级。
//
// 语义约定(与 MySQL 默认一致):
//   `"..."` 视为字符串字面量(非 ANSI_QUOTES 标识符),归入 Str;
//   `` `...` `` 为引号标识符,归入 Word(span 含反引号,便于尾部按原名剥离)。

/// token 类别
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// 标识符/关键字(含反引号引用的标识符;span 含反引号)。是否关键字由调用方按上下文比较
    Word,
    /// 单/双引号字符串字面量(span 含引号)
    Str,
    /// 数字字面量(十进制/小数/指数/0x/0b,span 为字面量原文)
    Num,
    /// 注释(`-- `/`#` 行注释、`/* */` 块注释;span 含注释界定符)
    Comment,
    /// `?` 占位符
    Placeholder,
    /// 操作符/标点
    Punct,
}

/// 一个 token:[start,end) 为源串字节区间,文本 = &sql[start..end]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub start: usize,
    pub end: usize,
}

impl Token {
    #[inline]
    pub fn text<'a>(&self, sql: &'a str) -> &'a str {
        &sql[self.start..self.end]
    }
}

/// 单遍词法器。
pub struct Lexer<'a> {
    bytes: &'a [u8],
    pos: usize,
    /// 遇到未闭合字符串/注释/非法结构后置位,后续 next() 返回 None
    err: bool,
}

impl<'a> Lexer<'a> {
    pub fn new(sql: &'a str) -> Self {
        Lexer {
            bytes: sql.as_bytes(),
            pos: 0,
            err: false,
        }
    }

    /// 是否遇到畸形输入(未闭合引号/注释等)
    pub fn is_err(&self) -> bool {
        self.err
    }

    /// 扫描下一个 token;跳过空白。
    pub fn next_token(&mut self) -> Option<Token> {
        if self.err {
            return None;
        }
        let n = self.bytes.len();
        // 跳过空白
        while self.pos < n && is_ws(self.bytes[self.pos]) {
            self.pos += 1;
        }
        if self.pos >= n {
            return None;
        }
        let start = self.pos;
        let c = self.bytes[self.pos];

        // ── 注释 ──
        if c == b'/' && self.pos + 1 < n && self.bytes[self.pos + 1] == b'*' {
            self.pos += 2;
            loop {
                if self.pos + 1 >= n {
                    // 未闭合块注释:视为错误但 token 覆盖到末尾
                    self.err = true;
                    break;
                }
                if self.bytes[self.pos] == b'*' && self.bytes[self.pos + 1] == b'/' {
                    self.pos += 2;
                    break;
                }
                self.pos += 1;
            }
            return Some(Token {
                kind: TokenKind::Comment,
                start,
                end: self.pos,
            });
        }
        if c == b'#' {
            self.pos += 1;
            while self.pos < n && self.bytes[self.pos] != b'\n' {
                self.pos += 1;
            }
            return Some(Token {
                kind: TokenKind::Comment,
                start,
                end: self.pos,
            });
        }
        if c == b'-' && self.pos + 1 < n && self.bytes[self.pos + 1] == b'-' {
            // MySQL:`--` 后必须跟空白/控制字符才是注释,否则是减号
            if self.pos + 2 >= n || is_ws(self.bytes[self.pos + 2]) {
                self.pos += 2;
                while self.pos < n && self.bytes[self.pos] != b'\n' {
                    self.pos += 1;
                }
                return Some(Token {
                    kind: TokenKind::Comment,
                    start,
                    end: self.pos,
                });
            }
        }

        // ── 字符串(单引号)──
        if c == b'\'' {
            self.pos += 1;
            let (end, ok) = self.scan_quoted(b'\'');
            if !ok {
                self.err = true;
            }
            return Some(Token {
                kind: TokenKind::Str,
                start,
                end,
            });
        }
        // ── 字符串(双引号;MySQL 默认双引号即字符串)──
        if c == b'"' {
            self.pos += 1;
            let (end, ok) = self.scan_quoted(b'"');
            if !ok {
                self.err = true;
            }
            return Some(Token {
                kind: TokenKind::Str,
                start,
                end,
            });
        }
        // ── 反引号标识符 ──
        if c == b'`' {
            self.pos += 1;
            let mut closed = false;
            while self.pos < n {
                let b = self.bytes[self.pos];
                if b == b'`' {
                    if self.pos + 1 < n && self.bytes[self.pos + 1] == b'`' {
                        self.pos += 2; // 转义反引号
                        continue;
                    }
                    self.pos += 1;
                    closed = true;
                    break;
                }
                self.pos += 1;
            }
            if !closed {
                self.err = true; // 未闭合
            }
            return Some(Token {
                kind: TokenKind::Word,
                start,
                end: self.pos,
            });
        }

        // ── 数字 ──
        if c.is_ascii_digit() || (c == b'.' && self.pos + 1 < n && self.bytes[self.pos + 1].is_ascii_digit())
        {
            let end = self.scan_number();
            return Some(Token {
                kind: TokenKind::Num,
                start,
                end,
            });
        }

        // ── 标识符/词 ──
        if is_word_start(c) {
            while self.pos < n && is_word_cont(self.bytes[self.pos]) {
                self.pos += 1;
            }
            return Some(Token {
                kind: TokenKind::Word,
                start,
                end: self.pos,
            });
        }

        // ── `?` 占位符 ──
        if c == b'?' {
            self.pos += 1;
            return Some(Token {
                kind: TokenKind::Placeholder,
                start,
                end: self.pos,
            });
        }

        // ── 操作符/标点(优先多字符)──
        if self.pos + 2 < n {
            let three = &self.bytes[self.pos..self.pos + 3];
            if three == b"->>" {
                self.pos += 3;
                return Some(Token {
                    kind: TokenKind::Punct,
                    start,
                    end: self.pos,
                });
            }
        }
        if self.pos + 1 < n {
            let two = &self.bytes[self.pos..self.pos + 2];
            if matches!(
                two,
                b"<=" | b">=" | b"<>" | b"!=" | b"&&" | b"||" | b"<<" | b">>" | b":=" | b"->"
            ) {
                self.pos += 2;
                return Some(Token {
                    kind: TokenKind::Punct,
                    start,
                    end: self.pos,
                });
            }
        }
        // 单字符标点(含未识别的字节:保守作为 punct 单字节推进,不 panic)
        self.pos += 1;
        Some(Token {
            kind: TokenKind::Punct,
            start,
            end: self.pos,
        })
    }

    /// 扫描 `'...'`/`"..."` 内容;返回 (end, 是否闭合)。
    fn scan_quoted(&mut self, quote: u8) -> (usize, bool) {
        let n = self.bytes.len();
        loop {
            if self.pos >= n {
                return (self.pos, false);
            }
            let b = self.bytes[self.pos];
            if b == b'\\' && self.pos + 1 < n {
                self.pos += 2; // 反斜杠转义(如 \'、\\)
                continue;
            }
            if b == quote {
                if self.pos + 1 < n && self.bytes[self.pos + 1] == quote {
                    self.pos += 2; // 双写引号('' / "")
                    continue;
                }
                self.pos += 1; // 闭合
                return (self.pos, true);
            }
            self.pos += 1;
        }
    }

    /// 扫描数字字面量(十进制/小数/指数/0x/0b),返回 end(不含,已推进 pos)。
    fn scan_number(&mut self) -> usize {
        let n = self.bytes.len();
        let start = self.pos;

        // 0x... / 0X... 十六进制
        if self.bytes[self.pos] == b'0'
            && self.pos + 1 < n
            && matches!(self.bytes[self.pos + 1], b'x' | b'X')
        {
            self.pos += 2;
            while self.pos < n && is_hex(self.bytes[self.pos]) {
                self.pos += 1;
            }
            return self.pos;
        }
        // 0b... / 0B... 二进制
        if self.bytes[self.pos] == b'0'
            && self.pos + 1 < n
            && matches!(self.bytes[self.pos + 1], b'b' | b'B')
        {
            self.pos += 2;
            while self.pos < n && matches!(self.bytes[self.pos], b'0' | b'1') {
                self.pos += 1;
            }
            return self.pos;
        }

        let _ = start;
        // 十进制:整数部分(若以 '.' 开头则无整数部分)
        while self.pos < n && self.bytes[self.pos].is_ascii_digit() {
            self.pos += 1;
        }
        // 小数部分
        if self.pos < n && self.bytes[self.pos] == b'.' {
            // 仅当 '.' 后是数字或当前是 '.' 开头数字时并入
            let mut q = self.pos + 1;
            let mut has = false;
            while q < n && self.bytes[q].is_ascii_digit() {
                q += 1;
                has = true;
            }
            if has {
                self.pos = q;
            } else if self.pos + 1 >= n || !self.bytes[self.pos + 1].is_ascii_digit() {
                // "1." 视作整数加小数点:并入 '.'(兼容 "1." 写法)
                self.pos += 1;
            }
        }
        // 指数部分
        if self.pos < n && matches!(self.bytes[self.pos], b'e' | b'E') {
            let mut q = self.pos + 1;
            if q < n && matches!(self.bytes[q], b'+' | b'-') {
                q += 1;
            }
            let d0 = q;
            while q < n && self.bytes[q].is_ascii_digit() {
                q += 1;
            }
            if q > d0 {
                self.pos = q;
            }
        }
        self.pos
    }
}

#[inline]
fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | 0x0b | 0x0c)
}

#[inline]
fn is_hex(b: u8) -> bool {
    b.is_ascii_digit() || matches!(b, b'a'..=b'f' | b'A'..=b'F')
}

#[inline]
fn is_word_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'$' || b >= 0x80
}

#[inline]
fn is_word_cont(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// 把整条 SQL 词法化为 token 列表。
pub fn tokenize(sql: &str) -> (Vec<Token>, bool) {
    let mut lex = Lexer::new(sql);
    let mut toks = Vec::with_capacity(sql.len() / 4 + 8);
    while let Some(t) = lex.next_token() {
        toks.push(t);
    }
    (toks, lex.is_err())
}

// ─── 关键字/词比较工具(零分配)───

/// ASCII 大小写不敏感比较两个字符串是否相等。
#[inline]
pub fn eq_ignore_ascii_case(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// 判断 token 是否为(大小写不敏感的)指定词;只有 Word token 可能为词。
#[inline]
pub fn is_word(sql: &str, t: Token, word: &str) -> bool {
    t.kind == TokenKind::Word && eq_ignore_ascii_case(t.text(sql), word)
}

/// 判断 token 是否为指定的单字符标点。
#[inline]
pub fn is_punct(sql: &str, t: Token, ch: char) -> bool {
    t.kind == TokenKind::Punct && t.text(sql).len() == 1 && t.text(sql).as_bytes()[0] == ch as u8
}

/// 从 token 列表中取第 `from` 个 token 之后下一个非注释 token 的索引。
pub fn next_meaningful(toks: &[Token], from: usize) -> Option<usize> {
    (from..toks.len()).find(|&i| toks[i].kind != TokenKind::Comment)
}

/// 从 token 列表中取第 `from` 个 token 之后第一个 Word token 的索引(跳过注释)。
pub fn next_word(toks: &[Token], from: usize) -> Option<usize> {
    (from..toks.len()).find(|&i| toks[i].kind == TokenKind::Word)
}

/// 找到与 `open`(第 from 处)配对的 `)` token 索引。
/// `open` 位置必须是 `(`(punct,单字符);返回闭括号 token 索引。
pub fn matching_paren(sql: &str, toks: &[Token], open: usize) -> Option<usize> {
    let t = *toks.get(open)?;
    if t.kind != TokenKind::Punct || t.text(sql) != "(" {
        return None;
    }
    let mut depth = 0usize;
    for (i, tk) in toks.iter().enumerate().skip(open) {
        if tk.kind != TokenKind::Punct {
            continue;
        }
        match tk.text(sql) {
            "(" => depth += 1,
            ")" => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(sql: &str) -> Vec<String> {
        let (toks, _) = tokenize(sql);
        toks.into_iter()
            .map(|t| {
                if t.kind == TokenKind::Punct {
                    format!("[{}]", t.text(sql))
                } else {
                    t.text(sql).to_string()
                }
            })
            .collect()
    }

    #[test]
    fn basic_words_and_punct() {
        assert_eq!(
            texts("SELECT * FROM t WHERE id=1"),
            vec![
                "SELECT", "[*]", "FROM", "t", "WHERE", "id", "[=]", "1"
            ]
        );
    }

    #[test]
    fn strings_with_escapes() {
        // it\'s 与 '' 双写
        assert_eq!(texts("'it\\'s'"), vec!["'it\\'s'"]);
        assert_eq!(texts("'McDonald''s'"), vec!["'McDonald''s'"]);
        // 未闭合 → err
        let (_, err) = tokenize("'abc");
        assert!(err);
    }

    #[test]
    fn comments_all_forms() {
        assert_eq!(texts("/* hint */ SELECT 1"), vec!["/* hint */", "SELECT", "1"]);
        assert_eq!(texts("-- c\nSELECT 1"), vec!["-- c", "SELECT", "1"]);
        assert_eq!(texts("# c\nSELECT 1"), vec!["# c", "SELECT", "1"]);
        // -- 后非空白 → 减号,非注释
        assert_eq!(texts("a--b"), vec!["a", "[-]", "[-]", "b"]);
        // 未闭合块注释 → err
        let (_, err) = tokenize("/* abc");
        assert!(err);
    }

    #[test]
    fn backtick_identifiers() {
        assert_eq!(texts("`order`"), vec!["`order`"]);
        // 转义反引号 a``b
        assert_eq!(texts("`a``b`"), vec!["`a``b`"]);
        let (_, err) = tokenize("`abc");
        assert!(err);
    }

    #[test]
    fn numbers() {
        assert_eq!(texts("1"), vec!["1"]);
        assert_eq!(texts("1.5"), vec!["1.5"]);
        assert_eq!(texts("1.5e-3"), vec!["1.5e-3"]);
        assert_eq!(texts("0xFF"), vec!["0xFF"]);
        assert_eq!(texts("0b101"), vec!["0b101"]);
        assert_eq!(texts(".5"), vec![".5"]);
        // t1 是标识符不是数字
        assert_eq!(texts("t1"), vec!["t1"]);
    }

    #[test]
    fn operators() {
        assert_eq!(
            texts("a<=b AND c<>d AND e!=f AND g&&h"),
            vec!["a", "[<=]", "b", "AND", "c", "[<>]", "d", "AND", "e", "[!=]", "f", "AND", "g", "[&&]", "h"]
        );
        assert_eq!(texts("? @x @@y"), vec!["?", "[@]", "x", "[@]", "[@]", "y"]);
    }

    #[test]
    fn unterminated_no_panic() {
        // 各种畸形输入都不应 panic
        for s in ["'abc", "\"abc", "/*", "-- ", "`abc", "a \\", "'''''", "(((", "0x"] {
            let _ = tokenize(s);
        }
    }

    #[test]
    fn utf8_ident_ok() {
        let (toks, err) = tokenize("SELECT 名称 FROM t");
        assert!(!err);
        assert_eq!(toks[1].text("SELECT 名称 FROM t"), "名称");
    }
}
