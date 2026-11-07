//! 并行 grid 合成叶子（方案 A：worker 直贴共享画布）。
//!
//! 线程安全模型（本模块是唯一含不安全散贴代码的叶子，`#![allow(unsafe_code)]`）：
//! - [`GridLayout::build`] 预检「均匀栅格 + 逐平面（亮度/色度）沿两轴精确铺满画布」：返回 `Some`
//!   意味着每个画布元素的写入区域唯一、两两不相交，且覆盖全画布 → N 个 worker 各自
//!   [`paste_tile`] 的裸指针写不同区域，天然无数据竞争。
//! - 预检通过后画布以**未初始化** `Vec` 分配（[`allocate_planes`]），每个元素恰好被一个 worker
//!   散贴写入一次：36.6MB 级清零不再占主线程（清零并入各 worker 的「贴满即写」）。
//! - 散贴行事时序与 `compose_grid` 完全同构（共用 `canvas::tile_plane_copies`），只是写目标从
//!   `&mut [u16]` 换成裸指针；写入前做与 `paste_plane` 同款的 src/dst 越界安全重检（纯防御），
//!   失败经 ack `Err` 回传，主线程按 index 首错返回、画布随之 drop——未初始化字节不外泄（任何
//!   worker 失败 → 整链返回 Err，未初始化画布不会到达色转/read_image）。
//! - 失败/预检不过 → 调用方落回既有 `decode_tiles_par + compose_grid_owned`，不改语义。

#![allow(unsafe_code)]

use crate::codecs::heif::canvas::{self, TileYuv};
use crate::codecs::heif::error::{HeifError, HeifResult};

/// 布局预检的输入：单个 tile 的输出尺寸（conformance window 之后；从 hvcC SPS 推导，零解码）。
#[derive(Debug, Clone, Copy)]
pub(super) struct TileInfo {
    pub out_w: i32,
    pub out_h: i32,
}

/// 通过预检的均匀栅格布局。worker 据 `i` 推 `col = i % cols` 后散贴。
#[derive(Debug, Clone)]
pub(super) struct GridLayout {
    pub cols: i32,
    pub canvas_w: i32,
    pub canvas_h: i32,
    /// 画布各平面元素数（Y 平面 / 色度平面，与 `compose_grid` 的 `vec![0; *]` 长度一致）。
    pub y_len: usize,
    pub c_len: usize,
}

impl GridLayout {
    /// 预检是否可用于并行散贴：
    /// - tile 数恰为 `cols×rows`（缺失尾格 → 画布有洞 → 不能未初始化分配，落回）；
    /// - 所有 tile 输出尺寸相同（均匀栅格，且与 `compose_grid` 的 `(col*tw, row*th)` 落位自洽）；
    /// - 亮度（shift 0）与色度（shift 1）沿两轴均精确铺满画布平面（无空洞、无重叠、末段达边）。
    /// 任一不满足 → `None`，调用方落回既有路径（不崩、不改语义）。
    pub fn build(
        infos: &[TileInfo],
        cols: i32,
        rows: i32,
        canvas_w: i32,
        canvas_h: i32,
    ) -> Option<GridLayout> {
        if cols <= 0 || rows <= 0 || canvas_w <= 0 || canvas_h <= 0 {
            return None;
        }
        if infos.is_empty() || infos.len() != (i64::from(cols) * i64::from(rows)) as usize {
            return None;
        }
        let tw = infos[0].out_w;
        let th = infos[0].out_h;
        if tw <= 0 || th <= 0 {
            return None;
        }
        if infos.iter().any(|t| t.out_w != tw || t.out_h != th) {
            return None;
        }
        // 沿两轴：亮度/色度都必须精确贴满 → 画布每元素恰好被一个 tile 的写入覆盖。
        if !axis_covers(tw, cols, canvas_w, 0)
            || !axis_covers(th, rows, canvas_h, 0)
            || !axis_covers(tw, cols, canvas_w, 1)
            || !axis_covers(th, rows, canvas_h, 1)
        {
            return None;
        }
        let y_len = (canvas_w as usize) * (canvas_h as usize);
        let c_len = ((canvas_w + 1) >> 1) as usize * ((canvas_h + 1) >> 1) as usize;
        Some(GridLayout {
            cols,
            canvas_w,
            canvas_h,
            y_len,
            c_len,
        })
    }
}

/// 沿单轴判定「每 tile 的矩形段精确铺满平面」：无空洞、无重叠、末段达边。
/// `extent` = tile 该轴亮度跨度（均匀），`n` = 该轴 tile 数，`canvas` = 该轴亮度画布跨度，
/// `shift` = 平面子抽样（0 = 亮度平面，1 = 色度 4:2:0）。与 `compose_grid` 的
/// `dst_x=(col*tw)>>shift`、`copy_w=min(tw>>shift, dst_w-dst_x)` 剪裁语义逐拍一致：
/// 逐段 `start` 必须紧接上段末，且全程覆盖到平面末端（末段可伸出画布、由剪裁截断）。
fn axis_covers(extent: i32, n: i32, canvas: i32, shift: u32) -> bool {
    let plane = (canvas + ((1u32 << shift) as i32) - 1) >> shift;
    if extent <= 0 || n <= 0 {
        return false;
    }
    let step = extent >> shift;
    if step <= 0 {
        return false;
    }
    let mut prev_end: i64 = 0;
    for c in 0..n {
        let start = ((i64::from(c) * i64::from(extent)) >> shift) as i64;
        if start != prev_end {
            return false;
        }
        prev_end = start + i64::from(step);
    }
    prev_end >= i64::from(plane)
}

/// 共享画布三平面的裸指针句柄。`Send + Sync` 的理由：`decode_grid_into` 在 `std::thread::scope`
/// 内派 worker 同时持有本句柄，各 worker 只写 `GridLayout` 预检过的**互不相交**矩形；主线程在
/// join 后才读/归还画布；scope 借用保证画布 `Vec` 在 join 前不被 drop/重分配。
#[derive(Clone, Copy)]
pub(super) struct CanvasPtrs(
    pub(crate) *mut u16,
    pub(crate) *mut u16,
    pub(crate) *mut u16,
);

// SAFETY：见本类型文档——写区域预检互不相交、生命周期由调用方 scope 保证，跨线程类型安全。
unsafe impl Send for CanvasPtrs {}
// SAFETY：同上；`&CanvasPtrs` 共享只用于复制裸指针，裸指针只写互不相交区域。
unsafe impl Sync for CanvasPtrs {}

/// 从三平面 `&mut Vec` 取裸指针句柄。**调用方契约**：画布 `Vec` 须由调用方持有到
/// `std::thread::scope(...)` join 之后才归还/移动/重分配。
pub(super) fn canvas_ptrs(y: &mut Vec<u16>, cb: &mut Vec<u16>, cr: &mut Vec<u16>) -> CanvasPtrs {
    CanvasPtrs(y.as_mut_ptr(), cb.as_mut_ptr(), cr.as_mut_ptr())
}

/// 分配共享画布三平面（**未初始化**：每一元素由成功路径的散贴恰好写一次；失败路径立即随 Err
/// drop，未初始化值不可能到达下游）。ε代价：`Vec::with_capacity` + `set_len` 不触发 memset。
pub(super) fn allocate_planes(y_len: usize, c_len: usize) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
    let mut y = Vec::with_capacity(y_len);
    let mut cb = Vec::with_capacity(c_len);
    let mut cr = Vec::with_capacity(c_len);
    // SAFETY：容量 == 长度；元素由各 worker 的散贴恰好各写一次；任一 worker 失败 → 画布随 Err
    // 返回被 drop，未初始化元素不会在可靠读上下文中被读取（u16 亦无 Drop）。
    unsafe {
        y.set_len(y_len);
        cb.set_len(c_len);
        cr.set_len(c_len);
    }
    (y, cb, cr)
}

/// 把 `i` 号 tile 的三平面散贴进共享画布（裸指针目标）。**调用方契约**：`i < cols*rows`；
/// 画布裸指针指向 [`allocate_planes`] 分的三平面；各 worker 的写区域（由 `GridLayout` 预检）
/// 互不相交、且全部在画布界内。
///
/// 行事时序照抄 `compose_grid`（共用 `canvas::tile_plane_copies`）；写入前对 src/dst 越界做安全
/// 重检（预检已保证界内，纯防御），失败回传 `Err` 由主线程按 index 首错返回。
pub(super) fn paste_tile(
    layout: &GridLayout,
    i: usize,
    tile: &TileYuv,
    dst: CanvasPtrs,
) -> HeifResult<()> {
    let col = (i as i32) % layout.cols;
    let row = (i as i32) / layout.cols;
    let specs =
        canvas::tile_plane_copies(tile, col, row, layout.canvas_w, layout.canvas_h)?;
    for spec in specs.into_iter().flatten() {
        let src = match spec.plane {
            0 => &tile.y,
            1 => &tile.cb,
            _ => &tile.cr,
        };
        let (dst_ptr, dst_len) = match spec.plane {
            0 => (dst.0, layout.y_len),
            1 => (dst.1, layout.c_len),
            _ => (dst.2, layout.c_len),
        };
        // 安全重检（与 `paste_plane` 同款、末行最坏情况一次判定）：src/dst 任何一侧越界 → Err。
        let src_end = i64::from(spec.src_stride) * i64::from(spec.src_h);
        let last = spec.copy_h - 1;
        let so = i64::from(spec.src_y + last) * i64::from(spec.src_stride) + i64::from(spec.src_x);
        let do_ =
            i64::from(spec.dst_y + last) * i64::from(spec.dst_stride) + i64::from(spec.dst_x);
        let sw = i64::from(spec.copy_w);
        if so + sw > src_end
            || so + sw > src.len() as i64
            || do_ + sw > dst_len as i64
        {
            return Err(HeifError::invalid("tile 平面拷贝越界。"));
        }
        // SAFETY：dst 区域经上式与 `GridLayout` 预检在画布内、与其它 worker 写区不相交（互不
        // 覆盖）；`src` 是本 worker 独占的 `TileYuv`（本线程唯一持有）；`copy_nonoverlapping`
        // 的两端属不同的堆分配。行主序、无跨步、每行前移一行。
        unsafe {
            paste_plane_raw(dst_ptr, &spec, src);
        }
    }
    Ok(())
}

/// 裸指针逐行 `copy_nonoverlapping`。调用方保证两端界内且不相交。
unsafe fn paste_plane_raw(dst: *mut u16, spec: &canvas::PlaneCopy, src: &[u16]) {
    let dst_stride = spec.dst_stride as usize;
    let src_stride = spec.src_stride as usize;
    let sw = spec.copy_w as usize;
    unsafe {
        let mut d = dst.add(spec.dst_y as usize * dst_stride + spec.dst_x as usize);
        let mut s = src
            .as_ptr()
            .add(spec.src_y as usize * src_stride + spec.src_x as usize);
        for _ in 0..spec.copy_h {
            std::ptr::copy_nonoverlapping(s, d, sw);
            s = s.add(src_stride);
            d = d.add(dst_stride);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codecs::heif::canvas::{compose_grid, ComposedYuv};

    /// 造一个 4:2:0 tight 平面 tile（色度 = w/2 × h/2；与 canvas 测试同款）。
    fn tile(w: i32, h: i32, fill: u16, crop_right: i32, crop_top: i32) -> TileYuv {
        let c_n = ((w / 2) * (h / 2)) as usize;
        TileYuv {
            y: vec![fill; (w * h) as usize],
            cb: vec![fill; c_n],
            cr: vec![fill; c_n],
            y_w: w,
            y_h: h,
            c_w: w / 2,
            c_h: h / 2,
            crop_left: 0,
            crop_right,
            crop_top,
            crop_bottom: 0,
        }
    }

    /// 一组夹具 → (tiles, infos, cols, rows, canvas)。
    fn tile_grid(
        cols: i32,
        rows: i32,
        tw: i32,
        th: i32,
        fill: u16,
        crop_right: i32,
        crop_top: i32,
    ) -> (Vec<TileYuv>, Vec<TileInfo>, i32, i32, i32, i32) {
        let n = (cols * rows) as usize;
        let mut tiles = Vec::with_capacity(n);
        let mut infos = Vec::with_capacity(n);
        for k in 0..n {
            let f = fill + k as u16;
            tiles.push(tile(tw, th, f, crop_right, crop_top));
            let out_w = tw - crop_right;
            let out_h = th - crop_top;
            infos.push(TileInfo { out_w, out_h });
        }
        let canvas_w = cols * (tw - crop_right);
        let canvas_h = rows * (th - crop_top);
        (tiles, infos, cols, rows, canvas_w, canvas_h)
    }

    /// 多线程散贴（每 tile 一个 worker 并发写同一共享画布）后与 `compose_grid` 逐字节对拍。
    fn scatter_equals_compose(
        tiles: Vec<TileYuv>,
        infos: Vec<TileInfo>,
        cols: i32,
        rows: i32,
        canvas_w: i32,
        canvas_h: i32,
    ) {
        let layout = GridLayout::build(&infos, cols, rows, canvas_w, canvas_h)
            .unwrap_or_else(|| panic!("布局预检应通过"));
        let (mut y, mut cb, mut cr) = allocate_planes(layout.y_len, layout.c_len);
        let ptrs = canvas_ptrs(&mut y, &mut cb, &mut cr);
        let layout_ref = &layout;
        std::thread::scope(|scope| {
            for (i, t) in tiles.iter().enumerate() {
                scope.spawn(move || {
                    paste_tile(layout_ref, i, t, ptrs).expect("散贴应成功");
                });
            }
        });
        let got = ComposedYuv {
            y,
            cb,
            cr,
            width: canvas_w,
            height: canvas_h,
        };
        let expect = compose_grid(&tiles, cols, rows, canvas_w, canvas_h)
            .expect("compose_grid 应成功");
        assert_eq!(got.width, expect.width);
        assert_eq!(got.height, expect.height);
        assert_eq!(got.y, expect.y, "Y 平面逐元素不一致");
        assert_eq!(got.cb, expect.cb, "Cb 平面逐元素不一致");
        assert_eq!(got.cr, expect.cr, "Cr 平面逐元素不一致");
    }

    #[test]
    fn scattered_paste_matches_compose_2x2() {
        let (tiles, infos, cols, rows, cw, ch) = tile_grid(2, 2, 2, 2, 1, 0, 0);
        scatter_equals_compose(tiles, infos, cols, rows, cw, ch);
    }

    #[test]
    fn scattered_paste_matches_compose_3x3() {
        let (tiles, infos, cols, rows, cw, ch) = tile_grid(3, 3, 2, 2, 5, 0, 0);
        scatter_equals_compose(tiles, infos, cols, rows, cw, ch);
    }

    #[test]
    fn scattered_paste_matches_compose_48_tile() {
        // 6x8、2x2/2x2 → canvas 12x16，48 个 worker 并发散贴（对齐 IMG_0083 的 48-tile 形态）。
        let (tiles, infos, cols, rows, cw, ch) = tile_grid(6, 8, 2, 2, 1, 0, 0);
        scatter_equals_compose(tiles, infos, cols, rows, cw, ch);
    }

    #[test]
    fn scattered_paste_matches_compose_crop_10bit() {
        // 带 conformance crop（crop_right=2、奇数步进子抽样）且填充值 >255（10 位采样口径）。
        let (tiles, infos, cols, rows, cw, ch) = tile_grid(2, 2, 6, 4, 1000, 2, 0);
        scatter_equals_compose(tiles, infos, cols, rows, cw, ch);
    }

    #[test]
    fn scattered_paste_matches_compose_last_col_clipped() {
        // 末列被画布剪裁（canvas_w 非 tw 整倍数）：tw=2、cols=2、canvas_w=3 → 末 tile 只写 1 列。
        let tiles = vec![tile(2, 2, 1, 0, 0), tile(2, 2, 2, 0, 0)];
        let infos = vec![TileInfo { out_w: 2, out_h: 2 }, TileInfo { out_w: 2, out_h: 2 }];
        // canvas 3x2：第一 tile [0,2)，第二 tile 剪到 [2,3)。
        let layout = GridLayout::build(&infos, 2, 1, 3, 2).expect("应通过（亮度/色度均贴满）");
        let (mut y, mut cb, mut cr) = allocate_planes(layout.y_len, layout.c_len);
        let ptrs = canvas_ptrs(&mut y, &mut cb, &mut cr);
        let layout_ref = &layout;
        std::thread::scope(|scope| {
            for (i, t) in tiles.iter().enumerate() {
                scope.spawn(move || {
                    paste_tile(layout_ref, i, t, ptrs).expect("散贴应成功");
                });
            }
        });
        let got = ComposedYuv { y, cb, cr, width: 3, height: 2 };
        let expect = compose_grid(&tiles, 2, 1, 3, 2).expect("compose 应成功");
        assert_eq!(got.y, expect.y, "Y 落位不一致");
        assert_eq!(got.cb, expect.cb);
        assert_eq!(got.cr, expect.cr);
    }

    #[test]
    fn build_rejects_odd_chroma_gap() {
        // 奇数亮度 tile（tw=3）两列：色度 3 的平面只被覆盖 2 → 空洞，必须 None（落回串行）。
        let infos = vec![TileInfo { out_w: 3, out_h: 3 }, TileInfo { out_w: 3, out_h: 3 }];
        assert!(GridLayout::build(&infos, 2, 1, 6, 3).is_none());
    }

    #[test]
    fn build_rejects_bad_geometry() {
        let ok = TileInfo { out_w: 2, out_h: 2 };
        let other = TileInfo { out_w: 3, out_h: 2 };
        // tile 数 != cols*rows。
        assert!(GridLayout::build(&[ok], 2, 2, 4, 4).is_none());
        // 空 / 尺寸非法。
        assert!(GridLayout::build(&[], 1, 1, 2, 2).is_none());
        assert!(GridLayout::build(&[ok], 0, 1, 2, 2).is_none());
        assert!(GridLayout::build(&[ok], 1, 1, 0, 2).is_none());
        // 非均匀栅格。
        assert!(GridLayout::build(&[ok, other], 2, 1, 5, 2).is_none());
        // 亮度不覆盖画布（4*2 < 10）。
        assert!(GridLayout::build(&[ok; 4], 4, 1, 10, 2).is_none());
    }
}
