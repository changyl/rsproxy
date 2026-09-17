// Xenon raft HA 真库核对诊断(手动运行,`cargo test --test ha_diag -- --ignored --nocapture`)
//
// 用法:设置环境变量 `HA_DIAG_CONF=<conf 路径>`(默认 conf/newproxy.conf,须含
// `[XenonRaft_*]` 段),然后运行本测试:
//   cargo test --test ha_diag -- --ignored --nocapture
//
// 输出:每个受管分片、每个成员的状态表原始行 + 判主结论,用于核对:
// - leader 列实际取值(host:raft_port)
// - view_id/epoch_id 是否随任期单调
// - updated_at 语义(秒/毫秒/字符串)与跨节点一致性(表是否复制传播)

#[cfg(test)]
mod diag {
    use newproxy::config::loader::load_config;

    fn conf_path() -> String {
        std::env::var("HA_DIAG_CONF").unwrap_or_else(|_| {
            format!("{}/conf/newproxy.conf", env!("CARGO_MANIFEST_DIR"))
        })
    }

    #[tokio::test]
    #[ignore]
    async fn dump_xenon_raft_status() {
        let cfg = load_config(&conf_path()).expect("load config");
        let mut managed = 0;
        for c in cfg.clusters.values() {
            for t in &c.tablets {
                let Some(x) = &t.xenon else { continue };
                managed += 1;
                println!(
                    "\n=== cluster {} tablet {} members={} consistency={} probe={}ms ===",
                    c.id,
                    t.tablet_id,
                    x.members.len(),
                    x.read_consistency,
                    x.probe_interval_ms
                );
                // 账号:集群第一个 db_user(需 mysql.xenon_raft_status SELECT 权限)
                let user = cfg
                    .db_users
                    .values()
                    .find(|u| u.cluster_name.eq_ignore_ascii_case(&c.name))
                    .cloned();
                let Some(user) = user else {
                    println!("  (no db_user for cluster {})", c.name);
                    continue;
                };
                let mut rows = Vec::new();
                for m in &x.members {
                    match newproxy::ha::probe::probe_raft_status(
                        &m.host,
                        m.mysql_port,
                        &user,
                        cfg.default_charset,
                        x.probe_timeout_ms,
                    )
                    .await
                    {
                        Ok(row) => {
                            println!(
                                "  member {:<12} rows={}  leader={:?} view={:?} epoch={:?} updated_at={:?}",
                                m.endpoint(),
                                row.row.len(),
                                row.leader_endpoint(),
                                row.view_id(),
                                row.epoch_id(),
                                row.row.get("updated_at")
                            );
                            rows.push(row);
                        }
                        Err(e) => println!("  member {:<12} probe FAILED: {e}", m.endpoint()),
                    }
                }
                let now = newproxy::ha::model::now_ms();
                match newproxy::ha::model::decide(&x.members, now, x.leader_stale_ms, rows) {
                    newproxy::ha::model::Decision::Valid(out) => println!(
                        "  >> leader = {}:{} (view {}, epoch {}, updated {})",
                        out.leader.host, out.leader.port, out.view_id, out.epoch_id, out.updated_at_ms
                    ),
                    newproxy::ha::model::Decision::Unknown => {
                        println!("  >> UNKNOWN: 无新鲜/可采纳判主(检查权限/表结构/成员清单)")
                    }
                }
            }
        }
        if managed == 0 {
            println!(
                "配置 {} 中没有 [XenonRaft_*] 段(受管分片数=0)。",
                conf_path()
            );
        }
    }
}
