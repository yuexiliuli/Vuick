//! 统一 `md_wy` WyEditor 预览。
//!
//! 右栏 / 全屏预览统一组件 `WyEditor`（A1 只读门控，MD / EPUB / 纯文本共用）的懒创建 /
//! 喂源 / 增量对齐 / 整篇装配。含 B3 后台整篇异步装配（`md_wy_feed_full` /
//! `md_wy_feed_full_rope`）、Step2/3 大文档「打开即提前整篇解析 + 首块 prefix 首装」两阶段
//! 换入（`md_wy_spawn_early_install` / `md_wy_spawn_prefix_install` /
//! `md_wy_install_full_swap`），以及编辑⇄预览联动读面（`md_toc_source` / `md_wy_preview` /
//! EPUB 页码 / 章节翻转）。
//!
//! 编辑事件**订阅 / feed 入口**（shell `app/sync.rs` 的 `on_editor_edited`）与 `commit_live`
//! 的宿主收尾留在 shell，经 `md_wy_feed_full_rope` / `md_wy_align_after_*` 调本模块。

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{App, AppContext, Context, Entity, SharedString, Window};

use markdown_wysiwyg::{WyEditor, WyOpenResult};
use ropey::Rope;
use shell_core::{AppModel, MdTocScroll, MdWyEarly};

/// 右栏只读预览 `md_wy` 走「逐 hunk 增量重放」的最大基线跨度（被替换的 base 字节总量）。
/// 超过即改用整篇 `set_source_text`：WYSIWYG 对深部大结构改动的增量会退化成本应整篇做
/// 的重排、并把它推迟到切回后第一渲染帧（实测 6.4MB 大段替换首帧卡 5.5s）；一次性 O(n)
/// Doc 打开（实测 ~1s）成本落在同步写回里、首帧零卡——总墙钟更短。典型小改动（几字符/
/// 几行）跨度远低于此，走增量（几十 ms）。
pub const MD_WY_DIFF_MAX_SPAN_BYTES: usize = 64 * 1024;

/// B3：右栏 `md_wy` 整篇喂源时「后台异步装配」的内容大小门槛（对齐 toggle ON 的
/// `ASYNC_LOAD_MIN_BYTES`）。大内容（如 6.4MB 大段切回）走后台 `open_from_str` 解析、
/// 点击路径零 O(n)，右栏内容「后到」；小内容保持同步（几十 KB 级 Doc::open 极快，
/// 无「旧→新」等待窗口）。
pub const MD_WY_ASYNC_INSTALL_MIN_BYTES: usize = 512 * 1024;

/// 测试探针：B3 **后台异步整篇装配**发起次数——大跨度切回登记 `open_from_str` 后台解析
/// （而非同步整篇 `set_source_text`）的路径计数。shell 侧回归测试据此断言「大段切回
/// 同步路径不再 O(n) 解析」。
///
/// 常驻编译（无 `#[cfg(test)]`）：shell 的测试构建把本 crate 当普通依赖编译（`cfg(test)`
/// 恒关），探针须常驻才可被 shell 测试经重导出读取。
#[doc(hidden)]
pub static LIVE_COMMIT_MD_WY_ASYNC: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// 选项卡默认文件名（用于保存对话框，shell `app/tabs.rs` 与 `app/file_ops.rs` 读）。
pub fn tab_file_name(m: &AppModel, idx: usize) -> String {
    m.tabs
        .get(idx)
        .and_then(|t| t.payload.file_path.as_ref())
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "未命名.txt".into())
}

/// 当前激活选项卡是否开启预览。
pub fn show_preview(m: &AppModel) -> bool {
    m.tabs
        .get(m.active_tab)
        .map(|t| t.payload.preview_on)
        .unwrap_or(false)
}

/// A2：当前激活选项卡的右栏 WyEditor 预览实体（仅当该选项卡**真正以 WyEditor 作
/// 预览渲染**时才返回）：Markdown / EPUB / 纯文本（都可能有 `md_wy`）返回宿主；
/// PDF / 图片 / 视频（自带阅读器）→ `None`。shell `render_preview_pane` /
/// `render_fullscreen_preview` 据此决定走统一组件宿主。
///（M5-B5 P4-2：`MarkdownPreview` 已删，预览统一 WyEditor。）
pub fn md_wy_preview(m: &AppModel) -> Option<Entity<WyEditor>> {
    let active = m.active_tab;
    let tab = m.tabs.get(active)?;
    // PDF/图片/视频用自带阅读器，绝不以 WyEditor 渲染。
    if tab.payload.reader.is_reader() {
        return None;
    }
    tab.payload.md_wy.clone()
}

/// A4：当前激活选项卡的 Markdown TOC 数据源 + 跳转目标（侧边栏目录共用）：
/// MD / EPUB 统一预览都走 WyEditor 只读宿主（条目 = 源码行，点击滚动统一组件）；
/// 无 WyEditor（未开启预览 / 非预览文档）→ `None`。
pub fn md_toc_source(
    m: &AppModel,
    cx: &App,
) -> Option<(markdown_wysiwyg::TocEntries, MdTocScroll)> {
    let active = m.active_tab;
    let tab = m.tabs.get(active)?;
    let wy = tab.payload.md_wy.as_ref()?;
    Some((wy.read(cx).toc_entries(), MdTocScroll::Unified(wy.clone())))
}

/// EPUB 轻量视口分页读数 `(当前页, 总页数)`（1 基；状态栏页码，P4-1/P4-2）：
/// 读 WyEditor 只读宿主 WYSIWYG 确定性量程（`MarkdownPreview` 回退后端已删）。
/// 非 EPUB / 就绪前 → `None`（状态栏不显示页码）。
pub fn epub_page_bounds(
    model: &Rc<RefCell<AppModel>>,
    window: &mut Window,
    cx: &App,
) -> Option<(usize, usize)> {
    let idx = model.borrow().active_tab;
    let m = model.borrow();
    if !crate::classify::is_epub_tab(&m, idx) {
        return None;
    }
    let tab = m.tabs.get(idx)?;
    // 视口高：以窗口内容高估算（容器 ≈ 窗口正文区；误差 ≤ 顶部/底部工具条高，
    // 对「第 x/N 页」展示无碍）。
    let view_h = f32::from(window.viewport_size().height).max(1.);
    tab.payload.md_wy.as_ref().and_then(|w| w.read(cx).viewport_page_bounds(view_h))
}

/// P4-1：EPUB 章节翻转「上一章/下一章」。章界 = 目录标题条目（每章以标题起始）。
/// 统一 WyEditor 宿主：当前章 = 顶行所在标题，滚动到相邻章标题（`set_scroll_target`，
/// prepaint 重钉，无需 window；`MarkdownPreview` 回退后端已删）。
/// 无 EPUB / 少于 2 章 / 已处边界章 → 不动，返回 `false`。
pub fn epub_flip_chapter<H: 'static>(
    model: &Rc<RefCell<AppModel>>,
    dir: i32,
    cx: &mut Context<H>,
) -> bool {
    let idx = model.borrow().active_tab;
    if !crate::classify::is_epub_tab(&model.borrow(), idx) {
        return false;
    }
    let Some((entries, scroll)) = md_toc_source(&model.borrow(), cx) else {
        return false;
    };
    if entries.len() < 2 {
        return false;
    }
    let MdTocScroll::Unified(wy) = scroll;
    let current = wy.read(cx).top_source_line().unwrap_or(0);
    let cur_ix = entries
        .iter()
        .rposition(|&(l, _, _)| l <= current)
        .unwrap_or(0);
    let next = (cur_ix as i32 + dir).clamp(0, entries.len() as i32 - 1) as usize;
    if next == cur_ix {
        return false;
    }
    wy.update(cx, |v, _| v.set_scroll_target(entries[next].0));
    cx.notify();
    true
}

/// 右栏 WyEditor 懒创建的共用引导块（`ensure_md_wy` / `ensure_epub_wy` 两处手抄的
/// ~15 行设置收敛于此）：尚未创建时新建并套 A1 只读门控 `set_editable(false)` +
/// 全局字号 + 基准目录；`configure` 注入阅读器差分支（当前 MD/EPUB 创建期均无专属
/// 设置 → 传空闭包；EPUB 的只读居中/内存图/注记等仍在调用方喂源块）。保持 lazy-once
/// 语义（不预建），各阅读器创建后的喂源差异逻辑留在各自调用方。
fn ensure_wy<H: 'static>(
    model: &Rc<RefCell<AppModel>>,
    idx: usize,
    window: &mut Window,
    cx: &mut Context<H>,
    configure: impl FnOnce(&mut WyEditor, &mut gpui::App),
) {
    let fs = model.borrow().font_size;
    let base_dir = model.borrow().tabs[idx]
        .payload.file_path
        .as_ref()
        .and_then(|p| p.parent())
        .map(|d| d.to_path_buf());
    if model.borrow().tabs[idx].payload.md_wy.is_none() {
        let ed = cx.new(|cx| WyEditor::new(window, cx));
        ed.update(cx, |ed, cx| {
            ed.set_editable(false); // A1 只读门控：右栏预览不可编辑
            ed.set_font_size(gpui::px(fs));
            ed.set_base_dir(base_dir);
            configure(ed, cx);
        });
        model.borrow_mut().tabs[idx].payload.md_wy = Some(ed.clone());
    }
}

/// A2：确保某选项卡的右栏 WyEditor 预览已创建（懒创建 + A1 只读门控 + 全局字号/
/// 基准目录）。`feed=true` 用当前编辑区文本**同步喂源**（小文档 / 切预览；
/// 大文档异步走 `open_from_rope`(后台) + `finish_file_text` 的 `install_open`，
/// 不在此做 O(n) UI 解析）。
pub fn ensure_md_wy<H: 'static>(
    model: &Rc<RefCell<AppModel>>,
    idx: usize,
    window: &mut Window,
    cx: &mut Context<H>,
    feed: bool,
) {
    if idx >= model.borrow().tabs.len() {
        return;
    }
    // B3：喂源/就地换源前作废在途后台装配（防旧切回的装配把新喂内容覆盖回去）。
    md_wy_invalidate_async(model, idx, cx);
    // 懒创建引导块共用；MD 创建期无专属设置 → 空配置闭包。
    ensure_wy(model, idx, window, cx, |_ed, _cx| {});
    if feed {
        // 同步喂源：源 = 编辑区规范文本（与预览宿主同口径）。
        let fs = model.borrow().font_size;
        let base_dir = model.borrow().tabs[idx]
            .payload.file_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|d| d.to_path_buf());
        let text = {
            let mut m = model.borrow_mut();
            let tab = &mut m.tabs[idx];
            tab.payload.editor.read(cx).value().to_string()
        };
        if let Some(ed) = model.borrow().tabs[idx].payload.md_wy.clone() {
            ed.update(cx, |ed, _| {
                ed.set_font_size(gpui::px(fs));
                ed.set_base_dir(base_dir);
                ed.set_source_text(&text);
            });
        }
    }
}

/// EPUB：确保右栏 WyEditor **只读宿主**已创建（懒创建 + A1 只读门控 + 全局字号/
/// 基准目录/内存图片表），并用 EPUB 源**同步喂源**（P4-1：EPUB 迁 WyEditor）。
/// 只读（`set_editable(false)`）、标题居中（`set_epub(true)`）、`epubimg://…` 内存图
/// （`set_memory_images`）、注记浮卡/引文跳转正文（`set_notes`）。
pub fn ensure_epub_wy<H: 'static>(
    model: &Rc<RefCell<AppModel>>,
    idx: usize,
    window: &mut Window,
    cx: &mut Context<H>,
    text: &str,
    images: &HashMap<String, Arc<gpui::Image>>,
    notes: &BTreeMap<usize, String>,
) {
    if idx >= model.borrow().tabs.len() {
        return;
    }
    // B3：喂源/就地换源前作废在途后台装配（防旧切回装配覆盖新喂内容）。
    md_wy_invalidate_async(model, idx, cx);
    // 懒创建引导块共用；EPUB 创建期无专属设置 → 空配置闭包。
    ensure_wy(model, idx, window, cx, |_ed, _cx| {});
    let fs = model.borrow().font_size;
    let base_dir = model.borrow().tabs[idx]
        .payload.file_path
        .as_ref()
        .and_then(|p| p.parent())
        .map(|d| d.to_path_buf());
    if let Some(ed) = model.borrow().tabs[idx].payload.md_wy.clone() {
        ed.update(cx, |ed, _| {
            ed.set_font_size(gpui::px(fs));
            ed.set_base_dir(base_dir);
            ed.set_memory_images(images.clone());
            ed.set_epub(true);
            ed.set_notes(notes.clone());
            ed.set_source_text(text);
        });
    }
}

/// B3：作废右栏 `md_wy` 的在途后台整篇装配（装配代号 +1、清在途）。任何**同步**
/// 改写 `md_wy` 内容的路径（增量重放 / 同步整篇喂源 / 换源 install_open / 预览关闭）
/// 都必须在改动前调用——否则旧装配完成后会按旧代号命中、把刚改好的内容覆盖回去
/// （装错）。shell `file_ops`（打开换源）也调用。
/// 装配代号与「在途」标志已收敛进 WyEditor（[`WyEditor::invalidate_install`]），此处只
/// 作 shell 域转发 + 清整篇在途标记。
pub fn md_wy_invalidate_async<H: 'static>(
    model: &Rc<RefCell<AppModel>>,
    idx: usize,
    cx: &mut Context<H>,
) {
    if let Some(t) = model.borrow_mut().tabs.get_mut(idx) {
        if let Some(wy) = t.payload.md_wy.clone() {
            wy.update(cx, |w, _| w.invalidate_install());
        }
        t.payload.md_wy_install.pending_full = None; // Step3：任何同步改写 md_wy 内容 → 整篇在途标记一并作废
    }
}

/// B3：整篇喂右栏 `md_wy` 的统一入口（替代同步 `set_source_text`）。`content` 大
/// （≥ [`MD_WY_ASYNC_INSTALL_MIN_BYTES`]）→ **后台** `open_from_str` 解析、就绪后带
/// tab 归属 + 装配代号守卫 `install_open`（点击路径零 O(n)，右栏内容「后到」可接受）；
/// 小内容 → 同步（几十 KB 级解析极快，无「旧→新」等待）。每次调用递增装配代号——
/// 本次装配胜出，覆盖在途旧装配。`open_from_str` 在本管线内无失败路径；若守卫不通过
/// （切走/关闭/更新换代）则完成回调直接丢弃结果，md_wy 保持当前（旧）内容，由后续
/// 喂源/装配追平，不装错、不泄漏。装配代号与在途标志收进 WyEditor（[`WyEditor::begin_install`]，
/// 语义 = 原宿主 `md_wy_install.seq` + `pending`）。
pub fn md_wy_feed_full<H: 'static>(
    model: &Rc<RefCell<AppModel>>,
    idx: usize,
    content: SharedString,
    cx: &mut Context<H>,
) {
    let Some(wy) = model.borrow().tabs.get(idx).and_then(|t| t.payload.md_wy.clone()) else {
        return;
    };
    // 装配代号 / 在途标志收进 WyEditor：发起即取新代号 + `begin_install`（在途）。
    let seq = wy.update(cx, |w, _| {
        let s = w.next_install_seq();
        w.begin_install(s);
        s
    });
    if content.len() < MD_WY_ASYNC_INSTALL_MIN_BYTES {
        // 同步小整篇（无「旧→新」等待；`open_from_str` 免 `set_source_text` 的一次
        // 字符串复制，直接共享这份 `content`）。
        wy.update(cx, |w, _| {
            w.install_open(markdown_wysiwyg::WyEditor::open_from_str(content));
            w.finish_install(seq);
        });
        if let Some(t) = model.borrow_mut().tabs.get_mut(idx) {
            t.payload.md_wy_install.pending_full = None; // Step3：同步装好即 == 编辑区 → 解除在途标记
        }
        return;
    }
    // Step3：异步整篇在途、预览仍为旧内容（<新整篇）→ 置在途标记，增量桥打旧内容会
    // 错位，转为「作废 + 重喂」（见 shell `sync::on_editor_edited`）。
    model.borrow_mut().tabs[idx].payload.md_wy_install.pending_full = Some((seq, 0));
    LIVE_COMMIT_MD_WY_ASYNC.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    md_wy_spawn_async_install(model, idx, wy, content, seq, cx);
}

/// B3：切回后右栏 `md_wy` 对齐——**op 重放主路径**版（A2/A3 语义保留）。
/// 小跨度（≤ 阈值）且 md_wy 与源同长 → 逐 op 增量 `preview_apply_edit`（1-3ms）；
/// 大跨度 / 长度失步 → 整篇喂源 [`md_wy_feed_full`]（大内容后台装配）。前提
/// 只做 O(1) 的 md_wy 长度比对（同源、live 期间未被改写 → 起点一致）——**不做全篇
/// 文本比对**（那会冷物化 md_wy 全文，6.4MB 又 +20ms）。
pub fn md_wy_align_after_ops<H: 'static>(
    model: &Rc<RefCell<AppModel>>,
    idx: usize,
    ops: &[(Range<usize>, SharedString)],
    src_len: usize,
    content: SharedString,
    cx: &mut Context<H>,
) {
    let Some(wy) = model.borrow().tabs.get(idx).and_then(|t| t.payload.md_wy.clone()) else {
        return;
    };
    let span: usize = ops
        .iter()
        .map(|(r, t)| (r.end - r.start).max(t.len()))
        .sum();
    if span <= MD_WY_DIFF_MAX_SPAN_BYTES && wy.read(cx).len() == src_len {
        // 增量重放会逐 op 把 md_wy 改成最终态 → 先作废在途后台装配（不装陈旧整篇）。
        md_wy_invalidate_async(model, idx, cx);
        for (r, t) in ops {
            wy.update(cx, |w, cx| w.preview_apply_edit(r.clone(), t.as_ref(), cx));
        }
        // 重放后 md_wy 长度必然 == content（同起点 + 确定性 op）；若因未知原因不等 →
        // 整篇喂源兜底（仅长度 O(1) 比对）。
        if wy.read(cx).len() != content.len() {
            md_wy_feed_full(model, idx, content, cx);
        }
    } else {
        md_wy_feed_full(model, idx, content, cx);
    }
}

/// B3：切回后右栏 `md_wy` 对齐——**行级 diff 兜底**版（A2/A3 语义保留）。
/// 增量 hunk（后→前 `preview_apply_edit`）；大跨度 diff 或 md_wy 内容 ≠ 基线 →
/// [`md_wy_feed_full`] 整篇喂源（大内容后台装配）。
pub fn md_wy_align_after_diff<H: 'static>(
    model: &Rc<RefCell<AppModel>>,
    idx: usize,
    replaces: &[(Range<usize>, String)],
    base: &str,
    content: SharedString,
    cx: &mut Context<H>,
) {
    let Some(wy) = model.borrow().tabs.get(idx).and_then(|t| t.payload.md_wy.clone()) else {
        return;
    };
    let span: usize = replaces.iter().map(|(r, _)| r.end - r.start).sum();
    let use_diff = !replaces.is_empty()
        && span <= MD_WY_DIFF_MAX_SPAN_BYTES
        && wy.read(cx).text().as_ref() == base;
    if use_diff {
        md_wy_invalidate_async(model, idx, cx);
        for (r, t) in replaces.iter().rev() {
            wy.update(cx, |w, cx| w.preview_apply_edit(r.clone(), t, cx));
        }
    } else if !replaces.is_empty() || wy.read(cx).text().as_ref() != base {
        md_wy_feed_full(model, idx, content, cx);
    }
}

/// Step2/3：大文档 md「打开即提前整篇解析」——首块挂载（`apply_open_chunk`）后立刻以
/// 独立后台任务**从文件路径读全文**（复用 `text_open::ChunkReader` 归一化：
/// CRLF/孤立 CR → LF、UTF-8 校验，结果与分块读盘后的编辑区逐字节一致）+ 整篇
/// `open_from_str` 解析，与编辑区分块追加**并行**（不再等 EOF 才开跑 Doc::open）。
/// Step3：与 [`md_wy_spawn_prefix_install`] **两阶段**协作——prefix 首装先行
/// （首屏真内容、`pending_full` 在途），本函数为「整篇在那之后就绪 → 带守卫换入
/// 全文」，两者共用同一装配代号，互不废对方；**不得双 install、不得 full→prefix
/// 降级**（见 prefix 完成回调对 `md_wy_early==Done` 的屏蔽）。
///
/// 完成回调守卫，任一不满足即丢弃（不装错、不泄漏）：
///   1) **tab 归属**：该 tab 的 `md_wy` 仍指向 `wy`（未关闭 / 未重建 / 未切走）；
///   2) **装配代号**：`wy.install_state.seq` 仍 == 本代号（期间换源/重喂/编辑在途作废；
///      代号检查收敛进 WyEditor [`WyEditor::begin_install`]，语义 = 原宿主 `seq == seq`）；
///   3) **未编辑**：`edit_version == 启动时版本`（用户已编辑 → 丢弃在途整篇，避免
///      把「装旧」覆盖用户对编辑器做的改动；该场景由 shell `sync.rs` 的重喂/增量链路
///      接管）。
///
/// 成功经 [`md_wy_install_full_swap`] 换入全文（滚动锚 src_line 钉顶行）+ 置
/// `md_wy_early = Done`（EOF 收尾据此跳过重复 install）；失败置 `Failed`（EOF 回退
/// 走既有整篇装配兜底）。返回本次装配代号（供 prefix 首装共用）。
pub fn md_wy_spawn_early_install<H: 'static>(
    model: &Rc<RefCell<AppModel>>,
    idx: usize,
    p: PathBuf,
    cx: &mut Context<H>,
) -> usize {
    let Some(wy) = model.borrow().tabs.get(idx).and_then(|t| t.payload.md_wy.clone()) else {
        return 0;
    };
    // 装配代号 / 在途标志收进 WyEditor：发起即取新代号 + `begin_install`（在途）。
    let seq = wy.update(cx, |w, _| {
        let s = w.next_install_seq();
        w.begin_install(s);
        s
    });
    let ver = model.borrow().tabs[idx].payload.edit_version;
    model.borrow_mut().tabs[idx].payload.md_wy_install.early =
        Some(MdWyEarly::Pending { seq, edit_version: ver });
    let model2 = model.clone();
    cx.spawn(async move |this, cx| {
        // 独立后台任务：读全文（归一化）→ 整篇解析（UI 线程不背 O(n)）。
        let result = cx
            .background_executor()
            .spawn(async move {
                use text_open::ChunkReader;
                let mut text = String::new();
                let mut reader = ChunkReader::open(&p)?;
                while let Some(chunk) = reader.next_chunk()? {
                    text.push_str(&chunk.text);
                }
                Ok::<_, std::io::Error>(
                    markdown_wysiwyg::WyEditor::open_from_str(text.into()),
                )
            })
            .await;
        let _ = this.update(cx, |_this, cx| {
            // 守卫 1/2（短借用）：tab 归属（保持原样）+ 装配代号（→ WyEditor 侧陈旧检查）。
            let owned = model2.borrow().tabs.get(idx).is_some_and(|t| {
                t.payload.md_wy.as_ref() == Some(&wy)
            });
            let current = owned && wy.update(cx, |w, _| w.begin_install(seq));
            if !current {
                // 代号已换代 / 实体已易主（换源、重喂、切走重建）→ 本结果作废；清掉
                // 本代号的在途标记（不误标 Failed——新装配会接管）。
                if let Some(t) = model2.borrow_mut().tabs.get_mut(idx)
                    && t.payload.md_wy_install.early.as_ref()
                        == Some(&MdWyEarly::Pending { seq, edit_version: ver })
                {
                    t.payload.md_wy_install.early = None;
                    t.payload.md_wy_install.pending_full = None;
                }
                return;
            }
            match result {
                // 守卫 3：未编辑 → 带滚动锚换入全文。
                Ok(result) if model2.borrow().tabs[idx].payload.edit_version == ver => {
                    md_wy_install_full_swap(&model2, idx, &wy, result, seq, cx);
                }
                Ok(_) => {
                    // 已编辑：丢弃整篇结果（sync 自愈/重喂链路接管），复位在途标记
                    //（EOF 收尾不再指望它，走既有收尾兜底）。整体装配已收尾 → 清在途。
                    let mut m = model2.borrow_mut();
                    let t = &mut m.tabs[idx];
                    t.payload.md_wy_install.early = None;
                    t.payload.md_wy_install.pending_full = None;
                    drop(m);
                    // 已编辑分支：`finish_install(seq)` 顺带置 `loaded=true` 属良性放宽——
                    // 早装 prefix 已把内容挂上（loaded 本已 true），此处只清「在途」；即
                    // 便无 prefix，后续 `feed_full_rope` 即时重喂会真正装上，不再重复置位。
                    wy.update(cx, |w, _| w.finish_install(seq));
                }
                Err(_) => {
                    // 读盘/解析失败：置 Failed，EOF 收尾回退走既有整篇装配兜底——*前提是
                    // EOF 尚未发生*（早装 Pending 已让位时 EOF 不会跑兜底 install）。
                    // 因此这里**即时兜底**：以当前编辑区 Rope（O(1) COW 快照）异步整篇
                    // 重喂，两种时序都不让右栏卡在占位：
                    //   - EOF 已过（早装 Pending 时让位）→ 本重喂落地装出预览；
                    //   - EOF 未过 → EOF 的 `md_wy_invalidate_async` 会作废本在途重喂，
                    //     EOF 兜底整篇装配接管（无双 install、不装陈旧）。
                    {
                        let mut m = model2.borrow_mut();
                        let t = &mut m.tabs[idx];
                        // 仅当依旧命中本代号（期间无换代）才接管，否则交新装配。
                        t.payload.md_wy_install.early = Some(MdWyEarly::Failed);
                        t.payload.md_wy_install.pending_full = None;
                    }
                    // 失败信息收进 WyEditor（error 轴；随后重喂换代+清在途接管）。
                    wy.update(cx, |w, _| w.fail_install(seq, String::from("提前整篇解析失败")));
                    let rope = model2.borrow().tabs[idx].payload.editor.read(cx).rope().clone();
                    md_wy_feed_full_rope(&model2, idx, rope, cx);
                }
            }
        });
    })
    .detach();
    seq
}

/// Step3：整篇就绪后**带守卫换入全文**的落地函数（`md_wy_spawn_early_install` 完成
/// 回调与测试共用）。换入前抓当前顶行 `src_line`（prefix 帧；帧未就绪则 `None` 跳过），
/// `install_open` 后经 `set_scroll_target` 由元素 prepaint（`advance_pending_scroll`）
/// 逐帧重钉宿主偏移——prefix→全文**视觉不跳**；随后置 `md_wy_early = Done`、清
/// `md_wy_pending_full`（整篇已装 → 增量桥恢复逐键）。`seq` = 本次整篇装配代号，
/// 收尾时 [`WyEditor::finish_install`]（清在途/失败 + 置 loaded）。
pub fn md_wy_install_full_swap<H: 'static>(
    model: &Rc<RefCell<AppModel>>,
    idx: usize,
    wy: &Entity<WyEditor>,
    result: WyOpenResult,
    seq: usize,
    cx: &mut Context<H>,
) {
    let anchor = wy.read(cx).top_source_line();
    wy.update(cx, |ed, _| {
        ed.install_open(result);
        ed.finish_install(seq);
    });
    if let Some(sl) = anchor {
        wy.update(cx, |ed, _| ed.set_scroll_target(sl));
    }
    let mut m = model.borrow_mut();
    if let Some(tab) = m.tabs.get_mut(idx) {
        tab.payload.md_wy_install.early = Some(MdWyEarly::Done);
        tab.payload.md_wy_install.pending_full = None;
    }
    cx.notify();
}

/// Step3：大文档 md「首块 prefix 首装」——首块挂载（`apply_open_chunk`）后即后台
/// `open_from_rope(prefix)` 并 install（`loaded=true`，首屏立刻真内容），并置
/// `md_wy_pending_full=(full_seq, prefix_len)`「整篇在途」；整篇（[`md_wy_spawn_early_install`]）
/// 就绪后带守卫换入全文。**prefix 来源 = 首块挂载时编辑区 rope 快照**（O(1) COW，
/// 与编辑器同值同归一化）→ 增量桥在 prefix 上按编辑区坐标天然对齐；不用磁盘首读
/// 字节（避免归一化口径分叉——同一份首块文本，磁盘读与编辑器 rope 理论上逐字节
/// 一致，但快照零风险且零拷贝）。
///
/// 完成守卫（任一不满足即丢弃，不装错、不降级）：
///   1) tab 归属：`md_wy` 仍指向 `wy`；
///   2) 装配代号：`wy.install_state.seq` 仍 == `full_seq`（与整篇**同代号**，两者互不废
///      对方；期间换源/重喂/编辑 → 代号换代皆弃；代号检查收敛进
///      [`WyEditor::begin_install`]，语义 = 原宿主 `seq == full_seq`）；
///   3) **整篇未先装**：`md_wy_early` 非 `Done`（整篇若已先到，prefix 再装会把全文
///      退回首屏——严禁降级）；
///   4) **未编辑**：`edit_version == 启动时版本`（已编辑 → prefix 快照已过期，交给
///      shell `sync` 自愈/重喂链路接管）。
pub fn md_wy_spawn_prefix_install<H: 'static>(
    model: &Rc<RefCell<AppModel>>,
    idx: usize,
    prefix: Rope,
    full_seq: usize,
    cx: &mut Context<H>,
) {
    let Some(wy) = model.borrow().tabs.get(idx).and_then(|t| t.payload.md_wy.clone()) else {
        return;
    };
    let ver = model.borrow().tabs[idx].payload.edit_version;
    let prefix_len = prefix.len();
    let model2 = model.clone();
    cx.spawn(async move |this, cx| {
        let result = cx
            .background_executor()
            .spawn(async move { markdown_wysiwyg::WyEditor::open_from_rope(prefix) })
            .await;
        let _ = this.update(cx, |_this, cx| {
            let mut m = model2.borrow_mut();
            let Some(tab) = m.tabs.get_mut(idx) else {
                return;
            };
            // 守卫 1：归属换代 → 丢弃（新装配接管）；守卫 3/4 为纯读，先于代号检查——
            // 与原实现「代号先查」在可观测结果上等价（任一返回即不装机），且避免
            // 在整篇已装/已编辑时无谓触碰 WyEditor 在途标志。
            if tab.payload.md_wy.as_ref() != Some(&wy) {
                return; // 归属换代 → 丢弃（新装配接管）
            }
            if matches!(tab.payload.md_wy_install.early, Some(MdWyEarly::Done)) {
                return; // 整篇已先装：不降级回 prefix
            }
            if tab.payload.edit_version != ver {
                return; // 已编辑：prefix 快照已过期，sync 重喂接管
            }
            drop(m);
            // 守卫 2：装配代号陈旧世序（→ WyEditor 侧）。陈旧 → 丢弃（新装配接管）。
            let current = wy.update(cx, |w, _| w.begin_install(full_seq));
            if !current {
                return;
            }
            wy.update(cx, |ed, _| {
                ed.install_open(result);
                ed.finish_install(full_seq);
            });
            let mut m = model2.borrow_mut();
            if let Some(tab) = m.tabs.get_mut(idx) {
                tab.payload.md_wy_install.pending_full = Some((full_seq, prefix_len));
                cx.notify();
            }
        });
    })
    .detach();
}

/// Step3：整篇重喂的 **Rope 版**（`md_wy_feed_full` 同语义）：以**当前编辑区 Rope**
/// （O(1) COW 快照）异步重喂全文——pending_full 期间（预览=prefix ≠ 编辑区）用户编辑
/// 时增量桥坐标会错位，由 shell `sync::on_editor_edited` 调用本函数把预览重拉回「== 编辑
/// 区」，随后退化逐键增量。小 rope 同步装、大 rope 后台 parse，均带装配代号守卫；
/// 异步在途期间置 `md_wy_pending_full`（禁止对旧内容增量）。装配代号与在途标志收进
/// WyEditor（[`WyEditor::begin_install`]）。
pub fn md_wy_feed_full_rope<H: 'static>(
    model: &Rc<RefCell<AppModel>>,
    idx: usize,
    rope: Rope,
    cx: &mut Context<H>,
) {
    let Some(wy) = model.borrow().tabs.get(idx).and_then(|t| t.payload.md_wy.clone()) else {
        return;
    };
    let seq = wy.update(cx, |w, _| {
        let s = w.next_install_seq();
        w.begin_install(s);
        s
    });
    if rope.len() < MD_WY_ASYNC_INSTALL_MIN_BYTES {
        // 同步小整篇：装好后预览 == 编辑区 → 解除在途标记。
        wy.update(cx, |w, _| {
            w.install_open(markdown_wysiwyg::WyEditor::open_from_rope(rope));
            w.finish_install(seq);
        });
        if let Some(t) = model.borrow_mut().tabs.get_mut(idx) {
            t.payload.md_wy_install.pending_full = None;
        }
        return;
    }
    model.borrow_mut().tabs[idx].payload.md_wy_install.pending_full = Some((seq, 0));
    md_wy_spawn_async_install_rope(model, idx, wy, rope, seq, cx);
}

/// Step3：`md_wy_feed_full` 的后台装配（Str 版）。完成回调两层守卫，任一不满足即丢弃
/// 结果（不装错、不泄漏）：
///   1) **tab 归属**：该 tab 的 `md_wy` 仍指向 `wy`（未关闭 / 未重建实体 / 未切走）；
///   2) **装配代号**：`wy.install_state.seq` 仍 == `seq`（期间又有新装配 / 同步喂源 /
///      换源 → 旧装配作废，不装陈旧整篇；代号检查收敛进 [`WyEditor::begin_install`]）。
/// 满足才 `install_open` 并 [`WyEditor::finish_install`] 清在途。整篇 O(n) 解析
/// （`Doc::open` + 派生标记计数）在 `background_executor`，UI 只做 O(1) 快照 + 完成
/// 回调里的 `install_open`（对齐 toggle ON 的既有异步通道）。
fn md_wy_spawn_async_install<H: 'static>(
    model: &Rc<RefCell<AppModel>>,
    tab_idx: usize,
    wy: Entity<WyEditor>,
    content: SharedString,
    seq: usize,
    cx: &mut Context<H>,
) {
    let model2 = model.clone();
    let wy_install = wy;
    cx.spawn(async move |this, cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                markdown_wysiwyg::WyEditor::open_from_str(content)
            })
            .await;
        let _ = this.update(cx, |_this, cx| {
            // 1) tab 归属守卫（保持原样）：md_wy 仍指向本实体（未关闭/未重建/未切走）。
            let owned = model2.borrow().tabs.get(tab_idx).is_some_and(|t| {
                t.payload.md_wy.as_ref() == Some(&wy_install)
            });
            if !owned {
                // 守卫不通过（切走/关闭/重建）→ 若 pending_full 仍挂本代号则清悬空标记
                //（已换新代号则保留新标记）。在途标志收在 WyEditor，随换代自清，不动。
                if let Some(tab) = model2.borrow_mut().tabs.get_mut(tab_idx)
                    && tab.payload.md_wy_install.pending_full.as_ref().is_some_and(|(s, _)| *s == seq)
                {
                    tab.payload.md_wy_install.pending_full = None;
                }
                return;
            }
            // 2) 装配代号陈旧世序（→ WyEditor 侧）：仍是最新代号才落盘。
            let current = wy_install.update(cx, |w, _| w.begin_install(seq));
            if current {
                wy_install.update(cx, |ed, _| {
                    ed.install_open(result);
                    ed.finish_install(seq);
                });
                let mut m = model2.borrow_mut();
                if let Some(tab) = m.tabs.get_mut(tab_idx) {
                    // Step3：异步整篇落地 → 预览已 == 目标，解除在途标记（增量桥恢复）。
                    tab.payload.md_wy_install.pending_full = None;
                }
                cx.notify();
            } else if let Some(tab) = model2.borrow_mut().tabs.get_mut(tab_idx)
                && tab.payload.md_wy_install.pending_full.as_ref().is_some_and(|(s, _)| *s == seq)
            {
                // 守卫不通过（换代后结果被丢弃）：若 pending_full 仍挂本代号则清掉悬空
                // 标记；已换新代号则保留新标记。
                tab.payload.md_wy_install.pending_full = None;
            }
        });
    })
    .detach();
}

/// Step3：`md_wy_feed_full_rope` 的后台装配（Rope 版，守卫/落地同
/// [`md_wy_spawn_async_install`]；成功后清 pending_full 与在途）。
fn md_wy_spawn_async_install_rope<H: 'static>(
    model: &Rc<RefCell<AppModel>>,
    tab_idx: usize,
    wy: Entity<WyEditor>,
    rope: Rope,
    seq: usize,
    cx: &mut Context<H>,
) {
    let model2 = model.clone();
    let wy_install = wy;
    cx.spawn(async move |this, cx| {
        let result = cx
            .background_executor()
            .spawn(async move {
                markdown_wysiwyg::WyEditor::open_from_rope(rope)
            })
            .await;
        let _ = this.update(cx, |_this, cx| {
            // 1) tab 归属守卫（保持原样）：md_wy 仍指向本实体（未关闭/未重建/未切走）。
            let owned = model2.borrow().tabs.get(tab_idx).is_some_and(|t| {
                t.payload.md_wy.as_ref() == Some(&wy_install)
            });
            if !owned {
                if let Some(tab) = model2.borrow_mut().tabs.get_mut(tab_idx)
                    && tab.payload.md_wy_install.pending_full.as_ref().is_some_and(|(s, _)| *s == seq)
                {
                    // 守卫不通过（切走/关闭/换代）→ 若仍挂本代号则清悬空标记；已换新代号保留。
                    tab.payload.md_wy_install.pending_full = None;
                }
                return;
            }
            // 2) 装配代号陈旧世序（→ WyEditor 侧）：仍是最新代号才落盘。
            let current = wy_install.update(cx, |w, _| w.begin_install(seq));
            if current {
                wy_install.update(cx, |ed, _| {
                    ed.install_open(result);
                    ed.finish_install(seq);
                });
                let mut m = model2.borrow_mut();
                if let Some(tab) = m.tabs.get_mut(tab_idx) {
                    tab.payload.md_wy_install.pending_full = None;
                }
                cx.notify();
            } else if let Some(tab) = model2.borrow_mut().tabs.get_mut(tab_idx)
                && tab.payload.md_wy_install.pending_full.as_ref().is_some_and(|(s, _)| *s == seq)
            {
                // 守卫不通过（切走/关闭/换代）→ 若仍挂本代号则清悬空标记；已换新代号保留。
                tab.payload.md_wy_install.pending_full = None;
            }
        });
    })
    .detach();
}

