-- 2 分片场景 — 分片 0 初始化
-- 分片 0 的 sbtest1 表插 5 行(id 1-5):与分片 1(7 行,id 101-107)行数不同,
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
  (1, 1, 'shard0', 'row-1'),
  (2, 2, 'shard0', 'row-2'),
  (3, 3, 'shard0', 'row-3'),
  (4, 4, 'shard0', 'row-4'),
  (5, 5, 'shard0', 'row-5');

-- 尾号路由测试表:sbtest_0 属分片 0(尾号 0 % 2 = 0),仅存在于分片 0。
-- 代理按表尾号路由:`SELECT COUNT(*) FROM sbtest_0` 应落分片 0(5 行,标记 shard0)。
CREATE TABLE IF NOT EXISTS sbtest_0 (
    id    INT PRIMARY KEY AUTO_INCREMENT,
    k     INT NOT NULL DEFAULT 0,
    c     CHAR(120) NOT NULL DEFAULT '',
    pad   CHAR(60) NOT NULL DEFAULT '',
    KEY idx_k (k)
) ENGINE=InnoDB;

INSERT INTO sbtest_0 (id, k, c, pad) VALUES
  (1, 1, 'shard0', 'tail-0-1'),
  (2, 2, 'shard0', 'tail-0-2'),
  (3, 3, 'shard0', 'tail-0-3'),
  (4, 4, 'shard0', 'tail-0-4'),
  (5, 5, 'shard0', 'tail-0-5');
