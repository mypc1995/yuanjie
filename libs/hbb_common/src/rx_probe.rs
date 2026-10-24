use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

/// 从对端接收到的字节数，在组帧之前统计。
///
/// 消息只有完整后才会到达应用层，因此一条大消息——比如剪贴板图片
/// 正是实践中常见的情况——会让接收循环在传输期间
/// 无事可观测，而一个持续发送的对端看起来和一个
/// 已经掉线的对端一模一样。这个计数器每次读取都会变化，因此跨越这类消息仍能观测到存活状态。
///
/// 故意使用计数器而非时间戳：`Instant::now()` 的开销要贵一个数量级，
/// 远超一次宽松加法，而读取方只会比较相邻的采样值，因此时钟
/// 只需每个采样周期取一次，而不是每次读取都取。
#[derive(Clone, Default)]
pub struct RxProbe(Arc<AtomicU64>);

impl RxProbe {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn add(&self, n: usize) {
        self.0.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// 每当有字节到达就会变化。比较相邻的采样值即可；其绝对值以及
    /// 回绕都没有意义。
    #[inline]
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_one_counter() {
        let probe = RxProbe::new();
        let cloned = probe.clone();
        assert_eq!(probe.get(), 0);
        cloned.add(7);
        assert_eq!(probe.get(), 7);
        probe.add(3);
        assert_eq!(cloned.get(), 10);
    }
}
