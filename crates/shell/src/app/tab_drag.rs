//! 跨窗口选项卡搬迁与拖拽：从本窗口「拔出」/收纳选项卡、**松开**拖出窗口拆新窗口 /
//! 还原回主窗口（多窗口 split/tear-off），以及全局 `EditorAppRegistry` 的读写。
//!
//! 拖拽出窗体验模型（2026-10 重构）：旧「光标一越界即 open_window（tear-off）」改作
//! 「松开才建窗」——越界只置 `drag_outside_armed` 预位 + 用置顶透明幽灵窗让被拖选项卡
//! 在窗外仍跟手可见（gpui 拖拽幽灵只画在源窗口帧缓冲内，窗外被裁剪），真文件窗口在
//! `release_tab_drag_outside`（`on_mouse_up_out`）时才建立。
//!
//! **纯逻辑在 `shell_tabs::drag`**（命中计算 `tab_insert_index` /
//! `tab_drag_over_row`、幽灵窗布局参数 `tab_drag_ghost_layout`、预告尺寸兜底
//! `tab_preview_size`、边缘遮断判定 `drag_picture_about_to_clip`、拖拽状态机单步迁移
//! `tab_drag_move_step`、拆分决策 `tab_drag_release_kind`）。本模块只剩宿主编排 + win32：
//! 拖拽动作编排（take/adopt/split/restore/release）、幽灵窗实体创建/销毁与
//! `SetWindowPos`/DWM 原生调用（经 `TabDragHost` 宿主服务面路由，`app/host_impl.rs`）。

use super::*;

impl EditorApp {
    /// 从本窗口「拔出」一个选项卡：移除选项卡（**不**释放 PDF 纹理、不重置编辑器——
    /// 实体与元数据整体移到目标窗口），下标与激活态自动校正。
    /// 整结构 move：一次解构 `EditorTab` 搬到 `DetachedTab`（`..` 丢弃源窗口专属的
    /// live 态 / 订阅），不再逐字段手抄。
    /// payload 动作体在 `AppModel::take_tab_payload`。glue 转发。
    /// `#[doc(hidden)] pub`：`tests/pdf.rs` 多窗口换位回归（真实拔出/收纳路径，§8.6 外提清单）。
    #[doc(hidden)]
    pub fn take_tab_payload(&mut self, idx: usize) -> Option<DetachedTab> {
        self.model.borrow_mut().take_tab_payload(idx)
    }

    /// 移出选项卡前的统一收尾（live2 守卫）：实时预览编辑实体不随选项卡搬迁——
    /// 先 `commit_live` 把 live2 写回编辑区再取载荷。拖拽移出路径（`release_tab_drag_outside`
    /// 窗口外松开）此前各自手抄「`live_edit` → `commit_live`」守卫，收敛为此 helper；
    /// `take_tab_payload` 本身保持无副作用（测试直调、不隐含 commit）。
    pub(super) fn commit_live_and_take_tab_payload(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<DetachedTab> {
        if self.model.borrow().tabs[idx].live_edit {
            self.commit_live(idx, window, cx);
        }
        self.take_tab_payload(idx)
    }

    /// 把一个「拔出」的选项卡收纳进本窗口：共享编辑器/预览实体、重建编辑订阅、
    /// 置为激活并同步预览/PDF 主题与软换行偏好。
    /// `window=None` 用于跨窗口还原（此时不抢焦点，由调用方在目标窗口聚焦）；
    /// `Some(window)` 用于拆出新窗口（直接聚焦新窗口的编辑区）。
    /// 整结构 move：`EditorTab::from_detached` 把载荷（23 个搬迁字段）与目标窗口重建
    /// 的 live 态/订阅收束为完整 `EditorTab`，不再逐字段手抄。
    /// `#[doc(hidden)] pub`：`tests/pdf.rs` 多窗口换位回归（§8.6 外提清单）。
    #[doc(hidden)]
    pub fn adopt_tab(
        &mut self,
        payload: DetachedTab,
        mut window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        // `_edit_sub` 先在目标窗口重建（编辑器实体自载荷借出），再整包 move 回 EditorTab。
        let edit_sub = self.subscribe_editor(&payload.editor, cx);
        let is_md = payload
            .file_path
            .as_ref()
            .map(|p| Self::is_markdown_path(p))
            .unwrap_or(false);
        let idx = self.model.borrow().tabs.len();
        self.model.borrow_mut().tabs.push(EditorTab::from_detached(
            payload,
            edit_sub,
            false, // 预览编辑（WYSIWYG）不随选项卡搬迁（搬迁前已提交写回）
            None,  // live2
            false, // live2_dirty
            // live_vscroll 每次重建新句柄（同 take 前每选项卡独立）
            gpui::ScrollHandle::new(),
        ));
        self.model.borrow_mut().active_tab = idx;
        self.model.borrow_mut().search_scan = None;
        // 编辑区软换行与目标窗口偏好对齐（md 默认换行、预览开着强制换行）
        let wrap = is_md || self.model.borrow().wrap_enabled || self.model.borrow().tabs[idx].payload.preview_on;
        self.model.borrow().tabs[idx].payload.editor.update(cx, |ed, _cx| {
            ed.set_soft_wrap(wrap);
        });
        // 预览/PDF 主题背景刷新 + PDF 订阅重建
        let canvas_bg = self.window_bg();
        // P4-2：MD / EPUB 预览都走统一 WyEditor 只读宿主——实体随选项卡搬迁已带内容，
        // 无需再喂 MarkdownPreview（该后端已删除）。
        let reader = self.model.borrow().tabs[idx].payload.reader.clone();
        match reader {
            MediaReader::Pdf(pdf) => {
                // PDF 画布底色按当前偏好恢复（默认跟随主题；显式档保持冻结色）。
                self.apply_pdf_background(&pdf, cx);
                // PDF 订阅随选项卡重建（换位后订阅跟 tab 走；多 PDF 并存各自独立）
                self.subscribe_pdf(idx, &pdf, cx);
            }
            MediaReader::Image(img) => {
                img.update(cx, |view, _| view.set_background(canvas_bg));
            }
            #[cfg(feature = "video")]
            MediaReader::Video(vid) => {
                vid.update(cx, |view, _| view.set_background(canvas_bg));
            }
            MediaReader::None => {}
        }
        self.model.borrow_mut().pdf_search = None; // 重新推送检索查询
        if let Some(window) = window.as_mut() {
            self.activate_tab(idx, window, cx);
            self.model.borrow_mut().status = SharedString::from("已拆分为独立窗口");
            // 窗口标题跟随当前文件（拆出的新窗口直接以此命名）
            if let Some(p) = self.model.borrow().tabs[idx].payload.file_path.as_ref() {
                window.set_window_title(&p.to_string_lossy());
            } else {
                window.set_window_title("未命名");
            }
        } else {
            self.model.borrow_mut().status = SharedString::from("已还原到主窗口");
        }
        self.model.borrow_mut().status_ok = true;
        // 收纳后选项卡集合变化 → 立即持久化（detached 窗口由 persist 内部早退）。
        self.schedule_session_flush(cx, true);
        // 构造期（`new_detached` → `open_window` 同步首帧紧后）抑制多余 notify：
        // 入队效果在首帧之后才 flush → 白画一整帧冗余（具体见 `constructing` 字段注释）。
        if !self.constructing {
            cx.notify();
        }
    }

    /// 拖拽中的选项卡在**窗口外**松开：主窗口 → 拆成独立窗口；独立窗口 → 还原回主窗口。
    /// `window.remove_window()`（独立窗口还原后），标志窗口本帧结束后销毁。
    pub(crate) fn release_tab_drag_outside(
        &mut self,
        window: &mut Window,
        release_position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        // 窗口外松开：先销毁越界时创建的跟手幽灵窗，真文件窗口随后按落点建立/还原。
        self.destroy_drag_ghost(cx);
        self.model.borrow_mut().drag_outside_armed = false;
        // 先提纯 owned 再 let-else，避免 `tab_dragging.take()` 的 RefMut 临时
        // 存活整个 `let-else` 作用域（后续 borrow_mut 运行期双借 panic）。
        let tab_dragging = self.model.borrow_mut().tab_dragging.take();
        let Some(idx) = tab_dragging else {
            return;
        };
        if idx >= self.model.borrow().tabs.len() {
            return;
        }
        // 实时预览编辑模式下鼠标按住拖出：先把内容写回编辑区（实时实体不随选项卡搬）。
        let Some(payload) = self.commit_live_and_take_tab_payload(idx, window, cx) else {
            return;
        };
        if self.is_detached {
            // 独立文件窗口：把选项卡还给主窗口（找不到主窗口则自我转正为主窗口）
            self.restore_tab_to_primary(payload, cx);
            window.remove_window();
        } else {
            // 主窗口：把选项卡拆进新开的独立窗口（窗口出现在鼠标释放处）
            self.split_tab_to_new_window(payload, release_position, window, cx);
        }
    }

    /// 拖拽**开始**（`on_drag` 越过阈值那一拍）即预建幽灵窗：把 Windows 透明置顶 popup
    /// 的 OS 创建成本（首帧 DCOMP，真机可达几百 ms）**前移到拖拽起点**，首个
    /// `on_drag_move` 只做纯 `SetWindowPos` 定位 → 胶囊从拖拽一开始就跟手，不再「先
    /// 空几百 ms 才出现」。创建仍 `cx.defer` 到本周期 flush（handler 不阻塞）；此时
    /// `visible=false`（拖拽刚开始光标仍在窗内），初始位置用窗口左上角（在屏内，满足
    /// popup 落位约束；隐藏态位置无观感）。真实尺寸优先，首帧未记录走兜底估算。
    pub(super) fn prime_drag_ghost(
        &mut self,
        idx: usize,
        name: SharedString,
        dirty: bool,
        size_cell: Rc<Cell<Size<Pixels>>>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.drag_ghost.is_some() || self.drag_ghost_pending {
            return;
        }
        // on_drag 即拖拽真实开始：登记拖拽下标（否则 `ensure_drag_ghost` 的 defer 回调按
        // `tab_dragging.is_none()` 判定「拖拽已结束」而跳过创建，前移落空）。on_drag_move
        // 会重复写同值，幂等。
        self.model.borrow_mut().tab_dragging = Some(idx);
        let s = size_cell.get();
        let size = if s.width > px(0.) && s.height > px(0.) {
            s
        } else {
            shell_tabs::drag::tab_preview_size(&name, dirty, window)
        };
        self.ensure_drag_ghost(
            name,
            dirty,
            size,
            size_cell,
            window.bounds().origin,
            false,
            cx,
        );
    }

    /// 拖拽选项卡期间维护「幽灵胶囊」窗：本次拖拽**开始**（`on_drag`，见
    /// [`Self::prime_drag_ghost`]）已**隐藏创建**置顶透明小窗并全程复用；显示条件 = 光标越出窗口
    /// **或** 窗内拖拽 picture 将要被窗口边缘遮断（含 4px 余量，见
    /// [`shell_tabs::drag::drag_picture_about_to_clip`]）——picture 一旦贴边即提前切到同位置、
    /// 同尺寸的完整版幽灵窗（杜绝「一半被裁 → 越界才突然完整」）。因此：
    /// - 窗内 picture 远离边缘：只见原有 gpui 小胶囊，**没有浮窗/蓝块**（幽灵窗隐藏）；
    /// - 贴边 / 拖出拖入：只是「显示 ⇄ 隐藏 + 定位」（一次 `SetWindowPos`），无创建/销毁
    ///   卡顿、无抓取偏移跳变（窗内 picture 与幽灵窗都画在 `鼠标 - 抓取偏移`）；
    /// 真文件窗口仍然只在 [`Self::release_tab_drag_outside`] 松开时建立。拖拽结束
    /// （松开）由 [`Self::destroy_drag_ghost`] 销毁。
    pub(super) fn update_drag_ghost(
        &mut self,
        idx: usize,
        drag_size: Rc<Cell<Size<Pixels>>>,
        pos: Point<Pixels>,
        outside: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let model = self.model.borrow();
        let Some(tab) = model.tabs.get(idx) else {
            return;
        };
        let name: SharedString = tab
            .payload
            .file_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "未命名".to_string())
            .into();
        let dirty = tab.payload.dirty;
        drop(model);
        // 幽灵窗/预览尺寸 = 真实选项卡 laid-out 尺寸（`tabs::TabBoundsRecorder` 每帧写入
        // `drag_size`）；首帧尚无记录时回退到估算。同步给下面创建的窗口 bounds/min_size。
        let s = drag_size.get();
        let size = if s.width > px(0.) && s.height > px(0.) {
            s
        } else {
            shell_tabs::drag::tab_preview_size(&name, dirty, window)
        };
        let grab = self.model.borrow().tab_drag_grab.unwrap_or(Point::default());
        // 幽灵窗布局参数（域纯计算）：屏幕原点 = 源窗口左上角 + (拖拽点 − 抓取偏移)
        // （与 gpui 窗内绘制拖拽 picture 的位置完全一致）；显示条件 = 越出窗口 **或**
        // 窗内 picture 将要被窗口边缘遮断（含 4px 余量）——贴边即提前切同位置同尺寸的
        // 完整幽灵窗，杜绝「一半被裁 → 越界才突然完整」。记录决策供测试锁定。
        let layout = shell_tabs::drag::tab_drag_ghost_layout(
            window.bounds().origin,
            window.bounds().size,
            pos,
            grab,
            size,
            outside,
        );
        self.model.borrow_mut().drag_ghost_visible = layout.visible;
        // B：幽灵窗 open_window 延迟（内部 `cx.defer`）→ 首个 on_drag_move 的 dispatch
        // 不被 Windows 透明置顶 popup 创建 + 首帧 DCOMP 的 OS 成本阻塞；幽灵窗仍在拖拽
        // 开始后 1 帧内建成（defer 会跨过可见行构建 / SetWindowPos 前落下）。
        self.ensure_drag_ghost(
            name,
            dirty,
            size,
            drag_size,
            layout.screen,
            layout.visible,
            cx,
        );
        if let Some(gh) = self.drag_ghost {
            Self::place_drag_ghost(gh, layout.screen, layout.visible, cx);
        }
    }

    /// 懒创建 / 复用本次拖拽的幽灵窗（置顶透明无边框 PopUp，`focus:false` 不抢焦点、
    /// 不进任务栏），内容是与拖拽幽灵同款的半透明选项卡 [`TabDragGhost`]（持有
    /// `size_cell`，按真实 laid-out 尺寸渲染自身）。同一拖拽内只建一次、全程复用；
    /// 命名/脏点随新拖拽更新。
    /// **创建本身延迟（`cx.defer`）**：`open_window` 立建透明置顶 popup 的首帧 DCOMP
    /// 是 O(OS) 成本，阻塞首个 `on_drag_move` 会让胶囊几百 ms 不跟手。延迟到本周期
    /// effect flush 建（仍属「拖拽开始后 1 帧内」），首个移动事件先无阻返回；同周期内
    /// `drag_ghost_pending` 防重复排程；拖拽已结束（`tab_dragging=None`）不补建。
    /// 关键：**`show:true` + 起点 = 拖拽位置（在屏内）**。gpui_windows 在 `show:false` 时
    /// 不应用 `window_bounds`（窗口保持 CW_USEDEFAULT 巨大默认尺寸；window.rs:577-589），
    /// 而 `open_window` 创建时立即 draw 一帧（app.rs:1520）→ 巨大错误帧；之后 SetWindowPos
    /// 缩放只是裁剪它（即用户所见「更大/直角/文字消失/截断」）。`show:true` 让 placement
    /// 先落成正确尺寸再画首帧；起点在屏内保证 `check_given_bounds` 通过（**不可用屏外
    /// 原点**：会回落到默认大窗，display.rs:104-118）。窗口尺寸 = 真实选项卡尺寸 → 内容
    /// 不溢出、四角圆润。
    fn ensure_drag_ghost(
        &mut self,
        name: SharedString,
        dirty: bool,
        size: Size<Pixels>,
        size_cell: Rc<Cell<Size<Pixels>>>,
        origin: Point<Pixels>,
        visible: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(gh) = &self.drag_ghost {
            // 已有（防御：拖拽结束销毁失败的残留）→ 仅换名/脏点复用。
            let _ = gh.update(cx, |view, _window, cx| {
                if let Ok(ghost) = view.downcast::<TabDragGhost>() {
                    let _ = ghost.update(cx, |view, _cx| {
                        view.name = name;
                        view.dirty = dirty;
                    });
                }
            });
            return;
        }
        // 已延迟在建：同周期第 2 次 on_drag_move 直接跳过（不重复建第 2 个幽灵窗）。
        if self.drag_ghost_pending {
            return;
        }
        self.drag_ghost_pending = true;
        let weak = cx.weak_entity();
        let bounds = Bounds { origin, size };
        let titlebar = gpui::TitlebarOptions {
            title: None,
            appears_transparent: true,
            ..Default::default()
        };
        cx.defer(move |cx| {
            if let Some(this) = weak.upgrade() {
                this.update(cx, |this, cx| {
                    this.drag_ghost_pending = false;
                    // 拖拽已结束/取消（tab_dragging 已清空）或已另有幽灵窗 → 不再补建。
                    if this.model.borrow().tab_dragging.is_none() || this.drag_ghost.is_some() {
                        return;
                    }
                    let handle = cx.open_window(
                        WindowOptions {
                            window_bounds: Some(WindowBounds::Windowed(bounds)),
                            titlebar: Some(titlebar),
                            focus: false,
                            show: true,
                            kind: gpui::WindowKind::PopUp,
                            is_movable: false,
                            is_resizable: false,
                            is_minimizable: false,
                            window_background: gpui::WindowBackgroundAppearance::Transparent,
                            // 关键（2026-10-28）：幽灵窗**自身**已渲染完整跟手卡片（根视图
                            // `TabDragGhost ghost:true`），而 gpui 每个窗口 prepaint 都会把 App 级
                            // `active_drag` picture 画进自己帧缓冲 → 若不禁用，幽灵窗会把窗内拖拽 picture
                            // 再叠一层 = 用户真机所见「鼠标离窗瞬间屏上多出一张跟手选项卡、与原 tab 重叠」。
                            suppress_active_drag_picture: true,
                            ..Default::default()
                        },
                        |_new_window, cx| cx.new(|_| TabDragGhost {
                            name,
                            dirty,
                            size: size_cell,
                            ghost: true,
                        }),
                    );
                    if let Ok(wh) = handle {
                        let gh: AnyWindowHandle = wh.into();
                        this.drag_ghost = Some(gh.clone());
                        // 建完立刻剥掉 DWM 的圆角/过渡框线（见 strip 注释），并按创建时
                        // 落定的「是否可见 + 屏幕位置」定位——首帧不偏位、不闪。
                        Self::strip_ghost_dwm_frame(gh.clone(), cx);
                        Self::place_drag_ghost(gh, origin, visible, cx);
                    }
                });
            }
        });
    }

    /// 定位幽灵窗（**纯移动，不缩放**）。gpui 无「运行时移动窗口」API，用原生
    /// `SetWindowPos` 改原点 + 显/隐；**不带 w/h（SWP_NOSIZE）**——尺寸已在创建时按真实
    /// 选项卡落定，移动不改变内容尺寸，避免两套 scale/异步 viewport 造成的缩放裁剪/残像。
    /// 测试平台 rwh 报 NotSupported → 静默 no-op。
    fn place_drag_ghost(
        gh: AnyWindowHandle,
        screen: Point<Pixels>,
        visible: bool,
        cx: &mut Context<Self>,
    ) {
        let _ = gh.update(cx, |_view, ghost_window, _| {
            #[cfg(windows)]
            {
                use raw_window_handle::HasWindowHandle as _;
                if let Ok(handle) = ghost_window.window_handle()
                    && let Ok(raw_window_handle::RawWindowHandle::Win32(win32)) =
                        raw_window_handle::RawWindowHandle::try_from(handle)
                {
                    let scale = ghost_window.scale_factor();
                    let x = (screen.x.as_f32() * scale).round() as i32;
                    let y = (screen.y.as_f32() * scale).round() as i32;
                    let flags = SWP_NOSIZE
                        | SWP_NOZORDER
                        | SWP_NOACTIVATE
                        | if visible { SWP_SHOWWINDOW } else { SWP_HIDEWINDOW };
                    unsafe {
                        SetWindowPos(
                            win32.hwnd.get() as *mut core::ffi::c_void,
                            std::ptr::null_mut(),
                            x,
                            y,
                            0,
                            0,
                            flags,
                        );
                    }
                }
            }
        });
    }

    /// 剥掉幽灵窗的 Windows「四根黑色直线窗框」。根因（两子 agent 一致）：这张卡片的
    /// 内容层无边框（已去 border）、无 DWM 圆角、无 min-size 抬升，黑线来自**窗口外框
    /// 合成**——gpui_windows 把 `WindowKind::PopUp` 建成 `dwstyle=0` 的 **WS_OVERLAPPED
    /// 顶层窗**（window.rs:492-493），Win10/11 DWM 保留约 1px 非客户区框；该环在
    /// swapchain（只铺 client w×h）之外，被**实心黑类背景刷**（window.rs:1435）画成黑
    /// 线 = 「四根直线围住卡片」。
    /// 修复：改回真正的 `WS_POPUP`（无边框 → `GetWindowRect == GetClientRect`，非客户
    /// 黑环消失）并 `SWP_FRAMECHANGED` 通知 DWM 重建。**不动透明路径**（clear 仍是透明黑、
    /// DComp premultiplied 直通）。测试平台拿不到 HWND → 静默 no-op。
    fn strip_ghost_dwm_frame(gh: AnyWindowHandle, cx: &mut Context<Self>) {
        let _ = gh.update(cx, |_view, ghost_window, _| {
            #[cfg(windows)]
            {
                use raw_window_handle::HasWindowHandle as _;
                if let Ok(handle) = ghost_window.window_handle()
                    && let Ok(raw_window_handle::RawWindowHandle::Win32(win32)) =
                        raw_window_handle::RawWindowHandle::try_from(handle)
                {
                    let hwnd = win32.hwnd.get() as *mut core::ffi::c_void;
                    unsafe {
                        // DWM 圆角偏好 0（直角） + 关过渡帧（沿用上轮）。
                        let donotround: u32 = DWMWCP_DONOTROUND;
                        let no_transition: i32 = 1;
                        DwmSetWindowAttribute(
                            hwnd,
                            DWMWA_WINDOW_CORNER_PREFERENCE,
                            &donotround as *const u32 as *const core::ffi::c_void,
                            core::mem::size_of::<u32>() as u32,
                        );
                        DwmSetWindowAttribute(
                            hwnd,
                            DWMWA_TRANSITIONS_FORCEDISABLED,
                            &no_transition as *const i32 as *const core::ffi::c_void,
                            core::mem::size_of::<i32>() as u32,
                        );
                        // WS_OVERLAPPED(0x0) → WS_POPUP：无非客户区，黑色外环消失。
                        let old_style = GetWindowLongPtrW(hwnd, GWL_STYLE);
                        let new_style = (old_style | (WS_POPUP as isize))
                            & !(WS_CAPTION as isize | WS_THICKFRAME as isize | WS_SYSMENU as isize);
                        SetWindowLongPtrW(hwnd, GWL_STYLE, new_style);
                        SetWindowPos(
                            hwnd,
                            std::ptr::null_mut(),
                            0,
                            0,
                            0,
                            0,
                            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE
                                | SWP_NOZORDER | SWP_NOACTIVATE,
                        );
                        if std::env::var_os("VUICK_DIAG").is_some() {
                            let mut wr = Rect { left: 0, top: 0, right: 0, bottom: 0 };
                            let mut cr = Rect { left: 0, top: 0, right: 0, bottom: 0 };
                            GetWindowRect(hwnd, &mut wr);
                            GetClientRect(hwnd, &mut cr);
                            eprintln!(
                                "[GHOSTWND] window={}x{} client={}x{} off=({},{}) dpi={} dcomp_env={} style_after={:#x} bounds={:?} scale={}",
                                wr.right - wr.left,
                                wr.bottom - wr.top,
                                cr.right - cr.left,
                                cr.bottom - cr.top,
                                cr.left - wr.left,
                                cr.top - wr.top,
                                GetDpiForWindow(hwnd),
                                std::env::var("GPUI_DISABLE_DIRECT_COMPOSITION")
                                    .unwrap_or_default(),
                                new_style,
                                ghost_window.bounds(),
                                ghost_window.scale_factor(),
                            );
                        }
                    }
                }
            }
        });
    }

    /// 销毁跟随鼠标的幽灵窗口（回到窗内 / 松开 / 取消拖拽时）。幂等。
    pub(super) fn destroy_drag_ghost(&mut self, cx: &mut Context<Self>) {
        if let Some(gh) = self.drag_ghost.take() {
            let _ = gh.update(cx, |_view, w, _| w.remove_window());
        }
    }

    /// 把选项卡拆进一个新开的独立窗口：窗口左上角跟随鼠标释放位置（屏幕坐标 =
    /// 源窗口屏幕左上角 + 拖放点的窗口内坐标）；本窗口落下最后一个选项卡（`tabs`
    /// 变空）即回到零选项卡「欢迎页」——不再补空选项卡（与关闭最后一项语义一致）。
    pub(super) fn split_tab_to_new_window(
        &mut self,
        payload: DetachedTab,
        release_position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 主窗口剩下的选项卡接管焦点；零选项卡时 `activate_tab` 空返回（欢迎页）。
        let active_idx = self.model.borrow().active_tab;
        self.activate_tab(active_idx, window, cx);
        let src_bounds = window.bounds();
        let bounds = Bounds {
            origin: src_bounds.origin + release_position,
            size: src_bounds.size,
        };
        let titlebar_options = gpui::TitlebarOptions {
            title: Some("GPUI 编辑器".into()),
            appears_transparent: true,
            ..Default::default()
        };
        let _ = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(titlebar_options),
                ..Default::default()
            },
            |new_window, cx| {
                let view = cx.new(|cx| EditorApp::new_detached(new_window, cx, payload));
                cx.new(|cx| ui::root::Root::new(view, new_window, cx))
            },
        );
        cx.notify();
    }

    /// 把选项卡还原到主（非 detached）窗口；找不到主窗口时把本窗口转正为主窗口并原地收纳。
    pub(super) fn restore_tab_to_primary(&mut self, payload: DetachedTab, cx: &mut Context<Self>) {
        let registry = cx.global::<EditorAppRegistry>();
        let primary: Option<(gpui::Entity<EditorApp>, AnyWindowHandle)> = registry
            .windows
            .iter()
            .filter_map(|(weak, wh)| {
                let entity = weak.upgrade()?;
                let detached = entity.read_with(cx, |app, _| app.is_detached);
                (!detached).then_some((entity, *wh))
            })
            .next();
        let focus_handle = payload.editor.read(cx).focus_handle();
        match primary {
            Some((entity, wh)) => {
                // 把选项卡交给主窗口（数据收纳 + 不抢焦点），随后在主窗口里聚焦编辑区
                entity.update(cx, |app, cx| app.adopt_tab(payload, None, cx));
                let _ = wh.update(cx, |_root, window, cx| {
                    window.focus(&focus_handle, cx);
                });
            }
            None => {
                // 主窗口已不存在（例如被关闭）→ 本窗口转正为主窗口，选项卡原地保留
                self.is_detached = false;
                self.adopt_tab(payload, None, cx);
            }
        }
    }
}

// 幽灵窗口跟随用：Windows 原生 `SetWindowPos`（shell 已依赖 raw-window-handle=0.6 取
// HWND，不新增任何依赖；非 Windows 目标整块不编译）。
#[cfg(windows)]
const SWP_NOSIZE: u32 = 0x0001;
#[cfg(windows)]
const SWP_NOMOVE: u32 = 0x0002;
#[cfg(windows)]
const SWP_NOZORDER: u32 = 0x0004;
#[cfg(windows)]
const SWP_NOACTIVATE: u32 = 0x0010;
#[cfg(windows)]
const SWP_FRAMECHANGED: u32 = 0x0020;
#[cfg(windows)]
const SWP_SHOWWINDOW: u32 = 0x0040;
#[cfg(windows)]
const SWP_HIDEWINDOW: u32 = 0x0080;

// 幽灵窗改 WS_POPUP（去非客户黑环）用。
#[cfg(windows)]
const GWL_STYLE: i32 = -16;
#[cfg(windows)]
const WS_POPUP: u32 = 0x8000_0000;
#[cfg(windows)]
const WS_CAPTION: u32 = 0x00C0_0000;
#[cfg(windows)]
const WS_THICKFRAME: u32 = 0x0004_0000;
#[cfg(windows)]
const WS_SYSMENU: u32 = 0x0008_0000;

#[cfg(windows)]
#[repr(C)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[cfg(windows)]
#[link(name = "user32")]
unsafe extern "system" {
    #[allow(non_snake_case)]
    fn SetWindowPos(
        hWnd: *mut core::ffi::c_void,
        hWndInsertAfter: *mut core::ffi::c_void,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        uFlags: u32,
    ) -> i32;
    #[allow(non_snake_case)]
    fn GetWindowLongPtrW(hWnd: *mut core::ffi::c_void, nIndex: i32) -> isize;
    #[allow(non_snake_case)]
    fn SetWindowLongPtrW(hWnd: *mut core::ffi::c_void, nIndex: i32, dwNewLong: isize) -> isize;
    #[allow(non_snake_case)]
    fn GetWindowRect(hWnd: *mut core::ffi::c_void, lpRect: *mut Rect) -> i32;
    #[allow(non_snake_case)]
    fn GetClientRect(hWnd: *mut core::ffi::c_void, lpRect: *mut Rect) -> i32;
    #[allow(non_snake_case)]
    fn GetDpiForWindow(hWnd: *mut core::ffi::c_void) -> u32;
}

// 幽灵窗剥 DWM 框线用：dwmapi 的窗口圆角偏好（零新依赖，`#[link]` 姿势与上面 user32 同款）。
#[cfg(windows)]
const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
#[cfg(windows)]
const DWMWCP_DONOTROUND: u32 = 1;
#[cfg(windows)]
const DWMWA_TRANSITIONS_FORCEDISABLED: u32 = 3;

#[cfg(windows)]
#[link(name = "dwmapi")]
unsafe extern "system" {
    #[allow(non_snake_case)]
    fn DwmSetWindowAttribute(
        hwnd: *mut core::ffi::c_void,
        dwAttribute: u32,
        pvAttributeValue: *const core::ffi::c_void,
        cbAttribute: u32,
    ) -> i32;
}
