//! file_ops 纯逻辑——[`PreparedText`] /
//! [`prepare_text`]（读预处理物化产物，纯函数后台线程可跑）/[`is_large_path`]（大文档
//! 后台打开判定）/[`AppModel::set_tab_text`]（会话恢复的未命名选项卡灌文本）。宿主
//! file_ops 流水线的 async / host 段（`load_file_text` / `spawn_progressive_open` /
//! `apply_open_chunk` / `open_file*`）仍留 shell，本模块只承载上述纯逻辑。

use std::path::Path;

use gpui::App;

use config::limits::OPEN_ASYNC_MIN_BYTES;

use crate::host::AppHost;
use crate::model::AppModel;

/// 读盘→归一化→Rope→行宽统计 的打开预处理产物：UI 线程据此 O(1) 装配，
/// 不在 UI 线程做任何 O(n) 重扫。可跨线程（`Rope`/`BTreeMap` 均 `Send`）。
pub struct PreparedText {
    /// 全文 Rope（编辑器 + WyEditor 预览共享，O(1) 克隆）。
    pub rope: ropey::Rope,
    /// 显示行数（`str::lines()` 语义，尾换行不追加行）。
    pub lines: usize,
    /// 字符总数（Unicode 标量）。
    pub chars: usize,
    /// 原文件字节数。
    pub src_len: usize,
    /// 全文最长行（字符数）。
    pub max_line_chars: usize,
    /// 行宽直方图（最宽行 / 横向滚动条量程依据；通常由后台线程算好）。
    pub line_width_histogram: editor::buffer::LineWidthHistogram,
}

/// 把归一化文本预处理成编辑器装配产物（纯函数，可在后台线程跑）：
/// Rope::from + 行/字计数 + 行宽直方图 —— 这一整条 O(n) 链路（大文档合计
/// 几百 ms）由调用方决定放在后台线程，UI 线程不再背。
pub fn prepare_text(text: String) -> PreparedText {
    let src_len = text.len();
    let t = std::time::Instant::now();
    let lines = text.lines().count().max(1);
    let chars = text.chars().count();
    let counts_ms = t.elapsed().as_millis();
    let t = std::time::Instant::now();
    let rope = ropey::Rope::from(text.as_str());
    let rope_ms = t.elapsed().as_millis();
    let t = std::time::Instant::now();
    let (max_line_chars, line_width_histogram) = editor::buffer::compute_line_stats(&rope);
    let stats_ms = t.elapsed().as_millis();
    if std::env::var_os("MD_OPEN_DEBUG").is_some() {
        eprintln!(
            "[MD_OPEN_DEBUG] 准备文本: 计数(lines+chars)={counts_ms} ms, Rope::from({src_len} B)={rope_ms} ms, compute_line_stats({lines} 行)={stats_ms} ms"
        );
    }
    PreparedText {
        rope,
        lines,
        chars,
        src_len,
        max_line_chars,
        line_width_histogram,
    }
}

/// 是否走「后台打开」流水线：按文件大小元数据判断（无需读盘）。
pub fn is_large_path(p: &Path) -> bool {
    std::fs::metadata(p)
        .map(|m| m.len() > OPEN_ASYNC_MIN_BYTES)
        .unwrap_or(false)
}

impl AppModel {
    /// 把文本灌入**已建好**的选项卡（会话恢复的未命名选项卡用）：与宿主
    /// `load_file_text` 走同一条 O(n) 装配管线（`prepare_text` → Rope + 行宽统计 +
    /// 行/字计数），但不设 `file_path`（保持「未命名」语义）、不建 Markdown 预览
    /// （未命名选项卡 = 纯文本）。`make_tab` 已建空编辑器。
    pub fn set_tab_text(&mut self, host: &dyn AppHost, idx: usize, text: String, cx: &mut App) {
        if idx >= self.tabs.len() {
            return;
        }
        self.search_scan = None; // 同一选项卡内容被替换 → 搜索缓存失效
        let PreparedText {
            rope,
            lines,
            chars,
            max_line_chars,
            line_width_histogram,
            src_len: _,
        } = prepare_text(text);
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        tab.editor.update(cx, |ed, _cx| {
            // O(1) 装配：与 load_file_text 同款（Rope 共享、统计就地算好）。
            ed.set_rope_with_stats(rope, max_line_chars, line_width_histogram);
            // 未命名选项卡无文件 → 不设语言（保持纯文本高亮缺省）。
            ed.set_language(None);
        });
        tab.editor_pending = false;
        // 未命名选项卡正文以编辑器 Rope 为准，不整份镜像到 content（同 load_file_text）。
        tab.content = gpui::SharedString::default();
        tab.line_count = lines;
        tab.char_count = chars;
        tab.cursor_row = 0;
        tab.cursor_col = 0;
        tab.dirty = false;
        host.notify_host(cx);
    }
}
