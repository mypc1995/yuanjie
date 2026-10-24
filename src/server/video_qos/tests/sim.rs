//! 用于 QoS 控制器的闭环网络仿真。
//!
//! 控制器的驱动方式与 `Connection` 驱动它的方式相同：每秒一个 TestDelay 探测，
//! 同时只有一个探测未完成，每个定时器 tick 都调用 `user_delay_response_elapsed`，
//! 每秒调用一次 `update_display_data`。 视频帧和探测共用同一个
//! FIFO，它代表下游的共享路径（流、传输、链路）：
//! 探测测量的是排在它前面交给该路径的字节数。 它不是
//! 服务端的 `tx_video` 通道（探测并不经过它），而且该
//! 模型不会像真实循环那样在发送被阻塞时拖住定时器。
//!
//! 三条独立的随机流保证 A/B 对比是配对的：网络
//! 轨迹（容量波动、停顿、丢包事件）在运行前由
//! 网络流单独生成，探测抖动是从它自己的流得到的按秒表格，
//! 而场景切换跟随挂钟，因此具有相同随机种子的两个控制器
//! 无论做出什么决定，都会面对相同的链路、相同的抖动和相同的内容时间线。
//!  只有帧大小噪声取决于产生了多少帧。
//!
//! 编码器模型守恒其码率预算：一次场景切换的代价相当于三
//! 帧的数据量，多出的部分由随后的帧偿还，因此
//! 在 CBR 下长期提供的负载与帧率无关。
//!
//! 它仍然只是一个模型，而不是真实网络：它不会重现真实传输的
//! 拥塞控制或真实编码器。 它的任务是展示控制器
//! 对家用 Wi-Fi、稳定中继或饱和
//! 上行链路所产生的*那类*行为会如何反应，且是确定性地、在多个随机种子下进行。
//!
//! CI 运行使用 1 到 20 号种子。 场景
//! 参数在有录制的 `qos_trace` 对其校准之前都只是有根据的猜测。
use super::*;

/// xorshift64* 生成器，因此测试无需外部 crate，且保持可复现。
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng((seed ^ 0x9E37_79B9_7F4A_7C15).max(1))
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// 在 `[0, 1)` 上均匀分布。
    fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.uniform()
    }

    fn normal(&mut self) -> f64 {
        let u1 = (1.0 - self.uniform()).max(1e-12);
        let u2 = self.uniform();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }

    fn log_normal(&mut self, median: f64, sigma: f64) -> f64 {
        median * (sigma * self.normal()).exp()
    }

    fn exponential(&mut self, mean: f64) -> f64 {
        -mean * (1.0 - self.uniform()).max(1e-12).ln()
    }
}

#[derive(Clone)]
pub struct Link {
    /// 阶梯调度 `(from_ms, kbps)`，按时间排序。
    pub capacity_kbps: Vec<(u32, f64)>,
    /// 容量相对于标称值的缓慢随机游走。
    pub wobble: f64,
    pub base_rtt_ms: f64,
    /// 加到每次探测往返上的对数正态抖动。
    pub jitter_median_ms: f64,
    pub jitter_sigma: f64,
    /// 每秒的丢包事件数。 可靠的传输会把一次丢包变成 200-400 毫秒的
    /// 重传停顿，随后有一秒的速率减半。
    pub loss_per_s: f64,
    /// 链路停顿之间的平均间隔（秒），`0` 表示从不停顿。
    pub stall_mean_interval_s: f64,
    /// 停顿时长的均匀分布范围（毫秒）。
    pub stall_ms: (f64, f64),
}

impl Link {
    fn capacity_at(&self, now_ms: u32) -> f64 {
        self.capacity_kbps
            .iter()
            .rev()
            .find(|(from, _)| *from <= now_ms)
            .map(|(_, kbps)| *kbps)
            .unwrap_or(self.capacity_kbps[0].1)
    }

    /// 容量最后一次被恢复到初始值的时刻（如果它曾下降过）。
    fn restore_ms(&self) -> Option<u32> {
        let initial = self.capacity_kbps[0].1;
        let mut dropped = false;
        for (from, kbps) in &self.capacity_kbps {
            if *kbps < initial {
                dropped = true;
            } else if dropped && *kbps >= initial {
                return Some(*from);
            }
        }
        None
    }
}

/// 链路在一次运行期间会做的一切，全部在运行开始前决定。
struct LinkTrace {
    capacity_kbps: Vec<f64>, // per tick, wobble and retransmission backoff applied
    stalled: Vec<bool>,      // per tick
}

fn mark(flags: &mut [bool], from_ms: f64, to_ms: f64) {
    let from = (from_ms / TICK_MS as f64).max(0.0) as usize;
    let to = ((to_ms / TICK_MS as f64).ceil() as usize).min(flags.len());
    for flag in flags.iter_mut().take(to).skip(from) {
        * flag = true;
    }
}

fn link_trace(link: &Link, ticks: usize, rng: &mut Rng) -> LinkTrace {
    let mut capacity_kbps = vec![0.0; ticks];
    let mut stalled = vec![false; ticks];
    let mut backoff = vec![false; ticks];
    let mut wobble = 0.0_f64;
    for (i, capacity) in capacity_kbps.iter_mut().enumerate() {
        let now = i as u32 * TICK_MS;
        if now % 100 == 0 {
            wobble = (wobble + rng.normal() * 0.03).clamp(-link.wobble, link.wobble);
        }
        * capacity = link.capacity_at(now) * (1.0 + wobble);
    }
    if link.stall_mean_interval_s > 0.0 {
        let mut start = rng.exponential(link.stall_mean_interval_s) * 1000.0;
        while start < (ticks as f64) * TICK_MS as f64 {
            let len = rng.range(link.stall_ms.0, link.stall_ms.1);
            mark(&mut stalled, start, start + len);
            start += rng.exponential(link.stall_mean_interval_s) * 1000.0;
        }
    }
    if link.loss_per_s > 0.0 {
        let per_tick = link.loss_per_s * TICK_MS as f64 / 1000.0;
        for i in 0..ticks {
            if rng.uniform() < per_tick {
                let start = (i as u32 * TICK_MS) as f64;
                let len = rng.range(200.0, 400.0);
                mark(&mut stalled, start, start + len);
                mark(&mut backoff, start + len, start + len + 1000.0);
            }
        }
    }
    for (capacity, backoff) in capacity_kbps.iter_mut().zip(&backoff) {
        if *backoff {
            * capacity *= 0.5;
        }
    }
    LinkTrace {
        capacity_kbps,
        stalled,
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Content {
    /// 每一帧都在变化：视频通话或电影。
    Video,
    /// 基本静止：每秒只有几帧发生变化。
    Office,
}

/// 编码器如何把码率转换成帧大小。
#[derive(Clone, Copy, PartialEq)]
pub enum EncoderModel {
    /// VP8、VP9 和 AV1 针对毫秒级时间戳运行 CBR：每秒更少的帧
    /// 意味着更大的帧，码率保持不变。 只有比例能改变字节量。
    Cbr,
    /// 按固定 30 fps 码率控制假设配置的硬件编码器：
    /// 每一帧携带码率的三十分之一，因此更少的帧意味着更少的
    /// 字节。 实际的硬件行为取决于后端（Android 的 MediaCodec
    /// 路径运行 VBR）。
    FixedRate,
}

#[derive(Clone)]
pub struct Scenario {
    pub name: &'static str,
    pub seconds: u32,
    pub limit: u32,
    pub quality: Quality,
    pub abr: bool,
    pub content: Content,
    pub encoder: EncoderModel,
    pub link: Link,
    pub seed: u64,
}

/// 比例为 1.0 时的码率；均衡画质（0.67）则对应约 4 Mbps。
const BASE_KBPS: f64 = 6000.0;
/// 硬件编码器所配置的帧率。
const ENCODER_CONFIGURED_FPS: f64 = 30.0;
/// 帧大小围绕其预算的对数正态分布。
const FRAME_SIZE_SIGMA: f64 = 0.35;
const TICK_MS: u32 = 10;
/// 在这一时刻之前取得的采样属于冷启动，而不是稳态。
const WARM_UP_MS: u32 = 15_000;
/// 目标与队列保持这么久之后，一次恢复才算数。
const SUSTAINED_MS: u32 = 5_000;
/// 每个场景运行时使用的随机种子。
pub const SEEDS: std::ops::RangeInclusive<u64> = 1..=20;

/// 在守恒码率预算下的帧大小。
struct Encoder {
    model: EncoderModel,
    content: Content,
    rng: Rng,
    next_scene_ms: u32,
    debt_bits: f64,
}

/// 每五秒视频发生一次场景切换。
const SCENE_INTERVAL_MS: u32 = 5_000;

impl Encoder {
    fn frame_bits(&mut self, now_ms: u32, bitrate_kbps: f64, produce_rate: f64) -> f64 {
        let target = match (self.content, self.model) {
            // 无论码率控制怎么做，静止画面中变化的区域都很小。
            (Content::Office, _) => bitrate_kbps * 1000.0 / ENCODER_CONFIGURED_FPS * 0.3,
            (Content::Video, EncoderModel::Cbr) => bitrate_kbps * 1000.0 / produce_rate,
            (Content::Video, EncoderModel::FixedRate) => {
                bitrate_kbps * 1000.0 / ENCODER_CONFIGURED_FPS
            }
        };
        // 均值为 1：这种离散度不得改变所提供的负载。
        let noise = self.rng.log_normal(1.0, FRAME_SIZE_SIGMA)
            * (-FRAME_SIZE_SIGMA * FRAME_SIZE_SIGMA / 2.0).exp();
        let mut bits = target * noise;
        // 场景切换跟随挂钟而不是帧计数，因此每个
        // 控制器都面对相同的内容时间线。
        let scene_change = self.content == Content::Video && now_ms >= self.next_scene_ms;
        if scene_change {
            self.next_scene_ms += SCENE_INTERVAL_MS;
            // 一次场景切换的代价相当于几帧的数据；码率控制会从
            // 随后的帧中把它赚回来。
            bits *= 3.0;
            self.debt_bits += bits - target;
        } else if self.debt_bits > 0.0 {
            let repay = self.debt_bits.min(target * 0.5).min(bits * 0.5);
            bits -= repay;
            self.debt_bits -= repay;
        }
        bits
    }
}

struct Packet {
    bits: f64,
    enqueued_ms: u32,
    probe_sent_ms: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub name: String,
    pub seed: u64,
    pub limit: u32,
    /// 控制器目标，预热之后每 100 毫秒采样一次。
    pub mean_target_fps: f64,
    pub p10_target_fps: u32,
    pub min_target_fps: u32,
    /// 在测量时间内目标低于上限一半所占的比例。
    pub below_half_pct: f64,
    /// 编码器每秒产生的帧数。
    pub produced_fps: f64,
    /// 每秒离开共享路径的帧数。
    pub delivered_fps: f64,
    /// 已交付帧在共享路径中停留的时间，取 95 百分位。
    pub frame_age_p95_ms: u32,
    pub queue_p95_ms: u32,
    pub max_delay_ms: u32,
    /// 链路是否会下降并恢复其容量。
    pub has_restore: bool,
    /// 从容量恢复，到目标达到上限且队列低于
    /// 200 毫秒并持续 `SUSTAINED_MS` 所用的时间。
    pub recovery_ms: Option<u32>,
    /// 最初 `WARM_UP_MS` 期间的目标最低值。
    pub cold_start_min_fps: u32,
    /// 目标首次达到上限 90% 的时刻。
    pub time_to_90pct_ms: Option<u32>,
    pub trace: Vec<(u32, u32, u32, f32)>, // (time_ms, target fps, queue_ms, ratio)
}

fn percentile_u32(values: &[u32], p: f64) -> u32 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted[(((sorted.len() - 1) as f64) * p).round() as usize]
}

fn percentile_f64(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    sorted[(((sorted.len() - 1) as f64) * p).round() as usize]
}

pub fn run(sc: &Scenario) -> Report {
    let mut network_rng = Rng::new(sc.seed);
    let mut probe_rng = Rng::new(sc.seed ^ 0x5052_4F42_45);
    let mut encoder = Encoder {
        model: sc.encoder,
        content: sc.content,
        rng: Rng::new(sc.seed ^ 0x454E_434F_4445),
        next_scene_ms: SCENE_INTERVAL_MS,
        debt_bits: 0.0,
    };
    let total_ms = sc.seconds * 1000;
    let ticks = (total_ms / TICK_MS) as usize;
    let link = link_trace(&sc.link, ticks, &mut network_rng);
    // 探测抖动按探测发送的秒数索引，因此控制器成功发出的
    // 探测数量不会改变下一次探测遇到的抖动。
    let probe_jitter_ms: Vec<f64> = (0..=sc.seconds)
        .map(|_| probe_rng.log_normal(sc.link.jitter_median_ms, sc.link.jitter_sigma))
        .collect();

    let mut qos = super::smoke::session(sc.limit, sc.quality);
    qos.abr_config = sc.abr;
    if sc.abr {
        qos.new_display("sim".to_owned());
        qos.set_support_changing_quality("sim", true);
    }

    let mut queue: VecDeque<Packet> = VecDeque::new();
    let mut queued_bits = 0.0_f64;
    let mut encode_phase = 0.0_f64;
    let mut encoded_this_second = 0_usize;
    let mut probe_sent: Option<u32> = None;
    let mut replies: Vec<(u32, u32)> = Vec::new(); // (arrive_ms, delay_ms)
    let restore_ms = sc.link.restore_ms();

    let mut fps_samples = Vec::new();
    let mut queue_samples = Vec::new();
    let mut produced = 0_u64;
    let mut delivered = 0_u64;
    let mut frame_ages = Vec::new();
    let mut trace = Vec::new();
    let mut max_delay = 0_u32;
    let mut recovery_ms = None;
    let mut good_since: Option<u32> = None;
    let mut cold_start_min_fps = u32::MAX;
    let mut time_to_90pct_ms = None;

    for tick in 0..ticks {
        let now = tick as u32 * TICK_MS;
        qos.advance_ms(TICK_MS as u64);
        let capacity_kbps = link.capacity_kbps[tick];

        // 编码器：按控制器的速率产生帧，大小由控制器的比例决定。
        // 视频循环在应用新比例后立即上报码率。
        let fps = qos.fps();
        let ratio = qos.ratio();
        let bitrate_kbps = BASE_KBPS * ratio as f64;
        qos.store_bitrate(bitrate_kbps as u32);
        let produce_rate = match sc.content {
            Content::Video => fps as f64,
            Content::Office => (fps as f64).min(2.0),
        };
        encode_phase += produce_rate * TICK_MS as f64 / 1000.0;
        while encode_phase >= 1.0 {
            encode_phase -= 1.0;
            encoded_this_second += 1;
            if now >= WARM_UP_MS {
                produced += 1;
            }
            let bits = encoder.frame_bits(now, bitrate_kbps, produce_rate);
            queue.push_back(Packet {
                bits,
                enqueued_ms: now,
                probe_sent_ms: None,
            });
            queued_bits += bits;
        }

        // 共享路径的排空：探测很小，一旦到达队首就离开。
        if !link.stalled[tick] {
            let mut budget = capacity_kbps * TICK_MS as f64;
            while budget > 0.0 {
                let Some(head) = queue.front_mut() else { break };
                if let Some(sent) = head.probe_sent_ms {
                    let round_trip = sc.link.base_rtt_ms + probe_jitter_ms[(sent / 1000) as usize];
                    let arrive = now + round_trip as u32;
                    replies.push((arrive, arrive - sent));
                    queue.pop_front();
                    continue;
                }
                let take = budget.min(head.bits);
                head.bits -= take;
                queued_bits -= take;
                budget -= take;
                if head.bits <= 1e-9 {
                    if now >= WARM_UP_MS {
                        delivered += 1;
                        frame_ages.push(now - head.enqueued_ms);
                    }
                    queue.pop_front();
                }
            }
        }

        // 探测回复到达控制器。
        replies.sort_by_key(|r| r.0);
        while replies.first().is_some_and(|r| r.0 <= now) {
            let (_, delay) = replies.remove(0);
            max_delay = max_delay.max(delay);
            probe_sent = None;
            qos.user_network_delay(1, delay);
        }

        // 连接的一秒定时器。
        if now % 1000 == 0 {
            if probe_sent.is_none() {
                probe_sent = Some(now);
                queue.push_back(Packet {
                    bits: 0.0,
                    enqueued_ms: now,
                    probe_sent_ms: Some(now),
                });
            }
            qos.user_delay_response_elapsed(1, (now - probe_sent.unwrap()) as u128);
            if sc.abr {
                qos.update_display_data("sim", encoded_this_second);
            }
            encoded_this_second = 0;
        }

        if now % 100 == 0 {
            let queue_ms = (queued_bits / capacity_kbps.max(1.0)) as u32;
            let fps = qos.fps();
            trace.push((now, fps, queue_ms, qos.ratio()));
            if now < WARM_UP_MS {
                cold_start_min_fps = cold_start_min_fps.min(fps);
            } else {
                fps_samples.push(fps);
                queue_samples.push(queue_ms);
            }
            if time_to_90pct_ms.is_none() && fps * 10 >= sc.limit * 9 {
                time_to_90pct_ms = Some(now);
            }
            if let Some(restore) = restore_ms {
                if now >= restore && recovery_ms.is_none() {
                    if fps >= sc.limit && queue_ms < 200 {
                        let since = *good_since.get_or_insert(now);
                        if now - since >= SUSTAINED_MS {
                            recovery_ms = Some(since - restore);
                        }
                    } else {
                        good_since = None;
                    }
                }
            }
        }
    }

    let measured_s = (total_ms - WARM_UP_MS) as f64 / 1000.0;
    let below_half = fps_samples.iter().filter(|f| **f * 2 < sc.limit).count();
    Report {
        name: sc.name.to_owned(),
        seed: sc.seed,
        limit: sc.limit,
        mean_target_fps: fps_samples.iter().map(|f| *f as f64).sum::<f64>()
            / fps_samples.len().max(1) as f64,
        p10_target_fps: percentile_u32(&fps_samples, 0.10),
        min_target_fps: fps_samples.iter().copied().min().unwrap_or(0),
        below_half_pct: 100.0 * below_half as f64 / fps_samples.len().max(1) as f64,
        produced_fps: produced as f64 / measured_s,
        delivered_fps: delivered as f64 / measured_s,
        frame_age_p95_ms: percentile_u32(&frame_ages, 0.95),
        queue_p95_ms: percentile_u32(&queue_samples, 0.95),
        max_delay_ms: max_delay,
        has_restore: restore_ms.is_some(),
        recovery_ms,
        cold_start_min_fps,
        time_to_90pct_ms,
        trace,
    }
}

/// 一个场景在所有种子下的运行结果，按断言所使用的统计量汇总。
#[derive(Debug)]
pub struct Summary {
    pub name: String,
    pub limit: u32,
    pub mean_target_median: f64,
    pub p10_target_worst: u32,
    pub below_half_p90: f64,
    pub queue_p95_p90: u32,
    pub delivered_median: f64,
    pub frame_age_p95_p90: u32,
    pub has_restore: bool,
    /// 最慢的持续恢复时间，若有任一种子从未恢复则为 `None`。
    pub recovery_worst_ms: Option<u32>,
    pub cold_start_min_median: u32,
    /// 达到上限 90% 的最慢时间，若有任一种子从未达到则为 `None`。
    pub time_to_90pct_worst_ms: Option<u32>,
}

impl Summary {
    pub fn of(reports: &[Report]) -> Self {
        let f = |g: fn(&Report) -> f64| reports.iter().map(g).collect::<Vec<_>>();
        let u = |g: fn(&Report) -> u32| reports.iter().map(g).collect::<Vec<_>>();
        let all = |g: fn(&Report) -> Option<u32>| {
            reports
                .iter()
                .map(g)
                .try_fold(0, |worst, ms| ms.map(|ms| worst.max(ms)))
        };
        Summary {
            name: reports[0].name.clone(),
            limit: reports[0].limit,
            mean_target_median: percentile_f64(&f(|r| r.mean_target_fps), 0.5),
            p10_target_worst: percentile_u32(&u(|r| r.p10_target_fps), 0.0),
            below_half_p90: percentile_f64(&f(|r| r.below_half_pct), 0.9),
            queue_p95_p90: percentile_u32(&u(|r| r.queue_p95_ms), 0.9),
            delivered_median: percentile_f64(&f(|r| r.delivered_fps), 0.5),
            frame_age_p95_p90: percentile_u32(&u(|r| r.frame_age_p95_ms), 0.9),
            has_restore: reports[0].has_restore,
            recovery_worst_ms: all(|r| r.recovery_ms),
            cold_start_min_median: percentile_u32(&u(|r| r.cold_start_min_fps), 0.5),
            time_to_90pct_worst_ms: all(|r| r.time_to_90pct_ms),
        }
    }

    pub const HEADER: &'static str = "| scenario | limit | target fps (median of means) | worst p10 | below limit/2 (p90) | queue p95 (p90) | delivered fps (median) | frame age p95 (p90) | sustained recovery (worst) | cold-start min (median) | time to 90% (worst) |\n|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|";

    pub fn row(&self) -> String {
        let secs = |ms: Option<u32>| {
            ms.map(|ms| format!("{:.1}s", ms as f64 / 1000.0))
                .unwrap_or_else(|| "never".to_owned())
        };
        format!(
            "| {} | {} | {:.1} | {} | {:.1}% | {} ms | {:.1} | {} ms | {} | {} | {} |",
            self.name,
            self.limit,
            self.mean_target_median,
            self.p10_target_worst,
            self.below_half_p90,
            self.queue_p95_p90,
            self.delivered_median,
            self.frame_age_p95_p90,
            if self.has_restore {
                secs(self.recovery_worst_ms)
            } else {
                "-".to_owned()
            },
            self.cold_start_min_median,
            secs(self.time_to_90pct_worst_ms),
        )
    }
}

fn clean_link(capacity_kbps: f64) -> Link {
    Link {
        capacity_kbps: vec![(0, capacity_kbps)],
        wobble: 0.05,
        base_rtt_ms: 15.0,
        jitter_median_ms: 3.0,
        jitter_sigma: 0.5,
        loss_per_s: 0.0,
        stall_mean_interval_s: 0.0,
        stall_ms: (0.0, 0.0),
    }
}

/// 平均容量充足但信号弱的家用 Wi-Fi：重尾抖动、
/// 重传，以及大约每二十秒一次最长 2.5 秒的链路停顿。
/// 故意做得很糟糕；它专门隔离出「容量没问题、时序有问题」这一情形。
fn home_wifi_link() -> Link {
    Link {
        capacity_kbps: vec![(0, 20_000.0)],
        wobble: 0.5,
        base_rtt_ms: 8.0,
        jitter_median_ms: 15.0,
        jitter_sigma: 1.0,
        loss_per_s: 0.2,
        stall_mean_interval_s: 20.0,
        stall_ms: (300.0, 2500.0),
    }
}

fn intercontinental_link() -> Link {
    Link {
        capacity_kbps: vec![(0, 20_000.0)],
        wobble: 0.1,
        base_rtt_ms: 250.0,
        jitter_median_ms: 5.0,
        jitter_sigma: 0.5,
        loss_per_s: 0.05,
        stall_mean_interval_s: 0.0,
        stall_ms: (0.0, 0.0),
    }
}

/// 一分钟 8 Mbps，接着一分钟 2.5 Mbps，然后再回到 8 Mbps。
fn halved_link() -> Link {
    Link {
        capacity_kbps: vec![(0, 8_000.0), (60_000, 2_500.0), (120_000, 8_000.0)],
        ..clean_link(8_000.0)
    }
}

fn mobile_link() -> Link {
    Link {
        capacity_kbps: vec![(0, 6_000.0)],
        wobble: 0.4,
        base_rtt_ms: 40.0,
        jitter_median_ms: 30.0,
        jitter_sigma: 0.8,
        loss_per_s: 0.02,
        stall_mean_interval_s: 0.0,
        stall_ms: (0.0, 0.0),
    }
}

pub fn scenarios() -> Vec<Scenario> {
    let base = |name, limit, link, abr, encoder| Scenario {
        name,
        seconds: 180,
        limit,
        quality: Quality::Balanced,
        abr,
        content: Content::Video,
        encoder,
        link,
        seed: 1,
    };
    use EncoderModel::*;
    vec![
        base("home_wifi_30", 30, home_wifi_link(), true, Cbr),
        base("home_wifi_60", 60, home_wifi_link(), true, Cbr),
        base(
            "home_wifi_fixed_rate_30",
            30,
            home_wifi_link(),
            true,
            FixedRate,
        ),
        base("home_wifi_no_abr_30", 30, home_wifi_link(), false, Cbr),
        Scenario {
            content: Content::Office,
            ..base("office_home_wifi_30", 30, home_wifi_link(), true, Cbr)
        },
        base("city_relay_30", 30, clean_link(50_000.0), true, Cbr),
        base("city_relay_60", 60, clean_link(50_000.0), true, Cbr),
        base(
            "intercontinental_30",
            30,
            intercontinental_link(),
            true,
            Cbr,
        ),
        base("bandwidth_halved_30", 30, halved_link(), true, Cbr),
        base(
            "bandwidth_halved_fixed_rate_30",
            30,
            halved_link(),
            true,
            FixedRate,
        ),
        base(
            "bandwidth_halved_fixed_rate_no_abr_30",
            30,
            halved_link(),
            false,
            FixedRate,
        ),
        base("bandwidth_halved_no_abr_30", 30, halved_link(), false, Cbr),
        base("mobile_bufferbloat_30", 30, mobile_link(), true, Cbr),
    ]
}

/// 在 `SEEDS` 上运行每个场景，并返回各场景的汇总。
pub fn run_all() -> Vec<(Summary, Vec<Report>)> {
    scenarios()
        .iter()
        .map(|sc| {
            let reports: Vec<Report> = SEEDS
                .map(|seed| run(&Scenario { seed, ..sc.clone() }))
                .collect();
            (Summary::of(&reports), reports)
        })
        .collect()
}

fn write_traces(results: &[(Summary, Vec<Report>)]) {
    use std::fmt::Write;
    if let Ok(path) = std::env::var("RUSTDESK_QOS_SIM_CSV") {
        let mut csv = String::from("scenario,seed,time_ms,target_fps,queue_ms,ratio\n");
        for (_, reports) in results {
            for report in reports {
                for (t, fps, queue, ratio) in &report.trace {
                    writeln!(
                        csv,
                        "{},{},{t},{fps},{queue},{ratio:.3}",
                        report.name, report.seed
                    )
                    .unwrap();
                }
            }
        }
        std::fs::write(path, csv).unwrap();
    }
}

#[test]
fn sim_scenarios() {
    let results = run_all();
    println!("{}", Summary::HEADER);
    for (summary, _) in &results {
        println!("{}", summary.row());
    }
    if std::env::var("RUSTDESK_QOS_SIM_VERBOSE").is_ok() {
        for (_, reports) in &results {
            for r in reports {
                println!(
                    "{} seed {}: target mean {:.1} p10 {} min {} below-half {:.1}% delivered {:.1} age p95 {} queue p95 {} max probe {} recovery {:?} cold-start min {} t90 {:?}",
                    r.name, r.seed, r.mean_target_fps, r.p10_target_fps, r.min_target_fps,
                    r.below_half_pct, r.delivered_fps, r.frame_age_p95_ms, r.queue_p95_ms,
                    r.max_delay_ms, r.recovery_ms, r.cold_start_min_fps, r.time_to_90pct_ms
                );
            }
        }
    }
    write_traces(&results);
    for (summary, _) in &results {
        let violations = bound_violations(summary);
        assert!(
            violations.is_empty(),
            "{}: {violations:?}\n{summary:?}",
            summary.name
        );
    }
}

/// 在 `SEEDS` 上的 CI 运行中，每个场景汇总都必须满足的边界。
/// 它们表述的是产品所需，
/// 而不是某一个种子产生的结果。 如果新的种子或新的场景违反了某条边界，
/// 就修改设计或以书面理由放宽该边界；绝不要为了让边界通过而
/// 调整某个控制器常量。
pub fn bound_violations(s: &Summary) -> Vec<&'static str> {
    let name = s.name.as_str();
    let limit = s.limit as f64;
    let mut v = Vec::new();
    let mut check = |ok: bool, what: &'static str| {
        if !ok {
            v.push(what);
        }
    };
    if name.starts_with("home_wifi") || name.starts_with("office_home_wifi") {
        // 抖动但健康的链路必须保持快速：这正是本次改动的全部意义。
        // 目标很少偏离上限，从不崩塌，而最长
        // 2.5 秒的停顿最坏也只留下大约一秒的队列。
        check(
            s.mean_target_median >= 0.85 * limit,
            "median target below 85%",
        );
        check(s.p10_target_worst * 3 >= s.limit, "worst p10 below a third");
        check(s.below_half_p90 <= 10.0, "below half the limit over 10%");
        check(s.queue_p95_p90 < 1000, "queue p95 p90 over 1 s");
        if name != "office_home_wifi_30" {
            check(s.delivered_median >= 0.8 * limit, "delivered below 80%");
        }
        // 帧龄是已交付帧在共享路径中停留的时间：也就是
        // 观看者在往返之外还要等待的时间。 一条抖动的高容量链路
        // 包含最长 2.5 秒的孤立停顿，而边界针对的是
        // 每个种子 p95 帧龄的 p90：孤立停顿是可以容忍的，但它们不得
        // 演变成持续数秒的积压。 这是回归边界，而不是
        // 延迟目标；它由场景设定，而不是由某次运行得出。
        check(s.frame_age_p95_p90 < 1500, "frame age p95 p90 over 1.5 s");
    } else if name.starts_with("city_relay") {
        // 干净的链路是开发者测试的环境；每个种子都处在上限上，
        // 而一条新建连接在十秒内达到该上限的 90%。
        check(s.p10_target_worst == s.limit, "left the limit");
        check(s.queue_p95_p90 < 50, "queue on a clean link");
        check(s.frame_age_p95_p90 < 100, "frame age on a clean link");
        check(
            s.time_to_90pct_worst_ms.is_some_and(|ms| ms <= 10_000),
            "cold start over 10 s",
        );
    } else if name == "intercontinental_30" {
        // RTT 虽高但稳定并不是拥塞，即便在冷启动期间也不是。
        check(
            s.mean_target_median >= 0.9 * limit,
            "median target below 90%",
        );
        check(
            s.cold_start_min_median >= INIT_FPS,
            "cold start below INIT_FPS",
        );
        // 帧龄不含往返时间，因此高 RTT 得不到任何宽限。
        check(s.frame_age_p95_p90 < 150, "frame age over 150 ms");
        check(
            s.time_to_90pct_worst_ms.is_some_and(|ms| ms <= 10_000),
            "cold start over 10 s",
        );
    } else if let Some((queue_p95_bound_ms, below_half_bound_pct)) = match name {
        // 真实的拥塞必须被检测、被排空并从中恢复。 在 CBR
        // 编码器下只有码率能排空队列，而每秒节奏下需要三次探测回复
        // 再加上三秒的比例冷却期，才能进行一次
        // 确认后的降档，因此那里必然会有几秒的队列。 如果没有
        // ABR，就完全没有什么能排空 CBR 的队列，因此这种组合会被上报
        // 但不做断言。
        "bandwidth_halved_30" => Some((3000, 40.0)),
        "bandwidth_halved_fixed_rate_30" => Some((2000, 10.0)),
        "bandwidth_halved_fixed_rate_no_abr_30" => Some((3000, 30.0)),
        _ => None,
    } {
        check(s.queue_p95_p90 < queue_p95_bound_ms, "queue p95 p90 bound");
        check(
            s.frame_age_p95_p90 < queue_p95_bound_ms,
            "frame age p95 p90 bound",
        );
        check(s.below_half_p90 <= below_half_bound_pct, "below half bound");
        check(
            s.recovery_worst_ms.is_some_and(|ms| ms <= 20_000),
            "sustained recovery over 20 s",
        );
    } else if name == "mobile_bufferbloat_30" {
        check(s.queue_p95_p90 < 2000, "queue p95 p90 over 2 s");
        check(s.frame_age_p95_p90 < 2000, "frame age p95 p90 over 2 s");
        check(s.below_half_p90 <= 15.0, "below half the limit over 15%");
    }
    v
}

/// 把 `qos_trace` 的各行回放给一个全新的控制器，并返回
/// 每行的 `(time_ms, id, recorded_fps, replayed_fps)`。 开环，仅涉及 FPS：
/// 录制的延迟不会对回放出的决策作出反应，会话以
/// 关闭 ABR 的方式运行，各连接被回放为同一个 30 fps
/// 均衡会话的独立观看者。 时间按相邻两行之间的挂钟差值推进，
/// 无论它们属于哪个连接；当轨迹
/// 早于 `t=` 字段时，则每行推进一秒。
pub fn replay(text: &str) -> Vec<(u64, i32, u64, u32)> {
    // 存在但格式错误的值意味着轨迹损坏，而不是字段缺失。
    let field = |line: &str, key: &str| -> Option<u64> {
        line.split_whitespace()
            .find_map(|kv| kv.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
            .map(|v| {
                v.parse()
                    .unwrap_or_else(|e| panic!("bad {key}={v:?} in {line:?}: {e}"))
            })
    };
    let mut qos = super::smoke::session(30, Quality::Balanced);
    qos.users.clear();
    let mut last_t: Option<u64> = None;
    let mut now = 0_u64;
    let mut trace = Vec::new();
    for line in text.lines().filter(|l| l.contains("qos_trace")) {
        let id = field(line, "id").unwrap_or(1) as i32;
        qos.users.entry(id).or_default();
        let t = field(line, "t");
        let step = match (t, last_t) {
            (Some(t), Some(prev)) => t.saturating_sub(prev).clamp(1, 10_000),
            _ => 1000,
        };
        if t.is_some() {
            last_t = t;
        }
        now += step;
        qos.advance_ms(step);
        if let Some(elapsed) = field(line, "timeout") {
            qos.user_delay_response_elapsed(id, elapsed as u128);
        } else if let Some(delay) = field(line, "delay") {
            qos.user_delay_response_elapsed(id, 0);
            qos.user_network_delay(id, delay as u32);
        }
        let recorded = field(line, "fps").unwrap_or(0);
        trace.push((now, id, recorded, qos.fps()));
    }
    trace
}

/// 回放 `RUSTDESK_QOS_TRACE` 指定的日志并打印结果。
#[test]
fn replay_recorded_trace() {
    let Ok(path) = std::env::var("RUSTDESK_QOS_TRACE") else {
        return;
    };
    let trace = replay(&std::fs::read_to_string(&path).unwrap());
    println!("time_ms,id,recorded_fps,replayed_fps");
    for (t, id, recorded, replayed) in &trace {
        println!("{t},{id},{recorded},{replayed}");
    }
    let mean = trace.iter().map(|t| t.3 as f64).sum::<f64>() / trace.len().max(1) as f64;
    println!(
        "replayed mean target fps: {mean:.1} over {} lines",
        trace.len()
    );
}
