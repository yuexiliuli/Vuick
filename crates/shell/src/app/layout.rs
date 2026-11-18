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
        if self.model.borrow().music_mode {
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
            self.model.borrow_mut().music_mode = false;
        }
        // WY_TRACE 归因：操作（按键/滚动）起点 → 本帧 render 开始的等待，单独记账
        // 供「每键真实延迟」拆账——多为 60Hz vsync/帧调度，不是 CPU 段。
        if let Some(d) = crate::prof::op_elapsed() {
            crate::prof::add_absolute("app.frame_delay", d);
        }
        let _t = crate::prof::begin();
        // 拖分割条帧级合并（B4）：`on_mouse_move` 只记最新目标比例，这里本帧开始统一
        // 落地到 `split_ratio`（在 `render_body` 读取它之前，当帧即生效）。
        if let Some(r) = self.panes.pending_split_ratio.take() {
            self.panes.split_ratio = r;
        }
        // 编辑区⇄预览滚动同步：动了的一侧驱动另一侧到对应块（仅 MD 分屏、非 live2）。
        self.sync_editor_preview_scroll(cx);
        // Fix B：md 阅读进度恢复窗口的逐帧推进——预览顶块收敛到存档块即清位归还正常
        // 双向同步；预览就绪后超 [`MD_RESTORE_WAIT_FRAMES`] 帧仍未收敛也清位（防锁死）。
        // 放在 sync 之后执行：本帧 sync 仍见守卫开启（预览即便刚就绪也不会把编辑区拉回），
        // 清位后下一帧起恢复正常的「预览主导→回拉编辑区」语义。
        self.advance_md_restore_window(cx);
        // live（WYSIWYG）可见面阅读进度恢复的逐帧推进（`advance_md_restore_window` 之后）：
        // live2 装配完成才一次 `set_scroll_target` 钉到存档行并清位——不逐帧重钉，
        // live2 加载即渲染，WyEditor prepaint 自钉收敛；目标超行数等被
        // `advance_pending_scroll` 丢弃即为不可达、保持顶部（与 md_wy 恢复同口径）。
        self.advance_live2_restore(cx);
        // 阅读进度诊断：apply 命中 markdown 后 2 帧，上报右栏预览顶源码行（验证预览
        // 是否随编辑区恢复收敛——真机「重开停在顶部」候选 b 的证据通道）。
        if let Some(key) = shell_core::rp_diag::preview_pending_due() {
            let wy = self
                .model
                .borrow()
                .tabs
                .get(self.model.borrow().active_tab)
                .and_then(|t| t.payload.md_wy.clone());
            let app: &gpui::App = &**cx;
            match wy {
                Some(w) => {
                    let top = w.read(app).top_source_line();
                    shell_core::rp_diag::diag(
                        "apply_preview",
                        &format!("key={key} md_wy_top_source_line={top:?}"),
                    );
                }
                None => {
                    shell_core::rp_diag::diag("apply_preview", &format!("key={key} md_wy=none"));
                }
            }
        }
        // 阅读进度防抖落盘：活动选项卡位置变化 → 脏哨兵比对 + 重启防抖（滚停才写盘）。
        self.check_reading_progress(cx);
        // 实时预览编辑（WYSIWYG）本身即「所见即所得」，右侧 Markdown 预览冗余 →
        // 编辑区占满整块（不显示分割线与预览区），退出实时模式后恢复原分屏。
        let body = self.render_body(window, cx);

        let root = self.render_root(body, window, cx);
        crate::prof::end("wy.app.render", _t);
        root.into_any_element()
    }
}

impl EditorApp {
    /// Fix B：md 阅读进度恢复窗口的逐帧推进（每渲染帧调用）：守卫开启期
    /// ① 把右栏预览**重钉**到存档块（Fix A 的 `set_scroll_target` 在大文档两段式空壳期会被
    /// `advance_pending_scroll` 丢弃——空文档 `scroll_to_line_impl` 返 NAN → 清 scroll_target；
    /// 预览就绪后这里逐帧重钉直至收敛，蕴收藏档块不受「装配期读到顶部块」牵制）；
    /// ② 预览顶块已收敛到存档块（`top_source_line()` == `block_start_of_line(目标)`，
    /// 与 [`shell_preview::scroll`] 同口径块对齐）→ 清位；③ 预览就绪后计数超
    /// [`shell_core::model::MD_RESTORE_WAIT_FRAMES`] → 清位（防目标不可达 / 预览异常永久
    /// 锁死「预览→编辑区」驱动）。清位即归还编辑区⇄预览正常双向同步（含预览主导回拉）。
    fn advance_md_restore_window(&mut self, cx: &mut Context<Self>) {
        let app: &gpui::App = &**cx;
        let active = self.model.borrow().active_tab;
        let Some((target, n)) = self
            .model
            .borrow()
            .tabs
            .get(active)
            .and_then(|t| t.payload.md_restore_pending)
        else {
            return; // 无恢复窗口在途（绝大多数帧）
        };
        // NITS-③：文件 key（诊断行跨文件归因；仅恢复窗口期取，平时不触发）。
        let file_key = self
            .model
            .borrow()
            .tabs
            .get(active)
            .and_then(|t| t.payload.file_path.as_ref())
            .map(|p| session_store::reading_progress::normalize_key(p.as_path()))
            .unwrap_or_default();
        // 预览尚未就绪（空壳装配 / 元素未渲染）→ 只保持守卫，不计数（ready 才起算超时）。
        let Some(wy) = ({
            let m = self.model.borrow();
            let Some(tab) = m.tabs.get(active) else {
                return;
            };
            tab.payload.md_wy.clone()
        }) else {
            // NITS-①：纯 txt / 无预览（`md_wy` 恒 None）时守卫无物可守 → 直接清位归还
            // （防 `md_restore_pending` 悬空 Some 永久占用）。既有行为不变：txt 恢复只滚
            // 编辑区、不建预览，本就无预览可守。
            self.model.borrow_mut().tabs[active].payload.md_restore_pending = None;
            return;
        };
        let w = wy.read(app);
        let (top, target_block) = (w.top_source_line(), w.block_start_of_line(target));
        let Some(top) = top else {
            return;
        };
        if top == target_block {
            self.model.borrow_mut().tabs[active].payload.md_restore_pending = None;
            shell_core::rp_diag::diag(
                "md_restore",
                &format!("key={file_key} converged top={top} target_block={target_block}"),
            );
            return;
        }
        // 预览就绪但未收敛：**逐帧重钉到存档块**（拖住预览、不让它按装配初值停在顶部块；
        // 幂等——目标已到即下一帧进收敛分支清位）。此重钉经 `set_scroll_target` 写 Cell，
        // 不写宿主偏移，仍由 WyEditor prepaint 自钉，与渲染期布局复位解耦。
        wy.read(app).set_scroll_target(target);
        if n + 1 >= shell_core::model::MD_RESTORE_WAIT_FRAMES {
            self.model.borrow_mut().tabs[active].payload.md_restore_pending = None;
            shell_core::rp_diag::diag(
                "md_restore",
                &format!("key={file_key} timeout target={target} top={top} n={n}"),
            );
            return;
        }
        self.model.borrow_mut().tabs[active].payload.md_restore_pending = Some((target, n + 1));
    }

    /// live（WYSIWYG）态可见面阅读进度恢复的逐帧推进：`apply_reading_progress` 命中
    /// Markdown 时把存档源码行记入 [`shell_core::model::TabPayload::live2_restore`]；此处
    /// 逐帧等在途 live2（大文档异步 `install_open` 装配 / 空壳期）**装配完成**（`is_loaded()`）
    /// 后对其 `set_scroll_target(top)` 一次并清位。前置在 `render_body` 之前 → 本帧 WyEditor
    /// 元素 prepaint 的 `advance_pending_scroll` 即拾起钉定，无需宿主反复驱动。
    /// 非 live / live2 未就绪 / 无待恢复目标 → no-op（绝大多数帧）。
    fn advance_live2_restore(&mut self, cx: &mut Context<Self>) {
        let app: &gpui::App = &**cx;
        let active = self.model.borrow().active_tab;
        let live2 = {
            let m = self.model.borrow();
            let Some(tab) = m.tabs.get(active) else {
                return;
            };
            if !tab.live_edit || tab.payload.live2_restore.is_none() {
                return;
            }
            match tab.live2.clone() {
                Some(l2) => l2,
                None => return,
            }
        };
        if !live2.read(app).is_loaded() {
            return; // 大文档两段式空壳装配在途：等 install_open 完成再钉
        }
        let Some(top) = self.model.borrow_mut().tabs[active].payload.live2_restore.take() else {
            return;
        };
        live2.read(app).set_scroll_target(top);
        shell_core::rp_diag::diag("apply_live2", &format!("top={top} loaded=1"));
    }

    /// 内容区三态：有选项卡 → 实时编辑占满 / 预览分屏（EPUB 等全屏预览）/ 纯编辑占满；
    /// 零选项卡 → 欢迎页整屏占满正文区（不渲染编辑区）。
    pub(super) fn render_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        // 新模型：欢迎页只在「零选项卡」时呈现。只要有 ≥1 个选项卡——哪怕新建出的
        // 空白未命名文档——就直接渲染编辑区（可键入），绝不叠加欢迎页（「未命名空文档」
        // 也视为已打开，不再算空态）。
        if self.model.borrow().tabs.is_empty() {
            return Self::pane_wrap(
                Some("app-empty-body"),
                Some("app-empty-body"),
                true,
                self.render_empty_state(cx),
            );
        }
        let live_edit = self
            .model.borrow().tabs
            .get(self.model.borrow().active_tab)
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
            let ratio = self.panes.split_ratio;
            // EPUB 等全屏预览：预览区占满整个内容区，不显示编辑区与分割线。
            let fullscreen = self
                .model.borrow().tabs
                .get(self.model.borrow().active_tab)
                .map(|t| t.payload.preview_fullscreen)
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
                            if let Some((start_x, start_ratio)) = this.panes.split_drag {
                                let total = window.bounds().size.width.as_f32();
                                if total > 0. {
                                    let x = ev.position.x.as_f32();
                                    let ratio = (start_ratio + (x - start_x) / total)
                                        .clamp(SPLIT_RATIO_MIN, SPLIT_RATIO_MAX);
                                    if (ratio - this.panes.split_ratio).abs() > 1e-5 {
                                        this.panes.pending_split_ratio = Some(ratio);
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
                                this.panes.pending_split_ratio = None;
                                this.panes.split_drag = None;
                                cx.notify();
                            },
                        ),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(
                            |this: &mut EditorApp, _: &gpui::MouseUpEvent, _w, cx| {
                                this.panes.pending_split_ratio = None;
                                this.panes.split_drag = None;
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
    /// 防坍缩样板收敛为一处（`flex_1` 高度坍缩会毁滚动/命中），外加
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
        // 视频选项卡激活时隐藏底部状态栏与左侧侧边栏（**渲染级守卫**、不写
        // `sidebar.open` 等状态；切到非视频 tab 即恢复）。与 media_locked 互不重叠：
        // locked 是全隐（连标题栏/选项卡栏都藏），此处仅藏状态栏与侧边栏。
        let hide_video_chrome = self.active_media_is_video();
        let mut out: Vec<gpui::AnyElement> = Vec::new();
        if media_locked {
            // 锁定：正文（图片/视频查看器）直接铺满窗口，其余全藏 —— 媒体经重排后自动放大填满
            out.push(body);
        } else {
            out.push(self.render_title_bar(window, cx).into_any_element());
            // 侧边栏与【选项卡栏 + 编辑区】并列：render_body_sidebar 组装整行
            // [侧栏(紧贴标题栏下) + 分隔带 | 右列(选项卡栏在上、编辑区在下)]。
            out.push(
                self.render_body_sidebar(body, hide_video_chrome, window, cx)
                    .into_any_element(),
            );
            if !hide_video_chrome {
                out.push(self.render_status_bar(window, cx).into_any_element());
            }
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
                    // 真实选项卡 laid-out 尺寸（TabBoundsRecorder 每帧写入），供预览定宽。
                    let drag_size = ev.drag(cx).size.clone();
                    let pos = ev.event.position;
                    let grab = this
                        .model
                        .borrow()
                        .tab_drag_grab
                        .unwrap_or(gpui::Point::default());
                    // 拖拽状态机单步迁移（域 `shell_tabs::drag`）：已被拔出（idx 越界）→
                    // None 忽略后续移动；否则写 `tab_dragging` / `drag_outside_armed` /
                    // `tab_reorder_target`，返回本次移动 `outside`。栏内排序命中 / 幽灵窗
                    // 布局参数全部在域内纯计算（宿主 `tab_bounds` 快照以参数传入）。
                    let Some(step) = shell_tabs::drag::tab_drag_move_step(
                        &this.model,
                        &this.tab_bounds,
                        idx,
                        pos,
                        grab,
                        drag_size.get(),
                        window.bounds(),
                    ) else {
                        return;
                    };
                    // 越界 = 进入「松开才建窗」预位（step.outside）；回到窗内即复位。
                    // 真窗口只在松开时建。幽灵窗显示：光标越出窗口、或窗内拖拽 picture
                    // 将要被窗口边缘遮断（含 4px 余量）——贴边即提前切同位置同尺寸完整版；
                    // 远离边缘时窗内无浮窗、无蓝块（幽灵窗隐藏）。
                    this.update_drag_ghost(idx, drag_size, pos, step.outside, window, cx);
                    cx.notify();
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this: &mut EditorApp, _ev, _window, cx| {
                    // 窗口内松开：若本次拖拽落在选项卡行内的插入位 → 落地排序；
                    // 无插入位 = 原位保留。无论与否都撤预位、销毁幽灵窗。
                    // 先提纯 owned 再分支——避免 `if let` 掮获的 Ref/RefMut 临时
                    // 存活整个分支体（运行期双借 panic / E0502 拦 &mut self 方法）。
                    let dragging = this.model.borrow().tab_dragging;
                    if let Some(from) = dragging {
                        let reorder_target = this.model.borrow_mut().tab_reorder_target.take();
                        if let Some(target) = reorder_target {
                            this.reorder_tab(from, target, cx);
                        }
                        this.model.borrow_mut().drag_outside_armed = false;
                        this.destroy_drag_ghost(cx);
                    }
                    this.model.borrow_mut().tab_dragging = None;
                    this.model.borrow_mut().tab_reorder_target = None;
                    cx.notify();
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this: &mut EditorApp, ev: &gpui::MouseUpEvent, window, cx| {
                    // 窗口外松开 = 拆窗/还原路径，插入位不落地。
                    this.model.borrow_mut().tab_reorder_target = None;
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
                    let active_idx = this.model.borrow().active_tab;
                    this.close_tab(active_idx, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this: &mut EditorApp, _: &TogglePreview, window, cx| {
                    let idx = this.model.borrow().active_tab;
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
            // 命令面板浮层：同样作为外层 relative 的直子挂载（合成层约束同上）。
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
