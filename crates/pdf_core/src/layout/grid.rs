// ---- 网格视图（Grid）几何：页卡网格布局的纯函数（与页列视图同源数学，便于单测）----

/// 网格视图的页卡布局结果（不可变纯数据，与页列视图的 `tops` 缓存姿态一致）。
///
/// 坐标系与页列视图一致：内容原点 = 左上角、Y 轴向下为正；`padding` 为网格四周外留白。
/// `row_top`/`row_page_start`/`cell_h` 为一次性算好的直读缓存，命中/可见行/卡片矩形
/// 全靠本结构推导，不与 DOM/view 再各算一遍——保证滚动位置、命中判定与卡片实际放置
/// 严格一致（`row_index_at_y`/`col_slot_at_x` 原语与 `cell_rect` 用同一套加法，view 侧插入
/// 位 `compute_insert_index` 也基于这两个原语，不再重复手写二分/pitch 几何）。
#[derive(Clone, Debug)]
pub struct GridLayout {
    /// 列数（`cols = max(1, floor((viewport_width − 2·padding + gap) / (card_width + gap)))`）。
    pub cols: usize,
    /// 行数（`ceil(pages.len()/cols)`；0 页时 = 0）。
    pub rows: usize,
    /// 实际卡宽（**跟随请求卡宽**，缩放精调连续感知）：正常 = 请求卡宽 `card_width`
    /// （cols≥2 时 floor 已保证整行放得下 `cols·card_w + (cols−1)·g ≤ vw − 2p`，行尾
    /// 余量右侧留白）；唯一例外 = 单列且请求卡宽超内容区（`viewport_width − 2·padding`）
    /// 时夹到内容区宽，防横向溢出。
    pub cell_w: f32,
    /// 纵向行距（行间 gap，纵向语义不变；横向列距由 `col_gap` 负责）。
    pub gap: f32,
    /// 实际渲染列间距（≥ 入参 `gap`；「余量拉伸填满」优先，封顶后 = [`GRID_GAP_MAX`]；
    /// cols=1 时 = 入参 `gap` 无语义影响）。
    pub col_gap: f32,
    /// 左居留白（横向余量摊不下的部分左右对称居中；恰好填满 = 0；cols=1 单列居中用）。
    pub left_pad: f32,
    /// 网格四周外留白。
    pub padding: f32,
    /// 每行顶 y（含外 `padding`；`len = rows`，严格递增）。行高 = 该行内最大 `cell_h`。
    pub row_top: Vec<f32>,
    /// 每行起始页下标（`len = rows + 1`，末项 = 总页数）。
    pub row_page_start: Vec<usize>,
    /// 每页卡高（保持页纵横比，`cell_h[i] = cell_w·(页高/页宽)`；`len = pages`）。
    pub cell_h: Vec<f32>,
    /// 网格总高（上/下外 `padding` + 各行高与行间 `gap`）。
    pub total_height: f32,
}

/// 列间距封顶值 = 3 × GRID_GAP（36px；可调）。列数少、横向余量过大时列间距不无限
/// 拉大（避免离得过分稀疏）：封顶后剩余留白改左右对称居中（`left_pad`）。
const GRID_GAP_MAX: f32 = 36.0;

/// 计算网格视图布局（纯函数，与 [`compute_layout`] 同姿势）：`pages` 为每页 `(宽, 高)` pt
/// 尺寸，`card_width` 为请求卡宽。**新语义「卡宽跟随请求」**：实际 `cell_w` = 请求卡宽
/// （不再按填满视口反推）——Ctrl+滚轮缩放精调时，卡宽每步都连续变化、缩放逐档可感知，
/// 同列数区间内卡大小不再被冻结；行尾放不下时右侧留白（多余空间不再摊给卡）。
///
/// - 列数 = 能并排几列：`cols = max(1, floor((vw − 2·padding + gap) / (card_width + gap)))`；
/// - `cell_w` = `card_width`，唯一例外 = 单列且 `card_width` > 内容区（`vw − 2·padding`）
///   时夹到内容区宽（单卡填视口、防横向溢出）；窄视口（内容区 < 0）夹到 ≥0 防负宽退化；
/// - **列间距拉伸填满**（横向去右侧留白）：`col_gap = min(needed, GAP_MAX)`，
///   `needed = (C − cols·cell_w)/(cols−1)`（出发点是内容区宽、列数/卡宽固定后把余量全摊进
///   列间距，末列右缘贴齐内容区右缘）；余量过大（列数少、needed > GAP_MAX）时列间距封顶、
///   剩余留白左右对称居中（`left_pad = (C − cols·cell_w − (cols−1)·col_gap)/2`）；
///   cols=1 → `col_gap = gap`、`left_pad = (C − cell_w)/2`（单列居中、`needed` 无定义）；
/// - `cell_h` 保持各页纵横比（`宽 ≤ 0` 时按比例 1 处理，与 [`display_height`] 同源）；
/// - 行内页从左到右填满 `cols` 后换行，最后一行可不填满（部分行）；行高 = 该行最大卡高；
/// - 0 页 → `rows = 0`、`row_top`/`row_page_start` 空、`total_height = 0`（调用方按空布局处理）。
pub fn grid_layout(
    pages: &[(f32, f32)],
    card_width: f32,
    gap: f32,
    padding: f32,
    viewport_width: f32,
) -> GridLayout {
    // cols：能并排几列（分子 +gap 是「末列之后不留 gap」的放置会计，分母 = 卡宽 + 列间 gap）。
    let denom = card_width + gap;
    let cols = if denom > 0.0 {
        (((viewport_width - 2.0 * padding + gap) / denom).floor()).max(1.0) as usize
    } else {
        1
    };
    // cell_w：新语义 = 跟随请求卡宽 `card_width`（floor 已在 cols ≥ 1 时保证
    // `cols·card_w + (cols−1)·g ≤ vw − 2p`，整行放得下、行尾余量留白）；唯一例外 =
    // cols 被夹到 1 且请求卡宽超内容区（vw − 2p）→ 单卡填视口防横向溢出；窄视口
    // （vw − 2p < 0）夹到 ≥0 防负宽退化、无除零。
    let content_w = (viewport_width - 2.0 * padding).max(0.0);
    let cell_w = if cols <= 1 {
        card_width.min(content_w).max(0.0)
    } else {
        card_width.max(0.0)
    };
    // col_gap / left_pad：横向余量优先「全摊进列间距」填满内容区右缘；余量过大（needed 超封顶）
    // 时列间距封顶、剩余留白左右对称居中。cols=1 时 needed 无定义 → 单列居中、列距 = 入参 gap。
    // 演算（K143/K177）：needed = (C − cols·cell_w)/(cols−1)，由 cols 公式可证 always ≥ gap ≥ 0；
    //   cols ≥ 2：col_gap = min(needed, GRID_GAP_MAX)；needed ≤ GAP_MAX → left_pad = 0（填满）；
    //             needed > GAP_MAX → left_pad = (C − cols·cell_w − (cols−1)·col_gap)/2（封顶居中）；
    //   cols == 1：col_gap = gap、left_pad = (C − cell_w)/2（cell_w 已夹 ≤ C → left_pad ≥ 0）。
    let (col_gap, left_pad) = if cols >= 2 {
        let needed = (content_w - cols as f32 * cell_w) / (cols - 1) as f32;
        let col_gap = needed.min(GRID_GAP_MAX);
        let left_pad = if needed <= GRID_GAP_MAX {
            0.0
        } else {
            (content_w - cols as f32 * cell_w - (cols - 1) as f32 * col_gap) / 2.0
        };
        (col_gap, left_pad)
    } else {
        (gap, (content_w - cell_w) / 2.0)
    };
    let n = pages.len();
    let rows = n.div_ceil(cols);

    // 每页卡高：保持纵横比（`cell_h[i] = cell_w × (高/宽)`；宽 ≤ 0 按比例 1）。
    let cell_h: Vec<f32> = pages
        .iter()
        .map(|(w, h)| if *w > 0.0 { cell_w * (h / w) } else { cell_w })
        .collect();

    // row_page_start[r] = 第 r 行首张页下标（r = rows 时为总页数 n）。
    let row_page_start: Vec<usize> = (0..=rows).map(|r| (r * cols).min(n)).collect();

    // row_top：从 padding 起逐行累加「行高 + gap」（行高 = 该行内最大 cell_h）。
    let mut row_top = Vec::with_capacity(rows);
    let mut y = padding;
    for r in 0..rows {
        row_top.push(y);
        let s = row_page_start[r];
        let e = row_page_start[r + 1];
        let row_h = cell_h[s..e].iter().copied().fold(0.0f32, f32::max);
        y += row_h + gap;
    }
    // total_height = padding + Σ行高 + (rows−1)·gap + padding（去掉循环尾部多计的 gap）。
    let total_height = if rows == 0 { 0.0 } else { y - gap + padding };

    GridLayout {
        cols,
        rows,
        cell_w,
        gap,
        col_gap,
        left_pad,
        padding,
        row_top,
        row_page_start,
        cell_h,
        total_height,
    }
}

/// 网格缩放**列数钳制**：返回使列数恒在 `[least_cols, most_cols]` 内的缩放区间
/// `(z_min, z_max)`（`z_min` = 缩小端下限、`z_max` = 放大端上限；任意
/// `zoom ∈ [z_min, z_max]` ⇒ `least_cols ≤ cols(zoom) ≤ most_cols`）。
///
/// 语义对齐调用侧常量：本仓 `least_cols = GRID_MAX_COLS(2)`（放大极限正好 2 列，不可能到
/// 1 列）、`most_cols = GRID_MIN_COLS(9)`（缩小极限正好 9 列，不可能到 10+ 列）。网格缩放
/// 既有阅读共享的 `[ZOOM_MIN, ZOOM_MAX]` 不再钳网格（`set_grid_zoom` / input.rs 网格轮臂 /
/// render 每帧归一化改走本函数，随当前视口宽动态钳到 2/9 列）。
///
/// 演算（K143/K177）：`cols(zoom) = max(1, floor((C + g)/(base_card·zoom + g)))`，
/// `C = viewport_width − 2·padding`（内容区宽）、`g = gap`：
/// - 放大端 `cols ≥ k`（`k` = least_cols）：`floor((C+g)/(cell+g)) ≥ k ⇔ k·cell + (k−1)·g ≤ C
///   ⇔ cell ≤ (C − (k−1)·g)/k` ⇒ `z_max = (((C − (k−1)·g)/k) − 0.5)/base_card`（边界**再让
///   0.5px**，与缩小端 `+0.5px` 对称，防浮点把 `base_card × z_max` 抬过刀锋边界落到 k−1 列，
///   见下方实现注释；此值处 cols 恰 = k，以 k=2 验算 `(C+g)/((C−g)/2 + g) = 2`）；
/// - 缩小端 `cols ≤ m`（`m` = most_cols）：`floor((C+g)/(cell+g)) ≤ m ⇔ (m+1)·cell + m·g > C
///   ⇔ cell > (C − m·g)/(m+1)`；`+0.5px` 严格离开 open 边界，防浮点圈回 10+ 列；
///   `z_min = ((C − m·g)/(m+1) + 0.5)/base_card`。
///
/// 退化兜底（真实窗口不会触发，仅防病态输入）：负/极窄视口（`C < 0`）两端夹到极小正数
/// 防倒挂；参数倒挂（`z_min > z_max`）两端各取均值锁同一点，钳内 zoom 无定义但不产出
/// 越界病态值。
pub fn grid_zoom_limits_for_cols(
    base_card: f32,
    gap: f32,
    padding: f32,
    viewport_width: f32,
    least_cols: f32,
    most_cols: f32,
) -> (f32, f32) {
    let c = viewport_width - 2.0 * padding;
    // 放大端边界 = (C − (k−1)·g)/k 是**刀锋**：`fl(base_card × z_max)` 的 f32 舍入可能把
    // cell 抬过边界（评审位级实证 vw=402：`(179/280)×280` = 179.00002 > 179 → cols=1，违
    // 背「恒 2 列」）。从边界**再让 −0.5px**（与缩小端 +0.5px 对称）——`fl(base_card×z_max)
    // ≤ boundary` 恒成立，0.5px/280 ≈ 0.18% 不可见。
    let z_max = ((c - (least_cols - 1.0) * gap) / least_cols - 0.5) / base_card;
    let z_min = ((c - most_cols * gap) / (most_cols + 1.0) + 0.5) / base_card;
    const TINY: f32 = 1e-3;
    let (mut z_min, mut z_max) = (z_min.max(TINY), z_max.max(TINY));
    if z_min > z_max {
        let mid = (z_min + z_max) / 2.0;
        z_min = mid;
        z_max = mid;
    }
    (z_min, z_max)
}

impl GridLayout {
    /// 第 `r` 行的行高 = 该行内最大卡高（派生值，不入字段保持结构最小）。
    fn row_height(&self, r: usize) -> f32 {
        let s = self.row_page_start[r];
        let e = self.row_page_start[r + 1];
        self.cell_h[s..e].iter().copied().fold(0.0f32, f32::max)
    }

    /// 第 `i` 页卡的放置矩形 `(x, y, w, h)`（内容坐标系，见 [`GridLayout`]）；越界 → `None`。
    /// `x = padding + left_pad + (i % cols)·(cell_w + col_gap)`，`y = row_top[i / cols]`，
    /// 宽 `cell_w`、高 `cell_h[i]`（y/高不受横向拉伸影响）。
    pub fn cell_rect(&self, i: usize) -> Option<(f32, f32, f32, f32)> {
        if i >= self.cell_h.len() || self.cols == 0 {
            return None;
        }
        let r = i / self.cols;
        if r >= self.rows {
            return None;
        }
        let c = i % self.cols;
        let x = self.padding + self.left_pad + c as f32 * (self.cell_w + self.col_gap);
        let y = self.row_top[r];
        Some((x, y, self.cell_w, self.cell_h[i]))
    }

    /// 可见行区间（含端点，闭区间）：视口 `[offset, offset + viewport_h]` 命中的所有行，
    /// 二分行顶直读缓存 O(log rows)，与 [`keep_window`] 同姿势；0 行或视口完全落在内容
    /// 之外（上/下/悬在行间 gap 里）→ `None`。`viewport_h ≤ 1` 回退默认 720（首帧前口径）。
    pub fn first_visible_grid_row(&self, offset: f32, viewport_h: f32) -> Option<(usize, usize)> {
        if self.rows == 0 {
            return None;
        }
        let vh = if viewport_h > 1.0 { viewport_h } else { 720.0 };
        let top = offset.max(0.0);
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

    /// **行二分原语**：最后一个「行顶 ≤ y」的行下标（`row_top` 严格递增可二分；0 行 → 0）。
    /// 供 2D 命中 / 插入位实现共用同一套行几何（view 侧 `compute_insert_index` 与既有
    /// 命中语义都基于本原语，不再在 view 侧重复手写行二分）。调用方按需自行判定
    /// padding / 行间 gap 的语义（本原语只回答「y 属于哪一行」）。
    pub fn row_index_at_y(&self, y: f32) -> usize {
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
        lo.saturating_sub(1)
    }

    /// **列槽原语**：由内容 x 算整数列槽 `round((x − padding − left_pad) / (cell_w + col_gap))`
    /// （≥0；退化视口 pitch ≤ 0 → 0）。`round` 语义 = 卡左半/右半、列间 gap、右留白都归到最近
    /// 整数槽（gap 中点即阈值）；`x < padding + left_pad`（左留白内）→ 负值 round 后取 0。
    /// 供 2D 命中 / 插入位实现共用同一套列 pitch 几何（view 侧 `compute_insert_index` 与既有
    /// 命中语义都基于本原语）。
    pub fn col_slot_at_x(&self, x: f32) -> usize {
        let pitch = self.cell_w + self.col_gap;
        if pitch <= 0.0 {
            return 0; // 退化视口（卡宽 0）无法按列定位
        }
        (((x - self.padding - self.left_pad) / pitch)
            .round()
            .max(0.0)) as usize
    }
}
