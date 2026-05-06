//! 帧统计与轻量调试历史测试。
//!
//! 这一层很小，但它直接影响 UI 里显示的 FPS / Frame ms / 趋势图，
//! 所以至少要保证：
//! - 记录几帧之后，统计值不会一直是 0
//! - 历史窗口会正确保留最新样本

use std::time::Duration;

use flayout_wgpu::{
    perf::{FrameStats, RenderStatsHistory},
    renderer::RenderDebugStats,
};

/// 记录多帧后，FPS 和当前帧耗时都应该是正数。
#[test]
fn frame_stats_reports_positive_fps_after_ticks() {
    let mut stats = FrameStats::new();
    stats.record_frame(Duration::from_millis(16));
    stats.record_frame(Duration::from_millis(16));

    assert!(stats.fps() > 0.0);
    assert!(stats.frame_time_ms() > 0.0);
}

/// 渲染调试历史应能正确记录最新窗口值。
#[test]
fn render_stats_history_records_latest_renderer_samples() {
    let mut history = RenderStatsHistory::new();
    history.record(&RenderDebugStats::new(
        10, 4, 3, 2, 120, 4, 5, 1, 4, 6, 2, 7, 64, 2_048, 0, 2, 5, 9, 12, 16, 3, None, 0, 0, None, false, 8, 128, true, 10.0, 1.5,
    ));
    history.record(&RenderDebugStats::new(
        10, 4, 3, 2, 80, 3, 4, 2, 1, 5, 1, 7, 64, 1_024, 0, 2, 5, 9, 4, 16, 5, None, 0, 0, None, false, 8, 128, true, 10.0, 1.5,
    ));

    assert_eq!(history.vertices().latest(), 80.0);
    assert_eq!(history.vertices().max_value(), 120.0);
    assert_eq!(history.tile_misses().latest(), 1.0);
    assert_eq!(history.cache_bytes().latest(), 1_024.0);
    assert_eq!(history.pending_entries().latest(), 4.0);
    assert_eq!(history.vertices().samples().len(), 2);
}
