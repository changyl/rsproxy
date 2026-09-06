-- 2 分片场景 — 分片 1 初始化
-- 分片 1 的 sbtest1 表插 7 行(id 101-107):与分片 0(5 行,id 1-5)行数不同,
-- 用于验证查询实际落在了哪个分片:select count(*) from sbtest1 → 5 或 7

CREATE DATABASE IF NOT EXISTS sbtest;
USE sbtest;

CREATE TABLE IF NOT EXISTS sbtest1 (
    id    INT PRIMARY KEY AUTO_INCREMENT,
    k     INT NOT NULL DEFAULT 0,
    c     CHAR(120) NOT NULL DEFAULT '',
    pad   CHAR(60) NOT NULL DEFAULT '',
    KEY idx_k (k)
) ENGINE=InnoDB;

INSERT INTO sbtest1 (id, k, c, pad) VALUES
  (101, 11, 'shard1', 'row-1'),
  (102, 12, 'shard1', 'row-2'),
  (103, 13, 'shard1', 'row-3'),
  (104, 14, 'shard1', 'row-4'),
  (105, 15, 'shard1', 'row-5'),
  (106, 16, 'shard1', 'row-6'),
  (107, 17, 'shard1', 'row-7');

-- 尾号路由测试表:sbtest_1 属分片 1(尾号 1 % 2 = 1),仅存在于分片 1。
-- 代理按表尾号路由:`SELECT COUNT(*) FROM sbtest_1` 应落分片 1(7 行,标记 shard1)。
CREATE TABLE IF NOT EXISTS sbtest_1 (
    id    INT PRIMARY KEY AUTO_INCREMENT,
    k     INT NOT NULL DEFAULT 0,
    c     CHAR(120) NOT NULL DEFAULT '',
    pad   CHAR(60) NOT NULL DEFAULT '',
    KEY idx_k (k)
) ENGINE=InnoDB;

INSERT INTO sbtest_1 (id, k, c, pad) VALUES
  (101, 11, 'shard1', 'tail-1-1'),
  (102, 12, 'shard1', 'tail-1-2'),
  (103, 13, 'shard1', 'tail-1-3'),
  (104, 14, 'shard1', 'tail-1-4'),
  (105, 15, 'shard1', 'tail-1-5'),
  (106, 16, 'shard1', 'tail-1-6'),
  (107, 17, 'shard1', 'tail-1-7');
