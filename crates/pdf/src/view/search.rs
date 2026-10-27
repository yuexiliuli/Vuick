//! PDF 阅读器子模块：页文本层懒加载缓存 + P3 全文检索（原 `view.rs` 拆出）。
//!
//! - **文本缓存**：可见窗口文本按需预取（[`super::PdfViewer::prefetch_texts`]，
//!   [`TEXT_INFLIGHT`] 并发封顶），命中测试 / 选区复制 / 高亮前已就绪；LRU 淘汰
//!   （[`TEXT_CACHE_PAGES`]）。
//! - **全文检索**：由侧边栏搜索框驱动——查询经 [`super::PdfViewer::set_search`] 注入 →
//!   250ms 防抖 → worker 后台全页分片扫描 → [`super::PdfViewer::collect_search_results`]
//!   收集进 `search_matches`（代次过期即弃）→ 阅读器渲染命中高亮，侧边栏展示计数与
//!   页列表。阅读器只负责检索执行、高亮与跳转。

use super::*;

impl PdfViewer {
    /// 文本页入 LRU 缓存（超上限淘汰最久未用）。
    pub(super) fn cache_text_page(&mut self, page: usize, text: PageText) {
        self.text_lru.retain(|&p| p != page);
        self.text_lru.push_back(page);
        self.text_pages.insert(page, text);
        while self.text_pages.len() > TEXT_CACHE_PAGES {
            if let Some(evict) = self.text_lru.pop_front() {
                self.text_pages.remove(&evict);
            } else {
                break;
            }
        }
    }

    /// 测试探针（shell `app::tests` 用）：**同步**把某页文本读入缓存（阻塞等 worker，
    /// 超时/失败返回 false）。摆脱「可见页预取」时序——shell 测试定位字符盒（拖选起点）
    /// 前先调它，保证 `char_window_pos` 对任意页可用。缺文本层的页返回 `true`（空文本）。
    pub fn load_text_for_test(&mut self, page: usize) -> bool {
        let Some(worker) = self.worker.clone() else {
            return false;
        };
        if page >= self.dims.len() || self.doc_id == 0 {
            return false;
        }
        if self.text_pages.contains_key(&page) {
            return true;
        }
        let rx = worker.request_text(self.doc_id, page);
        match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(res) => {
                if let Some(text) = res {
                    self.cache_text_page(page, text);
                }
                true
            }
            Err(_) => false,
        }
    }

    /// 回收已完成的在途文本结果。返回是否有**新**文本入缓存（供调用方决定是否重绘）。
    pub(super) fn collect_text_results(&mut self) -> bool {
        let finished: Vec<(usize, Option<PageText>)> = self
            .pending_text
            .iter_mut()
            .filter_map(|(p, rx)| match rx.try_recv() {
                Ok(res) => Some((*p, res)),
                Err(_) => None,
            })
            .collect();
        let mut any = false;
        for (page, res) in finished {
            self.pending_text.remove(&page);
            if let Some(text) = res {
                self.cache_text_page(page, text);
                any = true;
            }
        }
        any
    }

    /// 可见窗口文本预取（低优先级懒加载，封顶 [`TEXT_INFLIGHT`] 并发）：
    /// 文本按需缓存，交互前已就绪；worker 侧调度已保证 Render 优先于 Text，
    /// 预取不会拖慢滚动画帧。
    pub(super) fn prefetch_texts(&mut self, start: usize, end: usize) {
        let Some(worker) = self.worker.clone() else {
            return;
        };
        if self.pending_text.len() >= TEXT_INFLIGHT {
            return;
        }
        let mut budget = TEXT_INFLIGHT - self.pending_text.len();
        let last = self.dims.len().saturating_sub(1);
        for page in start..=end.min(last) {
            if budget == 0 {
                break;
            }
            if self.text_pages.contains_key(&page) || self.pending_text.contains_key(&page) {
                continue;
            }
            let rx = worker.request_text(self.doc_id, page);
            self.pending_text.insert(page, rx);
            budget -= 1;
        }
    }

    // ---- P3 全文检索：由**侧边栏搜索框**驱动（PDF 内不自建搜索框）----
    //
    // 查询经 [`super::PdfViewer::set_search`] 注入 → 250ms 防抖 → worker 后台全页扫描 →
    // 结果经 [`super::PdfViewer::collect_search_results`] 收集进 `search_matches` → 阅读器
    // 渲染命中高亮，侧边栏展示计数与页列表。阅读器只负责检索执行、高亮与跳转。

    /// 注入新查询（侧边栏搜索框内容变化时调用）。空串立即清空命中；非空防抖后后台检索。
    pub fn set_search(&mut self, query: &str, cx: &mut Context<Self>) {
        let q = query.trim().to_string();
        if self.search_query == q {
            return;
        }
        self.search_query = q;
        if self.search_query.is_empty() {
            self.search_matches.clear();
            self.search_truncated = false;
            self.search_active = 0;
            self.search_pending = None;
            cx.notify();
            return;
        }
        self.schedule_search(cx);
    }

    /// 侧边栏读取检索状态：`(命中数, 是否截断, 当前激活命中序号)`。
    pub fn search_summary(&self) -> (usize, bool, usize) {
        (
            self.search_matches.len(),
            self.search_truncated,
            self.search_active,
        )
    }

    /// 命中上下文片段（侧边栏逐行展示）：`(页号, 前5字+目标+后5字)`，封顶 60 行。
    /// 片段在 worker 搜到时就地生成（`PdfSearchHit::snippet`），不依赖阅读器文本缓存。
    pub fn search_snippets(&self) -> Vec<(usize, String)> {
        self.search_matches
            .iter()
            .take(60)
            .map(|h| (h.page, h.snippet.clone()))
            .collect()
    }

    /// 跳到某页上**第一个命中**（侧边栏点"第 N 页"）。
    pub fn focus_match_on_page(&mut self, page: usize, cx: &mut Context<Self>) {
        let Some(pos) = self.search_matches.iter().position(|h| h.page == page) else {
            self.scroll_to_page(page);
            cx.notify();
            return;
        };
        self.search_active = pos;
        self.scroll_to_page(page);
        self.prefetch_texts(page, page);
        cx.notify();
    }

    /// 查询变化 → 防抖调度检索（防抖 250ms，只允许最新一次生效）。
    pub(super) fn schedule_search(&mut self, cx: &mut Context<Self>) {
        self.search_seq = self.search_seq.wrapping_add(1);
        let seq = self.search_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(250))
                .await;
            let _ = this.update(cx, |v, cx| {
                if v.search_seq == seq {
                    v.run_search(cx);
                }
            });
        })
        .detach();
    }

    /// 向 worker 发起文档级检索（后台低优先级）；结果通道暂存，帧循环里收集。
    pub(super) fn run_search(&mut self, cx: &mut Context<Self>) {
        let q = self.search_query.trim();
        if q.is_empty() {
            self.search_matches.clear();
            self.search_truncated = false;
            self.search_active = 0;
            self.search_pending = None;
            cx.notify();
            return;
        }
        let Some(worker) = self.worker.clone() else {
            return;
        };
        self.search_pending = None; // 丢弃未消费的旧结果
        let rx = worker.search(self.doc_id, q.to_string());
        self.search_pending = Some(rx);
        self.search_pending_seq = self.search_seq;
    }

    /// 帧循环：收集在途检索结果（代次过期即丢弃）。有**新**有效结果 → true（调用方 notify）。
    pub(super) fn collect_search_results(&mut self) -> bool {
        let Some(rx) = &self.search_pending else {
            return false;
        };
        let res = match rx.try_recv() {
            Ok(res) => res,
            Err(_) => return false,
        };
        self.search_pending = None;
        let Some(results) = res else {
            return false;
        };
        if self.search_pending_seq != self.search_seq {
            return false; // 过期结果
        }
        self.search_matches = results.hits;
        self.search_truncated = results.truncated;
        self.search_active = 0;
        true
    }

    /// 前进/后退一个命中（循环），滚动到命中页并预取其文本层（高亮立即可用）。
    /// 侧边栏「上一处/下一处」按钮驱动。
    pub fn move_match(&mut self, delta: i64, cx: &mut Context<Self>) {
        let Some(next) =
            advance_match_index(self.search_matches.len(), self.search_active, delta)
        else {
            return;
        };
        self.search_active = next;
        let page = self.search_matches[next].page;
        self.scroll_to_page(page);
        self.prefetch_texts(page, page);
        cx.notify();
    }
}
