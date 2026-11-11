//! tabs：选项卡栏渲染与交互。
//!
//! 文件名展示（2026-11-10 由状态栏迁移而来）：每个**有名文件**选项卡悬停 200ms 后在 tab
//! 下方浮出该 tab 自身完整路径 tooltip（P2-9 tooltip 复用）；未命名选项卡（无 file_path）
//! 悬停恒不显示浮层。

use super::*;
use config::timing::TAB_HSCROLL_HIDE_MS;
use shell_widgets::scrollbar::horizontal_scrollbar;
use shell_widgets::TabBoundsRecorder;
use ui::motion::hover_spring;

/// 选项卡栏渲染色集（主题各色一次取齐，打包传给各子元素助手，避免逐个传参）。
struct TabBarColors {
    text_primary: gpui::Hsla,
    text_minor: gpui::Hsla,
    tab_active_bg: gpui::Hsla,
    tab_active_border: gpui::Hsla,
    hover_fill: gpui::Hsla,
}

/// 拖拽排序的白色插入指示线画在哪一侧：`Left` = 本选项卡**左侧**边界（插到它之前）；
/// `Right` = 本选项卡**右侧**边界（插到它之后 / 末尾）。由 `render_tab_bar` 按
/// `tab_reorder_target` 指派给对应选项卡元素自行绘制，天然随横向滚动对齐。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ReorderSide {
    Left,
    Right,
}

impl EditorApp {
    /// SC-4：动作体已迁 `AppModel::make_tab`（shell_core::actions）。此处 glue 转发。
    #[doc(hidden)]
    pub fn make_tab(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> usize {
        let host: &dyn AppHost = &*self;
        self.model.borrow_mut().make_tab(host, &mut **cx)
    }

    /// SC-4：动作体已迁 `AppModel::new_tab`。glue 转发。
    pub(super) fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let host: &dyn AppHost = &*self;
        self.model.borrow_mut().new_tab(host, window, &mut **cx);
    }

    /// 计算拖拽光标 x 落在选项卡行内的「插入下标」（相对当前 `tabs`；`tabs.len()` =
    /// 插到末尾）：
    /// - 最左 / 最右 / 选项卡间空隙 → 对应插入位
    /// - 落在某选项卡内 → 左半 = 该选项卡之前（下标 = 该选项卡），右半 = 之后（下标 +1）
    /// 「是否已入行」由调用方 `tab_drag_over_row` 负责（被拖选项卡边缘进入行即算），
    /// 本函数只按 x 定下标。命中依据 `self.tab_bounds`（上一帧渲染的真实 laid-out
    /// 边界，与输入坐标同系、含横向滚动）。
    pub(super) fn tab_insert_index(&self, x: Pixels) -> Option<usize> {
        if self.model.borrow().tabs.is_empty() || self.tab_bounds.is_empty() {
            return None;
        }
        let first = self.tab_bounds[0].get();
        if first.size.width <= px(0.) {
            return None; // 尚未渲染出真实边界（首帧）
        }
        if x < first.origin.x {
            return Some(0);
        }
        for (i, cell) in self.tab_bounds.iter().enumerate() {
            let b = cell.get();
            if x < b.origin.x {
                return Some(i); // 落在选项卡 i 左侧的空隙
            }
            if x <= b.bottom_right().x {
                let mid = b.origin.x + b.size.width / 2.;
                return Some(if x < mid { i } else { i + 1 });
            }
        }
        Some(self.tab_bounds.len())
    }

    /// 拖拽中的选项卡（picture 盒 `drag`，= `鼠标 - 抓取偏移` 处、按真实 tab 尺寸）的
    /// 上下边缘是否**进入**选项卡行内的 y 区间（任一边进入即 true）：白线出现的触发——
    /// **不要求鼠标光标在行内**，只要被拖选项卡的边缘碰到行即可。
    pub(super) fn tab_drag_over_row(&self, drag: Bounds<Pixels>) -> bool {
        if self.tab_bounds.is_empty() {
            return false;
        }
        let first = self.tab_bounds[0].get();
        if first.size.width <= px(0.) {
            return false; // 尚未渲染出真实边界（首帧）
        }
        let row_top = first.origin.y;
        let row_bottom = first.bottom_right().y;
        drag.origin.y < row_bottom && (drag.origin.y + drag.size.height) > row_top
    }

    /// 栏内拖拽排序落地：把当前下标 `from` 的选项卡移动到「插入下标」`target`
    /// （`tab_insert_index` 输出，相对当前 `tabs`；`tabs.len()` = 末尾）。
    /// 插回原位 / 原相邻位（顺序不变）→ 不动作；单选项卡天然不动作。活动下标随移动
    /// 校正；各选项卡内容/脏点/路径不变；结构变化即时持久化。
    /// SC-4：动作体已迁 `AppModel::reorder_tab`。glue 转发。
    pub(super) fn reorder_tab(&mut self, from: usize, target: usize, cx: &mut Context<Self>) {
        let host: &dyn AppHost = &*self;
        self.model.borrow_mut().reorder_tab(host, from, target, &mut **cx);
    }

    /// SC-4：动作体已迁 `AppModel::activate_tab`。glue 转发。
    #[doc(hidden)]
    pub fn activate_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let host: &dyn AppHost = &*self;
        self.model.borrow_mut().activate_tab(host, idx, window, &mut **cx);
    }

    #[doc(hidden)]
    pub fn close_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.model.borrow().tabs.len() {
            return;
        }
        // 未命名选项卡（无论脏与否：空内容同样在盘上有会话槽位）→ 一律弹确认
        // （是=另存为转正、否=删除临时槽位、取消=留在应用）；命名文件仅当脏（有
        // 未落盘修改）才询问，干净直通关闭。
        if self.model.borrow().tabs[idx].file_path.is_some() && !self.model.borrow().tabs[idx].dirty {
            self.remove_tab(idx, window, cx);
            return;
        }
        // 未保存 / 未命名 → 弹原生任务对话框，异步等待用户选择
        let entity = cx.entity();
        let answer = window.prompt(
            PromptLevel::Warning,
            "未保存的更改",
            Some("此文件尚未保存，关闭前要保存吗？"),
            &["保存", "不保存", "cancel"],
            cx,
        );
        window
            .spawn(cx, async move |cx| {
                // oneshot 通道被取消视为"取消"
                let idx_ans = answer.await.unwrap_or(2);
                let should_close = match idx_ans {
                    0 => {
                        // PDF 选项卡：走待保存模型——触发 PdfViewer::save，等完成信号；
                        // 保存成功（或本就无待存）才关，失败保留标签（observe 已 toast）。
                        let is_pdf = cx
                            .update_window_entity(&entity, |this, _w, _cx| {
                                this.model.borrow().tabs
                                    .get(idx)
                                    .is_some_and(|t| matches!(t.reader, MediaReader::Pdf(_)))
                            })
                            .unwrap_or(false);
                        if is_pdf {
                            // 触发 PdfViewer::save（fire-and-forget，成功清 dirty + notify）；
                            // 随后在**前台**用 timer 轮询 dirty 是否清（save 成功即清）。不在
                            // 后台做阻塞 recv / 长自旋——与 save 自身的 worker 任务零竞争
                            // （后台线程池被阻塞 recv 占住会让 save 的 worker 任务饿死）。
                            let started = cx
                                .update_window_entity(&entity, |this, _w, cx| {
                                    let p = this
                                        .model.borrow().tabs
                                        .get(idx)
                                        .and_then(|t| match &t.reader {
                                            MediaReader::Pdf(p) => Some(p.clone()),
                                            _ => None,
                                        });
                                    p.map(|p| p.update(cx, |v, cx| v.save(cx)))
                                        .unwrap_or(false)
                                })
                                .unwrap_or(false);
                            if !started {
                                true // 无待存 → 直接关
                            } else {
                                // 等 save 完成（dirty 清）：200 × 25ms = 5s 上限；
                                // save 失败 → dirty 保持 → 超时返回 false → 保留标签。
                                let mut saved = false;
                                for _ in 0..200 {
                                    let clean = cx
                                        .update_window_entity(&entity, |this, _w, cx| {
                                            this.model.borrow().tabs
                                                .get(idx)
                                                .map(|t| match &t.reader {
                                                    MediaReader::Pdf(p) => {
                                                        !p.read(cx).is_dirty()
                                                    }
                                                    _ => true,
                                                })
                                                .unwrap_or(true)
                                        })
                                        .unwrap_or(true);
                                    if clean {
                                        saved = true;
                                        break;
                                    }
                                    cx.background_executor()
                                        .timer(std::time::Duration::from_millis(25))
                                        .await;
                                }
                                saved
                            }
                        } else {
                            let (saved, need_dialog) = cx
                                .update_window_entity(&entity, |this, _window, cx| {
                                    // 先取出 path（owned）再 match，避免 `match` 掮获 `borrow()`
                                    // 临时 Ref 跨过块内 `&mut self` 调用（E0502 / 借用纪律收窄）。
                                    let path = this
                                        .model.borrow().tabs
                                        .get(idx)
                                        .and_then(|t| t.file_path.clone());
                                    match path {
                                        Some(p) => {
                                            // 走统一口径：live2 开启时读 WyEditor 实时内容
                                            // （否则关闭保存的是陈旧的源码编辑区，丢整段编辑）。
                                            let text =
                                                this.model.borrow().tab_effective_text(idx, cx);
                                            (Some(this.write_to_path(idx, &p, text, cx)), false)
                                        }
                                        None => (None, true),
                                    }
                                })
                                .unwrap_or((None, false));
                            if need_dialog {
                                let _ =
                                    cx.update_window_entity(&entity, |this, window, cx| {
                                        this.save_unnamed_and_close(idx, window, cx);
                                    });
                                false
                            } else {
                                saved.unwrap_or(false)
                            }
                        }
                    }
                    1 => true,
                    _ => false,
                };
                if should_close {
                    let _ = cx.update_window_entity(&entity, |this, window, cx| {
                        this.remove_tab(idx, window, cx);
                    });
                }
            })
            .detach();
    }

    fn save_unnamed_and_close(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.model.borrow().tabs.len() {
            return;
        }
        let file_name = self.tab_file_name(idx);
        // live2 开启时读 WyEditor 实时内容（防关闭保存丢编辑）。
        let text = self.model.borrow().tab_effective_text(idx, cx);
        let entity = cx.entity();
        window
            .spawn(cx, async move |cx| {
                let Some(file) = rfd::AsyncFileDialog::new()
                    .set_title("保存文件")
                    .set_file_name(&file_name)
                    .save_file()
                    .await
                else {
                    return;
                };
                let file = file.path().to_path_buf();
                let saved = cx
                    .update_window_entity(&entity, |this, _window, cx| {
                        this.write_to_path(idx, &file, text.clone(), cx)
                    })
                    .unwrap_or(false);
                if saved {
                    let _ = cx.update_window_entity(&entity, |this, window, cx| {
                        this.remove_tab(idx, window, cx);
                    });
                }
            })
            .detach();
    }

    /// SC-4：动作体已迁 `AppModel::remove_tab`。glue 转发。
    #[doc(hidden)]
    pub fn remove_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let host: &dyn AppHost = &*self;
        self.model.borrow_mut().remove_tab(host, idx, window, &mut **cx);
    }

    /// 标记“选项卡栏最近在滚动”：横向滚动条立即可见，并重启自动隐藏计时
    /// （[`TAB_HSCROLL_HIDE_MS`] 后无新滚动则隐藏）。`SeqAfter` 代序作废旧定时器，
    /// 旧定时器到点不误清新滚动。
    pub(super) fn poke_tab_hscroll(&mut self, cx: &mut Context<Self>) {
        *self.tab_bar_hscroll.visible.borrow_mut() = true;
        cx.notify();
        let weak = cx.weak_entity();
        self.tab_hscroll_delay.after(
            cx,
            std::time::Duration::from_millis(TAB_HSCROLL_HIDE_MS),
            move |app| {
                let _ = weak.update(app, |app, cx| {
                    *app.tab_bar_hscroll.visible.borrow_mut() = false;
                    cx.notify();
                });
            },
        );
    }

    /// 选项卡栏渲染色集（主题各色一次取齐，打包传给各子元素助手，避免逐个传参）。
    fn tab_bar_colors(&self, _cx: &mut Context<Self>) -> TabBarColors {
        let c = self.chrome();
        TabBarColors {
            text_primary: self.text_primary(),
            text_minor: self.text_minor(),
            tab_active_bg: self.tab_active_bg(),
            tab_active_border: self.tab_active_border(),
            hover_fill: c.hover_fill.into(),
        }
    }

    /// 单个选项卡元素：名称/脏标记/关闭钮/激活顶线 + 拖拽幽灵 + 点击激活。
    /// `reorder_line` = 拖拽排序时本选项卡要绘制的白色插入指示线侧（None = 不画）；
    /// `bounds_cell` = 每帧真实 laid-out 边界写入点（`self.tab_bounds` 槽位，排序命中用）。
    fn tab_bar_item(
        name: &str,
        dirty: bool,
        path_hint: Option<String>,
        idx: usize,
        is_active: bool,
        reorder_line: Option<ReorderSide>,
        bounds_cell: Rc<Cell<Bounds<Pixels>>>,
        colors: &TabBarColors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let name = name.to_string();

        // 元素级 hover 状态：跨帧记住「该 tab 是否被悬停」，关闭 × 因此在
        // 激活项恒定可见、非激活项悬停显现（VS Code 观感）。
        let view_id = window.current_view();
        let tab_hover = window.use_keyed_state(("tab", idx), cx, |_, _| false);
        let hovered = *tab_hover.read(cx);

        // P2-9 鼠标跟随（2026-11-10）：记录下方路径 tooltip 的**窗口坐标锚点**（水平 = 鼠标
        // 窗口 x，垂直 = tab 底缘 y）。锚点在 `on_mouse_move`（事件期）算——此刻 `bounds_cell`
        // 才持有上一帧 laid-out 边界；render 期 `render_tab_bar` 每帧重建 cell（宽恒 0）不可用。
        // key 与既有 ("tab", idx) hover 态区分，互不冲突。
        let tab_mx = window.use_keyed_state(("tab-mx", idx), cx, |_, _| None::<gpui::Point<Pixels>>);
        let anchor = *tab_mx.read(cx);

        // P2-9 tooltip（2026-11-10 由状态栏文件名迁移）：每个 tab 独立 id 键控；悬停
        // 显示该 tab 自身完整路径；未命名（无路径）悬殊停恒不显示浮层。
        let path_hint = path_hint.unwrap_or_default();
        let has_path = !path_hint.is_empty();
        let tip_id = format!("tab-tip-{idx}");
        let tip = ui::tooltip::tooltip_state(&tip_id, window, cx);
        let tip_on = ui::tooltip::tooltip_visible(&tip, cx);

        // 浮层锚在 tab 下方、水平中心随鼠标 x 移动（`tooltip_layer_placed` Window 定位，锚=
        // 事件期算出的窗口坐标点；不盖相邻选项卡）：host_w = 内容宽近似 = px_2 双向 16 +
        // 脏点宽 + 名宽 + 两个 gap_1 8 + 关闭钮 18（仅 None 回退居中使用）。measure 闭包
        // 姿势同 `status_bar`（`window.text_style().font()` + `shape_line(..., px(12.), ...)`）。
        let base_font = window.text_style().font();
        let measure = |text: SharedString, weight: gpui::FontWeight| -> f32 {
            let len = text.len();
            let mut font = base_font.clone();
            font.weight = weight;
            f32::from(
                window
                    .text_system()
                    .shape_line(
                        text,
                        px(12.),
                        &[gpui::TextRun {
                            len,
                            font,
                            color: gpui::Hsla::default(),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                        None,
                    )
                    .width(),
            )
        };
        let dirty_dot_w = if dirty {
            measure("●".into(), gpui::FontWeight::NORMAL) + 4.0
        } else {
            0.0
        };
        let name_w = measure(name.clone().into(), gpui::FontWeight::NORMAL);
        let host_w = 16.0 + dirty_dot_w + name_w + 8.0 + 18.0;

        let close = div()
            .id(SharedString::from(format!("tab-close-{idx}")))
            .w(px(18.))
            .h(px(18.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_md()
            .text_size(px(13.))
            .text_color(colors.text_minor)
            .cursor(CursorStyle::PointingHand)
            .hover(|style| style.bg(colors.hover_fill))
            .child("×")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this: &mut EditorApp, _ev, window, cx| {
                    this.close_tab(idx, window, cx);
                    cx.stop_propagation();
                }),
            )
            // P2-10：仅视觉的 × 悬停显现淡入（激活项恒定、非激活项随行 hover）。
            .with_spring(
                format!("tab-close-op-{idx}"),
                hover_spring(if is_active || hovered { 1.0 } else { 0.0 }),
                move |this, v| this.opacity(v),
            );

        let mut el = div()
            .id(SharedString::from(format!("tab-{idx}")))
            .debug_selector(move || format!("tab-{idx}"))
            .h(px(26.))
            .my(px(3.))
            .px_2()
            .flex()
            .items_center()
            .flex_shrink(1.)
            .relative()
            .cursor(CursorStyle::PointingHand)
            .text_size(px(12.))
            .text_color(colors.text_primary)
            // 几何硬约束（2026-11-10）：[脏点, 名, 关闭钮] 包进内层 flex_row + gap_1，
            // 外层去掉 gap_1——tooltip_layer 空 div 作外层直子零宽度，tab 的 border-box
            // 与改动前逐像素一致；浮层锚在 tab 下方水平居中（host_w=内容宽近似）。
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(if dirty { "● " } else { "" })
                    .child(SharedString::from(name.clone()))
                    .child(close),
            )
            // 路径提示浮层（锚在 tab 下方、水平中心随鼠标 x 移动、不盖相邻选项卡；无路径
            // （未命名）恒不显示）：host_w = 内容宽近似 = px_2 双向 16 + 脏点宽 + 名宽 + 两个
            // gap_1 8 + 关闭钮 18。compact 密度（更小内边距/字号）仅本 tooltip 使用。
            .child(ui::tooltip::tooltip_layer_placed(
                &tip_id,
                &path_hint,
                has_path && tip_on,
                host_w,
                26.0,
                ui::tooltip::TooltipPlacement::Below,
                ui::tooltip::TooltipTone::Compact,
                anchor,
                cx,
            ));
        // 激活项 = VS Code 卡片式：26px 圆角白卡浮在灰带上（浅色下）；
        // 非激活项透明底 + hover 填充（背景由底部弹簧统一淡入，P2-10）——非激活项同样加
        // `rounded_md()`（**不加** `border_1/border_color`，边框仅激活卡片有，非激活透明底
        // 有边框会变方形框），保证 hover 高亮为**圆角矩形**而非尖角方矩形；border-radius
        // 不改 border-box 尺寸，既有几何测试不受影响。
        el = if is_active {
            el.rounded_md()
                .border_1()
                .border_color(colors.tab_active_border)
        } else {
            el.rounded_md()
        };
        // hover 状态跨帧记录，驱动关闭 × 悬停显现与行背景淡入；同时驱动路径 tooltip 出没。
        el = el.on_hover({
            let st = tab_hover.clone();
            let tip = tip.clone();
            move |hovered, _window, cx| {
                st.update(cx, |h, _| *h = *hovered);
                ui::tooltip::tooltip_hover(&tip, *hovered, cx);
                cx.notify(view_id);
            }
        });
        // 鼠标 x 跨帧记录（事件期）：此刻 `bounds_cell` 持上一帧 laid-out 边界（render 期被
        // 每帧重建、不可用）。只在 tooltip **尚未显示**时记录锚点（出现时对齐鼠标当时所在
        // x、垂直 tab 底缘）；一旦显示即冻结——`tip_on` 为真不再记录、也不再 notify 重渲染，
        // 后续任何重渲染都读到同一锚点（真冻结）。移出隐藏后重新进入会重新记录（恢复）。
        el = el.on_mouse_move({
            let st = tab_mx.clone();
            let bcell = bounds_cell.clone();
            move |ev: &gpui::MouseMoveEvent, _window, cx| {
                if !tip_on {
                    let b = bcell.get();
                    if b.size.width > px(0.) {
                        st.update(cx, |m, _| {
                            *m = Some(gpui::point(
                                px(ev.position.x.as_f32()),
                                b.bottom_right().y,
                            ))
                        });
                    }
                }
            }
        });
        // 多窗口拖拽：按住选项卡拖动 → 跟出“胶囊”幽灵；
        // 在窗口外松开由 EditorApp 根视图决定「拆独立窗口 / 还原回主窗口」。
        // 真实 laid-out 尺寸记录器：包住最终元素，每帧把选项卡 border-box 抄进 size_cell，
        // 供拖拽预览 / 幽灵窗以真实尺寸渲染（不缩不被裁）。
        let size_cell = Rc::new(Cell::new(gpui::Size {
            width: px(0.),
            height: px(0.),
        }));
        let drag_data = TabDragData {
            idx,
            name: SharedString::from(name),
            size: size_cell.clone(),
        };
        let ghost_name = drag_data.name.clone();
        // 拖拽开始时 gpui 把「抓取偏移」（按下点在点击矩形内的偏移）交给构造器；窗外
        // 跟手幽灵窗按同一偏移对齐（picture 恒画在鼠标-偏移处），保证窗口边缘无缝。
        let app_weak = cx.weak_entity();
        // P2-10：背景弹簧——激活=白卡轻淡入、非激活=hover 填充淡入（同一曲线）；
        // 激活底色 / hover 底色自带 alpha，`opacity` 乘性，v=1 即原色，语义不变。
        let card = if is_active {
            colors.tab_active_bg
        } else {
            colors.hover_fill
        };
        let el = el.on_drag(
            drag_data,
            move |_value: &TabDragData, offset, window, cx| {
                if let Some(app) = app_weak.upgrade() {
                    let _ = app.update(cx, |app, cx| {
                        app.model.borrow_mut().tab_drag_grab = Some(offset);
                        // 拖拽开始即预建幽灵窗（隐藏；本周期 flush 落地）：Windows 透明置顶
                        // popup 的 OS 创建成本前移到拖拽起点，首个 on_drag_move 只 SetWindowPos
                        // 定位 → 胶囊从拖动一开始就跟手（原「建在首个 on_drag_move」会几百 ms
                        // 才出现）。幂等：已有/在建则跳过。
                        app.prime_drag_ghost(
                            _value.idx,
                            _value.name.clone(),
                            dirty,
                            _value.size.clone(),
                            window,
                            cx,
                        );
                    });
                }
                cx.new(|_| TabDragGhost {
                    name: ghost_name.clone(),
                    dirty,
                    size: (_value).size.clone(),
                    ghost: false,
                })
            },
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this: &mut EditorApp, _ev, window, cx| {
                this.activate_tab(idx, window, cx);
            }),
        )
        // 选项卡右键 → 选项卡管理菜单（关闭组 / 移入·复制新窗口组）。
        // `stop_propagation` 在 `on_tab_right_click` 内：不冒泡到栏级右键（新建/打开）、
        // 也不触发图片/视频媒体右键菜单。
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this: &mut EditorApp, ev, window, cx| {
                this.on_tab_right_click(idx, ev, window, cx);
            }),
        );
        // 拖拽排序：本次拖拽光标落在「插入到本选项卡左/右边界」→ 在本选项卡边界画白色
        // 插入指示线（absolute 子元素定位于**本选项卡自身**边界，随横向滚动天然对齐；
        // 拖拽 picture 覆盖其上为半透明胶囊，不遮蔽整条指示线）。
        let el = if let Some(side) = reorder_line {
            let line = div()
                .absolute()
                .top_0()
                .bottom_0()
                .w(px(2.))
                .bg(gpui::white())
                .rounded_full()
                .debug_selector(|| "tab-reorder-line".into());
            match side {
                ReorderSide::Left => el.child(line.left_0()),
                ReorderSide::Right => el.child(line.right_0()),
            }
        } else {
            el
        };
        let el: gpui::AnyElement = el
            .with_spring(
                format!("tab-bg-{idx}"),
                hover_spring(if is_active || hovered { 1.0 } else { 0.0 }),
                move |this, v| this.bg(card.opacity(v)),
            )
            .into_any_element();
        // 包上尺寸记录器：由 recorder 持有 size_cell / bounds_cell（拖拽侧另有 Clone），
        // 每帧把真实 laid-out 尺寸 / 边界写入 → 拖拽预览 / 幽灵窗按真实尺寸渲染、
        // 栏内排序命中使用。
        TabBoundsRecorder {
            size: size_cell,
            bounds: bounds_cell,
            child: el,
        }
        .into_any_element()
    }

    /// 整条选项卡栏最右侧的眼睛图标：仅当"当前选中的文件"是 Markdown（.md / .markdown）
    /// 时才显示，点击切换当前选项卡的 Markdown 预览（与「查看 → 预览」一致）。
    /// 用纯 div 画出（椭圆外框 + 中心瞳孔），避免依赖 emoji 字体；蓝色=已开，灰色=已关。
    fn tab_eye_button(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let active_is_md = self
            .model.borrow().tabs
            .get(self.model.borrow().active_tab)
            .and_then(|t| t.file_path.as_ref())
            .map(|p| Self::is_markdown_path(p))
            .unwrap_or(false);
        if !active_is_md {
            return None;
        }
        let on = self.show_preview();
        let eye_color: gpui::Hsla = if on {
            self.chrome().pill_fg.into()
        } else {
            self.chrome().text_tertiary.into()
        };
        let hover_fill: gpui::Hsla = self.chrome().hover_fill.into();
        // P2-10：hover 状态跨帧记录 → 背景淡入。
        let hover = window.use_keyed_state("tab-eye-hover", cx, |_, _| false);
        let hovered = *hover.read(cx);
        let view_id = window.current_view();
        Some(
            div()
                .id("tab-bar-eye")
                .debug_selector(|| "tab-bar-eye".into())
                .w(px(22.))
                .h(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .cursor(CursorStyle::PointingHand)
                .on_hover({
                    let st = hover.clone();
                    move |h, _w, cx| {
                        st.update(cx, |v, _| *v = *h);
                        cx.notify(view_id);
                    }
                })
                .child(
                    div()
                        .w(px(16.))
                        .h(px(10.))
                        .rounded_full()
                        .border_1()
                        .border_color(eye_color)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(div().w(px(5.)).h(px(5.)).rounded_full().bg(eye_color)),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this: &mut EditorApp, _ev, window, cx| {
                        let idx = this.model.borrow().active_tab;
                        this.toggle_preview_for_tab(idx, window, cx);
                    }),
                )
                .with_spring(
                    "tab-eye-bg",
                    hover_spring(if hovered { 1.0 } else { 0.0 }),
                    move |this, v| this.bg(hover_fill.opacity(v)),
                )
                .into_any_element(),
        )
    }

    /// 横向滚动条自动隐藏：只在标签超宽 + （最近滚动 / 正在拖拽滑块）时出现；
    /// 半透明、贴着标签底部、浮在上方（不额外占高度）。
    fn tab_bar_hscroll_overlay(&self) -> Option<gpui::AnyElement> {
        let hscroll_dragging = self.tab_bar_hscroll.drag.borrow().is_some();
        if self.tab_bar_hscroll.handle.max_offset().x > px(0.)
            && (*self.tab_bar_hscroll.visible.borrow() || hscroll_dragging)
        {
            // 滑块底色取主题语义 token（ChromeColors::scrollbar_thumb，亮暗同值）。
            let thumb = ChromeColors::for_mode(self.model.borrow().dark_mode).scrollbar_thumb;
            Some(
                div()
                    .absolute()
                    .debug_selector(|| "tab-bar-hscroll-overlay".into())
                    .left(px(0.))
                    .right(px(0.))
                    .bottom(px(0.))
                    .h(px(3.))
                    .child(horizontal_scrollbar(
                        &self.tab_bar_hscroll,
                        "tab-bar-hscroll",
                        thumb,
                    ))
                    .into_any_element(),
            )
        } else {
            None
        }
    }

    /// 选项卡栏：横跨右列顶部的选项卡行，左侧滚动区 + （按需）眼睛按钮；
    /// 右键整条栏弹「新建文件 / 打开文件」上下文菜单（取代已移除的「+」按钮）。
    /// 底边绘一条右列宽发丝线（`bar_border`），分隔下方编辑区正文。
    /// 零选项卡（`tabs.is_empty()`，即欢迎页呈现时）提前返回空元素——选项卡栏
    /// 在 flex 列中占零高度，欢迎页从标题栏下直接铺到状态栏。
    pub(super) fn render_tab_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        if self.model.borrow().tabs.is_empty() {
            self.tab_bounds.clear();
            return div().into_any_element();
        }
        let active = self.model.borrow().active_tab;
        let colors = self.tab_bar_colors(cx);
        // 重建每帧的页卡边界槽（排序命中测试读上一帧值；记录器在 prepaint 写入）。
        self.tab_bounds = (0..self.model.borrow().tabs.len())
            .map(|_| Rc::new(Cell::new(Bounds::default())))
            .collect();
        // 拖拽排序插入指示线：仅拖拽中按 `tab_reorder_target` 指派——目标 < n → 该目标
        // 选项卡左缘一线；目标 == n（末尾）→ 最后一个选项卡右缘一线。
        let reorder_target = if self.model.borrow().tab_dragging.is_some() {
            self.model.borrow().tab_reorder_target
        } else {
            None
        };
        // 借用纪律（SC-5）：先一次 borrow 快照每个选项卡渲染所需的廉价字段（显示名 +
        // 脏标记 + 完整路径），guard 随即释放——构造期不再持 `tabs` 长借用。
        // `tab_bar_item` 只消费这三项派生数据，改动零渲染差异（未命名路径为 None）。
        let tab_meta: Vec<(String, bool, Option<String>)> = {
            let model = self.model.borrow();
            model
                .tabs
                .iter()
                .map(|t| {
                    let name = t
                        .file_path
                        .as_ref()
                        .and_then(|p| p.file_name())
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "未命名".to_string());
                    let path_hint = t
                        .file_path
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned());
                    (name, t.dirty, path_hint)
                })
                .collect()
        };
        let tabs_len = tab_meta.len();
        let mut tabs: Vec<gpui::AnyElement> = Vec::new();
        for (i, (name, dirty, path_hint)) in tab_meta.into_iter().enumerate() {
            let line = match reorder_target {
                Some(t) if i == t => Some(ReorderSide::Left),
                Some(t) if t == tabs_len && i + 1 == tabs_len => Some(ReorderSide::Right),
                _ => None,
            };
            tabs.push(Self::tab_bar_item(
                &name,
                dirty,
                path_hint,
                i,
                i == active,
                line,
                self.tab_bounds[i].clone(),
                &colors,
                window,
                cx,
            ));
        }

        // 选项卡栏右侧：固定放（按需）眼睛图标；标签过多时由左侧滚动区横向滚动
        let mut right: Vec<gpui::AnyElement> = Vec::new();
        if let Some(eye) = self.tab_eye_button(window, cx) {
            right.push(eye);
        }

        // 横向滚动条自动隐藏：只在标签超宽 + （最近滚动 / 正在拖拽滑块）时出现。
        let mut hscroll_overlay: Vec<gpui::AnyElement> = Vec::new();
        if let Some(o) = self.tab_bar_hscroll_overlay() {
            hscroll_overlay.push(o);
        }

        div()
            .id("tab-bar")
            .debug_selector(|| "tab-bar".into())
            .relative()
            .w_full()
            // 左侧不留内边距：第一个选项卡紧贴窗口左边缘；右侧保留 px_2 给眼睛按钮
            .pl_0()
            .pr_2()
            .flex()
            .items_center()
            .gap_1()
            .bg(self.mica_bar_bg())
            // 右键整条栏（含点在各选项卡上，事件冒泡到这）→ 「新建文件 / 打开文件」上下文菜单
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(Self::on_tab_bar_right_click),
            )
            // 选项卡栏↔编辑区的发丝线由下方编辑区顶边绘制（见 render_body_card，
            // 此处不画底边框，避免被后绘制的编辑区面板盖住）。
            // 选项卡横向滚动区：占满剩余宽度；标签不收缩，过多即横向溢出可滚轮/拖拽滚动。
            // 横向滚动条浮在标签底部（见 hscroll_overlay），不改变整体高度。
            .child(
                div()
                    .id("tab-bar-scroll")
                    .relative()
                    .flex_1()
                    .min_w(px(0.))
                    .h(px(32.))
                    .child(
                        div()
                            .id("tab-bar-tabs")
                            .debug_selector(|| "tab-bar-tabs".into())
                            .w_full()
                            .h_full()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .overflow_x_scroll()
                            .track_scroll(&self.tab_bar_hscroll.handle)
                            // 滚动时点亮横向滚动条（自动隐藏计时由此重启）
                            .on_scroll_wheel(cx.listener(
                                |this: &mut EditorApp, _ev, _window, cx| {
                                    this.poke_tab_hscroll(cx);
                                },
                            ))
                            .children(tabs),
                    )
                    .children(hscroll_overlay),
            )
            .children(right)
            .into_any_element()
    }

    /// 选项卡栏右键 → 在光标处弹「新建文件 / 打开文件」上下文菜单（取代已移除的「+」
    /// 按钮）。关闭（选择/Esc/点击外）经 `DismissEvent` 订阅清理；先收掉上一份菜单
    /// （若有）再在光标处新建（旧订阅带 emitter 身份校验，不会误清新菜单）。
    /// 装配/焦点/订阅闭环收敛自 `ui::menu::spawn_context_menu`。
    fn on_tab_bar_right_click(
        &mut self,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 事件继续冒泡到树根会额外触发图片/视频右键「锁定」菜单（on_media_right_click），
        // 此处拦截，保证右键选项卡栏只弹本菜单。
        cx.stop_propagation();
        self.tab_bar_menu_entity = None;
        self.tab_bar_menu_pos = None;
        self.tab_bar_menu_sub = None;
        let weak = cx.weak_entity();
        let (menu, sub) = ui::menu::spawn_context_menu(
            window,
            cx,
            move |menu, _window, _cx| {
                menu.item({
                    let weak = weak.clone();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("新建文件")
                            .debug_selector(|| "tab-bar-menu-item-new".into())
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| this.new_tab(window, cx));
                    })
                })
                .item({
                    let weak = weak.clone();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("打开文件")
                            .debug_selector(|| "tab-bar-menu-item-open".into())
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| this.open_dialog(window, cx));
                    })
                })
            },
            |this, emitter, cx| {
                // 关闭（选择/Esc/点击外）→ 清理；emitter 身份校验避免旧菜单的关闭误清新菜单
                if this.tab_bar_menu_entity.as_ref() == Some(&emitter) {
                    this.tab_bar_menu_entity = None;
                    this.tab_bar_menu_pos = None;
                }
                cx.notify();
            },
        );
        self.tab_bar_menu_pos = Some(ev.position);
        self.tab_bar_menu_entity = Some(menu);
        self.tab_bar_menu_sub = Some(sub);
        cx.notify();
    }

    /// 选项卡栏右键菜单 Overlay：按右键时记录的**窗口坐标**定位
    /// （`AnchoredPositionMode::Window` + `position(pos)`——`position()` 才是 gpui 语义里
    /// 「窗口坐标」入参；旧的 `offset(pos)` 缺 `position()` 时会被当作「相对锚定元素自身
    /// 静态布局位置的偏移」，而本菜单包在 `#tab-bar-menu` 外壳 flex 流内、静态位在窗口
    /// 底部 → 菜单被推出窗口再被 `snap_to_window_with_margin` 钳回底部（沉底 bug）。
    /// 媒体右键菜单因直接作为外壳 flex 直子、静态位恰为 (0,0) 才表现出正常而已，同隐患。
    pub(super) fn render_tab_bar_menu(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(pos) = self.tab_bar_menu_pos else {
            return div().into_any_element();
        };
        let Some(menu) = self.tab_bar_menu_entity.clone() else {
            return div().into_any_element();
        };
        div()
            .id("tab-bar-menu")
            .debug_selector(|| "tab-bar-menu".into())
            .child(
                deferred(
                    anchored()
                        .position_mode(gpui::AnchoredPositionMode::Window)
                        .anchor(Anchor::TopLeft)
                        .position(pos)
                        .snap_to_window_with_margin(Edges::all(px(8.)))
                        .child(menu),
                )
                .with_priority(100),
            )
            .into_any_element()
    }

    // -----------------------------------------------------------------
    // 选项卡右键管理菜单 + 批量关闭 + 移入/复制新窗口
    // -----------------------------------------------------------------

    /// 选项卡右键 → 在光标处弹**选项卡管理菜单**（复用 `tab_bar_menu_entity/pos/sub`
    /// 与 `render_tab_bar_menu` Overlay，同一时刻只开一个）。菜单分组：
    /// 关闭 / 关闭其他 / 关闭右侧 / 关闭已保存 / 关闭全部 ── 分割线 ──
    /// 移入新窗口 / 复制到新窗口。
    /// `stop_propagation`：不冒泡到选项卡栏空白区右键（新建/打开），也不触发媒体右键菜单。
    pub(super) fn on_tab_right_click(
        &mut self,
        idx: usize,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        self.tab_bar_menu_entity = None;
        self.tab_bar_menu_pos = None;
        self.tab_bar_menu_sub = None;
        let weak = cx.weak_entity();
        let (menu, sub) = ui::menu::spawn_context_menu(
            window,
            cx,
            move |menu, _window, _cx| {
                menu.item({
                    let weak = weak.clone();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("关闭")
                            .debug_selector(|| "tab-menu-close".into())
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| this.close_tab(idx, window, cx));
                    })
                })
                .item({
                    let weak = weak.clone();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("关闭其他")
                            .debug_selector(|| "tab-menu-close-others".into())
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.tab_close_others(idx, window, cx);
                        });
                    })
                })
                .item({
                    let weak = weak.clone();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("关闭右侧")
                            .debug_selector(|| "tab-menu-close-right".into())
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.tab_close_right(idx, window, cx);
                        });
                    })
                })
                .item({
                    let weak = weak.clone();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("关闭已保存")
                            .debug_selector(|| "tab-menu-close-saved".into())
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.tab_close_saved(window, cx);
                        });
                    })
                })
                .item({
                    let weak = weak.clone();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("关闭全部")
                            .debug_selector(|| "tab-menu-close-all".into())
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.tab_close_all(window, cx);
                        });
                    })
                })
                .separator()
                .item({
                    let weak = weak.clone();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("移入新窗口")
                            .debug_selector(|| "tab-menu-move-new-window".into())
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.tab_move_to_new_window(idx, window, cx);
                        });
                    })
                })
                .item({
                    let weak = weak.clone();
                    PopupMenuItem::element(move |_, _| {
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .child("复制到新窗口")
                            .debug_selector(|| "tab-menu-copy-new-window".into())
                    })
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.tab_copy_to_new_window(idx, window, cx);
                        });
                    })
                })
            },
            |this, emitter, cx| {
                if this.tab_bar_menu_entity.as_ref() == Some(&emitter) {
                    this.tab_bar_menu_entity = None;
                    this.tab_bar_menu_pos = None;
                }
                cx.notify();
            },
        );
        self.tab_bar_menu_pos = Some(ev.position);
        self.tab_bar_menu_entity = Some(menu);
        self.tab_bar_menu_sub = Some(sub);
        cx.notify();
    }

    /// SC-4：批量关闭动作体已迁 `AppModel`（tab_close_guard/others/right/all/saved）。
    /// 「关闭其他」：关闭除 `idx` 外全部；任一受影响脏/未命名 → 中止一个都不关。
    pub(super) fn tab_close_others(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let host: &dyn AppHost = &*self;
        self.model.borrow_mut().tab_close_others(host, idx, window, &mut **cx);
    }

    /// 「关闭右侧」：关闭 `idx` 右侧全部；任一受影响脏/未命名 → 中止一个都不关。
    pub(super) fn tab_close_right(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let host: &dyn AppHost = &*self;
        self.model.borrow_mut().tab_close_right(host, idx, window, &mut **cx);
    }

    /// 「关闭全部」：关闭全部；任一脏/未命名 → 中止一个都不关。
    pub(super) fn tab_close_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let host: &dyn AppHost = &*self;
        self.model.borrow_mut().tab_close_all(host, window, &mut **cx);
    }

    /// 「关闭已保存」：只关 `有路径 && !dirty` 的选项卡（定义如此，无中止语义）；
    /// 脏 / 未命名 / 无路径空选项卡保留。
    pub(super) fn tab_close_saved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let host: &dyn AppHost = &*self;
        self.model.borrow_mut().tab_close_saved(host, window, &mut **cx);
    }

    /// 「移入新窗口」：取载荷 → 拆进新独立窗口（级联偏移 = 源窗口左上 + (24,24)）。
    /// 源窗口剩零选项卡 → 现行为回欢迎页；detached 窗口同样可再拆。
    pub(super) fn tab_move_to_new_window(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if idx >= self.model.borrow().tabs.len() {
            return;
        }
        let Some(payload) = self.commit_live_and_take_tab_payload(idx, window, cx) else {
            return;
        };
        self.split_tab_to_new_window(payload, Point::new(px(24.), px(24.)), window, cx);
    }

    /// 「复制到新窗口」：源选项卡**保留原状**；文本类 = 内容快照副本（独立编辑器缓冲，
    /// 可独立编辑）；媒体类 = 新窗口按原路径重新打开（文件不可变，路径即快照）。
    pub(super) fn tab_copy_to_new_window(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if idx >= self.model.borrow().tabs.len() {
            return;
        }
        // 先提纯 reader_none / media_path（owned）再分支，避免 `if` 条件里的 `borrow()`
        // 临时 Ref 跨过块内 `&mut self` 调用（E0502 / 借用纪律收窄）。
        let reader_none = self.model.borrow().tabs[idx].reader.is_none();
        let media_path = self.model.borrow().tabs[idx].file_path.clone();
        if reader_none {
            self.tab_copy_text_to_new_window(idx, window, cx);
        } else if let Some(p) = media_path {
            // 媒体（PDF/图片/视频）：文件型不可变 → 新窗口按路径重开（路径即快照）。
            self.tab_copy_media_to_new_window(&p, window, cx);
        }
    }

    /// 文本类「复制到新窗口」：先 `make_tab` 得到空 EditorTab（免手抄 25 字段）→
    /// 用新的独立 CodeEditor 种子（有效文本/语言/光标）→ 回填元数据（路径/dirty/
    /// 预览等）→ `take_tab_payload` 转 DetachedTab → `split_tab_to_new_window`。
    /// 副本不共享源编辑器实体（独立缓冲）；源选项卡不动。复制期间源的编辑事件订阅会
    /// 置脏/刷新缓存，故**先喂编辑器、后回填元数据**（覆盖事件副作用，保 dirty 语义）。
    fn tab_copy_text_to_new_window(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (text, path, dirty, preview_on, readonly_preview, cursor_row, cursor_col, line_count, char_count, src_enc) = {
            let model = self.model.borrow();
            let t = &model.tabs[idx];
            (
                model.tab_effective_text(idx, &**cx),
                t.file_path.clone(),
                t.dirty,
                t.preview_on,
                t.readonly_preview,
                t.cursor_row,
                t.cursor_col,
                t.line_count,
                t.char_count,
                t.source_encoding, // 复制 → 编码随文档走（保存回写不丢）
            )
        };
        let lang = path.as_ref().map(|p| Self::language_id(p));
        // 复制前记下源窗口活动选项卡：`make_tab` 把 `active_tab` 指到瞬态副本、
        // `take_tab_payload` 拔出副本后又把它改到末位选项卡——复制「非末位」选项卡时
        // 源窗口活动会跳到最后一个选项卡（可见闪烁）。取下后还原到复制前活动。
        let kept_active = self.model.borrow().active_tab;
        let new_idx = self.make_tab(window, cx);
        // ① 先喂独立编辑器（新实体，不共享源缓冲）：内容 / 语言沿用 / 光标保留。
        self.model.borrow().tabs[new_idx].editor.update(cx, |ed, _cx| {
            ed.set_text(&text);
            if let Some(l) = lang {
                ed.set_language(Some(l));
            }
            ed.set_caret_to(ed.offset_for_line_col(cursor_row, cursor_col));
        });
        // ② 后回填元数据（覆盖 set_text 触发的编辑事件副作用，保 dirty/preview 语义）。
        {
            let mut model = self.model.borrow_mut();
            let t = &mut model.tabs[new_idx];
            t.file_path = path.clone();
            t.source_encoding = src_enc;
            t.dirty = dirty;
            t.content = SharedString::default();
            t.line_count = line_count;
            t.char_count = char_count;
            t.cursor_row = cursor_row;
            t.cursor_col = cursor_col;
            t.preview_on = preview_on;
            t.readonly_preview = readonly_preview;
        }
        let Some(payload) = self.take_tab_payload(new_idx) else {
            return;
        };
        // 还原源窗口活动选项卡（瞬态副本已拔出，原下标全部未变）。
        self.model.borrow_mut().active_tab = kept_active;
        // make_tab 期间的会话快照带瞬态副本 tab → 取下后立即补刷（detached 内部早退）。
        self.schedule_session_flush(cx, true);
        self.split_tab_to_new_window(payload, Point::new(px(24.), px(24.)), window, cx);
    }

    /// 媒体类「复制到新窗口」：新开一个 detached 窗口（占位空选项卡）→ 找到该窗口 →
    /// 占位让位于「按原路径重开」媒体（路径即不可变文件的快照，源选项卡保留）。
    fn tab_copy_media_to_new_window(
        &mut self,
        path: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // ① 记当前已注册窗口句柄 → 开新 detached 窗口（占位空载荷）。
        let before: Vec<AnyWindowHandle> = if cx.has_global::<EditorAppRegistry>() {
            cx.global::<EditorAppRegistry>()
                .windows
                .iter()
                .map(|(_, wh)| *wh)
                .collect()
        } else {
            Vec::new()
        };
        let editor = cx.new(|cx| editor::CodeEditor::new(cx));
        let placeholder = DetachedTab {
            editor,
            reader: MediaReader::None,
            md_wy: None,
            md_wy_vscroll: gpui::ScrollHandle::new(),
            md_wy_install: MdWyInstall::default(),
            epub_images: Default::default(),
            epub_notes: Default::default(),
            file_path: None,
            dirty: false,
            source_encoding: None, // 媒体副本：按原路径重开，编码由新窗口打开时重新探测
            content: SharedString::default(),
            line_count: 1,
            char_count: 0,
            cursor_row: 0,
            cursor_col: 0,
            preview_on: false,
            preview_fullscreen: false,
            readonly_preview: false,
            editor_pending: false,
            loading_lock: false,
            edit_version: 0,
        };
        self.split_tab_to_new_window(placeholder, Point::new(px(24.), px(24.)), window, cx);
        // ② 找到刚注册的 detached 新窗口实体（不在 before）→ 占位让位、按原路径重开。
        let Some((target, wh)) = cx
            .global::<EditorAppRegistry>()
            .windows
            .iter()
            .find(|(weak, wh)| {
                !before.contains(wh)
                    && weak
                        .upgrade()
                        .is_some_and(|e| e.read_with(cx, |a, _| a.is_detached))
            })
            .map(|(weak, wh)| (weak.clone(), *wh))
        else {
            return;
        };
        let reopen = path.to_path_buf();
        let _ = wh.update(cx, |_root, w, cx| {
            let _ = target.update(cx, |app, cx| {
                if !app.model.borrow().tabs.is_empty() {
                    app.remove_tab(0, w, cx); // 占位空选项卡让位
                }
                // 直调 `open_file_here`（而非 `open_file`）：本动作的目标窗口就是刚建的新
                // detached 窗口，若走 `open_file` 的媒体重定向，视频会被再投到媒体窗口、
                // 留下空 detached 窗口（media_window.rs 分流只认 `open_file` 一个入口）。
                // 新窗口不含该路径 → 无需「已开则激活」分支。
                app.open_file_here(&reopen.to_string_lossy(), w, cx);
            });
        });
    }
}
