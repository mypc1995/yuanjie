use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 把调用频率由他人决定的日志点——比如对端的消息速率，或
/// 某个重试循环——压缩为每个间隔最多一行。
///
/// 调试输出会写入日志文件，因此一个每收到一个包就触发一次的日志点会让
/// 对端决定一台机器向磁盘写入多少数据。若完全丢弃该行，则会
/// 掩盖真实故障，因此每个间隔保留一行，并带上自上次以来被抑制的全部
/// 次数。
///
/// 优先使用 [`throttled_log!`](crate::throttled_log) 宏，它会替你声明静态变量。
/// 只有当计数要放在行的末尾之外的地方时，才直接使用这个类型，
/// 或者当该计数还要驱动日志调用之外的行为时。
///
/// 每个日志点声明一个（它们之间不共享计数）：
///
/// ```ignore
/// static DROPPED_ICE: LogThrottle = LogThrottle::new(Duration::from_secs(60));
///
/// if let Some(n) = DROPPED_ICE.due() {
///     log::debug!("dropped {n} ICE candidate(s) with no route");
/// }
/// ```
pub struct LogThrottle {
    interval: Duration,
    state: Mutex<ThrottleState>,
}

struct ThrottleState {
    suppressed: u64,
    last: Option<Instant>,
}

impl LogThrottle {
    pub const fn new(interval: Duration) -> Self {
        Self {
            interval,
            state: Mutex::new(ThrottleState {
                suppressed: 0,
                last: None,
            }),
        }
    }

    /// 记录一次发生。返回需要报告的发生次数（含本次）
    /// （当该行到期时），若仍在间隔内则返回 `None`。
    ///
    /// 静默期后的第一次发生总是会上报，因此偶发故障不会
    /// 被间隔延迟。
    pub fn due(&self) -> Option<u64> {
        // 锁中毒只意味着其他某个线程在持有它时 panic 了；被保护的
        // 数据只是两个普通计数器，依然可用；而在此后整个进程生命周期内保持沉默
        // 比一个过期的计数更糟糕。
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.suppressed += 1;
        // 用 `map_or(true, ..)` 而不是 clippy 更偏好的 `is_none_or`：后者在
        // Rust 1.82 才稳定，而本 crate 依赖 CI 固定的 1.75 版本构建。
        #[allow(clippy::unnecessary_map_or)]
        let due = state
            .last
            .map_or(true, |last| last.elapsed() >= self.interval);
        if !due {
            return None;
        }
        state.last = Some(Instant::now());
        Some(std::mem::replace(&mut state.suppressed, 0))
    }
}

/// 从该调用点每个间隔最多记录一行，并以后缀形式带上它所代表的
/// 发生次数。
///
/// 每次展开都会声明自己的隐藏静态变量，因此两个日志点不会共享计数，
/// 新增一个只需一行：
///
/// ```ignore
/// throttled_log!(Duration::from_secs(5), warn, "rejected ipc peer {peer_pid:?}");
/// ```
///
/// 偶发事件记录保持不变；一阵爆发会压缩为 `... (x47)`。该计数包含
/// 正在上报的那一次发生，因此读起来是总数，而不是「还有 N 次」。
#[macro_export]
macro_rules! throttled_log {
    ($interval:expr, $level:ident, $($arg:tt)+) => {{
        static THROTTLE: $crate::log_throttle::LogThrottle =
            $crate::log_throttle::LogThrottle::new($interval);
        if let Some(n) = THROTTLE.due() {
            if n > 1 {
                $crate::log::$level!("{} (x{})", format_args!($($arg)+), n);
            } else {
                $crate::log::$level!("{}", format_args!($($arg)+));
            }
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    // 同一个 socket 的两个方向需要两个节流器：已连接 socket 上的 ICMP 错误
    // 只上报一次就会被清除，因此稳态是交替的（发送成功，下一次 recv
    // 上报它），而它们之间共享的任何东西都会被成功的一方在每个周期重置。
    #[test]
    fn separate_throttles_do_not_reset_each_other() {
        let send = LogThrottle::new(Duration::from_secs(60));
        let recv = LogThrottle::new(Duration::from_secs(60));
        assert_eq!(recv.due(), Some(1));
        for _ in 0..1_000 {
            // 发送侧成功不应给 recv 侧一个新的输出时机。
            assert_eq!(recv.due(), None);
        }
        assert_eq!(
            send.due(),
            Some(1),
            "the other direction keeps its own slot"
        );
    }

    #[test]
    fn first_call_reports_immediately() {
        let t = LogThrottle::new(Duration::from_secs(60));
        assert_eq!(t.due(), Some(1));
    }

    #[test]
    fn calls_inside_the_interval_are_counted_not_reported() {
        let t = LogThrottle::new(Duration::from_secs(60));
        assert_eq!(t.due(), Some(1));
        for _ in 0..100 {
            assert_eq!(t.due(), None);
        }
    }

    #[test]
    fn the_next_due_line_carries_everything_suppressed() {
        let t = LogThrottle::new(Duration::ZERO);
        assert_eq!(t.due(), Some(1));
        // 间隔为 0 时总是到期，因此每次调用恰好上报自己。
        assert_eq!(t.due(), Some(1));

        let t = LogThrottle::new(Duration::from_millis(30));
        assert_eq!(t.due(), Some(1));
        assert_eq!(t.due(), None);
        assert_eq!(t.due(), None);
        std::thread::sleep(Duration::from_millis(40));
        // 两次被抑制的调用加上这一次。
        assert_eq!(t.due(), Some(3));
    }
}
