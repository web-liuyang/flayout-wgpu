//! 2D 相机模块。
//!
//! 在这个项目里，相机只做三件事：
//! - 平移（pan）
//! - 缩放（zoom）
//! - 根据场景范围自动 fit 到窗口
//!
//! 我们故意把相机做得很简单，因为对版图查看器来说，
//! “世界坐标 -> 屏幕坐标”的稳定和可控，比花哨的相机能力更重要。

use glam::Vec2;

use crate::scene::Bounds;

/// 最小缩放倍率。
///
/// 这个值不能太大，否则大版图在 fit 后缩不回去；
/// 也不能小到接近 0，否则容易引入数值不稳定。
const MIN_ZOOM: f32 = 0.000_001;

/// 最大缩放倍率。
///
/// 主要作用是防止用户滚轮放大到极端倍率，导致坐标和线宽计算失真。
const MAX_ZOOM: f32 = 10_000.0;

/// 一个非常轻量的 2D 相机。
#[derive(Debug, Clone)]
pub struct Camera2D {
    /// 屏幕空间平移量。
    ///
    /// 这里存的是“已经缩放后”的屏幕偏移，而不是世界坐标中心点。
    /// 这样可以让拖拽平移逻辑更直观：鼠标拖多少，画面就平移多少。
    pan: Vec2,

    /// 当前缩放倍率。
    zoom: f32,
}

impl Camera2D {
    /// 创建默认相机。
    pub fn new() -> Self {
        Self {
            pan: Vec2::ZERO,
            zoom: 1.0,
        }
    }

    /// 当前平移量。
    pub fn pan(&self) -> Vec2 {
        self.pan
    }

    /// 当前缩放倍率。
    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    /// 直接设置缩放倍率。
    ///
    /// 一般只在测试或特殊控制下使用；日常交互更常用 `zoom_by`。
    pub fn set_zoom(&mut self, zoom: f32) {
        self.zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
    }

    /// 直接恢复一组持久化后的相机状态。
    ///
    /// 这里单独提供一个入口，而不是让外层分别写 `set_zoom + translate`，
    /// 是为了让“恢复配置”这种场景更直接，也避免调用方误把 pan 当成增量。
    pub fn set_state(&mut self, pan: Vec2, zoom: f32) {
        self.pan = pan;
        self.zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
    }

    /// 根据屏幕位移做平移。
    ///
    /// 这里用的是“屏幕坐标系的 delta”，
    /// 所以鼠标拖动和画布跟手会更自然，不需要额外乘除 zoom。
    pub fn translate_screen(&mut self, delta: Vec2) {
        self.pan += delta;
    }

    /// 以某个光标位置为中心进行缩放。
    ///
    /// 这是查看器里最常见的“围绕鼠标滚轮缩放”逻辑：
    /// 缩放后，鼠标指向的内容尽量还停留在鼠标附近，而不是整张图乱跳。
    pub fn zoom_by(&mut self, factor: f32, cursor: Vec2) {
        let previous_zoom = self.zoom;
        let next_zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        if (next_zoom - previous_zoom).abs() < f32::EPSILON {
            return;
        }

        let ratio = next_zoom / previous_zoom;

        // 推导思路：
        // 先把屏幕中的缩放中心固定在 cursor，
        // 再让 pan 相对这个中心按缩放比例拉伸/收缩。
        self.pan = cursor + (self.pan - cursor) * ratio;
        self.zoom = next_zoom;
    }

    /// 让给定 bounds 自动适配当前 viewport。
    ///
    /// 这里保留了 20% 边距（0.8 系数），目的是让版图不要紧贴边缘，
    /// 读起来更舒服，也给后续框选/hover 留出一点视觉缓冲空间。
    pub fn fit_bounds(&mut self, bounds: Bounds, viewport_size: Vec2) {
        let safe_width = bounds.width().max(1.0);
        let safe_height = bounds.height().max(1.0);
        let scale_x = viewport_size.x.max(1.0) * 0.8 / safe_width;
        let scale_y = viewport_size.y.max(1.0) * 0.8 / safe_height;
        self.zoom = scale_x.min(scale_y).clamp(MIN_ZOOM, MAX_ZOOM);

        let center_world = bounds.center();
        let viewport_center = viewport_size * 0.5;

        // 让“世界中心点”经过缩放后落在“视口中心”。
        self.pan = viewport_center - center_world * self.zoom;
    }
}

impl Default for Camera2D {
    fn default() -> Self {
        Self::new()
    }
}
