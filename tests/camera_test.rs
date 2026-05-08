//! 相机相关回归测试。
//!
//! 这组测试的目标不是覆盖所有数学细节，
//! 而是锁住用户最容易直接感知到的几个行为：
//! - 滚轮缩放后倍率确实变化
//! - 平移辅助逻辑正确
//! - fit to window 会给出合理缩放
//! - 放大后还能缩回 fit 比例

use approx::assert_relative_eq;
use flayout_wgpu::{
    app::{
        apply_pan_delta, should_degrade_interaction_render, should_request_continuous_redraw,
        should_request_redraw_after_window_event,
    },
    camera::Camera2D,
};
use glam::Vec2;

/// 最基础的缩放行为：滚轮放大后 zoom 应该变大。
#[test]
fn zoom_increases_scale() {
    let mut camera = Camera2D::new();
    let before = camera.zoom();
    camera.zoom_by(1.25, Vec2::new(100.0, 50.0));
    assert!(camera.zoom() > before);
}

/// `app` 里对 pan delta 的叠加应当是线性的。
#[test]
fn app_pan_helper_moves_camera() {
    let next = apply_pan_delta(Vec2::new(5.0, 6.0), Vec2::new(-2.0, 3.0));
    assert_eq!(next, Vec2::new(3.0, 9.0));
}

#[test]
fn continuous_redraw_only_runs_when_ui_or_progressive_work_needs_it() {
    assert!(should_request_continuous_redraw(3, false));
    assert!(should_request_continuous_redraw(0, true));
    assert!(!should_request_continuous_redraw(0, false));
}

#[test]
fn input_events_wake_redraw_even_without_continuous_mode() {
    assert!(should_request_redraw_after_window_event(
        false,
        &winit::event::WindowEvent::MouseWheel {
            device_id: unsafe { std::mem::zeroed() },
            delta: winit::event::MouseScrollDelta::LineDelta(0.0, 1.0),
            phase: winit::event::TouchPhase::Moved,
        },
    ));
    assert!(should_request_redraw_after_window_event(
        true,
        &winit::event::WindowEvent::Focused(true),
    ));
    assert!(!should_request_redraw_after_window_event(
        false,
        &winit::event::WindowEvent::Focused(true),
    ));
}

#[test]
fn recent_camera_interaction_temporarily_degrades_closed_shape_rendering() {
    assert!(should_degrade_interaction_render(
        Some(std::time::Duration::from_millis(60)),
        std::time::Duration::from_millis(120),
    ));
    assert!(!should_degrade_interaction_render(
        Some(std::time::Duration::from_millis(180)),
        std::time::Duration::from_millis(120),
    ));
    assert!(!should_degrade_interaction_render(
        None,
        std::time::Duration::from_millis(120),
    ));
}

#[test]
fn interaction_dirty_view_requires_follow_up_redraw_after_freeze() {
    let interaction_degraded = should_degrade_interaction_render(
        Some(std::time::Duration::from_millis(60)),
        std::time::Duration::from_millis(120),
    );
    assert!(interaction_degraded);
    let interaction_expired = should_degrade_interaction_render(
        Some(std::time::Duration::from_millis(180)),
        std::time::Duration::from_millis(120),
    );
    assert!(!interaction_expired);
}

/// fit 到窗口后，缩放倍率必须是正数，且这里顺便锁住当前计算结果。
#[test]
fn fit_bounds_selects_positive_zoom() {
    let mut camera = Camera2D::new();
    camera.fit_bounds(
        flayout_wgpu::scene::Bounds::new(0.0, 0.0, 100.0, 50.0),
        Vec2::new(800.0, 600.0),
    );
    assert_relative_eq!(camera.zoom(), 6.4, epsilon = 0.001);
}

/// 这是一个很重要的回归测试：
/// 之前最小缩放限制过大时，用户放大后再缩小，无法回到 fit 视图。
#[test]
fn zoom_can_return_to_fit_scale_after_zooming_in() {
    let mut camera = Camera2D::new();
    camera.fit_bounds(
        flayout_wgpu::scene::Bounds::new(0.0, -2225.0, 95730.0, 93505.0),
        Vec2::new(1668.0, 1243.0),
    );
    let fit_zoom = camera.zoom();

    camera.zoom_by(8.0, Vec2::new(800.0, 600.0));
    camera.zoom_by(0.125, Vec2::new(800.0, 600.0));

    assert_relative_eq!(camera.zoom(), fit_zoom, epsilon = 0.0001);
}
