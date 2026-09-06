-- sysbench 性能测试表（对齐 sysbench 标准 schema）
-- 表会在 prepare 阶段由 sysbench 自动创建，这里做额外定制

CREATE DATABASE IF NOT EXISTS sbtest;
USE sbtest;

-- 预创建 sbtest1 确保表存在（sysbench prepare 会 DROP + CREATE）
CREATE TABLE IF NOT EXISTS sbtest1 (
    id    INT PRIMARY KEY AUTO_INCREMENT,
    k     INT NOT NULL DEFAULT 0,
    c     CHAR(120) NOT NULL DEFAULT '',
    pad   CHAR(60) NOT NULL DEFAULT '',
    KEY idx_k (k)
) ENGINE=InnoDB;
