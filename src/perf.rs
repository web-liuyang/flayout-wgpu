//! 简单的帧性能统计。
//!
//! 这里没有引入复杂 profiler，而是先维护一个滑动窗口：
//! - 最近若干帧的耗时
//! - 当前帧耗时
//! - 由平均帧耗时换算出来的 FPS
//!
//! 同时我们也会维护一组很轻量的渲染调试历史：
//! - 顶点数
//! - tile miss 数
//! - cache bytes
//!
//! 这样做的好处是：
//! - 足够轻量，适合一直开着
//! - 可以让 UI 直接显示“当前大概的运行状态”
//! - 在做缓存、裁剪、tile 优化时，能马上看到趋势变化

use std::{collections::VecDeque, time::Duration};

use crate::renderer::RenderDebugStats;

const DEFAULT_HISTORY_CAPACITY: usize = 120;

/// 帧时间统计器。
#[derive(Debug, Clone)]
pub struct FrameStats {
    /// 最近一段时间内的帧耗时（毫秒）。
    ///
    /// 这里用 `VecDeque`，因为我们会不停地“尾部追加、头部丢弃”。
    frame_times: VecDeque<f32>,

    /// 最新一帧的耗时，单独存下来是为了 UI 直接显示，不必每次都去取队尾。
    frame_time_ms: f32,
}

/// 一个固定窗口的数值历史。
///
/// 这层故意保持很简单：
/// - 只关心追加和裁剪
/// - 不做复杂统计
/// - 让 UI 可以随时拿到一条 sparkline 所需的数据
#[derive(Debug, Clone)]
pub struct MetricHistory {
    /// 固定窗口里的样本队列。
    samples: VecDeque<f32>,
    /// 窗口允许保留的最大样本数。
    capacity: usize,
}

/// 渲染调试历史。
///
/// 我们不把整份 `RenderDebugStats` 原样存满整个窗口，
/// 而是只挑当前最值得长期观察的几项：
/// - `vertex_count`：看几何量有没有下降
/// - `tile_cache_misses`：看缓存是否越来越稳定
/// - `cache_bytes`：看缓存体积有没有膨胀
/// - `pending_entries`：看渐进式补全是否还在追赶当前视图
#[derive(Debug, Clone)]
pub struct RenderStatsHistory {
    /// 当前帧真正准备提交给 GPU 的顶点量。
    ///
    /// 它不等于 shape 数，也不等于 draw call 数；
    /// 更适合用来观察 LOD / hatch / clipping / 合批对几何量的影响。
    vertices: MetricHistory,
    /// 当前帧有多少 tile 在请求时没有命中 GPU cache。
    ///
    /// 这个值越低，说明当前视图越稳定，或者 cache 策略越有效。
    tile_misses: MetricHistory,
    /// tile cache 当前粗略占用的 GPU/几何缓存体积。
    cache_bytes: MetricHistory,
    /// 当前帧结束后还有多少 `tile + layer` 任务在排队。
    ///
    /// 对“为什么感觉还在补图”这类问题，这个值通常比 FPS 更有解释力。
    pending_entries: MetricHistory,
}

impl FrameStats {
    /// 创建一个空统计器。
    pub fn new() -> Self {
        Self {
            // 120 帧大约对应 2 秒（按 60 FPS 估算），
            // 这个窗口长度能在“平滑”和“及时反馈”之间取一个比较舒服的平衡。
            frame_times: VecDeque::with_capacity(DEFAULT_HISTORY_CAPACITY),
            frame_time_ms: 0.0,
        }
    }

    /// 记录新的一帧耗时。
    pub fn record_frame(&mut self, delta: Duration) {
        let ms = delta.as_secs_f32() * 1000.0;
        self.frame_time_ms = ms;

        // 固定窗口大小，避免统计序列无限增长。
        if self.frame_times.len() == DEFAULT_HISTORY_CAPACITY {
            self.frame_times.pop_front();
        }
        self.frame_times.push_back(ms);
    }

    /// 根据滑动窗口的平均帧时间估算 FPS。
    pub fn fps(&self) -> f32 {
        if self.frame_times.is_empty() {
            return 0.0;
        }

        let avg_ms = self.frame_times.iter().sum::<f32>() / self.frame_times.len() as f32;
        if avg_ms <= f32::EPSILON {
            0.0
        } else {
            1000.0 / avg_ms
        }
    }

    /// 直接返回最近一帧耗时，便于 UI 展示瞬时状态。
    pub fn frame_time_ms(&self) -> f32 {
        self.frame_time_ms
    }
}

impl MetricHistory {
    /// 创建固定长度的历史窗口。
    pub fn new(capacity: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// 追加一个新样本，并自动维持固定窗口大小。
    pub fn push(&mut self, value: f32) {
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(value.max(0.0));
    }

    /// 返回当前窗口里的样本，供 UI 直接绘制 sparkline。
    pub fn samples(&self) -> Vec<f32> {
        self.samples.iter().copied().collect()
    }

    /// 当前窗口中的最大值。
    pub fn max_value(&self) -> f32 {
        self.samples.iter().copied().fold(0.0, f32::max)
    }

    /// 当前窗口里的最新值。
    pub fn latest(&self) -> f32 {
        self.samples.back().copied().unwrap_or(0.0)
    }
}

impl RenderStatsHistory {
    /// 创建一组空的渲染调试历史。
    pub fn new() -> Self {
        Self {
            vertices: MetricHistory::new(DEFAULT_HISTORY_CAPACITY),
            tile_misses: MetricHistory::new(DEFAULT_HISTORY_CAPACITY),
            cache_bytes: MetricHistory::new(DEFAULT_HISTORY_CAPACITY),
            pending_entries: MetricHistory::new(DEFAULT_HISTORY_CAPACITY),
        }
    }

    /// 把一帧 renderer 调试统计压进历史窗口。
    pub fn record(&mut self, stats: &RenderDebugStats) {
        self.vertices.push(stats.vertex_count as f32);
        self.tile_misses.push(stats.tile_cache_misses as f32);
        self.cache_bytes.push(stats.cache_bytes as f32);
        self.pending_entries.push(stats.pending_entries as f32);
    }

    /// 顶点量历史。
    pub fn vertices(&self) -> &MetricHistory {
        &self.vertices
    }

    /// tile cache miss 历史。
    pub fn tile_misses(&self) -> &MetricHistory {
        &self.tile_misses
    }

    /// cache 占用字节历史。
    pub fn cache_bytes(&self) -> &MetricHistory {
        &self.cache_bytes
    }

    /// pending tile build 条目历史。
    pub fn pending_entries(&self) -> &MetricHistory {
        &self.pending_entries
    }
}

impl Default for FrameStats {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for RenderStatsHistory {
    fn default() -> Self {
        Self::new()
    }
}
