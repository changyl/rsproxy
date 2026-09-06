//! 进程资源采样(CPU/内存/FD),供面板与 Prometheus 负载指标展示,辅助扩缩容决策。
//!
//! 实现:Linux 读 `/proc/self/stat|statm|fd`(生产环境);非 Linux(如开发机
//! macOS)返回 `None`,面板显示 "-"。CPU 利用率 = 两次采样间进程 CPU 时间增量 /
//! 墙钟增量(窗口自适应,即上次请求到本次请求的间隔)。

use std::sync::atomic::{AtomicU64, Ordering};

/// 进程 CPU 时间与墙钟的最近采样点(跨请求共享,窗口 = 两次采样间隔)
struct Sample {
    /// 进程 CPU 总时间(µs)
    cpu_us: u64,
    /// 采样时刻墙钟(µs,自进程启动)
    wall_us: u64,
}

/// 进程负载采样器(需 Mutex 保护;仅在指标采集时短暂加锁)
pub struct ProcMeter {
    last: Sample,
    inited: bool,
    /// 采样器创建时刻(≈进程启动,AppCtx 于 main 启动时创建),uptime 基准
    started: std::time::Instant,
}

impl ProcMeter {
    pub fn new() -> Self {
        Self {
            last: Sample {
                cpu_us: 0,
                wall_us: 0,
            },
            inited: false,
            started: std::time::Instant::now(),
        }
    }

    /// 进程运行时长(秒,自采样器创建 ≈ 进程启动)
    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// 进程 CPU 利用率(%,0-1000 上限防异常)。
    /// 窗口 = 自上次采样以来的间隔,适配低频/高频采集。
    pub fn cpu_percent(&mut self) -> Option<f64> {
        let cpu = process_cpu_time_us()?;
        let wall = wall_us();
        if !self.inited {
            self.inited = true;
            self.last = Sample { cpu_us: cpu, wall_us: wall };
            return Some(0.0);
        }
        let dt_cpu = cpu.saturating_sub(self.last.cpu_us);
        let dt_wall = wall.saturating_sub(self.last.wall_us);
        self.last = Sample { cpu_us: cpu, wall_us: wall };
        if dt_wall == 0 {
            return Some(0.0);
        }
        let pct = dt_cpu as f64 / dt_wall as f64 * 100.0;
        Some(pct.min(1000.0))
    }

    /// 当前 RSS 内存(字节)
    pub fn memory_bytes(&self) -> Option<u64> {
        process_memory_bytes()
    }

    /// 打开的文件描述符数
    pub fn fd_count(&self) -> Option<u64> {
        process_fd_count()
    }
}

/// 进程累计 CPU 时间(µs)
fn process_cpu_time_us() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/self/stat").ok()?;
        // 字段 3(comm)可能含空格/括号,从最后一个 ')' 之后解析
        let rest = s.rsplit_once(')')?.1;
        let f: Vec<&str> = rest.split_whitespace().collect();
        // 从 state 起: [0]=state [1]=ppid ... [11]=utime [12]=stime
        // (对应 /proc/pid/stat 完整字段的第 14、15 个)
        let utime: u64 = f.get(11)?.parse().ok()?;
        let stime: u64 = f.get(12)?.parse().ok()?;
        // Linux 时钟频率(USER_HZ)通常 100
        let ticks_per_sec: u64 = 100;
        Some((utime + stime) * 1_000_000 / ticks_per_sec)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = Ordering::Relaxed; // 占位避免未用导入告警
        None
    }
}

/// 当前 RSS(字节)
fn process_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/self/statm").ok()?;
        // 第 2 字段 = RSS pages
        let rss_pages: u64 = s.split_whitespace().nth(1)?.parse().ok()?;
        // page 大小(通常 4096)
        Some(rss_pages * 4096)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// 打开的文件描述符数
fn process_fd_count() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_dir("/proc/self/fd").ok().map(|d| d.count() as u64)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// 墙钟(µs,自进程启动)——跨平台用单调时钟基准 + 进程启动时间估算。
/// 简化:用 Instant 基准换算,保证差值正确即可(绝对值无意义)。
fn wall_us() -> u64 {
    static START: AtomicU64 = AtomicU64::new(0);
    static BASE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let base = BASE.get_or_init(std::time::Instant::now);
    // 以固定基准换算为 µs 计数(差值语义正确)
    let _ = START.fetch_add(0, Ordering::Relaxed);
    base.elapsed().as_micros() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_never_panics_and_returns_finite() {
        let mut m = ProcMeter::new();
        // 首次调用返回 0;后续调用返回有限值(平台无数据时为 None 或 0)
        let first = m.cpu_percent();
        assert!(first.unwrap_or(0.0).is_finite());
        let second = m.cpu_percent();
        assert!(second.unwrap_or(0.0).is_finite());
        // 内存/FD:不 panic(可能为 None)
        let _ = m.memory_bytes();
        let _ = m.fd_count();
    }
}
