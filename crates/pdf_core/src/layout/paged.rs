use super::single::display_height;

// ---- 分页阅读视图（Paged）几何：单页/双页（非网格），纯 f32 数学 ----

/// 页平面上的轴对齐矩形（纯数据，f32）。本文件 Grid 系用 4 元组；Paged 的命中/吸附/
/// 测试需要命名字段与 Copy，故自建最小结构（不引入外部依赖，本仓无既有 Rect/Point）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// 单页/双页阅读分页布局（`cols ∈ {1,2}`；每行 contain-fit，页高 ≤ 视口高）。
///
/// 行内页宽 = min(列宽上限 `cap_w`，使「行内最竖页」恰一屏高的宽 `viewport_h·min_ar`)——
/// 页高永不超视口高；行按页高堆叠，行间 `v_gap`、同行左右页由 `h_gap` 分隔、上下/左右
/// `padding`。`row_top` /
/// `row_page_start` / `cell_w_per_row` / `cell_h` / `left_pad_per_row` 为一次性直读缓存，
/// `cell_rect` / 可见行 / 命中 / 吸附全由本结构推导（与 [`GridLayout`] 同一"结构即几何"姿态）。
///
/// 坐标系：内容原点 = 左上角、Y 轴向下为正。**`scroll_y` 为负偏移**：视口顶内容 y =
/// `−scroll_y`，`scroll_y = 0` = 停在内容顶，向下翻页使 `scroll_y` 变负——与网格/页列
/// 用的「正偏移」是同一视口的参数差一正负号（详见 [`PagedLayout::first_visible_row`]）。
#[derive(Clone, Debug)]
pub struct PagedLayout {
    /// 列数（1=单列（内部/测试用），2=双页（Facing）；构造时 0→1、>2→2 归一）。
    pub cols: usize,
    /// 行数（`ceil(pages.len()/cols)`；0 页时 = 0）。
    pub rows: usize,
    /// 水平页间距（同行左页↔右页之间，×zoom）。
    pub h_gap: f32,
    /// 垂直行间距（上页↔下页之间，×zoom）。
    pub v_gap: f32,
    /// 上下/左右外白。
    pub padding: f32,
    /// 每行顶 y（含外 `padding`；`len = rows`，严格递增）。行高 = 该行内最大 `cell_h`。
    pub row_top: Vec<f32>,
    /// 每行起始页下标（`len = rows + 1`，末项 = 总页数，方便取区间）。
    pub row_page_start: Vec<usize>,
    /// 每行统一页宽（`len = rows`；行内各页同宽）。
    pub cell_w_per_row: Vec<f32>,
    /// 每页高（保持页纵横比，`len = pages`）。
    pub cell_h: Vec<f32>,
    /// 每行水平左留白（行内居中用；`len = rows`，列 x 起点 = padding + 此项）。
    pub left_pad_per_row: Vec<f32>,
    /// 所有行内容宽的最大值（供容器宽）。
    pub spread_max: f32,
    /// 上/下外 `padding` + Σ各“行高 + 行间 gap”的总高。
    pub total_height: f32,
}

impl PagedLayout {
    /// 第 `r` 行的行高 = 该行内最大页高（派生值，不入字段保持结构最小）。
    /// `pub`：view 层 `reading_progress`/`restore` 分页分支取行高算行内比例（K177 手算）。
    pub fn row_height(&self, r: usize) -> f32 {
        let s = self.row_page_start[r];
        let e = self.row_page_start[r + 1];
        self.cell_h[s..e].iter().copied().fold(0.0f32, f32::max)
    }

    /// 第 `page` 页的放置矩形 `(x, y, w, h)`（内容坐标系，见 [`PagedLayout`]）；越界/空布局 → `None`。
    /// `x = padding + left_pad_per_row[r] + c·(cell_w_per_row[r] + h_gap)`，`y = row_top[r]`，
    /// `w = cell_w_per_row[r]`（行内统一页宽）、`h = cell_h[page]`（每页自身纵横比）。
    pub fn cell_rect(&self, page: usize) -> Option<Rect> {
        if self.rows == 0 || self.cols == 0 || page >= self.cell_h.len() {
            return None;
        }
        let r = page / self.cols;
        let c = page % self.cols;
        let w = self.cell_w_per_row[r];
        // 演算（K177）：列原点 = padding + left_pad[r]；列 c 再平移 c·pitch（pitch = 页宽 + h_gap）。
        let x = self.padding + self.left_pad_per_row[r] + c as f32 * (w + self.h_gap);
        Some(Rect {
            x,
            y: self.row_top[r],
            w,
            h: self.cell_h[page],
        })
    }

    /// 可见行区间（含端点，闭区间）：视口 `[−scroll_y, −scroll_y + viewport_h]`（**负偏移**，
    /// 视口顶内容 y = `−scroll_y`）命中的所有行，二分行顶直读缓存 O(log rows)，与
    /// [`GridLayout::first_visible_grid_row`] 同姿势（正偏移 `offset` 等价于 `−scroll_y`）。
    /// 0 行或视口完全落在内容之外（上/下/悬在行间 gap 里）→ `None`；`viewport_h ≤ 1` 回退
    /// 默认 720（首帧前口径，与既有网格一致）。
    ///
    /// 行 r 可见 ⇔ `row_bottom > −scroll_y ∧ row_top < −scroll_y + viewport_h`
    /// ⇔ `row_top[r] + scroll_y + row_h[r] > 0 ∧ row_top[r] + scroll_y < viewport_h`。
    pub fn first_visible_row(&self, scroll_y: f32, viewport_h: f32) -> Option<(usize, usize)> {
        if self.rows == 0 {
            return None;
        }
        let vh = if viewport_h > 1.0 { viewport_h } else { 720.0 };
        let top = (-scroll_y).max(0.0); // 视口顶内容 y（负偏移；钳掉"滚过头顶"）
        let bottom = top + vh;

        // 首行 = 第一个「行底 > 视口顶」的行（`row_top` 严格递增 → 行底单调，可二分）。
        let mut lo = 0usize;
        let mut hi = self.rows;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.row_top[mid] + self.row_height(mid) <= top {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == self.rows {
            return None; // 已滚过末行：视口完全在内容之下
        }
        let first = lo;

        // 末行 = 最后一个「行顶 < 视口底」的行。
        let mut lo = 0usize;
        let mut hi = self.rows;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.row_top[mid] < bottom {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 || lo - 1 < first {
            return None; // 视口完全在内容之上，或仅悬在行间 gap 里（无任何行相交）
        }
        Some((first, lo - 1))
    }

    /// 行二分原语：最后一个「行顶 ≤ y」的行下标（`row_top` 严格递增可二分）；0 行 → `None`。
    pub fn row_index_at_y(&self, y: f32) -> Option<usize> {
        if self.rows == 0 {
            return None;
        }
        let mut lo = 0usize;
        let mut hi = self.rows;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.row_top[mid] <= y {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Some(lo.saturating_sub(1))
    }

    /// 命中定位：内容坐标 (x, y) → `(页号, 页内局部 x, 页内局部 y)`；间隙/留白/越界/空布局 → `None`。
    ///
    /// 行由 [`row_index_at_y`] 定位；列用 **floor** 求候选槽
    /// `c = floor((x − (padding + left_pad_r)) / (cell_w_per_row[r] + h_gap))` 并在 `[0, cols)`
    /// 且落在本行页区间（`i = r·cols + c < n`）内，随后用 [`cell_rect`] 做**包含性复核**
    /// （点须落 `[0, w)×[0, h)` 内）——间隙/留白/越界点都过不了复核 → `None`。
    ///
    /// 注：**不用 round** —— round 的槽中点 `(c+0.5)·pitch` 会从卡片内部切过
    /// （如 w=479.5/h_gap=1（cap_w=(960−1)/2=479.5）⇒ pitch=480.5、round 槽界 = x0+240.25
    /// （=0.5×pitch），而真卡右缘 = x0+479.5），把卡右半的真命中误判到
    /// 邻列 / 间隙；floor 给出「含卡右缘前」的真列，再经包含性复核即得正确命中。
    /// （对比 [`GridLayout::col_slot_at_x`] 的 round 是"最近插槽"语义，用途不同。）
    pub fn page_at(&self, x: f32, y: f32) -> Option<(usize, f32, f32)> {
        if self.rows == 0 {
            return None;
        }
        let r = self.row_index_at_y(y)?;
        let pitch = self.cell_w_per_row[r] + self.h_gap;
        if pitch <= 0.0 {
            return None; // 退化视口（页宽 0）无法按列定位
        }
        let x0 = self.padding + self.left_pad_per_row[r];
        let cf = ((x - x0) / pitch).floor(); // 候选列（可为负 → 左留白）
        if cf < 0.0 || cf as usize >= self.cols {
            return None;
        }
        let i = r * self.cols + cf as usize;
        let rect = self.cell_rect(i)?; // i 已在行/列/页界内复核过，必有矩形
        if x >= rect.x && x < rect.x + rect.w && y >= rect.y && y < rect.y + rect.h {
            Some((i, x - rect.x, y - rect.y))
        } else {
            None
        }
    }

    /// 吸附目标：返回「行顶距视口顶最近」的行号 r 与其内容 y（= `row_top[r]`）。
    ///
    /// 语义：视口顶内容 y = `−scroll_y`；`r = argmin_r |(−scroll_y) − row_top[r]|`；
    /// 首行之上（或空布局）→ 回落 `(0, 0.0)`；平局取行号小者（确定性）。
    ///
    /// 已休眠（K24）：生产 0 消费——Facing 已改连续自由滚动（不吸附行顶），view 层
    /// `paged_snap_target`/步进在 2026-11-20 连滚战役已删；仅本模块单测保留（行为锚）。
    pub fn snap_target(&self, scroll_y: f32) -> (usize, f32) {
        if self.rows == 0 {
            return (0, 0.0);
        }
        let top = -scroll_y; // 视口顶内容 y
        let mut best = 0usize;
        let mut best_d = f32::INFINITY;
        for r in 0..self.rows {
            let d = (top - self.row_top[r]).abs();
            if d < best_d {
                best_d = d;
                best = r;
            }
        }
        (best, self.row_top[best])
    }
}

/// 分页布局的**指纹**（问题 B）：由 [`paged_layout`] 的全部输入压实而成，供
/// `view::ensure_window` 判断「上帧分页快照是否仍可用」——纯滚动/静止帧输入不变 →
/// 复用上帧快照，跳过每帧 5×O(rows) 的向量分配与几何重算。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PagedFingerprint {
    /// `pages.len()`（页数；页数变了必重建，快路径）。
    pub dims_len: usize,
    /// `pages` 内容压实的 64 位哈希（FNV-1a over 每页 (w,h) 的 `f32::to_bits`）——
    /// 页数相同但尺寸变化（如页操作 reload 后同页数异尺寸）也能检出。
    pub dims_hash: u64,
    /// 分页视口宽/高的 bit 表示（`f32::to_bits` 逐位比较，避免浮点近似把「微变」放行）。
    pub viewport_w_bits: u32,
    pub viewport_h_bits: u32,
    /// 列数（当前恒 2 = Facing；保留进指纹防未来列数可变化时漏重建）。
    pub cols: usize,
}

impl PagedFingerprint {
    /// 由布局输入压实（纯 u64/u32 归并，无分配）：`pages` 逐页 (w,h) 的
    /// `f32::to_bits` 混入 FNV-1a，`viewport_w/h` 与 `cols` 直接并入尾项。
    pub fn of(pages: &[(f32, f32)], viewport_w: f32, viewport_h: f32, cols: usize) -> Self {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for &(w, hh) in pages {
            h ^= w.to_bits() as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
            h ^= hh.to_bits() as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Self {
            dims_len: pages.len(),
            dims_hash: h,
            viewport_w_bits: viewport_w.to_bits(),
            viewport_h_bits: viewport_h.to_bits(),
            cols,
        }
    }
}

/// 计算阅读分页布局（纯函数，与 [`grid_layout`] 同姿势）：`pages` 为每页 `(宽, 高)` pt
/// （只用纵横比），`viewport_w/h` 视口、`cols ∈ {1,2}`（0→1、>2→2）、`h_gap` 水平页间距
/// （双页同行左页↔右页之间）、`v_gap` 垂直行间距（上页↔下页之间）、`padding` 四周外白。
///
/// 数学（每行 r，页区间 s..=e；K177 演算）：
/// 1. 列宽上限 `cap_w = max(0, (max(0, viewport_w − 2·padding) − (cols==2 ? h_gap : 0)) / cols)`
///    （防负宽：内容区先夹 ≥0，减列间 h_gap 后再夹 ≥0）；
/// 2. 行内最竖页 `min_ar = min_{i∈s..e}(w_i/h_i)`（跳过 w≤0 ∨ h≤0 退化页，全退化 → 1）；
///    `fit_w = viewport_h · min_ar`（恰一屏高）；页宽 `cell_w_r = min(cap_w, fit_w).max(0)`；
/// 3. 每页高 `cell_h[i] = cell_w_r·(h_i/w_i)`（w≤0 → cell_w_r，与 [`display_height`] 同源兜底）；
/// 4. 行宽统一按 cols 满行 `spread_r = cols·cell_w_r + (cols−1)·h_gap`（partial 行页靠列左、
///    宽度与满行一致，不重算居中基线），`left_pad_r = max(0, (content_w − spread_r)/2)`；
///    `spread_max = max_r(spread_r)`；
/// 5. `row_top[0] = padding`；`row_top[r+1] = row_top[r] + row_h[r] + v_gap`，
///    `row_h[r] = max_{i∈s..e} cell_h[i]`；
/// 6. `total_height = (rows==0) ? 0 : row_top[rows-1] + row_h[rows-1] + padding`
///    （= padding + Σ(行高+v_gap) + padding，删循环尾部多计的 v_gap）。
pub fn paged_layout(
    pages: &[(f32, f32)],
    viewport_w: f32,
    viewport_h: f32,
    cols: usize,
    h_gap: f32,
    v_gap: f32,
    padding: f32,
) -> PagedLayout {
    let cols = match cols {
        0 | 1 => 1,
        _ => 2,
    };
    let n = pages.len();
    let rows = n.div_ceil(cols);
    // 空布局：rows=0、total=0；row_page_start 留端点 [0]（len = rows+1），几何方法一律安全空。
    if rows == 0 {
        return PagedLayout {
            cols,
            rows: 0,
            h_gap,
            v_gap,
            padding,
            row_top: Vec::new(),
            row_page_start: vec![0],
            cell_w_per_row: Vec::new(),
            cell_h: Vec::new(),
            left_pad_per_row: Vec::new(),
            spread_max: 0.0,
            total_height: 0.0,
        };
    }
    let content_w = (viewport_w - 2.0 * padding).max(0.0);
    let cap_w = ((content_w - if cols == 2 { h_gap } else { 0.0 }) / cols as f32).max(0.0);
    let vh = viewport_h.max(0.0);

    let row_page_start: Vec<usize> = (0..=rows).map(|r| (r * cols).min(n)).collect();
    let mut cell_h = vec![0.0f32; n];
    let mut row_top = Vec::with_capacity(rows);
    let mut cell_w_per_row = Vec::with_capacity(rows);
    let mut left_pad_per_row = Vec::with_capacity(rows);
    let mut spread_max = 0.0f32;
    let mut y = padding;
    for r in 0..rows {
        let s = row_page_start[r];
        let e = row_page_start[r + 1];
        // 行内最竖页（w/h 最小 = 按宽度限高时最高者）；跳过退化页，全退化回退比例 1。
        let mut min_ar = f32::INFINITY;
        for &(w, h) in &pages[s..e] {
            if w > 0.0 && h > 0.0 {
                min_ar = min_ar.min(w / h);
            }
        }
        let min_ar = if min_ar.is_finite() { min_ar } else { 1.0 };
        let cell_w_r = cap_w.min(vh * min_ar).max(0.0);
        for (k, &(w, h)) in pages[s..e].iter().enumerate() {
            cell_h[s + k] = if w > 0.0 {
                cell_w_r * (h / w)
            } else {
                cell_w_r
            };
        }
        let row_h = cell_h[s..e].iter().copied().fold(0.0f32, f32::max);
        let spread_r = cols as f32 * cell_w_r + (cols - 1) as f32 * h_gap;
        spread_max = spread_max.max(spread_r);
        row_top.push(y);
        cell_w_per_row.push(cell_w_r);
        left_pad_per_row.push(((content_w - spread_r) / 2.0).max(0.0));
        y += row_h + v_gap;
    }
    // total = padding + Σ行高 + (rows−1)·v_gap + padding（去循环尾部多计的 v_gap）。
    let total_height = y - v_gap + padding;

    PagedLayout {
        cols,
        rows,
        h_gap,
        v_gap,
        padding,
        row_top,
        row_page_start,
        cell_w_per_row,
        cell_h,
        left_pad_per_row,
        spread_max,
        total_height,
    }
}

/// 分页**可见行页区间**（未扩）：由 [`PagedLayout::first_visible_row`] 的首行 r0 得
/// `[row_page_start[r0], row_page_start[r0+1]−1]`——分页模式每行 ≈ 一屏，只物化首行即可
/// 覆盖当前屏（render_pdf_pages_paged 与 A 线调度共用同一口径）。`scroll_y` 为负偏移
/// （视口顶内容 y = −scroll_y，见 [`PagedLayout`]）；仍由首行推出闭区间页下标。
/// 无可见行 / 空布局 → `None`。
pub fn paged_visible_pages(
    layout: &PagedLayout,
    scroll_y: f32,
    viewport_h: f32,
    n: usize,
) -> Option<(usize, usize)> {
    let (r0, _) = layout.first_visible_row(scroll_y, viewport_h)?;
    let start = layout.row_page_start[r0];
    let end = layout.row_page_start[r0 + 1]
        .saturating_sub(1)
        .min(n.saturating_sub(1));
    if start > end || n == 0 {
        return None;
    }
    Some((start, end))
}

/// 分页**调度窗**（闭区间页下标）：可见行页区间再向两侧扩 `k_rows` 行（× `cols` 页）。
/// 语义对齐连续 [`keep_window`] 的 ±[`MARGIN`] 页缓冲：分页每行 ≈ 一屏，±`k_rows` 行 =
/// 保住「显示页必有底图 + 下一行提前暖」；再大由封面驻留窗/走廊预算独立限流。无可见行 /
/// 空布局 → `None`。
pub fn paged_schedule_window(
    layout: &PagedLayout,
    scroll_y: f32,
    viewport_h: f32,
    n: usize,
    k_rows: usize,
) -> Option<(usize, usize)> {
    let (vis_start, vis_end) = paged_visible_pages(layout, scroll_y, viewport_h, n)?;
    let expand = k_rows.saturating_mul(layout.cols.max(1));
    let start = vis_start.saturating_sub(expand);
    let end = (vis_end + expand).min(n.saturating_sub(1));
    Some((start, end))
}

/// 分页**渲染窗**（闭区间页下标）：视口相交的**全可见行** `[r0, r1]`（可含行中位自由滚动时
/// 视口底部压到的下一行 r1）再向两侧各扩 `buf_rows` 行，`clamp [0, rows−1]`，映射回行页区间
/// `[row_page_start[r_lo], row_page_start[r_hi+1]−1]` clamp `[0, n−1]`。语义对齐连续
/// `render_pdf_pages` 的 ±[`MARGIN`] 页缓冲：Facing=连续自由滚动，行中停位要同时看到上下
/// 两行——`paged_visible_pages` 只按首行 r0 会漏掉视口底部的下一行（只物化 r0 = 露底）。
/// 画面机械化缓冲由 A 线调度窗（±[`PAGED_WINDOW_ROWS`] 行）兜底，本窗只保「本帧显示行
/// 都有卡」。`scroll_y` 为负偏移（视口顶内容 y = −scroll_y，见 [`PagedLayout`]）；无可见行 /
/// 空布局 → `None`。
pub fn paged_render_window(
    layout: &PagedLayout,
    scroll_y: f32,
    viewport_h: f32,
    n: usize,
    buf_rows: usize,
) -> Option<(usize, usize)> {
    let (r0, r1) = layout.first_visible_row(scroll_y, viewport_h)?;
    let r_lo = r0.saturating_sub(buf_rows);
    let r_hi = (r1 + buf_rows).min(layout.rows.saturating_sub(1));
    let start = layout.row_page_start[r_lo];
    let end = layout.row_page_start[r_hi + 1]
        .saturating_sub(1)
        .min(n.saturating_sub(1));
    if start > end || n == 0 {
        return None;
    }
    Some((start, end))
}

/// 内容列坐标 (cx, cy) → (页号, 页内局部 x, 页内局部 y)。
///
/// - `cx/cy` 为**页列空间**坐标（wrap 内减去 [`column_left`] 后的列偏移；无需关心滚动
///   偏移——那是 view 层把窗口坐标换算到 wrap 空间时消掉的）。
/// - 列宽 = `page_width`；点到列的空白（左/右页边、页间纵向间隙）→ `None`（清空选区）。
pub fn content_to_local(
    dims: &[(f32, f32)],
    tops: &[f32],
    page_width: f32,
    cx: f32,
    cy: f32,
) -> Option<(usize, f32, f32)> {
    if dims.is_empty() || cx < 0.0 || cx > page_width {
        return None;
    }
    // 二分定位「页带（`页顶-1 ..= 页底+1`）含 `cy`」的页：页带互不交叠（PAGE_GAP > 2）
    // 且随 `tops` 严格递增 → 谓词「页带起点 ≤ cy」单调，可二分（对齐 `first_visible_page`
    // 既有姿势，千页命中测试 O(页数)→O(log 页数)），与旧线性扫逐例等价：同套浮点运算，
    // 页间沟槽 / 首页之上仍按「任何页带都不含 `cy`」返回 `None`（清空选区）。
    let n = dims.len();
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if tops[mid] - 1.0 <= cy {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    // `lo-1` = 最后一个「页带起点 ≤ cy」的页；其页带底（页底 + 1）未包住 `cy`（页间沟槽）
    // 或 `lo == 0`（首页带起点之上）→ `None`，与旧线性扫的沟槽判据一致。
    if lo == 0 {
        return None;
    }
    let i = lo - 1;
    let h = display_height(dims[i], page_width);
    if cy <= tops[i] + h + 1.0 {
        Some((i, cx, cy - tops[i]))
    } else {
        None
    }
}
