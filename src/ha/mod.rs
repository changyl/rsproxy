// Xenon raft 高可用适配
//
// 模块职责(见实现计划):
// - model: 运行时状态 + 判主纯逻辑(状态表真实结构:id, leader(raft 端点),
//   view_id, epoch_id, updated_at)
// - result: 最小 MySQL 文本结果集解析(探测/采样用)
// - probe:  最小探测客户端(复用 proto 握手,不进连接池)
// - (runner/center): 受管分片发现任务 + supervisor(见 mod/center 后续增量)

pub mod center;
pub mod consistency;
pub mod model;
pub mod probe;
pub mod result;
