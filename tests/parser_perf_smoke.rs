// parser 热路径性能冒烟(非 CI 门槛,`cargo test --release --test parser_perf_smoke -- --ignored --nocapture`)

use std::time::Instant;

use newproxy::parser::classify::{classify, table_names_for_routing};
use newproxy::parser::rewrite::normalize_sql;

/// 混合语料:简单(热路径常见)+ 复杂
fn corpus() -> Vec<String> {
    [
        "SELECT * FROM sbtest_1 WHERE id=1",
        "SELECT * FROM t WHERE id = 42 AND name = 'test' ORDER BY id LIMIT 10",
        "INSERT INTO sbtest_2 (id, k, c, pad) VALUES (1, 2, 'a', 'b')",
        "UPDATE sbtest_3 SET c='x' WHERE id=2",
        "DELETE FROM sbtest_4 WHERE id=3",
        "SHOW TABLES",
        "BEGIN",
        "COMMIT",
        "SET NAMES utf8mb4",
        "USE sbtest",
        "SELECT * FROM t1 JOIN t2 ON t1.id=t2.id LEFT JOIN t3 ON t3.id=t1.id WHERE t1.x=1",
        "SELECT (SELECT MAX(x) FROM t2) FROM t1 WHERE t1.id IN (SELECT id FROM t3)",
        "SELECT * FROM (SELECT id FROM t2) d JOIN t3 ON d.id=t3.id WHERE d.id=5",
        "INSERT INTO t1 (a) SELECT b FROM t2 WHERE c=1",
        "WITH c AS (SELECT id FROM t2) SELECT * FROM sbtest_1 JOIN c ON c.id=sbtest_1.id",
        "UPDATE t1 JOIN t2 ON t1.id=t2.id SET t1.a=1 WHERE t2.b=2",
        "DELETE t1 FROM t1 JOIN t2 ON t1.id=t2.id WHERE t2.x=1",
        "SELECT 'a FROM b' AS s, x FROM t1 WHERE y='-- and or'",
        "SELECT * FROM t1 UNION SELECT * FROM t2 UNION ALL SELECT 1",
        "SELECT * FROM `order` WHERE `order`.id=1 AND status='x'",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn bench<F: FnMut(&str)>(name: &str, iters: usize, mut f: F) {
    let c = corpus();
    // 预热
    for _ in 0..1000 {
        for s in &c {
            f(s);
        }
    }
    let t0 = Instant::now();
    for _ in 0..iters {
        for s in &c {
            f(s);
        }
    }
    let total = c.len() as u128 * iters as u128;
    let per = t0.elapsed().as_nanos() as f64 / total as f64;
    println!("{name:<28} {per:>8.0} ns/op  (共 {iters}×{} 条)", c.len());
}

#[test]
#[ignore]
fn parser_hot_path_perf_smoke() {
    bench("table_names_for_routing", 50_000, |s| {
        std::hint::black_box(table_names_for_routing(s));
    });
    bench("normalize_sql", 50_000, |s| {
        std::hint::black_box(normalize_sql(s));
    });
    bench("classify(全量)", 20_000, |s| {
        std::hint::black_box(classify(s));
    });
}
