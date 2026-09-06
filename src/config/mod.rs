// 配置解析:newproxy.conf INI 格式
pub mod loader;
pub mod model;

pub use loader::{load_config, ConfigError};
pub use model::{
    AppConfig, Cluster, ClusterId, ClusterTablet, ConfigCenterCfg, Database, DatabaseGroup, DbUser,
    GroupId, LogLevel, MasterSlave, ProductUser, RouteRule, ShardStrategy, TabletId, UserId,
};
