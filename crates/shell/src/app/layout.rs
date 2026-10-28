//! 主体布局与根视图渲染：`Render` 实现、零选项卡欢迎页、正文区三态（实时编辑 /
//! 预览分屏 / 纯编辑）、外壳子元素装配，以及根元素上的拖入文件 / 选项卡拖拽 /
//! 快捷键动作监听。

use super::*;

impl Render for EditorApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 音乐模式：整窗渲染 MusicView。外层包一个壳 div 承载 `ToggleMusicPlayer` 重入
        // （Ctrl+M 再次触发可切回编辑器界面）——与文档模式根元素的同名 on_action 并存，
        // 两套都挂 `toggle_music_player`（同一 listener 两处挂载，命中任一即可切关）。
        // 返回 `AnyElement`：与下方 render_root 的 opaque 返回统一成同一具体类型
        //（`impl IntoElement` 的所有返回路径必须是同一类型）。
        if self.music_mode {
            if let Some(mv) = &self.music {
                return div()
                    .id("music-root-shell")
                    .debug_selector(|| "music-root-shell".into())
                    .size_full()
                    .on_action(cx.listener(Self::toggle_music_player_on_action))
                    .child(mv.clone().into_any_element())
                    .into_any_element();
            }
            // 兜底：视图缺失（异常销毁）→ 退出音乐模式，走正常编辑器界面。
            self.music_mode = false;
        }
        // WY_TRACE 归因：操作（按键/滚动）起点 → 本帧 render 开始的等待，单独记账
        // 供「每键真实延迟」拆账——多为 60Hz vsync/帧调度，不是 CPU 段。
        if let Some(d) = crate::prof::op_elapsed() {
            crate::prof::add_absolute("app.frame_delay", d);
        }
        let _t = crate::prof::begin();
        // 拖分割条帧级合并（B4）：`on_mouse_move` 只记最新目标比例，这里本帧开始统一
        // 落地到 `split_ratio`（在 `render_body` 读取它之前，当帧即生效）。
        if let Some(r) = self.pending_split_ratio.take() {
            self.split_ratio = r;
        }
        // 编辑区⇄预览滚动同步：动了的一侧驱动另一侧到对应块（仅 MD 分屏、非 live2）。
        self.sync_editor_preview_scroll(cx);
        // 实时预览编辑（WYSIWYG）本身即「所见即所得」，右侧 Markdown 预览冗余 →
        // 编辑区占满整块（不显示分割线与预览区），退出实时模式后恢复原分屏。
        let body = self.render_body(window, cx);

        let root = self.render_root(body, window, cx);
        crate::prof::end("wy.app.render", _t);
        root.into_any_element()
    }
}

impl EditorApp {
    /// 内容区三态：有选项卡 → 实时编辑占满 / 预览分屏（EPUB 等全屏预览）/ 纯编辑占满；
    /// 零选项卡 → 欢迎页整屏占满正文区（不渲染编辑区）。
    pub(super) fn render_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        // 新模型：欢迎页只在「零选项卡」时呈现。只要有 ≥1 个选项卡——哪怕新建出的
        // 空白未命名文档——就直接渲染编辑区（可键入），绝不叠欢迎页（原 `is_empty_body`
        // 把「未命名空文档」也算空态的语义已被模型 3 取代：有 tab 即不显示欢迎页）。
        if self.tabs.is_empty() {
            return Self::pane_wrap(
                Some("app-empty-body"),
                Some("app-empty-body"),
                true,
                self.render_empty_state(cx),
            );
        }
        let live_edit = self
            .tabs
            .get(self.active_tab)
            .map(|t| t.live_edit)
            .unwrap_or(false);
        if live_edit {
            // 实时预览编辑（WYSIWYG）本身即「所见即所得」，右侧 Markdown 预览冗余 →
            // 编辑区占满整块（不显示分割线与预览区），退出实时模式后恢复原分屏。
            Self::pane_wrap(
                Some("app-live-body"),
                Some("app-live-body"),
                false,
                self.render_editor_pane(window, 1.0, cx),
            )
        } else if self.show_preview() {
            let ratio = self.split_ratio;
            // EPUB 等全屏预览：预览区占满整个内容区，不显示编辑区与分割线。
            let fullscreen = self
                .tabs
                .get(self.active_tab)
                .map(|t| t.preview_fullscreen)
                .unwrap_or(false);
            if fullscreen {
                div()
                    .id("app-fullscreen-preview")
                    .debug_selector(|| "app-fullscreen-preview".into())
                    .flex()
                    .flex_1()
                    .min_h(px(0.))
                    .child(self.render_fullscreen_preview(window, cx))
                    .into_any_element()
            } else {
                div()
                    .id("app-split-body")
                    .debug_selector(|| "app-split-body".into())
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.))
                    // 分割线拖拽：鼠标移动/松开事件冒泡到容器统一处理。拖拽只记**最新
                    // 目标比例**（帧级合并，本帧 `render` 开头统一落地到 `split_ratio`）
                    // ——事件可比帧快，不逐事件重复写 + notify。无变化（拖到尽头被
                    // clamp 住）直接跳过 notify，不再白重建一帧。
                    .on_mouse_move(cx.listener(
                        |this: &mut EditorApp, ev: &gpui::MouseMoveEvent, window, cx| {
                            if let Some((start_x, start_ratio)) = this.split_drag {
                                let total = window.bounds().size.width.as_f32();
                                if total > 0. {
                                    let x = ev.position.x.as_f32();
                                    let ratio = (start_ratio + (x - start_x) / total)
                                        .clamp(SPLIT_RATIO_MIN, SPLIT_RATIO_MAX);
                                    if (ratio - this.split_ratio).abs() > 1e-5 {
                                        this.pending_split_ratio = Some(ratio);
                                        cx.notify();
                                    }
                                }
                            }
                        },
                    ))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(
                            |this: &mut EditorApp, _: &gpui::MouseUpEvent, _w, cx| {
                                this.pending_split_ratio = None;
                                this.split_drag = None;
                                cx.notify();
                            },
                        ),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(
                            |this: &mut EditorApp, _: &gpui::MouseUpEvent, _w, cx| {
                                this.pending_split_ratio = None;
                                this.split_drag = None;
                                cx.notify();
                            },
                        ),
                    )
                    .child(self.render_editor_pane(window, ratio, cx))
                    .child(self.render_splitter(cx))
                    .child(self.render_preview_pane(window, 1.0 - ratio, cx))
                    .into_any_element()
            }
        } else {
            // 无预览：编辑区占满剩余。必须 min_w(0)+overflow_hidden——
            // 否则该容器 min-width:auto 被编辑器 980px 内容撑住，右侧溢出窗口
            // 46px，编辑器元素右缘落在屏幕外 → 滚动条画在窗口外看不见。
            Self::pane_wrap(None, None, false, self.render_editor_pane(window, 1.0, cx))
        }
    }

    /// 正文区面板统一外壳：`.flex().flex_1().min_h(0).min_w(0).overflow_hidden()`
    /// 防坍缩样板收敛为一处（`flex_1` 高度坍缩会毁滚动/命中，K19 同款先例），外加
    /// 可选 id / debug_selector / relative（欢迎页需 relative，其余分支不设）。
    /// id/debug_selector 的组合在宏内每臂保持单一具体元素类型（`Div` 与
    /// `Stateful<Div>` 互斥），避免 `if` 赋值跨类型分叉。
    fn pane_wrap(
        with_layout_id: Option<&'static str>,
        debug_selector: Option<&'static str>,
        relative: bool,
        inner: gpui::AnyElement,
    ) -> gpui::AnyElement {
        macro_rules! finish {
            ($el:expr) => {{
                let el = if relative { $el.relative() } else { $el };
                el.flex()
                    .flex_1()
                    .min_h(px(0.))
                    .min_w(px(0.))
                    .overflow_hidden()
                    .child(inner)
                    .into_any_element()
            }};
        }
        match (with_layout_id, debug_selector) {
            (Some(id), Some(sel)) => {
                finish!(div().id(id).debug_selector(move || sel.into()))
            }
            (Some(id), None) => finish!(div().id(id)),
            (None, Some(sel)) => finish!(div().debug_selector(move || sel.into())),
            (None, None) => finish!(div()),
        }
    }

    /// 正文区欢迎页：自绘插图 + 主/副文案 + 提示行 + 「打开文件…」+「新建文件」两个按钮。
    /// 只在「零选项卡」时被 `render_body` 调用（新模型：只要有选项卡——哪怕空白未命名
    /// 文档——就渲染编辑区，绝不叠欢迎页）。「打开文件…」直接触发系统打开对话框
    /// （`open_dialog`）；「新建文件」/ 根元素**双击**都走 `new_tab`——新建即空未命名
    /// 选项卡、编辑区聚焦、可立即键入（用户痛点「多一步双击」由双击/按钮消解，不
    /// 再引入第二层「进入已存在空 tab」的旧语义）。按钮外包同名 div 并挂 `debug_selector`
    /// 供测试定位（`debug_bounds` 只收录 debug_selector，`Button` 内部 id 不可见）。
    /// 双击回调经 `Entity::update` 回写（`empty_state_hero` 是统计组件、无视图上下文，
    /// 不能用 `cx.listener`）；不 `stop_propagation`，与编辑区单击/按钮点击互不干扰。
    pub(super) fn render_empty_state(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let open_btn = div()
            .id("empty-open-btn")
            .debug_selector(|| "empty-open-btn".into())
            .flex()
            .child(
                ui::button::Button::new("empty-open-btn-action")
                    .label("打开文件…")
                    .h(px(32.))
                    .px_4()
                    .rounded_md()
                    .text_size(px(13.))
                    .on_click(
                        cx.listener(|this: &mut EditorApp, _: &gpui::ClickEvent, window, cx| {
                            this.open_dialog(window, cx);
                        }),
                    ),
            )
            .into_any_element();
        let new_btn = div()
            .id("empty-new-btn")
            .debug_selector(|| "empty-new-btn".into())
            .flex()
            .child(
                ui::button::Button::new("empty-new-btn-action")
                    .label("新建文件")
                    .h(px(32.))
                    .px_4()
                    .rounded_md()
                    .text_size(px(13.))
                    .on_click(
                        cx.listener(|this: &mut EditorApp, _: &gpui::ClickEvent, window, cx| {
                            this.new_tab(window, cx);
                        }),
                    ),
            )
            .into_any_element();
        // 两个按钮并排：保留「打开文件…」原位置，右侧新增「新建文件」。
        let actions = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(open_btn)
            .child(new_btn)
            .into_any_element();
        // 双击回调捕获实体句柄：事件在渲染之后的输入分发阶段才触发，经 `Entity::update`
        // 回写（不能用 `cx.listener`——`empty_state_hero` 是统计组件、无视图上下文）。
        let app = cx.entity();
        ui::empty_state::empty_state_hero(
            "打开文件开始阅读",
            "按 Ctrl+O 打开文件 · Ctrl+P 打开文件快选",
            Some(actions),
            Some("双击空白处新建文件"),
            Some(Box::new(
                move |ev: &gpui::MouseDownEvent, window: &mut gpui::Window, cx: &mut gpui::App| {
                    if ev.click_count >= 2 {
                        // 双击欢迎页 = 新建文件选项卡（`new_tab` 内部已 `make_tab`，
                        // 并把焦点放到新编辑区，开箱即输入）。
                        app.update(cx, |this, cx| this.new_tab(window, cx));
                    }
                },
            )),
            cx,
        )
    }

    /// 外壳子元素：锁定图片/视频视图时只放「媒体铺满窗口」（藏菜单栏/选项卡栏/侧边栏/
    /// 状态栏），否则齐全；正常态为「标题栏 + [侧栏 | 右列(选项卡栏在上、编辑区在下)] +
    /// 状态栏」；始终带通知层与右键菜单（媒体锁定 + 选项卡栏）Overlay。
    pub(super) fn render_shell_children(
        &mut self,
        body: gpui::AnyElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let media_locked = self.active_media_locked(cx);
        let mut out: Vec<gpui::AnyElement> = Vec::new();
        if media_locked {
            // 锁定：正文（图片/视频查看器）直接铺满窗口，其余全藏 —— 媒体经重排后自动放大填满
            out.push(body);
        } else {
            out.push(self.render_title_bar(window, cx).into_any_element());
            // 侧边栏与【选项卡栏 + 编辑区】并列：render_body_sidebar 组装整行
            // [侧栏(紧贴标题栏下) + 分隔带 | 右列(选项卡栏在上、编辑区在下)]。
            out.push(self.render_body_sidebar(body, window, cx).into_any_element());
            out.push(self.render_status_bar(window, cx).into_any_element());
        }
        // 右键菜单 Overlay（媒体锁定 + PDF 页操作 + 选项卡栏 + 文件树）恒在最上层（通知层提升到外层根最后绘制，见 render_root）
        out.push(self.render_media_lock_menu(window, cx).into_any_element());
        out.push(self.render_pdf_menu(window, cx).into_any_element());
        out.push(self.render_tab_bar_menu(window, cx).into_any_element());
        out.push(self.render_file_tree_menu(window, cx).into_any_element());
        out.push(self.render_recent_panel(window, cx).into_any_element());
        out.push(self.render_file_tree_prompt(window, cx).into_any_element());
        out
    }

    /// 根视图：顶部栏 + 选项卡栏 + 内容区 + 状态栏 + 通知叠层，并挂载「拖入文件打开 /
    /// 多窗口选项卡拖拽 / 12 个快捷键动作」监听（一次组装完，render 保持薄分发）。
    pub(super) fn render_root(
        &mut self,
        body: gpui::AnyElement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .relative()
            .size_full()
            .track_focus(&self.focus_handle(cx))
            // 从资源管理器拖入文件到窗口任意处 → 打开每个文件（目录跳过）
            .can_drop(|drag: &dyn Any, _w, _c| drag.downcast_ref::<ExternalPaths>().is_some())
            .on_drop(cx.listener(
                |this: &mut EditorApp, paths: &ExternalPaths, window, cx| {
                    this.open_dropped_paths(paths, window, cx);
                },
            ))
            // 多窗口：选项卡拖拽——「松开才建窗」预位模型（旧 tear-off「越界即开窗」
            // 已移除，见 tab_drag.rs 模块注释）：
            // 拖拽全程（窗内/窗外）由 update_drag_ghost 维护同一个跟手幽灵窗：拖拽开始
            // 即建、全程跟随鼠标（与窗内 gpui 拖拽 picture 同位对齐，窗口边缘无缝无卡顿）；
            // 光标越出边缘仅置 drag_outside_armed 预位（不建真窗口）。
            // 真文件窗口只在 on_mouse_up_out 松开时建立（release_tab_drag_outside）：
            // 主窗口 → 拆新窗；独立窗口 → 还原回主窗口。
            // 栏内排序：光标停留在选项卡行内 → 记插入位（渲染白色指示线，窗内松开时
            // reorder_tab 落地）；拖出窗口 / 不在行内 → 复位，仍原位保留或拆窗。
            .on_drag_move(cx.listener(
                |this: &mut EditorApp, ev: &DragMoveEvent<TabDragData>, window, cx| {
                    let idx = ev.drag(cx).idx; // 拷贝下标，避免持有对 cx 的借用
                    // 已拔出（松开后源窗口不再有该选项卡）→ 忽略后续移动
                    if idx >= this.tabs.len() {
                        return;
                    }
                    // 真实选项卡 laid-out 尺寸（TabBoundsRecorder 每帧写入），供预览定宽。
                    let drag_size = ev.drag(cx).size.clone();
                    this.tab_dragging = Some(idx);
                    let pos = ev.event.position;
                    let inside = gpui::Bounds::new(gpui::Point::default(), window.bounds().size)
                        .contains(&pos);
                    // 越界 = 进入「松开才建窗」预位；回到窗内即复位。真窗口只在松开时建。
                    // 幽灵窗显示：光标越出窗口、或窗内拖拽 picture 将要被窗口边缘遮断
                    // （含 4px 余量，见 tab_drag::drag_picture_about_to_clip）——贴边即提前
                    // 切同位置同尺寸完整版；远离边缘时窗内无浮窗、无蓝块（幽灵窗隐藏）。
                    this.drag_outside_armed = !inside;
                    // 栏内排序插入位：以**被拖选项卡 picture 盒的边缘**是否进入选项卡行为准
                    // （不再要求鼠标光标在行内）——盒上/下任一边碰到行即激活白线；插入下标
                    // 按光标 x 判定。拖出窗口 / 盒未入行 → None（不排序 / 拆窗）。
                    let grab = this.tab_drag_grab.unwrap_or(gpui::Point::default());
                    let drag_bounds = gpui::Bounds::new(pos - grab, drag_size.get());
                    this.tab_reorder_target = if inside && this.tab_drag_over_row(drag_bounds) {
                        this.tab_insert_index(pos.x)
                    } else {
                        None
                    };
                    this.update_drag_ghost(idx, drag_size, pos, !inside, window, cx);
                    cx.notify();
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this: &mut EditorApp, _ev, _window, cx| {
                    // 窗口内松开：若本次拖拽落在选项卡行内的插入位 → 落地排序；
                    // 无插入位 = 原位保留。无论与否都撤预位、销毁幽灵窗。
                    if let Some(from) = this.tab_dragging {
                        if let Some(target) = this.tab_reorder_target.take() {
                            this.reorder_tab(from, target, cx);
                        }
                        this.drag_outside_armed = false;
                        this.destroy_drag_ghost(cx);
                    }
                    this.tab_dragging = None;
                    this.tab_reorder_target = None;
                    cx.notify();
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this: &mut EditorApp, ev: &gpui::MouseUpEvent, window, cx| {
                    // 窗口外松开 = 拆窗/还原路径，插入位不落地。
                    this.tab_reorder_target = None;
                    this.release_tab_drag_outside(window, ev.position, cx);
                }),
            )
            // 图片/视频选项卡：右键 → 「锁定/解锁」干净查看/播放菜单（气泡到根统一处理）
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(Self::on_media_right_click),
            )
            .on_action(cx.listener(|this: &mut EditorApp, _: &Save, window, cx| {
                this.save(window, cx)
            }))
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &PasteNormalized, window, cx| {
                    this.paste_normalized(window, cx)
                }),
            )
            .on_action(cx.listener(|this: &mut EditorApp, _: &SaveAs, window, cx| {
                this.save_as(window, cx)
            }))
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &NewFile, window, cx| {
                    this.new_tab(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &OpenFile, window, cx| {
                    this.open_dialog(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &OpenFolder, window, cx| {
                    this.open_folder_dialog(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &CloseTab, window, cx| {
                    this.close_tab(this.active_tab, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &TogglePreview, window, cx| {
                    let idx = this.active_tab;
                    this.toggle_preview_for_tab(idx, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &IncreaseFontSize, _w, cx| {
                    this.change_font_size(1.0, cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &DecreaseFontSize, _w, cx| {
                    this.change_font_size(-1.0, cx)
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &ResetFontSize, _w, cx| {
                    this.reset_font_size(cx);
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &FollowSystemTheme, _w, cx| {
                    this.follow_system_theme(cx);
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &ToggleWordWrap, window, cx| {
                    this.toggle_wrap(window, cx);
                }),
            )
            // P2-8 命令面板：Ctrl+Shift+P 命令模式 / Ctrl+P 文件快选（动作经 `main.rs`
            // 全局键位绑定 → 分发到根元素这两条 on_action）。
            .on_action(
                cx.listener(
                    |this: &mut EditorApp, _: &ToggleCommandPalette, window, cx| {
                        this.toggle_command_palette(window, cx);
                    },
                ),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &QuickOpenFile, window, cx| {
                    this.quick_open_file(window, cx);
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &GoToLine, window, cx| {
                    this.goto_line_in_active_tab(window, cx);
                }),
            )
            // 编辑区内查找：Ctrl+F 打开查找条；F3 / Shift+F3 复用上次查询上一个/下一个
            // （分发到活动文本选项卡的 CodeEditor；非文本选项卡 toast「不支持查找」不崩）。
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &Find, window, cx| {
                    this.find_in_active_tab(window, cx);
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &FindNext, window, cx| {
                    this.find_step_in_active_tab(window, cx, true);
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &FindPrev, window, cx| {
                    this.find_step_in_active_tab(window, cx, false);
                }),
            )
            // 音乐播放器：Ctrl+M 切换整窗音乐模式（同 music 模式壳 div 的 on_action 并存，
            // 见 `render` 顶部音乐模式分支——任一命中都走 `toggle_music_player`）。
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &ToggleMusicPlayer, window, cx| {
                    this.toggle_music_player(window, cx)
                }),
            )
            .child(self.render_material_layer(window))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .size_full()
                    .children(self.render_shell_children(body, window, cx)),
            )
            // 通知层提升为外层根的最后子元素（与材质层同级、画在最上）：作为 shell
            // flex 内部容器的 absolute 子元素在真机合成路径一直不被绘制（历史遗留 bug）——
            // 作为外层 relative 的直子（如材质层）才正常渲染。
            .child(self.render_notifications(window, cx))
            // 命令面板浮层（P2-8）：同样作为外层 relative 的直子挂载（合成层纪律同上）。
            .child(self.render_palette(window, cx))
    }

    /// 「打开最近」大面板浮层挂点：宿主持有面板实体时以根级直子渲染（面板自身包
    /// `deferred` 全窗 occlude 居中卡）；关闭后为空 → 返回空元素（同 [`Self::render_palette`]）。
    pub(super) fn render_recent_panel(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match self.recent_panel.clone() {
            Some(p) => p.into_any_element(),
            None => div().into_any_element(),
        }
    }

    /// 命令面板浮层挂点：宿主持有面板实体时以根级直子渲染（面板自身包 `deferred`）；
    /// 关闭后 `palette` 为空 → 返回空元素（不再有持续逐帧重绘）。
    pub(super) fn render_palette(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match self.palette.clone() {
            Some(p) => p.into_any_element(),
            None => div().into_any_element(),
        }
    }

    /// ToggleMusicPlayer 动作入口（文档模式根元素与音乐模式壳 div 共用同一 listener 转发）。
    fn toggle_music_player_on_action(
        &mut self,
        _: &ToggleMusicPlayer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_music_player(window, cx);
    }
}
