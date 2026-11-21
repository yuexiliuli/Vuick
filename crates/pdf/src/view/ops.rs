//! PDF 阅读器子模块：待保存模型（内存级页操作 / 保存 / 撤销 / `open_from_bytes`）：
//! 页操作只改 worker 内存工作副本，成功回传新镜像 → 内存级重载；撤销栈记录逆操作。

// 页操作回调状态类型较长 → 放行
#![allow(clippy::type_complexity)]

use super::*;
use super::open::enforce_undo_mem_budget;
use super::open::extract_target_precheck;
use super::open::read_merge_source_bounded;

/// 待保存页面操作的种类（[`PdfViewer::submit_mem_op`]）：只改 worker 内存工作副本。
enum MemOpKind {
    /// 在 `at_index` 处插入一页空白页（0 基；= 当前页数即末尾追加）。`anchor` = 重载后要
    /// 定位的阅读页（`Some` 单列跳页）；网格模式插入 → `None`（留在网格、不滚动阅读）。
    Insert {
        at_index: usize,
        w: f32,
        h: f32,
        anchor: Option<usize>,
    },
    /// 删除第 `index` 页。`anchor` = 重载后定位页（删除保位）。
    Delete { index: usize, anchor: Option<usize> },
}

impl PdfViewer {
    /// 提交一次**内存级**页面操作（插页/删页）：worker 只改内存工作副本、**不写盘**，
    /// 成功回传新镜像 → viewer 内存级重载；待存注释页号随后平移/丢弃；撤销栈记录
    /// 逆操作（恢复操作前字节镜像 + 注释快照）。失败记 [`Self::last_modify_error`]。
    fn submit_mem_op(&mut self, op: MemOpKind, cx: &mut Context<Self>) {
        let Some(worker) = self.worker.clone() else {
            self.last_modify_error = Some("文档尚未加载".into());
            return;
        };
        if self.doc_id == 0 {
            self.last_modify_error = Some("文档尚未加载".into());
            return;
        }
        let doc_id = self.doc_id;
        self.last_modify_error = None;
        let anchor = match op {
            MemOpKind::Insert { anchor, .. } | MemOpKind::Delete { anchor, .. } => anchor,
        };
        // 页号平移闭包：插页 → 原 ≥ at_index 的待存注释 +1；删页 → 原 > index 的 -1、
        // == index 的丢弃（在待存注释上原地平移，确保结构变化后叠加重载仍对位）。
        let shift: Box<dyn Fn(&mut Vec<PendingAnnot>) + Send> = match op {
            MemOpKind::Insert { at_index, .. } => Box::new(move |annots| {
                for a in annots.iter_mut() {
                    if a.page >= at_index {
                        a.page += 1;
                    }
                }
            }),
            MemOpKind::Delete { index, .. } => Box::new(move |annots| {
                annots.retain(|a| a.page != index);
                for a in annots.iter_mut() {
                    if a.page > index {
                        a.page -= 1;
                    }
                }
            }),
        };
        // ⚠️ 必须 `.detach()`（K37）：不 detach 句柄在 drop 即取消，页操作静默不执行。
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    match op {
                        MemOpKind::Insert { at_index, w, h, .. } => {
                            worker.mem_insert_blank_page(doc_id, at_index, w, h)
                        }
                        MemOpKind::Delete { index, .. } => worker.mem_delete_page(doc_id, index),
                    }
                })
                .await;
            let _ = this.update(cx, move |v, cx| match res {
                Ok(mem) => {
                    v.last_modify_error = None;
                    // 撤销条目：恢复操作前的内存镜像 + 操作前待存注释快照
                    v.push_undo_restore(mem.before, v.pending_annots.clone());
                    v.pending_page_ops += 1;
                    shift(&mut v.pending_annots);
                    // 结构变了 → 灰框几何可能漂移，清空最稳
                    v.focused_annot = None;
                    v.open_from_bytes(mem.after, anchor, cx);
                    cx.notify();
                }
                Err(e) => {
                    v.last_modify_error = Some(format!("{e:#}"));
                    // R2（对齐 reorder/batch_delete/split/merge 的 Err 分支）：失败无 reload →
                    // 显式复位「保持网格」待办位，防残留波及后续无关重载意外回网格。
                    v.grid_hold_on_reload = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 在 `page_index` 之后插入一页空白页（**内存级**：只改 worker 内存工作副本并重载，
    /// **不写盘**；`save()` 时才落盘）。（新页尺寸 = 当前页。）
    /// 网格模式下插入：置 `grid_hold_on_reload` 保持网格 + anchor = `None`（留在网格、
    /// 不滚动到新页——网格不消费阅读滚动，退出网格仍回原阅读位置）；单列插入保持
    /// anchor 滚动到新页的既有行为。
    pub fn insert_blank_page_after(&mut self, page_index: usize, cx: &mut Context<Self>) {
        let Some(&(w, h)) = self.dims.get(page_index) else {
            self.last_modify_error = Some(format!("页码越界：第 {page_index} 页"));
            return;
        };
        if self.grid_mode {
            self.grid_hold_on_reload = true;
        }
        let anchor = if self.grid_mode {
            None
        } else {
            Some(page_index + 1)
        };
        self.submit_mem_op(
            MemOpKind::Insert {
                at_index: page_index + 1,
                w,
                h,
                anchor,
            },
            cx,
        );
    }

    /// 删除 `page_index` 页（**内存级**：不写盘；仅剩 1 页拒绝）；删除后滚动保位（夹取）。
    pub fn delete_page(&mut self, page_index: usize, cx: &mut Context<Self>) {
        if page_index >= self.dims.len() {
            self.last_modify_error = Some(format!("页码越界：第 {page_index} 页"));
            return;
        }
        if self.dims.len() <= 1 {
            self.last_modify_error = Some("不能删除仅剩的 1 页".into());
            return;
        }
        self.submit_mem_op(
            MemOpKind::Delete {
                index: page_index,
                anchor: Some(page_index),
            },
            cx,
        );
    }

    /// **内存级重排页序**（wave7a 页操作原语；不写盘）：`new_order` 为 0 基**全排列**
    /// （长度=当前页数、0..n 各恰一次；非法 → worker 严格校验 Err，文档不变、无副作用）。
    /// 成功按既有 mem op 链路重载并 notify；待存注释页号按新序反查表平移；失败记
    /// [`Self::last_modify_error`]（不吞）。撤销 = 恢复操作前字节镜像（`UndoEntry::RestoreMemory`）。
    pub fn reorder_pages(&mut self, new_order: Vec<usize>, cx: &mut Context<Self>) {
        let Some(worker) = self.worker.clone() else {
            self.last_modify_error = Some("文档尚未加载".into());
            return;
        };
        if self.doc_id == 0 {
            self.last_modify_error = Some("文档尚未加载".into());
            return;
        }
        let n = self.dims.len();
        // view 侧轻预检（worker 仍做权威校验）：长度不匹配先给中文文案。
        if new_order.len() != n {
            self.last_modify_error = Some(format!(
                "重排页序长度应为 {n}（当前页数），实际 {}",
                new_order.len()
            ));
            return;
        }
        // P2-A2 identity 早退：`new_order == 恒等序（0..n）` → 无实际变化，不提交 worker
        // （不整档重编码）、不置脏（pending_page_ops 不动）、不撤销快照——与 wave6「拖回
        // 原位不提交」同语义，顺带避免无谓的 reload。
        if new_order.iter().copied().eq(0..n) {
            return;
        }
        let doc_id = self.doc_id;
        self.last_modify_error = None;
        // 反查表：新位置 → 原页（inv[old] = old 页落到的新位置；待存注释按此平移）。
        let mut inv = vec![0usize; n];
        for (pos, &old) in new_order.iter().enumerate() {
            if old < n {
                inv[old] = pos;
            }
        }
        // ⚠️ 必须 `.detach()`（K37）：不 detach 句柄在 drop 即取消，重排静默不执行。
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { worker.mem_reorder_pages(doc_id, new_order) })
                .await;
            let _ = this.update(cx, move |v, cx| match res {
                Ok(mem) => {
                    v.last_modify_error = None;
                    // 撤销条目：恢复操作前的内存镜像 + 操作前待存注释快照
                    v.push_undo_restore(mem.before, v.pending_annots.clone());
                    v.pending_page_ops += 1;
                    // 待存注释按反查表平移（页随重排落到新位置）。
                    for a in v.pending_annots.iter_mut() {
                        if let Some(&np) = inv.get(a.page) {
                            a.page = np;
                        }
                    }
                    // 结构变了 → 灰框几何可能漂移，清空最稳。
                    v.focused_annot = None;
                    v.open_from_bytes(mem.after, None, cx);
                    cx.notify();
                }
                Err(e) => {
                    v.last_modify_error = Some(format!("{e:#}"));
                    // wave6：重排失败不会有 reload → 显式复位「保持网格」位，防残留波及
                    // 后续无关重载意外回网格。
                    v.grid_hold_on_reload = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// **内存级批量删除页面**（wave7a 页操作原语；不写盘）：`pages` 为 0 基待删页数组
    /// （逐页 < 当前页数、重复去重、保留至少 1 页；非法 → worker 校验 Err，文档不变）。
    /// worker 按索引**降序**循环删除（升序删会因下标位移错删），成功返回删除后页数并按
    /// mem op 链路重载；待存注释对删除页丢弃、其后的页号顺移；失败记 [`Self::last_modify_error`]
    /// （不吞）。撤销 = 一次恢复操作前字节镜像（整批可撤）。
    pub fn batch_delete_pages(&mut self, pages: Vec<usize>, cx: &mut Context<Self>) {
        let Some(worker) = self.worker.clone() else {
            self.last_modify_error = Some("文档尚未加载".into());
            // R2：失败无 reload → 复位「保持网格」待办位（防残留波及后续无关重载）。
            self.grid_hold_on_reload = false;
            return;
        };
        if self.doc_id == 0 {
            self.last_modify_error = Some("文档尚未加载".into());
            self.grid_hold_on_reload = false;
            return;
        }
        let n = self.dims.len();
        self.last_modify_error = None;
        if pages.is_empty() {
            // 空清单 = no-op：不发消息、不重载（避免无谓的重新编码）。
            return;
        }
        // 预计算待存注释平移（与 worker 同口径：每页 < n、去重后全有效才发）。
        // 删除集：升序排序用于「页号顺移」计数；并发 worker 仍做权威校验。
        let mut deleted: Vec<usize> = pages.to_vec();
        deleted.sort_unstable();
        deleted.dedup();
        // view 侧轻预检（worker 仍做权威校验）：越界 / 删光全部页先给中文文案。
        if deleted.iter().any(|&p| p >= n) {
            self.last_modify_error = Some("批量删除页码越界（应逐页 < 当前页数）".into());
            self.grid_hold_on_reload = false;
            return;
        }
        if deleted.len() >= n {
            self.last_modify_error = Some("批量删除将删光全部页面（须保留至少 1 页）".into());
            self.grid_hold_on_reload = false;
            return;
        }
        let doc_id = self.doc_id;
        // 归一化注释页号：删除页丢弃；其后页号减去「<= 该页的删除数」。
        let shift: Box<dyn Fn(&mut Vec<PendingAnnot>) + Send> = {
            let deleted = deleted.clone();
            Box::new(move |annots| {
                annots.retain(|a| deleted.binary_search(&a.page).is_err());
                for a in annots.iter_mut() {
                    let removed = deleted.iter().filter(|&&d| d < a.page).count();
                    a.page -= removed;
                }
            })
        };
        // ⚠️ 必须 `.detach()`（K37）：不 detach 句柄在 drop 即取消，批删静默不执行。
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { worker.mem_batch_delete_pages(doc_id, pages) })
                .await;
            let _ = this.update(cx, move |v, cx| match res {
                Ok(mem) => {
                    v.last_modify_error = None;
                    // 撤销条目：恢复操作前的内存镜像 + 操作前待存注释快照（整批一次可撤）。
                    v.push_undo_restore(mem.before, v.pending_annots.clone());
                    v.pending_page_ops += 1;
                    shift(&mut v.pending_annots);
                    // 结构变了 → 灰框几何可能漂移，清空最稳。
                    v.focused_annot = None;
                    v.open_from_bytes(mem.after, None, cx);
                    cx.notify();
                }
                Err(e) => {
                    v.last_modify_error = Some(format!("{e:#}"));
                    // R2（对齐 reorder_pages Err 分支）：失败无 reload → 复位保持网格待办位。
                    v.grid_hold_on_reload = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 把 `page_index` 页提取成独立 PDF 文件并原子写盘（源文档不变，**即时生效、不影响
    /// 当前 dirty**——提取本质是「另存为新文件」）。失败记 [`Self::last_modify_error`]，
    /// 成功置 [`Self::last_extract_success`]（壳层 `_pdf_sub` observe 消费弹成功 toast）。
    pub fn extract_page_to(
        &mut self,
        page_index: usize,
        out_path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if page_index >= self.dims.len() {
            self.last_modify_error = Some(format!("页码越界：第 {page_index} 页"));
            return;
        }
        let Some(worker) = self.worker.clone() else {
            self.last_modify_error = Some("文档尚未加载".into());
            return;
        };
        if self.doc_id == 0 {
            self.last_modify_error = Some("文档尚未加载".into());
            return;
        }
        // 同步预检输出路径可写性：注定失败的提取（目标目录缺失 / 目标为现有目录 /
        // 目标 == 当前文档路径 P1）在**菜单决策点即置错**，不复现「成功 toast 恒假」——
        // 否则异步写盘失败也先弹「已提取页面」、随后 observe 再补失败（自相矛盾双 toast）。
        if let Some(msg) = extract_target_precheck(&self.path, &out_path) {
            self.last_modify_error = Some(msg);
            return;
        }
        let doc_id = self.doc_id;
        self.last_modify_error = None;
        let success_msg = format!("第 {} 页 → {}", page_index + 1, out_path.display());
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { worker.extract_page_to(doc_id, page_index, out_path) })
                .await;
            let _ = this.update(cx, move |v, cx| match res {
                Ok(_) => {
                    v.last_modify_error = None;
                    // 成功结果信号：置位 + notify → 壳层 `_pdf_sub` observe 消费一次弹
                    // 成功 toast（成败提示都收敛到异步完成的确定性结果）。
                    v.last_extract_success = Some(success_msg);
                    cx.notify();
                }
                Err(e) => {
                    v.last_modify_error = Some(format!("{e:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 取走最近一次「提取页面」**成功**提示文案（消费后清空，供壳层 observe 一次性弹
    /// 成功 toast；`None` = 无待提示成功）。
    pub fn take_last_extract_success(&mut self) -> Option<String> {
        self.last_extract_success.take()
    }

    /// **提取多页到新文件**（wave7b；副本语义）：`pages` 为 0 基待提取页数组（**可任意
    /// 顺序/非连续**、可少于全页；worker 严格校验 + 去重，非法 → Err）。成功后**源文档
    /// 不变**——不重载、不入撤销栈（提取=另存），实际写入的（去重后）页数经
    /// [`Self::last_extract_success`] 供壳层弹成功 toast。失败记 [`Self::last_modify_error`]（不吞）。
    pub fn export_pages_to(
        &mut self,
        pages: Vec<usize>,
        out_path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let Some(worker) = self.worker.clone() else {
            self.last_modify_error = Some("文档尚未加载".into());
            return;
        };
        if self.doc_id == 0 {
            self.last_modify_error = Some("文档尚未加载".into());
            return;
        }
        let n = self.dims.len();
        // view 侧轻预检（worker 仍做权威校验）：空清单 / 越界先给中文文案。
        if pages.is_empty() {
            self.last_modify_error = Some("提取页清单为空（须至少 1 页）".into());
            return;
        }
        if pages.iter().any(|&p| p >= n) {
            self.last_modify_error = Some(format!("提取页码越界（应逐页 < 当前页数 {n}）"));
            return;
        }
        // 同步预检输出路径可写性（对齐 extract_page_to：注定失败的提取在决策点即置错，
        // 含目标 == 当前文档路径的 P1 覆盖防护）。
        if let Some(msg) = extract_target_precheck(&self.path, &out_path) {
            self.last_modify_error = Some(msg);
            return;
        }
        let doc_id = self.doc_id;
        let out_display = out_path.display().to_string();
        self.last_modify_error = None;
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { worker.export_pages_to(doc_id, pages, out_path) })
                .await;
            let _ = this.update(cx, move |v, cx| match res {
                Ok(written) => {
                    v.last_modify_error = None;
                    // 副本语义：不改变当前文档、不重载、不入撤销栈；成功信号供壳层 toast。
                    // 网格提取的选中集消费清空**只在异步成功后**执行——失败（含同步预检失败）
                    // 不清选保持可重试（本函数唯一生产调用方 = `grid_apply_extract` 网格提取；
                    // 其它调用方网格选中集为空，本清选无副作用）。
                    v.grid_selected.clear();
                    v.last_extract_success = Some(format!("已导出 {written} 页 → {out_display}"));
                    cx.notify();
                }
                Err(e) => {
                    v.last_modify_error = Some(format!("{e:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// **拆分区间到新文件并移除**（wave7b）：`range = (start, end)` 含端点的 0 基区间。
    /// worker 把区间页按**原序**提取落盘 `out_path`（先写文件，失败则整个操作失败）+ 从
    /// 当前 stores 镜像**移除**该区间（降序删 + 撤销快照）。成功后当前文档重载为移除后
    /// 镜像（页数降）、入撤销栈（一次性可撤：当前回 N 页、新文件仍在）；待存注释对区间内
    /// 丢弃、其后页号顺移；成功信号经 [`Self::last_extract_success`]。失败记
    /// [`Self::last_modify_error`]（不吞）。
    pub fn split_pages_to(
        &mut self,
        range: (usize, usize),
        out_path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let Some(worker) = self.worker.clone() else {
            self.last_modify_error = Some("文档尚未加载".into());
            self.grid_hold_on_reload = false;
            return;
        };
        if self.doc_id == 0 {
            self.last_modify_error = Some("文档尚未加载".into());
            self.grid_hold_on_reload = false;
            return;
        }
        let n = self.dims.len();
        let (start, end) = range;
        // view 侧轻预检（worker 仍做权威校验）。
        if start > end {
            self.last_modify_error = Some(format!("拆分区间起点 {start} 不能大于终点 {end}"));
            self.grid_hold_on_reload = false;
            return;
        }
        if end >= n {
            self.last_modify_error = Some(format!("拆分页码越界：{start}..={end}（共 {n} 页）"));
            self.grid_hold_on_reload = false;
            return;
        }
        if end - start + 1 >= n {
            self.last_modify_error = Some("拆分将移除全部页面（须保留至少 1 页）".into());
            self.grid_hold_on_reload = false;
            return;
        }
        if let Some(msg) = extract_target_precheck(&self.path, &out_path) {
            self.last_modify_error = Some(msg);
            self.grid_hold_on_reload = false;
            return;
        }
        let span = end - start + 1;
        let doc_id = self.doc_id;
        let out_display = out_path.display().to_string();
        let success_msg = format!("已将第 {}..{} 页拆分到 {out_display}", start + 1, end + 1);
        // 待存注释页号归一化：区间内丢弃；其后页号减去区间长（worker 已权威移除区间）。
        // 保留的注释页 > end ≥ span-1 → 减法不潜溢。
        let shift: Box<dyn Fn(&mut Vec<PendingAnnot>) + Send> = Box::new(move |annots| {
            annots.retain(|a| a.page < start || a.page > end);
            for a in annots.iter_mut() {
                if a.page > end {
                    a.page -= span;
                }
            }
        });
        self.last_modify_error = None;
        // ⚠️ 必须 `.detach()`（K37）：不 detach 句柄在 drop 即取消，拆分静默不执行。
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { worker.split_to_file(doc_id, (start, end), out_path) })
                .await;
            let _ = this.update(cx, move |v, cx| match res {
                Ok((_written, mem)) => {
                    v.last_modify_error = None;
                    v.push_undo_restore(mem.before, v.pending_annots.clone());
                    v.pending_page_ops += 1;
                    shift(&mut v.pending_annots);
                    v.focused_annot = None;
                    v.last_extract_success = Some(success_msg);
                    v.open_from_bytes(mem.after, None, cx);
                    cx.notify();
                }
                Err(e) => {
                    v.last_modify_error = Some(format!("{e:#}"));
                    // R2：失败无 reload → 复位保持网格待办位（防残留波及后续无关重载）。
                    v.grid_hold_on_reload = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// **合并另一 PDF 到当前（追加）**（wave7b）：**读端**（本函数 background_executor 线程）
    /// 对 `src_path` 做「元数据预检 + `Read::take(上限+1)` 真边界」的有界读盘（超限即
    /// Err，见 [`read_merge_source_bounded`]），把其字节发 pdfium worker 追加到当前文档尾部
    /// （index = 当前页数）——worker 不再同步读盘、不再因合并源阻塞网格/渲染队列。成功后
    /// 当前文档重载为合并后镜像（页数升）、入撤销栈（一次性可撤：回 N 页且待存注释快照
    /// 还原）；失败记 [`Self::last_modify_error`] 且当前文档/待存状态不变。
    /// 追加不搬移既有页 → 待存注释页号无需平移。
    pub fn merge_pdf_file(&mut self, src_path: PathBuf, cx: &mut Context<Self>) {
        let Some(worker) = self.worker.clone() else {
            self.last_modify_error = Some("文档尚未加载".into());
            self.grid_hold_on_reload = false;
            return;
        };
        if self.doc_id == 0 {
            self.last_modify_error = Some("文档尚未加载".into());
            self.grid_hold_on_reload = false;
            return;
        }
        let doc_id = self.doc_id;
        self.last_modify_error = None;
        // ⚠️ 必须 `.detach()`（K37）：不 detach 句柄在 drop 即取消，合并静默不执行。
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    // 读端：元数据预检 + 有界读盘（真边界，超限不送字节进 worker）→ 字节消息。
                    let src_bytes = read_merge_source_bounded(&src_path)?;
                    worker.merge_from_bytes(doc_id, src_bytes)
                })
                .await;
            let _ = this.update(cx, move |v, cx| match res {
                Ok(mem) => {
                    v.last_modify_error = None;
                    v.push_undo_restore(mem.before, v.pending_annots.clone());
                    v.pending_page_ops += 1;
                    v.focused_annot = None;
                    v.open_from_bytes(mem.after, None, cx);
                    cx.notify();
                }
                Err(e) => {
                    v.last_modify_error = Some(format!("{e:#}"));
                    // R2：失败无 reload → 复位保持网格待办位（防残留波及后续无关重载）。
                    v.grid_hold_on_reload = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    // ---- 网格工具栏页操作执行 seam（wave8）：确认框/文件选择**之后**的动作，测试直调 ----

    /// 网格工具栏「删除(n)」确认后的执行 seam：整批删除 `pages`（按钮 handler 从选中集
    /// 快照传入）。页数变化重载前先置 `grid_hold_on_reload`，令 `apply_loaded` 保持网格
    /// 模式（不退出到阅读视图）；删除成功经重载自动清空选中集。失败走 `batch_delete_pages`
    /// 既有错误面（`last_modify_error` 中文报错），不清选、不吞错。
    pub(crate) fn grid_apply_delete(&mut self, pages: Vec<usize>, cx: &mut Context<Self>) {
        if pages.is_empty() {
            return;
        }
        if self.grid_mode {
            self.grid_hold_on_reload = true;
        }
        self.batch_delete_pages(pages, cx);
    }

    /// 网格工具栏「拆分(n)」确认后的执行 seam：按 `[min,max]` 闭区间把区间页拆分到
    /// `out_path` 并从当前移除。页数变化重载前先置 `grid_hold_on_reload` 保持网格模式；
    /// 成功重载后选中集自动清。失败走 `split_pages_to` 既有错误面（不吞错）。
    pub(crate) fn grid_apply_split(
        &mut self,
        range: (usize, usize),
        out_path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if self.grid_mode {
            self.grid_hold_on_reload = true;
        }
        self.split_pages_to(range, out_path, cx);
    }

    /// 网格工具栏「提取(n)」确认后的执行 seam：按 `pages`（**保持勾选升序**——本 seam 是
    /// 唯一权威排序点，按钮 handler 从 HashSet 选中集快照传入的无序数组在此归序）导出副本
    /// 到 `out_path`——源文档不变、不重载、不入撤销栈（`export_pages_to` 副本语义）。导出
    /// **异步成功**后选中集消费清空（本操作无重载、选中不会自动清；清选落在成功回调，
    /// 异步写盘失败不清选保持可重试）；同步预检失败（如目标目录缺失）已由 `export_pages_to`
    /// 置 `last_modify_error` → 不清选保持可重试，不吞错。
    pub(crate) fn grid_apply_extract(
        &mut self,
        mut pages: Vec<usize>,
        out_path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if pages.is_empty() {
            return;
        }
        // 勾选页按升序导出（HashSet 快照无序；worker 按给定序逐段 import，故先归序才能
        // 保证产物页序 = 源页序升序；页下标天然无重复——来自勾选集快照）。
        pages.sort_unstable();
        self.export_pages_to(pages, out_path, cx);
    }

    /// 网格工具栏「合并」确认后的执行 seam：把 `src_path` 全部页追加到当前文档尾部。
    /// 页数变化重载前先置 `grid_hold_on_reload` 保持网格模式；成功重载后选中集自动清。
    /// 失败走 `merge_pdf_file` 既有错误面（不吞错）。
    pub(crate) fn grid_apply_merge(&mut self, src_path: PathBuf, cx: &mut Context<Self>) {
        if self.grid_mode {
            self.grid_hold_on_reload = true;
        }
        self.merge_pdf_file(src_path, cx);
    }

    // ---- 网格工具栏按钮点击（wave8 接线）：确认框 / 文件选择 → 上述 seam ----
    // 全部经 `window.prompt` + `rfd::AsyncFileDialog` 异步流程（K167：原生对话框/文件框
    // 一律后台 spawn，禁在持 gpui App RefCell 的事件帧内同步弹；headless 测试构建
    // `#[cfg(test)]` 下走 no-op 桩，行为只经 seam 单测覆盖——对话框真机观感留人工）。

    /// 网格工具栏「删除(n)」点击：快照选中集 → 破坏性确认框 → `grid_apply_delete`。
    /// 空选中早退（按钮禁用态兜底，不触发）。文案对齐既有删除确认风格 + 可撤销提示。
    pub(crate) fn grid_delete_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(not(test))]
        {
            let pages: Vec<usize> = self.grid_selected.iter().copied().collect();
            if pages.is_empty() {
                return;
            }
            let n = pages.len();
            let entity = cx.entity();
            let answer = window.prompt(
                PromptLevel::Warning,
                "删除页面",
                Some(&format!(
                    "确定删除选中的 {n} 页？（尚未保存，保存时方写回原文件，可用 Ctrl+Z 撤销）"
                )),
                &["删除", "取消"],
                cx,
            );
            window
                .spawn(cx, async move |cx| {
                    if !matches!(answer.await, Ok(0)) {
                        return;
                    }
                    cx.update_entity(&entity, |v, cx| v.grid_apply_delete(pages, cx));
                })
                .detach();
        }
        #[cfg(test)]
        {
            let _ = (window, cx);
        }
    }

    /// 网格工具栏「拆分(n)」点击：另存为选目标路径（默认 `原文件名-拆分.pdf`）→ 确认
    /// [min,max] 闭区间（选中页视为区间端点；非连续时提示按闭区间拆分、含未选中页）
    /// → `grid_apply_split`。任一步取消不动作。
    pub(crate) fn grid_split_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(not(test))]
        {
            let selected: Vec<usize> = self.grid_selected.iter().copied().collect();
            if selected.is_empty() {
                return;
            }
            let n = self.dims.len();
            let (min, max) = (
                selected.iter().copied().min().unwrap_or(0),
                selected.iter().copied().max().unwrap_or(0),
            );
            // 越界兜底（选中集应在页数内；万一越界由 split_pages_to 错误面报错）。
            if min >= n || max >= n {
                return;
            }
            let orientation = if max - min + 1 == selected.len() {
                format!("将第 {}..{} 页拆分到新文件并从当前移除", min + 1, max + 1)
            } else {
                format!(
                    "选中页不连续，将按第 {}..{} 页闭区间拆分（含区间内未选中页）并从当前移除",
                    min + 1,
                    max + 1
                )
            };
            let base_name = self
                .path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "拆分".to_string());
            let entity = cx.entity();
            window
                .spawn(cx, async move |cx| {
                    let Some(file) = rfd::AsyncFileDialog::new()
                        .set_title("拆分页面")
                        .set_file_name(format!("{base_name}-拆分.pdf"))
                        .save_file()
                        .await
                    else {
                        return;
                    };
                    let out = file.path().to_path_buf();
                    // 目标路径已定，再确认区间（防误触；取消任一步不动作）。
                    let answer = cx.prompt(
                        PromptLevel::Warning,
                        "拆分页面",
                        Some(&format!(
                            "{orientation}？（尚未保存，保存时方写回原文件，可用 Ctrl+Z 撤销）"
                        )),
                        &["拆分", "取消"],
                    );
                    if !matches!(answer.await, Ok(0)) {
                        return;
                    }
                    cx.update_entity(&entity, |v, cx| v.grid_apply_split((min, max), out, cx));
                })
                .detach();
        }
        #[cfg(test)]
        {
            let _ = (window, cx);
        }
    }

    /// 网格工具栏「提取(n)」点击：另存为选目标路径（默认 `原文件名-提取.pdf`）→ 确认
    /// 勾选页 → `grid_apply_extract`（保持勾选升序，副本语义、源文档不变）。取消不动作。
    pub(crate) fn grid_extract_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(not(test))]
        {
            let mut pages: Vec<usize> = self.grid_selected.iter().copied().collect();
            // HashSet 快照无序 → 先按页升序（seam `grid_apply_extract` 亦归序，双保险，
            // 保证提取产物页序 = 勾选页升序、注释「保持勾选升序」成立）。
            pages.sort_unstable();
            if pages.is_empty() {
                return;
            }
            let n = pages.len();
            let base_name = self
                .path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "提取".to_string());
            let entity = cx.entity();
            window
                .spawn(cx, async move |cx| {
                    let Some(file) = rfd::AsyncFileDialog::new()
                        .set_title("提取页面")
                        .set_file_name(format!("{base_name}-提取.pdf"))
                        .save_file()
                        .await
                    else {
                        return;
                    };
                    let out = file.path().to_path_buf();
                    let answer = cx.prompt(
                        PromptLevel::Warning,
                        "提取页面",
                        Some(&format!("将选中的 {n} 页导出为新文件（源文档不变）？")),
                        &["提取", "取消"],
                    );
                    if !matches!(answer.await, Ok(0)) {
                        return;
                    }
                    cx.update_entity(&entity, |v, cx| v.grid_apply_extract(pages, out, cx));
                })
                .detach();
        }
        #[cfg(test)]
        {
            let _ = (window, cx);
        }
    }

    /// 网格工具栏「合并」点击：选源 PDF（`rfd` 过滤 pdf）→ 确认追加全部页 → `grid_apply_merge`。
    /// 取消任一步不动作。
    pub(crate) fn grid_merge_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(not(test))]
        {
            let entity = cx.entity();
            window
                .spawn(cx, async move |cx| {
                    let Some(file) = rfd::AsyncFileDialog::new()
                        .set_title("合并 PDF")
                        .add_filter("PDF", &["pdf"])
                        .pick_file()
                        .await
                    else {
                        return;
                    };
                    let src = file.path().to_path_buf();
                    let answer = cx.prompt(
                        PromptLevel::Warning,
                        "合并 PDF",
                        Some(&format!(
                            "将 {} 的全部页面追加到当前文档末尾？（尚未保存，保存时方写回原文件，可用 Ctrl+Z 撤销）",
                            file.file_name()
                        )),
                        &["合并", "取消"],
                    );
                    if !matches!(answer.await, Ok(0)) {
                        return;
                    }
                    cx.update_entity(&entity, |v, cx| v.grid_apply_merge(src, cx));
                })
                .detach();
        }
        #[cfg(test)]
        {
            let _ = (window, cx);
        }
    }

    /// 显式保存（Ctrl+S / 保存菜单）：把待存注释合并 + 内存工作副本原子写盘。无待存 →
    /// 直接返回 false（已是干净态）；发起异步保存返回 true——成功清 pending/undo/dirty
    /// 并 notify，失败记 [`Self::last_modify_error`]（壳层经 `_pdf_sub` toast 透出）。
    pub fn save(&mut self, cx: &mut Context<Self>) -> bool {
        self.submit_save(cx).is_some()
    }

    /// 另存为：把**当前内容**（内存工作副本 + 待存注释合并）写到 `new_path`，成功后当前
    /// 阅读器重指新路径并清 dirty（原文件保持原样）；有注释待存时顺带把 live 文档重载为
    /// 「含已写注释」的新文件副本（对齐 save 路径，防叠加层清掉后高亮从视图消失，P1-18）。
    /// 失败记 [`Self::last_modify_error`]，当前文档/待存状态不变。异步 fire-and-forget
    /// （壳层「另存为」对话框后调用）。
    pub fn save_as(&mut self, new_path: PathBuf, cx: &mut Context<Self>) {
        let Some(worker) = self.worker.clone() else {
            self.last_modify_error = Some("文档尚未加载".into());
            return;
        };
        if self.doc_id == 0 {
            self.last_modify_error = Some("文档尚未加载".into());
            return;
        }
        let doc_id = self.doc_id;
        self.last_modify_error = None;
        let has_annots = !self.pending_annots.is_empty();
        let items: Vec<AnnotateItem> = self
            .pending_annots
            .iter()
            .map(|a| AnnotateItem {
                page_index: a.page,
                kind: a.kind,
                quads: a.quads_pt.clone(),
            })
            .collect();
        let new_path_set = new_path.clone();
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    if has_annots {
                        // 注释合并写盘成功后，顺带读回已写文件 bytes 供 live 文档重载——
                        // 否则另存后叠加层清掉、基础文档并不含注释，高亮会在视图中消失
                        // 直到重开（对齐 save 路径的 read-back + open_from_bytes，P1-18）。
                        worker
                            .annotate(doc_id, items, new_path.clone())
                            .map(|_| std::fs::read(&new_path).ok())
                    } else {
                        worker.save(doc_id, new_path.clone()).map(|_| None)
                    }
                })
                .await;
            let _ = this.update(cx, move |v, cx| match res {
                Ok(annotated_bytes) => {
                    v.last_modify_error = None;
                    v.path = new_path_set;
                    // 另存后：内容已（合并待存/含页操作后）写入新路径 → 一律清待存
                    // （dirty 回落；worker.annotate 已顺带同步 stores 镜像）。
                    v.pending_annots.clear();
                    v.undo_stack.clear();
                    v.pending_page_ops = 0;
                    v.focused_annot = None;
                    v.sel_card = None;
                    // 有注释待存 → live 文档重载为「含已写注释」的新文件并保持当前页，
                    // 叠加层清空后注释仍在基础文档里上图（对齐 save 路径语义）。
                    if let Some(bytes) = annotated_bytes {
                        let anchor = v.current_page().map(|(pg, _)| pg.saturating_sub(1));
                        v.open_from_bytes(bytes, anchor, cx);
                    }
                    cx.notify();
                }
                Err(e) => {
                    v.last_modify_error = Some(format!("{e:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 显式保存并返回完成信号通道（关闭标签「保存并关闭」用；`None` = 无待存）。
    /// 接收端 `Ok(())` = 已落盘；`Err` = 保存失败（仍留在待存态）。
    pub fn save_channels(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<mpsc::Receiver<std::result::Result<(), String>>> {
        self.submit_save(cx)
    }

    #[allow(clippy::type_complexity)]
    fn submit_save(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<mpsc::Receiver<std::result::Result<(), String>>> {
        if !self.is_dirty() {
            return None;
        }
        let worker = self.worker.clone()?;
        if self.doc_id == 0 {
            self.last_modify_error = Some("文档尚未加载".into());
            return None;
        }
        let doc_id = self.doc_id;
        let path = self.path.clone();
        self.last_modify_error = None;
        let items: Vec<AnnotateItem> = self
            .pending_annots
            .iter()
            .map(|a| AnnotateItem {
                page_index: a.page,
                kind: a.kind,
                quads: a.quads_pt.clone(),
            })
            .collect();
        let (tx, rx) = mpsc::channel();
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    if items.is_empty() {
                        worker.save(doc_id, path.clone()).map(|_| None)
                    } else {
                        // 注释合并写盘成功后，顺带读回已写文件 bytes 供 live 文档重载——
                        // 否则 save 清掉叠加层后基础文档并不含注释，高亮会在视图中消失
                        // 直到重开才回来（b1 旧口径靠「重开读回」绕开了这个可见缺陷）。
                        worker
                            .annotate(doc_id, items, path.clone())
                            .map(|_| std::fs::read(&path).ok())
                    }
                })
                .await;
            let _ = tx.send(
                res.as_ref()
                    .map(|_| ())
                    .map_err(|e: &anyhow::Error| format!("{e:#}")),
            );
            let _ = this.update(cx, move |v, cx| match res {
                Ok(annotated_bytes) => {
                    v.last_modify_error = None;
                    v.pending_annots.clear();
                    v.undo_stack.clear();
                    v.pending_page_ops = 0;
                    v.focused_annot = None;
                    v.sel_card = None;
                    // 有注释待存 → live 文档重载为「含已写注释的新文件」并保持当前页，
                    // 叠加层清空后注释仍在基础文档里上图（零闪烁的注释添加路径不受影响）。
                    if let Some(bytes) = annotated_bytes {
                        let anchor = v.current_page().map(|(pg, _)| pg.saturating_sub(1));
                        v.open_from_bytes(bytes, anchor, cx);
                    }
                    cx.notify();
                }
                Err(e) => {
                    v.last_modify_error = Some(format!("{e:#}"));
                    cx.notify();
                }
            });
        })
        .detach();
        Some(rx)
    }

    /// 推进一条「整档内存快照」撤销条目并裁剪预算（所有 `RestoreMemory` 入栈点的唯一入口，
    /// 见 [`enforce_undo_mem_budget`]；`RemoveAnnot`/`AddAnnotBack` 这类不含整档镜像的条目
    /// 仍走 `undo_stack.push_back` 直推）。预算只在一处执行，杜绝各操作各写各推漏约束。
    fn push_undo_restore(&mut self, bytes: Vec<u8>, annots_before: Vec<PendingAnnot>) {
        self.undo_stack.push_back(UndoEntry::RestoreMemory {
            bytes,
            annots_before,
        });
        enforce_undo_mem_budget(&mut self.undo_stack);
    }

    /// 撤销上一条 PDF 编辑（Ctrl+Z）：注释逐条撤（加/删互逆）；页面插入/删除按**操作前
    /// 内存镜像**恢复（内容精确还原）。无待回退条目 → 空操作。
    pub fn undo_last(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.undo_stack.pop_back() else {
            return;
        };
        match entry {
            UndoEntry::RemoveAnnot { id } => {
                self.pending_annots.retain(|a| a.id != id);
            }
            UndoEntry::AddAnnotBack { annot } => {
                self.pending_annots.push(annot);
            }
            UndoEntry::RestoreMemory {
                bytes,
                annots_before,
            } => {
                self.pending_page_ops = self.pending_page_ops.saturating_sub(1);
                self.pending_annots = annots_before;
                self.focused_annot = None;
                let anchor = self.current_page().map(|(pg, _)| pg.saturating_sub(1));
                self.open_from_bytes(bytes, anchor, cx);
            }
        }
        cx.notify();
    }

    /// 删除一条待存注释（右键「删除注释」）：从 pending 移除并清灰框焦；撤销栈记录
    /// 逆操作（Ctrl+Z 可恢复该条）。
    pub fn delete_pending_annot(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(pos) = self.pending_annots.iter().position(|a| a.id == id) {
            let annot = self.pending_annots.remove(pos);
            self.undo_stack.push_back(UndoEntry::AddAnnotBack { annot });
            if self.focused_annot == Some(id) {
                self.focused_annot = None;
            }
            cx.notify();
        }
    }

    /// 最近一次写回/页操作的错误文案（None = 无错误）。
    pub fn last_modify_error(&self) -> Option<&str> {
        self.last_modify_error.as_deref()
    }

    /// 读回某一页已**写盘**的注释（类型 + quad 几何）。需要已加载文档；走 worker 读回。
    /// 待保存模型下只反映 save() 之后落盘的内容；待存注释以 [`Self::pending_annots`]
    /// 叠加层渲染呈现。
    ///
    /// P1-17：**非阻塞版**——worker 读回已下放后台执行器执行，本方法只发起并返回事件
    /// 结果通道（`None` = 无 worker / 文档未加载）。调用方稍后用 `try_recv`/`recv_timeout`
    /// 轮询结果，**绝不在调用线程（主线程）同步 `rx.recv()` 等 pdfium worker**——worker
    /// 忙时主线程等数百 ms 会冻结 UI（对齐 K167 异步回传姿势）。
    pub fn annotations_for_page(
        &self,
        page_index: usize,
        cx: &mut Context<Self>,
    ) -> Option<mpsc::Receiver<Option<Vec<AnnotationInfo>>>> {
        let worker = self.worker.clone()?;
        if self.doc_id == 0 {
            return None;
        }
        let doc_id = self.doc_id;
        let (tx, rx) = mpsc::channel();
        cx.spawn(async move |_, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { worker.annotations_for_page(doc_id, page_index).ok() })
                .await;
            let _ = tx.send(res);
        })
        .detach();
        Some(rx)
    }

    /// 从**内存字节**（非磁盘）重开文档：worker 侧 Close 旧文档 → 失效渲染派生缓存 →
    /// 后台打开新 bytes。待保存模型的页面操作/撤销恢复用它做「只一次内存级重载、不写盘」。
    /// 待存状态（pending_annots / undo / dirty）**保留**（页号已由调用方平移妥当）；
    /// 渲染派生缓存（文本层/选区/检索/门控）随结构变化全量失效。
    fn open_from_bytes(&mut self, bytes: Vec<u8>, anchor: Option<usize>, cx: &mut Context<Self>) {
        if let Some(w) = &self.worker
            && self.doc_id != 0
        {
            w.close(self.doc_id);
        }
        self.worker = None;
        self.doc_id = 0;
        // 全量失效渲染派生缓存（同旧 reload_after_modify；不 pending_drops.clear()——
        // 旧位图应由下一帧 ensure_window 逐个 drop_image 下线，清掉只会泄漏显存提交，K14）。
        self.text_pages.clear();
        self.text_lru.clear();
        self.pending_text.clear();
        // 图片盒缓存与选中图片随结构变化全量失效（页号可能已平移）。
        self.image_pages.clear();
        self.image_lru.clear();
        self.pending_images.clear();
        self.selected_image = None;
        self.pending_thumb.clear();
        self.pending_mid.clear();
        self.pending_tiles.clear();
        self.pending_cover.clear();
        self.pending_recolor = None;
        self.recolor_scope.clear();
        self.recolor_received.clear();
        self.mid_lru.clear();
        self.tile_bytes = 0;
        self.tile_pump_running = false;
        self.sel_anchor = None;
        self.sel_active = None;
        self.selecting = false;
        self.sel_card = None;
        self.search_query.clear();
        self.search_matches.clear();
        self.search_truncated = false;
        self.search_active = 0;
        self.search_pending = None;
        self.window_sig = None;
        self.window_settled = false;
        self.reload_anchor = anchor;
        cx.spawn(async move |weak, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    PdfWorker::open_and_dims_full(bytes)
                        .map_err(|e| format!("无法启动 pdfium：{e}"))
                })
                .await;
            // 与 `start_load` 同款 H1 防泄漏：打开成功即已登记 worker 文档，实体若在加载中
            // 被释放（`weak.update` Err）须补 `close(doc_id)`，否则 pdfium 句柄 + 字节附件残留。
            match result {
                Ok((worker, doc_id, dims, outlines)) => {
                    if weak
                        .update(cx, |this, cx| {
                            this.apply_loaded(worker.clone(), doc_id, dims, outlines, cx);
                        })
                        .is_err()
                    {
                        worker.close(doc_id);
                    }
                }
                Err(e) => {
                    let _ = weak.update(cx, |this, cx| {
                        this.state.set_error(e);
                        cx.refresh_windows();
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }
}
