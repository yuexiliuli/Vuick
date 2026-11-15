//! 新管线 WYSIWYG 自绘元素的**宿主侧**：`WyEditorElement`（gpui `Element` 实现）与
//! 元素级 `paint`/`hit` 胶水（`WyPrepaint`、`paint_band_rows`、`link_hit_grips`、
//! `frame_stale_reason`）、`mod paint`/`mod hit`。数据的**纯类型核心**已拆到
//! `markdown_wysiwyg_engine` crate——本模块经下方重导出别名把引擎的纯类型/几何/常量
//! 原样搬进 `element::` 命名空间，宿主全部既有路径（`element::Frame`/`element::geom::*`/
//! `element::ShapedRow` 等）不变。
//!
//! 数据完全来自 `Doc`（`rows_in_block_window` + `byte_map`），零 zorite 依赖。
//!
//! **窗口化 shaping**：元素是 auto-grow（宿主滚动），整篇 shape 会在大文档上每帧
//! 卡死。这里复刻 `live` 的带机制——每帧只真正 shape **可视带**（`prepaint` 由
//! `bounds.origin.y` + `viewport_size` 推算并发布），带外块用 `BlockMeta` 的
//! `raw_len`/`aux` 估算高度占位（不 shape、不物化行），帧缓存按
//! `(content_gen, width, band)` 命中后整帧跳过重建。

use gpui::{
    App, Bounds, CursorStyle, DispatchPhase, Element, ElementId,
    ElementInputHandler, Entity, GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId,
    IntoElement, LayoutId, MouseMoveEvent, MouseUpEvent, Pixels, ScrollWheelEvent,
    Window, point, px, relative, size,
};

/// 视为「真实预览面板」的最小 definite 宽度（px）：更窄的测量趟在**内容未变**时一律按
/// gpui 过渡探针处理（见 `request_layout` 里 `probe` 的 P4-3 注）——不建帧、不覆写帧
/// 缓存。实测探针宽：~滚动条宽 14.5px、flex 求解 68-75px；真实面板 ≥ 分割最小比例
/// 0.2×窗宽（≥800px 窗 ≈ ≥112px），故 100px 不会误伤真实面板；内容代变化的窄宽仍建帧
/// （极小面板不退化）。
const WY_PROBE_MIN_WIDTH_PX: f32 = 100.0;

pub use super::frame::build_frame;
pub(crate) use super::frame::{prewarm_band, warm_emoji_rasterizer};
// 数学渲染子系统（独立 crate `markdown_wysiwyg_math`）：整块 glob 重导出，宿主对
// `MathPos`/`math_to_segments`/`display_to_source`/`inline_math_fit_h` 等的既有路径不变。
#[allow(unused_imports)]
pub(crate) use markdown_wysiwyg_math::*;
// shape 整块 glob：`super::shape::*` 的宿主旧路径（`element::shape_docrow`/`math_card_height`
// 等）原样成立；lib 构建不再消费的全部由 paint/hit（已迁引擎）代接，名面允许压。
#[allow(unused_imports)]
pub(crate) use super::shape::*;
#[allow(unused_imports)]
pub(crate) use markdown_wysiwyg_mermaid::*;
use super::{CaretVis, WyEditor, WyTheme};
use crate::markdown::ir::IrKind;
// 仅测试模块经 `use super::LinkHit` 消费（随 `RowLink` 语义，命中测试构造用）。
#[cfg(test)]
use crate::markdown::parse::recognition::LinkHit;
// —— 纯类型核心自 `markdown_wysiwyg_engine` crate 重导出（git mv 后别名接管）——
// `element::Frame`/`BlockLayout`/`FoldPlan`/`ShapedRow` 等全部原路径照旧成立。
// 注意必须是 `pub use`（而非 pub(crate)）：宿主的 `pub use self::element::{…}` 跨 crate
// 展开面（`wysiwyg::Frame` 等）要求 `element` 内绑定本身的声明可见性是 pub。
// `geom` 子模块则**显式绑一次**再做 `use geom::*` 平铺——glob 透传只到两层：paint/hit 的
// `use super::*`（一层）← element 的 `use geom::*`（二层）← 引擎 `geom` 直接项；若只靠
// 上面的整块 glob（三层）则 Rust 不透传（E0425），故两层路径在此显式保留。
pub use markdown_wysiwyg_engine::element::*;
pub use markdown_wysiwyg_engine::element::geom;
pub(crate) use geom::*;

mod paint;
mod hit;
pub(crate) use paint::*;
pub use hit::NoteMarkerHit;
pub(crate) use hit::*;

/// 元素：auto-grow、宿主滚动。
pub(crate) struct WyEditorElement {
    pub editor: Entity<WyEditor>,
}

/// 元素一帧的 prepaint 产物：可点击链接的 hand 光标 hitbox（paint 期设光标样式）。
#[derive(Default)]
pub(crate) struct WyPrepaint {
    link_grips: Vec<Hitbox>,
    /// 图片调整条（右中边缘）的 hitbox：既触发 hit_test 变化（进/出该区即刷新光标），
    /// 也让 paint 用其 bounds 判定「指针落在调整区」→ 发窗口级 resize 光标。
    resize_grips: Vec<Hitbox>,
}

impl IntoElement for WyEditorElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for WyEditorElement {
    type RequestLayoutState = ();
    type PrepaintState = WyPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        base::util::source_location()
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        // 光标闪烁：首次渲染注册焦点事件 + 每帧按窗口焦点态对齐（聚焦启动/
        // 失焦退出 blink 循环；见 `WyEditor::sync_focus`）。
        self.editor.update(cx, |e, ecx| e.sync_focus(window, ecx));
        let editor = self.editor.clone();
        let mut style = gpui::Style::default();
        style.size.width = relative(1.).into();
        let id = window.request_measured_layout(style, move |known, available, window, cx| {
            let e = editor.read(cx);
            let width_definite = matches!(
                available.width,
                gpui::AvailableSpace::Definite(w)
                    if f32::from(w).is_finite() && f32::from(w) > 1.
            );
            let width = match available.width {
                gpui::AvailableSpace::Definite(w) => w,
                _ => known.width.unwrap_or(px(800.)),
            };
            let width = if f32::from(width).is_finite() && f32::from(width) > 1. {
                width
            } else {
                px(800.)
            };
            // 居中阅读栏：内容**布局宽**钳到 `min(72ch_px, 可用宽)`，整列（含 gutter）在
            // 面板内水平居中。帧按 `build_w` 折行/整形，返回的**元素尺寸仍取 `width`**
            // （= 面板可用宽）：元素占满面板，滚动/命中/IME 的窗口坐标语义不变；列居中
            // 由 paint/hit 侧 `WyEditor::reading_col_shift` 平移完成，内容区内
            // (`padding`/`x_off`/gutter) 坐标统一不变。72ch 由正文字号驱动缩放。
            let build_w = if config::markdown::READING_COL_ENABLED {
                let body_fs = f32::from(e.rd.borrow().theme.font_size);
                px(config::markdown::reading_col_px(body_fs).min(f32::from(width)))
            } else {
                width
            };
            // 非确定宽度（Indefinite / 探测性 / `Definite(0px)` 等占位）一律不建帧：
            // 复用现成帧总高估算，只让真正的正 definite 宽度趟建帧。否则 0px/探测趟会
            // 以 `known`/800 兜底建一帧，下一趟 definite 真实宽又重建——帧缓存永不命中
            // （右栏 md_wy 宿主每键 ×N 次 build_frame，debug ~100-500ms/键，即 A2 引入
            // 的卡顿根因；实测根治后每键降到单次重建）。旧版仅在 `known` 为空时才跳过，
            // known 非空（如上一趟 800 兜底）时 0px 趟仍会建占位帧。
            // P4-3：**过渡探针宽**也按探测对待——同一布局趟里 gpui 会以多个宽度反复测量
            // 子元素（0px / ~滚动条宽的 14.5px / 68px… / 最终固定宽 376-486px），只要
            // >1px 的 definite 探针就会被当成真宽建帧：`ensure_block_layout` 按
            // `(struct_ver, 宽度)` 换代 → 对该 15 万块大文档整篇 Pass1 重建（~5-9ms），
            // 并把结果存进唯一帧缓存槽，覆写掉真实宽度帧 → 下一趟真宽再整篇重建一次。
            // 每键因此 ×3 次 build_frame（实测单键 ~21ms，正是「预览一开就卡」的主导）。
            // 判据：非 definite 宽一律探测；definite 但**窄于真实面板最小宽**（分割最小
            // 比例 0.2×窗宽 ≈ ≥100px）的宽度只在与缓存内容代一致时按探针（不建帧、不覆写
            // 缓存、返回现成帧总高）——真实宽度的测量趟自然落到缓存命中或唯一一次建帧，
            // 内容真实变化（rgen 等换代）的窄宽仍会正常建帧（极小面板不退化）。
            let rgen = e.content_gen();
            let igen = e.images_gen();
            let mgen = e.math_gen();
            let zgen = e.mermaid_zoom_gen();
            let content_stale = {
                let rd = e.rd.borrow();
                let f = rd.frame.borrow();
                f.as_ref().is_none_or(|f| {
                    f.rgen != rgen || f.igen != igen || f.mgen != mgen || f.zgen != zgen
                })
            };
            let probe = !width_definite
                || (f32::from(width) < WY_PROBE_MIN_WIDTH_PX && !content_stale);
            // 帧缓存：内容/宽度/可视带/图片代数都没变 → 直接复用总高，不再整篇重建
            // （大文档的每帧“卡死”就是这里：每次 notify 都重排全 16 万行）。
            let band = e.shape_band().unwrap_or((0., 2048.));
            let cached_total = e
                .rd
                .borrow()
                .frame
                .borrow()
                .as_ref()
                .filter(|f| {
                    f.rgen == rgen
                        && f.igen == igen
                        && f.mgen == mgen
                        && f.zgen == zgen
                        && f.width == build_w
                        && f.band == band
                })
                .map(|f| f.total);
            let total = match cached_total {
                Some(t) => t,
                // 探测性测量：不建占位帧，直接返回现成总高 / 行数估算。
                None if probe => e
                    .rd
                    .borrow()
                    .frame
                    .borrow()
                    .as_ref()
                    .map(|f| f.total)
                    .unwrap_or(px((e.doc.line_count() as f32 * 18.).max(400.))),
                None => {
                    // 诊断：本次 measure 缓存未命中的原因（排查双重建/换宽；仅 WY_TRACE）。
                    if crate::prof::trace_active() {
                        let old = e.rd.borrow().frame.borrow().clone();
                        let reason = old
                            .as_ref()
                            .map(|f| {
                                let mut v = Vec::new();
                                if f.rgen != rgen {
                                    v.push("rgen".into());
                                }
                                if f.igen != igen {
                                    v.push("igen".into());
                                }
                                if f.mgen != mgen {
                                    v.push("mgen".into());
                                }
                                if f.zgen != zgen {
                                    v.push("zgen".into());
                                }
                                if f.width != width {
                                    v.push(format!(
                                        "width:{:.0}!={:.0}",
                                        f32::from(f.width),
                                        f32::from(width)
                                    ));
                                }
                                if f.band != band {
                                    v.push(format!("band:{band:?}"));
                                }
                                v.join(",")
                            })
                            .unwrap_or_else(|| "no-frame".into());
                        crate::prof::op_detail_append("mmiss", &reason);
                    }
                    // 操作级实时计时：首帧（frame 从未建成）在此登记 open，让这次
                    // measure→prepaint 的整帧构建都计进同一次操作（输入类操作已在
                    // 事件入口 op_start，这里只补 frame 为空的初始构建）。
                    if std::env::var_os("WY_TRACE").is_some()
                        && !crate::prof::op_pending()
                        && e.rd.borrow().frame.borrow().is_none()
                    {
                        crate::prof::op_start_detail(
                            "open",
                            &format!("band={band:?} width={:.0}", f32::from(width)),
                        );
                    }
                    let _t_bf = crate::prof::begin();
                    let frame = build_frame(e, window, build_w, band);
                    crate::prof::end("wy.measure.bf", _t_bf);
                    let total = frame.total;
                    let _ = e;
                    editor.update(cx, |er, _| {
                        *er.rd.borrow_mut().frame.borrow_mut() = Some(std::rc::Rc::new(frame));
                    });
                    total
                }
            };
            size(width.max(px(1.)), total)
        });
        (id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> WyPrepaint {
        let _t_pp = crate::prof::begin();
        // 居中阅读栏：内容列原点 = 元素屏幕原点 + 水平移位（见 `WyEditor::reading_col_shift`）。
        // prepaint 期所有摆到窗口坐标的 hitbox / 卡片锚点都从「列原点」起算，与 paint 画到的
        // 位置一致（否则命中/光标/卡片横向差一列）。
        let col_origin = {
            let shift = self.editor.read(cx).reading_col_shift(f32::from(bounds.size.width));
            point(bounds.origin.x + px(shift), bounds.origin.y)
        };
        // 可视带（窗口坐标 → 元素局部 y）：元素在滚动容器内的原点即滚动偏移；
        // 量程化使小滚动不动带（measure→prepaint 的带保持一致）。与 cache::zoom_band_for_top
        // 同式（q=512 分带，`top` = 元素局部 y 的视口顶部）。
        let top = (-f32::from(bounds.origin.y)).max(0.);
        let band = super::zoom_band_for_top(top, window.viewport_size().height);
        self.editor.update(cx, |e, _| e.set_shape_band(Some(band)));

        // 带移动 / 内容 / 宽度 / 图片/数学/mermaid缩放 变化 → 按新状态重建窗口化帧。
        let rgen = self.editor.read(cx).content_gen();
        let igen = self.editor.read(cx).images_gen();
        let mgen = self.editor.read(cx).math_gen();
        let zgen = self.editor.read(cx).mermaid_zoom_gen();
        let width = {
            let rd = self.editor.read(cx).rd.borrow();
            let f = rd.frame.borrow();
            f.as_ref().map(|f| f.width).unwrap_or(px(800.))
        };
        let stale = {
            let rd = self.editor.read(cx).rd.borrow();
            let f = rd.frame.borrow();
            f.as_ref().is_none_or(|f| {
                f.rgen != rgen
                    || f.igen != igen
                    || f.mgen != mgen
                    || f.zgen != zgen
                    || f.width != width
                    || f.band != band
            })
        };
        if stale {
            // 诊断：本次重建的诱因（排查 measure/prepaint 双重建；仅 WY_TRACE）。
            if crate::prof::trace_active() {
                let rd = self.editor.read(cx).rd.borrow();
                let f = rd.frame.borrow();
                let reason = Self::frame_stale_reason(
                    f.as_ref().map(|v| &**v),
                    rgen,
                    igen,
                    mgen,
                    zgen,
                    width,
                    band,
                );
                crate::prof::op_detail_append("stale", &reason);
            }
            // 操作级实时计时：无输入类操作在途时，按重建诱因标记 scroll/resize。
            // 输入类操作已在事件处理入口 `op_start`（type/newline/backspace…）；
            // 首帧（frame None）已在 measure 登记 open——这里只补纯滚动/尺寸帧。
            if !crate::prof::op_pending() {
                let old = self.editor.read(cx).rd.borrow().frame.borrow().clone();
                let kind = match old.as_ref() {
                    Some(f) if f.rgen == rgen && f.band != band => "scroll",
                    _ => "resize",
                };
                crate::prof::op_start_detail(kind, &format!("band={band:?} top={top:.0}"));
            }
            let _t_bb = crate::prof::begin();
            let frame = build_frame(self.editor.read(cx), window, width, band);
            crate::prof::end("wy.prepaint.bf", _t_bb);
            self.editor.update(cx, |e, _| {
                *e.rd.borrow_mut().frame.borrow_mut() = Some(std::rc::Rc::new(frame));
            });
        }

        // 字号缩放**视口锚定**（WyEditor 自驱动）：字号变化后 frame 逐帧把「估计高」
        // 细化为「整形真值」，`block_top` 缓慢变动——这里在 WY 自己的每帧 prepaint 里
        // 重钉视口顶，直到恢复目标收敛才消费锚点（对齐 `markdown/view`、`editor` 的
        // Ctrl+滚轮缩放不滚动）。`done` 前 `window.refresh()` 保证被钉的目标逐帧生效；
        // 收敛（`done`）后不再请求帧，避免空转。
        if let Some((target, done)) = self.editor.read(cx).advance_zoom_restore()
            && let Some((vh, pad_y)) = self.editor.read(cx).host_scroll() {
                vh.set_offset(gpui::point(gpui::px(0.), gpui::px(-(target + pad_y))));
                if !done {
                    window.refresh();
                }
            }

        // 编辑区⇄预览滚动同步（WY 自驱动落地）：宿主把目标行写进 `scroll_target`，
        // 在此每帧经 `advance_pending_scroll` 重钉宿主偏移直至稳住（仿缩放恢复收敛，
        // 规避「单次写宿主偏移被后续布局复位」）。目标在或刚清除都要刷下一帧。
        if self.editor.read(cx).scroll_target_active() {
            self.editor.update(cx, |e, _| e.advance_pending_scroll(window));
            if self.editor.read(cx).scroll_target_active() {
                // ⚠️ 不能在这里用 `window.refresh()`：gpui 的 `Window::refresh` 只在
                // `draw_phase == None`（即 draw 之外）才 set_dirty 调度下一帧；prepaint
                // 处于 Prepaint 相位调用是 **no-op**（只置一个从未被读取的 `refreshing`
                // 标志，见 deps gpui src/window.rs）。后果：注记跳转首帧写完宿主偏移后，
                // 若再没有输入事件，就再也没有下一帧——滚动容器不按新偏移重排 → 远带帧
                // 永远补不成显示帧 → 「thumb 已到目标、内容冻结，鼠标一动突然跳」（P4-2
                // 实测）。改走 gpui 动画同款 `request_animation_frame`：它登记「下一帧
                // notify 当前 view」进 on_next_frame 并请求平台一帧，下一平台帧投递时（已
                // 出 draw）notify 有效 → 强制重绘 → advance 继续推进；目标消费后此分支
                // 不再进入 → 收敛即静止，不持续逐帧重绘。
                window.request_animation_frame();
            }
        }

        // 光标可视位置（元素局部）→ IME bounds。
        let frame = {
            let e = self.editor.read(cx);
            e.rd.borrow().frame.borrow().as_ref().map(std::rc::Rc::clone)
        };
        if let Some(f) = frame.as_ref() {
            let editor = self.editor.read(cx);
            let (on, x, ctop) = caret_geometry(editor, f);
            // S3：光标竖条高度跟随所在行行高（标题大字 → 高于正文），IME bounds
            // 与 paint 光标条共用同一高度口径。
            let lh = caret_bar_height(editor, f);
            super::set_caret_vis(CaretVis {
                x,
                top: if on { ctop } else { px(0.) },
                line_height: lh,
            });
        }

        // C5: 行内链接的 hand 光标 hitbox（paint 期 set_cursor_style）。几何与
        // `link_hit_at` 一致（wrapped 首行的字形几何头/尾两段）。
        let mut link_grips = Vec::new();
        if let Some(f) = frame.as_ref() {
            link_grips = Self::link_hit_grips(f, col_origin, window);
        }
        // D1：滚动预热——带穿越前把下一量子带块提前 shape 进缓存（预算内摊平突发）。
        // 传**未量程化**滚动 `top`（静置门签名追真实滚动；frame.band 是 512px 量子栅格，
        // 带内微滚不动带，签名只看 band 会把慢速滚动误判为静置，见 `PrewarmSig`）。
        if let Some(f) = frame.as_ref() {
            prewarm_band(self.editor.read(cx), f, window, top);
        }
        // 图片调整条（右中边缘）hitbox：可编辑模式对所有可见图片行登记。作用有二：
        // (a) 指针进/出该区时 `dispatch_mouse_event` 的 hit_test 变化 → 触发光标刷新；
        // (b) paint 用其 bounds 判定「指针在调整区」→ 发窗口级 `ResizeLeftRight` 光标。
        let mut resize_grips = Vec::new();
        if self.editor.read(cx).is_editable()
            && let Some(f) = frame.as_ref()
        {
            for row in &f.rows {
                if row.kind != crate::markdown::ir::IrKind::Image {
                    continue;
                }
                let Some(bb) = super::image_box_local(f, row) else {
                    continue;
                };
                let hb = super::image_resize_handle_window(col_origin, &bb);
                resize_grips.push(window.insert_hitbox(hb, HitboxBehavior::Normal));
            }
        }
        crate::prof::end("wy.prepaint", _t_pp);
        WyPrepaint {
            link_grips,
            resize_grips,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepaint: &mut WyPrepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let _t_paint = crate::prof::begin();
        crate::prof::paint_mark();
        let (focus, theme) = {
            let e = self.editor.read(cx);
            (e.focus_handle(), e.rd.borrow().theme.clone())
        };
        let frame = {
            let e = self.editor.read(cx);
            e.rd.borrow().frame.borrow().as_ref().map(std::rc::Rc::clone)
        };
        let Some(f) = frame.as_ref() else {
            return;
        };
        // 居中阅读栏：内容列在面板内水平居中——paint 原点 = 元素屏幕原点 + 列水平移位。
        // 之后全部子绘制（行/背景/选区/搜索/光标/IME 竖条）+ 下方卡片锚点都相对列原点
        // 起算，内容区内部坐标（padding/x_off/gutter）不变，拖拽/点击/光标天然对齐。
        let shift = self
            .editor
            .read(cx)
            .reading_col_shift(f32::from(bounds.size.width));
        let origin = point(bounds.origin.x + px(shift), bounds.origin.y);
        // 记录本帧元素在窗口中的 bounds：`MouseDownEvent.position` 是窗口坐标，
        // `source_offset_at` 需用它把命中换算回元素局部坐标。
        self.editor
            .update(cx, |e, _| e.set_last_bounds(Some(bounds)));
        // 图片编辑卡片锚点：以**本帧** `bounds.origin` + 当前帧几何**同代配对**算出并
        // 缓存（render 只读缓存摆卡片）。⚠️ 不能由 render 现算 frame——`end_selection`
        // 清帧后 `ensure_frame_for_input` 用兜底 800 宽重建的临时帧与真实帧宽不同，
        // render 期每次读到不同宽度 → 卡片横向左右跳一帧。
        self.editor
            .update(cx, |e, _| {
                e.image_edit_anchor
                    .set(frame.as_ref().and_then(|f| e.image_edit_anchor_now(origin, f)));
            });
        // 数学编辑卡片锚点：同图片编辑卡——以**本帧** `bounds.origin` + 当前帧几何**同代
        // 配对**算出并缓存（render 只读缓存摆卡片，绝不现算 frame）。
        self.editor
            .update(cx, |e, _| {
                e.math_edit_anchor
                    .set(frame.as_ref().and_then(|f| e.math_edit_anchor_now(origin, f)));
            });
        // 数学排版同步（可见带懒光栅）：只对**当前带内**公式发起排版（对齐
        // view/editor 只渲染可见区；不再开篇全量光栅 1.3 万公式占满后台 CPU）。
        let _t_sm = crate::prof::begin();
        self.editor
            .update(cx, |e, ecx| e.sync_math(ecx, f));
        crate::prof::end("wy.paint.synmath", _t_sm);
        // 代码块高亮后台解析（可见带懒加载）：消费本帧登记的 `request_code_parse`，
        // 后台跑 syntect 整块解析；就绪后定向作废该块整形缓存 → 下帧高亮行重建该块。
        let _t_sp = crate::prof::begin();
        self.editor
            .update(cx, |e, ecx| e.sync_code_parse(ecx));
        crate::prof::end("wy.paint.syncparse", _t_sp);
        // 首帧光栅预热（一次/进程）：先打热 DWrite 彩字管线再画真实行（见
        // [`warm_emoji_rasterizer`]——冷进程首次 COLR 光栅失败 → emoji 变空白/方框）。
        let _t_warm = crate::prof::begin();
        warm_emoji_rasterizer(window);
        crate::prof::end("wy.paint.warmemoji", _t_warm);
        // 背景层：先给所有可见引述行画灰底带 + 左指示条（`py_1` 上下各越 4px，相邻带连片）。
        // ⚠️ 必须**先画全部背景再画全部文字**——若逐行「带 + 文字」连画，后一行的带会盖住
        // 前一行文字底部（引述第一行被第二行背景盖掉的 bug）。
        // callout 区域（marker + body 行）的左条统一用警报色，普通引述用灰条。
        // `alert_region_kinds` 是内容纯函数 → 走 derived 缓存（滚动/每帧不重扫全篇）。
        let derived = {
            let editor = self.editor.read(cx);
            editor.derived()
        };
        let alert_kinds = &derived.borrow().alert_region_kinds;
        self.paint_band_rows(origin, &theme, f, alert_kinds, window, cx);
        // C5: 悬浮在可点击链接上 → hand 光标。
        for hb in &prepaint.link_grips {
            window.set_cursor_style(CursorStyle::PointingHand, hb);
        }
        // 图片调整条：指针落在任一可见图片的右中边缘调整区（或正在拖拽）→
        // **窗口级** `ew-resize`（←||→）光标。⚠️ 不用 per-hitbox `set_cursor_style`：
        // hitbox 光标会被宿主 `#live-vscrollbar-overlay` 等顶层叠层 hitbox 压住而失效，
        // 窗口级 `set_window_cursor_style` 优先级最高；且只在命中此区时才发，其余位置
        // 沿用根元素的 IBeam。hitbox（`resize_grips`）负责触发进/出时的光标刷新。
        let mouse = window.mouse_position();
        let over_resize = self.editor.read(cx).image_resizing()
            || prepaint
                .resize_grips
                .iter()
                .any(|hb| hb.bounds.contains(&mouse));
        if over_resize {
            window.set_window_cursor_style(CursorStyle::ResizeLeftRight);
        }
        // 选区高亮（可编辑行与源选区相交的部分，按字形几何逐个 WrappedLine 画）。
        // 搜索跳转高亮（预览独有能力，搬入统一组件）：命中 → 以命中色复用同一几何。
        let hl = crate::markdown::view::selection_color(cx);
        if let Some(range) = editor_selection(&self.editor, cx) {
            paint_selection(&self.editor, origin, &theme, f, &range, hl, window, cx);
        }
        paint_search_highlight(&self.editor, origin, &theme, f, window, cx);
        // P4-2: 注记跳转加亮（点击 `[n]` 跳对端后给目标标记短暂显色，同搜索高亮机制）。
        paint_note_marker_highlight(&self.editor, origin, &theme, f, window, cx);
        // 光标几何每帧按 caret 现算（帧缓存不含 caret，改 caret 无需整帧重建）。
        // A1 只读门控：`editable=false` 不画光标（光标移动/选区仍工作，只是无竖条）。
        // 闪烁相位：`caret_shown_now()`（聚焦 + 静止期常亮/定时器翻转），替代旧的
        // 恒 `focus.is_focused(window)` —— 修「光标位置对但静止不闪」。
        let (caret_on, caret_x, caret_top) = {
            let e = self.editor.read(cx);
            caret_geometry(e, f)
        };
        // S3：光标竖条高度跟随所在行行高（标题行 → 更高光标、与 `r.lh`/该行字号匹配；
        // 正文/B lank → 现状不变）。与 IME bounds（`caret_vis`）同源同口径。
        let caret_bar_h = {
            let e = self.editor.read(cx);
            caret_bar_height(e, f)
        };
        let caret_shown = self.editor.read(cx).caret_phase.caret_shown_now();
        if self.editor.read(cx).is_editable() && caret_on && caret_shown {
            window.paint_quad(gpui::PaintQuad {
                bounds: Bounds::new(
                    point(origin.x + caret_x, origin.y + caret_top),
                    size(px(2.), caret_bar_h),
                ),
                corner_radii: gpui::Corners::default(),
                background: theme.text.into(),
                border_widths: gpui::Edges::default(),
                border_color: gpui::Hsla::default(),
                border_style: gpui::BorderStyle::Solid,
            });
        }
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );
        // 滚轮：**在宿主 zoom_overlay / 滚动容器之前**（bubble）拦截——光标悬停在某张
        // mermaid 卡片上时，普通滚轮=整卡缩放（`WyEditor::on_scroll_wheel` 返回 true →
        // `stop_propagation`，宿主不滚动）；其余位置放行（Ctrl 归全局字号缩放；
        // 非 Ctrl 归宿主滚动）。必须走 `window.on_mouse_event`：元素在宿主 `zoom_overlay`
        // 顶层兄弟之下，元素级 `.on_scroll_wheel` 会被遮罩吞掉（对比 `editor/view/render.rs`）。
        let wzeditor = self.editor.clone();
        window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
            if !matches!(phase, DispatchPhase::Bubble) {
                return;
            }
            let consumed = wzeditor.update(cx, |e, c| e.on_scroll_wheel(event, window, c));
            if consumed {
                cx.stop_propagation();
            }
        });
        // 图片悬浮态（编辑按钮亮显）：也走窗口级 mouse-move——元素在宿主
        // `#live-vscrollbar-overlay`（inset_0）等顶层叠层之下，元素级 `.on_mouse_move`
        // 会被遮罩吞掉（与上方滚轮同因）。Bubble 阶段读 `last_bounds` 判命中后更新
        // `hover_image`。拖选中（鼠标按下）由元素级拖选事件驱动，这里跳过。
        let wzed_hover = self.editor.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
            if !matches!(phase, DispatchPhase::Bubble) {
                return;
            }
            wzed_hover.update(cx, |e, c| e.on_window_mouse_move(event, c));
        });
        // 鼠标松开兜底：元素级 `on_mouse_up` 受 `hitbox.is_hovered` 门控——松开点落在
        // 元素外 / 顶层叠层（`#live-vscrollbar-overlay`/`#zoom-overlay`）上 / 窗口外时
        // 不会被派发，图片调整条拖拽状态就会残留（后续移动突然改宽）。跟随上方
        // mouse-move 同一窗口级注册路径，无 hover 门控、bubble 阶段无条件清残留。
        let wzed_up = self.editor.clone();
        window.on_mouse_event(move |_event: &MouseUpEvent, phase, _, cx| {
            if !matches!(phase, DispatchPhase::Bubble) {
                return;
            }
            wzed_up.update(cx, |e, c| e.on_window_mouse_up(c));
        });
        crate::prof::end("wy.paint", _t_paint);
        // 操作级实时计时结算：本帧若含未结算操作（type/backspace/scroll/open…），
        // 在此以「开始→绘制完成」的墙钟 + 帧内阶段 breakdown 落地一行 OP 记录。
        crate::prof::op_finish_after_paint();
    }
}

impl WyEditorElement {
    /// 本帧带重建的诱因（WY_TRACE 诊断）：frame `None` → `"no-frame"`，否则以逗号连接
    /// 变化的代标签（rgen/igen/mgen/zgen/width/band）。
    #[allow(clippy::too_many_arguments)] // 各代数标签 + 宽 + 带，纯只读诊断
    fn frame_stale_reason(
        old: Option<&Frame>,
        rgen: u64,
        igen: u64,
        mgen: u64,
        zgen: u64,
        width: gpui::Pixels,
        band: (f32, f32),
    ) -> String {
        old.map_or("no-frame".into(), |f| {
            let mut v = Vec::new();
            if f.rgen != rgen {
                v.push("rgen");
            }
            if f.igen != igen {
                v.push("igen");
            }
            if f.mgen != mgen {
                v.push("mgen");
            }
            if f.zgen != zgen {
                v.push("zgen");
            }
            if f.width != width {
                v.push("width");
            }
            if f.band != band {
                v.push("band");
            }
            v.join(",")
        })
    }

    /// 行内链接的 hand 光标 hitbox（paint 期 `set_cursor_style`）。几何与 `link_hit_at`
    /// 一致（wrapped 首行的字形几何头/尾两段）——折行时头段到行尾、尾段从行首起。
    /// `origin` 传入**内容列原点**（元素屏幕原点 + 居中阅读栏移位），hitbox 与 paint 对齐。
    fn link_hit_grips(
        f: &Frame,
        origin: gpui::Point<gpui::Pixels>,
        window: &mut Window,
    ) -> Vec<gpui::Hitbox> {
        let mut link_grips = Vec::new();
        for row in &f.rows {
            if row.extra.links.is_empty() || row.wrapped.is_empty() {
                continue;
            }
            let Some(w0) = row.wrapped.first() else {
                continue;
            };
            let ox = origin.x + f.padding + row.x_off
                + row.align_offsets.first().copied().unwrap_or(px(0.));
            let oy = origin.y + row.top + px(text_row_shift(row));
            for lk in &row.extra.links {
                let (Some(p0), Some(p1)) = (
                    w0.position_for_index(lk.disp.start, row.lh),
                    w0.position_for_index(lk.disp.end, row.lh),
                ) else {
                    continue;
                };
                if p0.y == p1.y {
                    let hit = Bounds::new(
                        point(ox + p0.x, oy + p0.y),
                        size((p1.x - p0.x).max(px(2.)), row.lh),
                    );
                    link_grips.push(window.insert_hitbox(hit, HitboxBehavior::Normal));
                } else {
                    // 折行链接：头段到行尾、尾段从行首起。
                    let head = Bounds::new(
                        point(ox + p0.x, oy + p0.y),
                        size((w0.width() - p0.x).max(px(2.)), row.lh),
                    );
                    let tail = Bounds::new(point(ox, oy + p1.y), size(p1.x.max(px(2.)), row.lh));
                    link_grips.push(window.insert_hitbox(head, HitboxBehavior::Normal));
                    link_grips.push(window.insert_hitbox(tail, HitboxBehavior::Normal));
                }
            }
        }
        link_grips
    }

    /// 逐行绘制：先给所有可见引述行画灰底带 + 左指示条，再逐行按类型自绘。
    /// ⚠️ 必须**先画全部背景再画全部文字**——若逐行「带 + 文字」连画，后一行的带会盖住
    /// 前一行文字底部（引述第一行被第二行背景盖掉的 bug）。
    /// callout 区域（marker + body 行）的左条统一用警报色，普通引述用灰条。
    #[allow(clippy::too_many_arguments)] // 原点 + 主题 + 帧 + 警报表 + 窗口 + ctx，自绘管线固有
    fn paint_band_rows(
        &self,
        origin: gpui::Point<gpui::Pixels>,
        theme: &WyTheme,
        f: &Frame,
        alert_kinds: &rustc_hash::FxHashMap<
            usize,
            crate::markdown::parse::recognition::AlertKind,
        >,
        window: &mut Window,
        cx: &mut App,
    ) {
        let _t_row = crate::prof::begin();
        for row in &f.rows {
            // 引号区统一画灰底带 + 左指示条：普通 Quote 行 + 引号内结构行
            //（表格/代码/列表/标题/图片/规则，2026-11-13）。仅对已整形可见行画
            //（带外占位 `wrapped` 空且无 `table` 的行跳过，避免画空带）。
            if row.quoted && (row.table.is_some() || !row.wrapped.is_empty()) {
                paint_quote_band(origin, theme, f, row, alert_kinds, window);
            }
        }
        for row in &f.rows {
            // 块级卡片（数学块 / Mermaid 图）优先自绘，跳过普通文本/代码路径。
            if let Some(card) = &row.extra.card {
                paint_card(&self.editor, origin, theme, f, row, card, window, cx);
                continue;
            }
            // 非文本块按类型自绘：代码面板 / 表格网格 / 图片。
            match row.kind {
                IrKind::Code => {
                    // 带外（未 shape）代码块不画（占位无内容）。
                    if row.wrapped.is_empty() {
                        continue;
                    }
                    paint_code_row(&self.editor, origin, theme, f, row, window, cx);
                    continue;
                }
                IrKind::Table => {
                    if let Some(t) = &row.table {
                        paint_table_row(&self.editor, origin, theme, f, row, t, window, cx);
                    }
                    continue;
                }
                IrKind::Image => {
                    if let Some(img) = &row.image {
                        paint_image_row(&self.editor, origin, theme, f, row, img, window, cx);
                    }
                    continue;
                }
                _ => {}
            }
            // 带外（未 shape）行不可见：跳过文本与背景，避免为整篇占位块画屏。
            if row.wrapped.is_empty() {
                continue;
            }
            // 行首装饰（列表符号 / 任务框 / 有序号）画在 gutter；警报 chrome 稍后叠画。
            paint_row_gutter(origin, theme, f, row, window, cx);
            // 引述块背景已在上面预画；这里只画文字 + 警报 chrome（标签/箭头叠在文字上）。
            if row.kind == IrKind::Quote {
                paint_text_row(origin, theme, f, row, window, cx);
                paint_alert_chrome(origin, theme, f, row, window, cx);
            } else {
                paint_text_row(origin, theme, f, row, window, cx);
                // 标题折叠箭头（标题保留行右缘 muted 小三角；点击翻转视图本地折叠态）。
                if matches!(row.kind, IrKind::Heading(_)) {
                    paint_heading_chevron(&self.editor, origin, theme, f, row, window, cx);
                }
            }
            // 行内数学 / 行内图片叠加（B1/B2）。
            paint_inline_maths(&self.editor, origin, theme, f, row, window, cx);
            paint_inline_images(&self.editor, origin, theme, f, row, window, cx);
        }
        crate::prof::end("wy.paint.rowloop", _t_row);
    }
}

#[cfg(test)]
mod tests;
