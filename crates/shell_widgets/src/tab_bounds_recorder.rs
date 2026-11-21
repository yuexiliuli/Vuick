//! 选项卡边框记录器（L0 纯 UI 组件）。
//!
//! 包在一个子元素外，每帧把子元素 laid-out 的 border-box **边界**写入调用方持有的
//! `Rc<Cell<..>>`：`size` 供拖拽预览 / 窗外幽灵窗按真实选项卡尺寸定宽定高；`bounds`
//! 供栏内拖拽排序的命中测试，与拖拽事件 `position` 同一输入坐标空间。wrapper 与 child
//! **共享同一 LayoutId**（`request_layout` 直接转发 child 的），故 `prepaint` 收到的
//! `bounds` 即 child 的真实布局边界；host `debug_selector` 仍在 child 上、不受影响。

use std::cell::Cell;
use std::rc::Rc;

use gpui::{px, AnyElement, Bounds, IntoElement, Pixels, Size};

/// 记录子元素（选项卡）每一帧 laid-out 的 border-box **边界**：`size` 供拖拽预览
/// （TabDragGhost / 窗外幽灵窗）按真实选项卡尺寸定宽定高；`bounds` 供栏内拖拽排序
/// 的命中测试（与拖拽事件 `position` 同一输入坐标空间）。wrapper 与 child **共享同一
/// LayoutId**（`request_layout` 直接转发 child 的），故 `prepaint` 收到的 `bounds` 即
/// child 的真实布局边界；host `debug_selector` 仍在 child 上、不受影响。
pub struct TabBoundsRecorder {
    /// 真实 laid-out 尺寸槽位（拖拽预览 / 幽灵窗定宽高）。
    pub size: Rc<Cell<Size<Pixels>>>,
    /// 真实 laid-out 边界槽位（栏内拖拽排序命中测试）。
    pub bounds: Rc<Cell<Bounds<Pixels>>>,
    /// 被包裹的子元素（child 持有自身 debug_selector，不受影响）。
    pub child: AnyElement,
}

impl IntoElement for TabBoundsRecorder {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl gpui::Element for TabBoundsRecorder {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut gpui::Window,
        cx: &mut gpui::App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: gpui::Bounds<gpui::Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut gpui::Window,
        cx: &mut gpui::App,
    ) -> Self::PrepaintState {
        if bounds.size.width > px(0.) && bounds.size.height > px(0.) {
            self.size.set(bounds.size);
            self.bounds.set(bounds);
        }
        self.child.prepaint(window, cx);
        
    }

    fn paint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: gpui::Bounds<gpui::Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut gpui::Window,
        cx: &mut gpui::App,
    ) {
        self.child.paint(window, cx);
    }
}
