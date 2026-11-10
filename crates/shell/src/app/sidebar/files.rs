//! 侧边栏「文件树」子模块：以打开文件夹为根的嵌套目录树渲染 / 懒加载子项扫描 /
//! 展开折叠 / 过滤防抖与行级打开跳转。

use super::*;

impl EditorApp {
    /// 打开文件行：已打开 → 激活既有选项卡（**不新建**）；未打开 → 复用 `open_file`
    /// 完整打开语义（内部已分流 PDF / EPUB / 图 / 视频 / 文本）。
    pub(crate) fn open_path_or_activate(
        &mut self,
        path: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let idx = self
            .model.borrow().tabs
            .iter()
            .position(|t| t.file_path.as_deref() == Some(path));
        if let Some(idx) = idx {
            self.activate_tab(idx, window, cx);
        } else {
            self.open_file(&path.to_string_lossy(), window, cx);
        }
    }

    /// 文件树行点击（目录行）：展开/折叠切换。折叠只移出 `expanded`（children 缓存
    /// 保留，折叠再展开复用不重读）；首展未扫描 → 后台读直接子项。
    pub(crate) fn file_tree_toggle(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.sidebar.files.expanded.contains(&path) {
            self.sidebar.files.expanded.remove(&path);
            self.sidebar.files.layout_gen = self.sidebar.files.layout_gen.wrapping_add(1);
            cx.notify();
            return;
        }
        // 展开：先入集合（行态立到「展开」），未扫描才异步读这一层子项。
        self.sidebar.files.expanded.insert(path.clone());
        self.sidebar.files.layout_gen = self.sidebar.files.layout_gen.wrapping_add(1);
        if !self.sidebar.files.children.contains_key(&path) {
            self.spawn_file_tree_scan(path, window, cx);
        }
        cx.notify();
    }

    /// 后台读取某目录的**直接子项**并缓存（懒加载：一次只读这一层，绝不整棵递归预
    /// 扫描）。`std::fs::read_dir` 在后台线程执行（大目录万级不卡主线程），完成后
    /// 回主线程更新 state + notify。已在缓存 → 不复读；已在途 → 幂等返回。
    pub(crate) fn spawn_file_tree_scan(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.sidebar.files.children.contains_key(&path)
            || !self.sidebar.files.pending.insert(path.clone())
        {
            return;
        }
        let entity = cx.entity();
        window.spawn(cx, async move |cx| {
            let p_read = path.clone();
            let entries = cx
                .background_executor()
                .spawn(async move { read_dir_entries(&p_read) })
                .await;
            // 换根期间旧目录的在途扫描结果不落地（路径已不在当前根下 → 丢弃，防孤儿
            // 缓存膨胀；新根路径不同则互不干扰、同路径重开则幂等覆盖）。
            let _ = cx.update_window_entity(&entity, move |this, _w, cx| {
                this.sidebar.files.pending.remove(&path);
                let stale = this
                    .sidebar
                    .files
                    .root
                    .as_ref()
                    .map(|r| !path.starts_with(r))
                    .unwrap_or(true);
                if !stale {
                    this.sidebar.files.layout_gen = this.sidebar.files.layout_gen.wrapping_add(1);
                    this.sidebar.files.children.insert(path.clone(), entries);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 「文件」视图顶栏：「文件」标题 + 根目录名（路径尾段，超长省略）。
    fn render_sidebar_files_header(&mut self) -> impl IntoElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let border = self.bar_border();
        let root = self
            .sidebar
            .files
            .root
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned());
        div()
            .id("sidebar-files-header")
            .debug_selector(|| "sidebar-files-header".into())
            .flex()
            .items_center()
            .gap_2()
            .px(px(10.))
            .h(px(36.))
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .text_size(px(13.5))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(fg)
                    .child("文件"),
            )
            .child(if let Some(name) = root {
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_overflow(gpui::TextOverflow::Truncate("…".into()))
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(name)
                    .into_any_element()
            } else {
                div().into_any_element()
            })
    }

    /// 未打开文件夹 / 根路径失效的空态：文件夹图标 + 文案 + 可点「打开文件夹…」。
    fn sidebar_files_empty(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let muted = self.text_minor();
        let accent: gpui::Hsla = ui::theme::Theme::global(cx).colors.accent.into();
        div()
            .id("sidebar-files-empty")
            .debug_selector(|| "sidebar-files-empty".into())
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(8.))
            .px_3()
            .py_4()
            .child(Icon::new(IconName::Folder).size(px(20.)).text_color(muted))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(muted)
                    .child("未打开文件夹"),
            )
            .child(
                div()
                    .id("sidebar-files-open-btn")
                    .debug_selector(|| "sidebar-files-open-btn".into())
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .text_size(px(12.))
                    .text_color(accent)
                    .cursor(CursorStyle::PointingHand)
                    .on_click(cx.listener(
                        |this: &mut EditorApp, _: &gpui::ClickEvent, window, cx| {
                            this.open_folder_dialog(window, cx);
                        },
                    ))
                    .child("打开文件夹…"),
            )
            .into_any_element()
    }

    /// 文件树滚动容器包装：`relative` 包装层 + `overflow_y_scroll` 滚动容器 + 常显
    /// 纵向滚动条——与搜索/目录同款语义（内容超量可滚时随 [`Self::files_vscroll`]
    /// 显示 thumb，轨道点击跳转、thumb 可拖）。结构/样式收敛到
    /// `ui::scroll_panel::ScrollPanelBuilder`：包装层 / 滚动条覆盖层 id 显式覆盖为
    /// sidebar 既有 id（派生 `{id}-wrap` / `{id}-vscrollbar` 与之不一致）。
    fn wrap_files_scroll(&self, inner: impl IntoElement) -> gpui::AnyElement {
        let vhand = self.sidebar.files_vscroll.clone();
        ui::scroll_panel::ScrollPanelBuilder::new("sidebar-files-scroll", vhand)
            .wrap_id("sidebar-files-wrap")
            .bar_id("sidebar-files-vscrollbar")
            .build(inner)
            .into_any_element()
    }

    /// 「文件」视图：以打开文件夹为根的嵌套目录树。
    /// - 根 = `files.root`；未打开 / 路径失效 → 空态（可点「打开文件夹…」）。
    /// - 可见行 = 根 + 按 `expanded` 递归展开的子目录（懒加载：只物化已读层）。
    /// - 行样式复用搜索文件名行（active_bg hover、gap icon、单行截断）。
    pub(super) fn render_sidebar_files(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (fg, muted) = (self.text_primary(), self.text_minor());
        let active_bg = self.tab_active_bg();
        let content: gpui::AnyElement = if self.sidebar.files.root.is_none() {
            self.sidebar_files_empty(cx)
        } else {
            // 行描述按 `files.layout_gen` 缓存：树结构（根/展开/子项）没变逐帧复用，只把
            // 可视带内的行物化为元素——万级子项下不再每帧全量构造行元素/键控状态。
            let layout_gen = self.sidebar.files.layout_gen;
            let cached = match &self.sidebar.files_layout {
                Some((g, _)) => *g == layout_gen,
                None => false,
            };
            if !cached {
                let rows = self.sidebar_files_collect();
                self.sidebar.files_layout = Some((layout_gen, rows));
            }            let layout_rows = &self.sidebar.files_layout.as_ref().unwrap().1;
            let total = layout_rows.len();
            // 虚拟化：只物化可视带 + 上下 M 行缓冲（同目录视图 render_sidebar_toc 口径），
            // 行等高 `FILE_TREE_ROW_H` → divisor 精确；带外以等高 spacer 占位，滚动量程稳定。
            let (start, end) = if total <= TOC_VIRTUALIZE_MIN {
                (0usize, total)
            } else {
                let vhand = &self.sidebar.files_vscroll;
                let offset = (-vhand.offset().y).max(px(0.)).as_f32();
                let viewport_h = vhand.bounds().size.height.as_f32();
                let vh = if viewport_h > 0.0 {
                    viewport_h
                } else {
                    700.0
                };
                let first = (offset / FILE_TREE_ROW_H) as usize;
                let last = ((offset + vh) / FILE_TREE_ROW_H) as usize + 1;
                let start = first.saturating_sub(TOC_BAND_BUFFER);
                let end = (last + TOC_BAND_BUFFER).min(total);
                (start.min(end), end.max(start))
            };
            let mut rows: Vec<gpui::AnyElement> = Vec::with_capacity(end - start);
            for (i, row) in layout_rows.iter().enumerate().skip(start).take(end - start) {
                rows.push(self.sidebar_files_row(window, i, row, fg, muted, active_bg, cx));
            }
            let (top_h, bottom_h) = (
                start as f32 * FILE_TREE_ROW_H,
                (total - end) as f32 * FILE_TREE_ROW_H,
            );
            div()
                .flex()
                .flex_col()
                .py_1()
                .child(if top_h > 0.0 {
                    div().h(px(top_h)).into_any_element()
                } else {
                    div().into_any_element()
                })
                .children(rows)
                .child(if bottom_h > 0.0 {
                    div().h(px(bottom_h)).into_any_element()
                } else {
                    div().into_any_element()
                })
                .into_any_element()
        };
        div()
            .id("sidebar-files")
            .debug_selector(|| "sidebar-files".into())
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .overflow_hidden()
            .flex()
            .flex_col()
            .child(self.render_sidebar_files_header())
            .child(self.render_files_filter_input(window, cx))
            .child(self.wrap_files_scroll(content))
            // P2-10：面板开合轻淡入（同目录/搜索面板）。
            .with_spring(
                "sidebar-files-fade",
                hover_spring(1.0).from(0.0),
                |this, v| this.opacity(v),
            )
            .into_any_element()
    }

    /// 收集文件树全部**可见行描述**（根行 + 已展开递归子行 + 空目录占位）。
    /// 过滤词非空时按「名称不区分大小写子串」过滤：不匹配的条目（含目录）整体隐藏，
    /// 目录不匹配则其子树一并隐藏；过滤中省略「（空）」占位行（无意义）。
    /// O(可见行) 且只克隆轻量描述，不构造元素；结果按 [`SidebarChromeState::files_layout`]
    /// 缓存（`files.layout_gen` 失效），渲染逐帧 O(可视带) 物化。
    fn sidebar_files_collect(&self) -> Vec<FileRow> {
        let mut out: Vec<FileRow> = Vec::new();
        let Some(root) = self.sidebar.files.root.clone() else {
            return out;
        };
        // SC-3：hold Ref guard 供 `filter` 引用（`borrow()` 临时在某语句末即释放，
        // 不能直接 `self.model.borrow().files_filter.as_str()` 返回 &str）。
        let m = self.model.borrow();
        let filter = m.files_filter.as_str();
        // 根行恒可见（树根不因过滤消失；其下子项按过滤词裁剪）。
        let root_name = root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.to_string_lossy().into_owned());
        out.push(FileRow {
            path: root.clone(),
            label: root_name.into(),
            is_dir: true,
            depth: 0,
            placeholder: false,
        });
        self.sidebar_files_collect_children(&root, 0, filter, &mut out);
        // 过滤无匹配空态：仅剩根行且根的子项已加载（过滤有意义）→ 追加「无匹配结果」占位。
        if !filter.is_empty()
            && out.len() == 1
            && self.sidebar.files.children.contains_key(&root)
        {
            out.push(FileRow {
                path: root.clone(),
                label: SharedString::from("无匹配结果"),
                is_dir: false,
                depth: 1,
                placeholder: true,
            });
        }
        out
    }

    /// 递归收集某个**已展开目录**的直接子行描述（空目录 → 「（空）」占位行；过滤中省略）。
    fn sidebar_files_collect_children(
        &self,
        dir: &std::path::Path,
        depth: usize,
        filter: &str,
        out: &mut Vec<FileRow>,
    ) {
        let Some(children) = self.sidebar.files.children.get(dir) else {
            return; // 尚未扫描完成（加载中）→ 无子行，扫描完成后 `layout_gen` 递增重收
        };
        if children.is_empty() {
            if filter.is_empty() {
                out.push(FileRow {
                    path: dir.to_path_buf(),
                    label: SharedString::default(),
                    is_dir: false,
                    depth: depth + 1,
                    placeholder: true,
                });
            }
            return;
        }
        for child in children {
            // 过滤：名称不区分大小写子串不匹配 → 隐藏（含目录及其子树）。
            if !filter.is_empty() && !child.name.to_lowercase().contains(filter) {
                continue;
            }
            let is_expanded = child.is_dir && self.sidebar.files.expanded.contains(&child.path);
            out.push(FileRow {
                path: child.path.clone(),
                label: child.name.clone().into(),
                is_dir: child.is_dir,
                depth: depth + 1,
                placeholder: false,
            });
            if is_expanded {
                self.sidebar_files_collect_children(&child.path, depth + 1, filter, out);
            }
        }
    }

    /// 单条文件树行：缩进 + 目录指示（ChevronRight/Down + 文件夹图标）+ 名。
    /// 点击：目录行展开/折叠；文件行打开（已打开 → 激活既有选项卡）。当前激活
    /// 文件行高亮（active_bg 常显 + 主前景文字）。行强制 `FILE_TREE_ROW_H` 等高
    /// （虚拟化 divisor 精确）；空目录占位行显示「（空）」，无点击语义。
    fn sidebar_files_row(
        &self,
        window: &mut Window,
        key: usize,
        row: &FileRow,
        fg: gpui::Hsla,
        muted: gpui::Hsla,
        active_bg: gpui::Hsla,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let rows_id = format!("sidebar-file-row-{key}");
        // 空目录占位行（「（空）」）与过滤无匹配占位行（「无匹配结果」）：均不可点。
        if row.placeholder {
            let placeholder_text = if row.label.is_empty() {
                SharedString::from("（空）")
            } else {
                row.label.clone()
            };
            return div()
                .id(SharedString::from(rows_id.clone()))
                .debug_selector(move || rows_id.clone())
                .pl(px(10. + row.depth as f32 * FILE_TREE_INDENT))
                .py_1()
                .h(px(FILE_TREE_ROW_H))
                .mx(px(4.))
                .rounded_md()
                .text_size(px(12.))
                .text_color(muted)
                .child(placeholder_text)
                .into_any_element();
        }
        let is_expanded = row.is_dir && self.sidebar.files.expanded.contains(&row.path);
        let is_active = !row.is_dir && self.active_file_path_matches(&row.path);
        let row_fg = if is_active { fg } else { muted };
        // P2-10：hover 状态跨帧记录 → 行背景淡入（与搜索行同款）。
        let hover =
            window.use_keyed_state(format!("sidebar-file-row-{key}-hover"), cx, |_, _| false);
        let hovered = *hover.read(cx);
        let view_id = window.current_view();
        let chevron = if row.is_dir {
            Icon::new(if is_expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .size(px(12.))
            .text_color(row_fg)
            .into_any_element()
        } else {
            div().w(px(12.)).into_any_element()
        };
        let type_icon = if row.is_dir {
            Icon::new(if is_expanded {
                IconName::FolderOpen
            } else {
                IconName::Folder
            })
            .size(px(14.))
            .text_color(row_fg)
            .into_any_element()
        } else {
            Icon::new(IconName::File).size(px(13.)).text_color(row_fg).into_any_element()
        };
        let path = row.path.clone();
        let is_dir = row.is_dir;
        let label = row.label.clone();
        let depth = row.depth;
        div()
            .id(SharedString::from(rows_id.clone()))
            .debug_selector(move || rows_id.clone())
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .pl(px(8. + depth as f32 * FILE_TREE_INDENT))
            .pr_1()
            .py_1()
            .h(px(FILE_TREE_ROW_H))
            .mx(px(4.))
            .rounded_md()
            .min_w(px(0.))
            .text_color(row_fg)
            .cursor(CursorStyle::PointingHand)
            .on_hover({
                let st = hover.clone();
                move |h, _w, cx| {
                    st.update(cx, |v, _| *v = *h);
                    cx.notify(view_id);
                }
            })
            .on_click({
                let lc_path = path.clone();
                cx.listener(move |this: &mut EditorApp, _ev, window, cx| {
                    if is_dir {
                        this.file_tree_toggle(lc_path.clone(), window, cx);
                    } else {
                        this.open_path_or_activate(&lc_path, window, cx);
                    }
                })
            })
            .on_mouse_down(
                MouseButton::Right,
                {
                    let rc_path = path.clone();
                    cx.listener(move |this: &mut EditorApp, ev, window, cx| {
                        this.on_file_tree_right_click(&rc_path, is_dir, ev, window, cx);
                    })
                },
            )
            .child(chevron)
            .child(type_icon)
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_overflow(gpui::TextOverflow::Truncate("…".into()))
                    .text_size(px(12.))
                    .text_color(row_fg)
                    .child(label.to_string()),
            )
            .bg(if is_active || hovered {
                active_bg
            } else {
                active_bg.opacity(0.)
            })
            .into_any_element()
    }

    /// 某路径是否为当前激活选项卡的文件（文件树行高亮依据）。
    fn active_file_path_matches(&self, path: &std::path::Path) -> bool {
        self.model.borrow().tabs
            .get(self.model.borrow().active_tab)
            .and_then(|t| t.file_path.as_ref())
            .is_some_and(|p| p == path)
    }

    // -----------------------------------------------------------------
    // 文件树增强：过滤 / 右键菜单 / 新建 / 重命名 / 删除
    // -----------------------------------------------------------------

    /// 文件树过滤输入框：typeahead 按名字过滤树（防抖提交见
    /// [`Self::schedule_files_filter_debounce`]），清空恢复全树。
    /// 规则：过滤只作用于**已加载（已扫描且所在目录已展开）的条目**——匹配折叠/未
    /// 扫描目录内的名字不会命中（懒加载不整棵预扫，见 [`FileTreeState`]）；命中的目录
    /// 行保留并可展开（其子树仍按同词继续过滤）。
    fn render_files_filter_input(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        // 未打开文件夹 → 过滤框不显示（空态占满）。
        if self.sidebar.files.root.is_none() {
            return div().into_any_element();
        }
        let search = self.files_filter_state.clone();
        let focused = window.focused(cx) == Some(self.files_filter_state.read(cx).focus_handle(cx));
        let border_color: gpui::Hsla = if focused {
            ui::theme::Theme::global(cx).colors.focus_ring.into()
        } else {
            self.bar_border()
        };
        div()
            .id("sidebar-files-filter-input")
            .debug_selector(|| "sidebar-files-filter-input".into())
            .px_2()
            .py_1()
            .mx_2()
            .my_1()
            .w_full()
            .rounded_sm()
            .border_1()
            .border_color(border_color)
            .child(Input::new(&search))
            .into_any_element()
    }

    /// 输入变化 → 登记防抖待提交过滤词并重启防抖计时器（`SeqAfter` 代序作废旧定时器）。
    /// 防抖期满提交 `files_filter` + 作废文件树布局缓存（下次渲染按新过滤词重收集可见行）。
    pub(crate) fn schedule_files_filter_debounce(&mut self, value: String, cx: &mut Context<Self>) {
        self.model.borrow_mut().files_filter_pending = value.trim().to_lowercase();
        cx.notify();
        let weak = cx.weak_entity();
        self.model.borrow().files_filter_debounce.after(
            cx,
            std::time::Duration::from_millis(FILES_FILTER_DEBOUNCE_MS),
            move |app| {
                let _ = weak.update(app, |this, cx| {
                    // SC-3：先提纯 owned 再比较（`if` 条件里的 borrow 临时 Ref 会存活
                    // 整个分支块 → 块内 borrow_mut 运行期双借 panic）。
                    let pending = std::mem::take(&mut this.model.borrow_mut().files_filter_pending);
                    let files_filter = this.model.borrow().files_filter.clone();
                    if files_filter != pending {
                        this.model.borrow_mut().files_filter = pending;
                        this.sidebar.files.layout_gen = this.sidebar.files.layout_gen.wrapping_add(1);
                        cx.notify();
                    }
                });
            },
        );
    }
}
