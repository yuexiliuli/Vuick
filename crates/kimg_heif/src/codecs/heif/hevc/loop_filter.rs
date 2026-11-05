//! 环路滤波/deblocking（§8.7）：`HevcFrameDecoder` 的 in-loop 滤波方法，逐函数移植自 C#
//! `Hevc\HevcLoopFilter.cs` 的 partial class（`Deblock()`/`ApplySao()` 及其下钻），与 C# 字节恒等。
//!
//! 次序与 C# 相同：`filter_picture` 先跑全图竖直边，再跑全图水平边（8.7.2.1），最后逐分量按 CTB 行
//! 应用 SAO（8.7.3）。β/tC 查表、bS/qp 取用、强弱滤波选择、4/2 样本更新、SAO 边/带分类与偏移装配
//! 全部逐元素照抄；`>>`/溢出按 C# 未检查语义（`wrapping_*` / 算术移位），零 panic（越界返回 `Err`）。
//!
//! 去块边标志（`edge_ver`/`edge_hor`）与 `no_filter`（transquant-bypass CU）由解码期
//! `frame_decoder::MarkTransformEdges` / `coding_unit` 写入（对齐 C# 的 deblocking_filter_control）。

use crate::codecs::heif::error::{HeifError, HeifResult};
use crate::codecs::heif::hevc::frame_decoder::{HevcFrameDecoder, SaoParams};
use crate::codecs::heif::hevc::sao::HevcSao;
use crate::codecs::heif::hevc::slice_header::HevcSliceHeader;

/// β′（表 8-12，Q = 0..51）。
static BETA_TABLE: [u8; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 20, 22, 24,
    26, 28, 30, 32, 34, 36, 38, 40, 42, 44, 46, 48, 50, 52, 54, 56, 58, 60, 62, 64,
];

/// tC′（表 8-12，Q = 0..53）。
static TC_TABLE: [u8; 54] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 3,
    3, 3, 3, 4, 4, 4, 5, 5, 6, 6, 7, 8, 9, 10, 11, 13, 14, 16, 18, 20, 22, 24,
];

fn get_s(s: &[u16], idx: i32) -> HeifResult<u16> {
    if idx < 0 || (idx as usize) >= s.len() {
        return Err(HeifError::invalid("去块样本访问越界。"));
    }
    Ok(s[idx as usize])
}

fn set_s(s: &mut [u16], idx: i32, v: u16) -> HeifResult<()> {
    if idx < 0 || (idx as usize) >= s.len() {
        return Err(HeifError::invalid("去块样本写入越界。"));
    }
    s[idx as usize] = v;
    Ok(())
}

/// P 侧采样（i 个跨边滞后、k 个沿边推进）。
fn p_at(s: &[u16], origin: i32, across: i32, along: i32, i: i32, k: i32) -> HeifResult<i32> {
    Ok(i32::from(get_s(s, origin - (i + 1) * across + k * along)?))
}

/// Q 侧采样。
fn q_at(s: &[u16], origin: i32, across: i32, along: i32, i: i32, k: i32) -> HeifResult<i32> {
    Ok(i32::from(get_s(s, origin + i * across + k * along)?))
}

/// 8.7.2.5.3（判定）与 8.7.2.5.4 / 8.7.2.5.7（滤波）：一段 4 采样亮度边。
#[allow(clippy::too_many_arguments)]
fn filter_luma_edge(
    s: &mut [u16],
    origin: i32,
    across: i32,
    along: i32,
    beta: i32,
    tc: i32,
    no_p: bool,
    no_q: bool,
    max: i32,
) -> HeifResult<()> {
    let dp0 = (p_at(s, origin, across, along, 2, 0)? - 2 * p_at(s, origin, across, along, 1, 0)?
        + p_at(s, origin, across, along, 0, 0)?)
        .abs();
    let dp3 = (p_at(s, origin, across, along, 2, 3)? - 2 * p_at(s, origin, across, along, 1, 3)?
        + p_at(s, origin, across, along, 0, 3)?)
        .abs();
    let dq0 = (q_at(s, origin, across, along, 2, 0)? - 2 * q_at(s, origin, across, along, 1, 0)?
        + q_at(s, origin, across, along, 0, 0)?)
        .abs();
    let dq3 = (q_at(s, origin, across, along, 2, 3)? - 2 * q_at(s, origin, across, along, 1, 3)?
        + q_at(s, origin, across, along, 0, 3)?)
        .abs();
    let dpq0 = dp0 + dq0;
    let dpq3 = dp3 + dq3;
    let dp = dp0 + dp3;
    let dq = dq0 + dq3;
    let d = dpq0 + dpq3;
    if d >= beta {
        return Ok(());
    }

    let d_sam0 = strong_decision(
        2 * dpq0,
        p_at(s, origin, across, along, 0, 0)?,
        p_at(s, origin, across, along, 3, 0)?,
        q_at(s, origin, across, along, 0, 0)?,
        q_at(s, origin, across, along, 3, 0)?,
        beta,
        tc,
    );
    let d_sam3 = strong_decision(
        2 * dpq3,
        p_at(s, origin, across, along, 0, 3)?,
        p_at(s, origin, across, along, 3, 3)?,
        q_at(s, origin, across, along, 0, 3)?,
        q_at(s, origin, across, along, 3, 3)?,
        beta,
        tc,
    );
    let d_e = if d_sam0 && d_sam3 { 2 } else { 1 };
    let d_ep = dp < ((beta + (beta >> 1)) >> 3);
    let d_eq = dq < ((beta + (beta >> 1)) >> 3);

    for k in 0..4 {
        let p0 = p_at(s, origin, across, along, 0, k)?;
        let p1 = p_at(s, origin, across, along, 1, k)?;
        let p2 = p_at(s, origin, across, along, 2, k)?;
        let p3 = p_at(s, origin, across, along, 3, k)?;
        let q0 = q_at(s, origin, across, along, 0, k)?;
        let q1 = q_at(s, origin, across, along, 1, k)?;
        let q2 = q_at(s, origin, across, along, 2, k)?;
        let q3 = q_at(s, origin, across, along, 3, k)?;
        let base_p = origin + k * along - across;
        let base_q = origin + k * along;
        if d_e == 2 {
            // (8-389 .. 8-394)。
            if !no_p {
                set_s(s, base_p, ((p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3).clamp(p0 - 2 * tc, p0 + 2 * tc) as u16)?;
                set_s(s, base_p - across, ((p2 + p1 + p0 + q0 + 2) >> 2).clamp(p1 - 2 * tc, p1 + 2 * tc) as u16)?;
                set_s(s, base_p - 2 * across, ((2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3).clamp(p2 - 2 * tc, p2 + 2 * tc) as u16)?;
            }
            if !no_q {
                set_s(s, base_q, ((p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3).clamp(q0 - 2 * tc, q0 + 2 * tc) as u16)?;
                set_s(s, base_q + across, ((p0 + q0 + q1 + q2 + 2) >> 2).clamp(q1 - 2 * tc, q1 + 2 * tc) as u16)?;
                set_s(s, base_q + 2 * across, ((p0 + q0 + q1 + 3 * q2 + 2 * q3 + 4) >> 3).clamp(q2 - 2 * tc, q2 + 2 * tc) as u16)?;
            }
        } else {
            // (8-395 .. 8-402)。
            let mut delta = (9 * (q0 - p0) - 3 * (q1 - p1) + 8) >> 4;
            if delta.abs() >= tc * 10 {
                continue;
            }
            delta = delta.clamp(-tc, tc);
            if !no_p {
                set_s(s, base_p, (p0 + delta).clamp(0, max) as u16)?;
                if d_ep {
                    let delta_p = ((((p2 + p0 + 1) >> 1) - p1 + delta) >> 1).clamp(-(tc >> 1), tc >> 1);
                    set_s(s, base_p - across, (p1 + delta_p).clamp(0, max) as u16)?;
                }
            }
            if !no_q {
                set_s(s, base_q, (q0 - delta).clamp(0, max) as u16)?;
                if d_eq {
                    let delta_q = ((((q2 + q0 + 1) >> 1) - q1 - delta) >> 1).clamp(-(tc >> 1), tc >> 1);
                    set_s(s, base_q + across, (q1 + delta_q).clamp(0, max) as u16)?;
                }
            }
        }
    }
    Ok(())
}

/// 8.7.2.5.6。
fn strong_decision(dpq: i32, p0: i32, p3: i32, q0: i32, q3: i32, beta: i32, tc: i32) -> bool {
    dpq < (beta >> 2)
        && (p3 - p0).abs() + (q0 - q3).abs() < (beta >> 3)
        && (p0 - q0).abs() < ((5 * tc + 1) >> 1)
}

/// 8.7.2.5.5 / 8.7.2.5.8：一段 4 采样色度边（2 样本更新）。
#[allow(clippy::too_many_arguments)]
fn filter_chroma_edge(
    s: &mut [u16],
    origin: i32,
    across: i32,
    along: i32,
    tc: i32,
    no_p: bool,
    no_q: bool,
    max: i32,
) -> HeifResult<()> {
    for k in 0..4 {
        let q = origin + k * along;
        let p = q - across;
        let p0 = i32::from(get_s(s, p)?);
        let p1 = i32::from(get_s(s, p - across)?);
        let q0 = i32::from(get_s(s, q)?);
        let q1 = i32::from(get_s(s, q + across)?);
        let delta = ((((q0 - p0) << 2) + p1 - q1 + 4) >> 3).clamp(-tc, tc);
        if !no_p {
            set_s(s, p, (p0 + delta).clamp(0, max) as u16)?;
        }
        if !no_q {
            set_s(s, q, (q0 - delta).clamp(0, max) as u16)?;
        }
    }
    Ok(())
}

/// 取 `s` 从 `start`（须 ≥ 0）起 `count` 个样本的只读跨度（越界返回错误而非 panic）。
fn span(s: &[u16], start: i32, count: usize) -> HeifResult<&[u16]> {
    if start < 0 {
        return Err(HeifError::invalid("SAO 源越界。"));
    }
    s.get(start as usize..(start as usize + count))
        .ok_or_else(|| HeifError::invalid("SAO 源越界。"))
}

/// 边偏移的快速路径（C# `SaoEdgeFast`）：邻居全部可用时每行一整段走 `HevcSao::edge_row`，
/// 只由图画边界排除样本（8.7.3.2）。读 `strip`（去块后），写 `dst`（输出平面）。
#[allow(clippy::too_many_arguments)]
fn sao_edge_fast(
    source: &[u16],
    source_offset: i32,
    dst: &mut [u16],
    width: i32,
    height: i32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    eo_class: i32,
    bit_depth: i32,
    offsets: &[i32],
) -> HeifResult<()> {
    // 表 8-13；hPos[1] = −hPos[0]、vPos[1] = −vPos[0]。
    let (h_pos, v_pos) = match eo_class {
        0 => (-1, 0),
        1 => (0, -1),
        2 => (-1, -1),
        _ => (1, -1),
    };
    let (mut xs, mut xe, mut ys, mut ye) = (x0, x1, y0, y1);
    if h_pos != 0 {
        if xs == 0 {
            xs = 1;
        }
        if xe == width {
            xe = width - 1;
        }
    }
    if v_pos != 0 {
        if ys == 0 {
            ys = 1;
        }
        if ye == height {
            ye = height - 1;
        }
    }
    let count = (xe - xs) as usize;
    if count == 0 {
        return Ok(());
    }
    let max = (1 << bit_depth) - 1;
    let neighbour = v_pos * width + h_pos;
    for y in ys..ye {
        let at = y * width + xs;
        let src = source_offset + at;
        let cur = span(source, src, count)?;
        let a = span(source, src + neighbour, count)?;
        let b = span(source, src - neighbour, count)?;
        let drow = dst
            .get_mut(at as usize..(at + count as i32) as usize)
            .ok_or_else(|| HeifError::invalid("SAO 边偏移目标越界。"))?;
        HevcSao::edge_row(cur, a, b, drow, count, offsets, max)?;
    }
    Ok(())
}

/// 带偏移的逐样本路径（C# `SaoBand`）：`anyNoFilter` 时按样本跳过 transquant-bypass CU。
#[allow(clippy::too_many_arguments)]
fn sao_band(
    plane: &mut [u16],
    stride: i32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    c: usize,
    p: &SaoParams,
    bit_depth: i32,
    no_filter: &[u8],
    w4: i32,
    luma_shift: i32,
) -> HeifResult<()> {
    let mut band_table = [0i32; 32];
    let mut offset_val = [0i32; 5];
    let band = i32::from(p.band(c));
    for k in 0..4 {
        offset_val[k + 1] = i32::from(p.offset(c, k));
        band_table[((k as i32 + band) & 31) as usize] = (k + 1) as i32;
    }
    let shift = bit_depth - 5;
    let max = (1 << bit_depth) - 1;
    let pw = stride as usize;
    for y in y0..y1 {
        for x in x0..x1 {
            let block = (((y << luma_shift) >> 2) * w4 + ((x << luma_shift) >> 2)) as usize;
            if no_filter[block] != 0 {
                continue;
            }
            let idx = y as usize * pw + x as usize;
            let v = i32::from(plane[idx]);
            let band_idx = band_table[(v.checked_shr(shift as u32).unwrap_or(0) as usize) & 31];
            if band_idx != 0 {
                plane[idx] = (v + offset_val[band_idx as usize]).clamp(0, max) as u16;
            }
        }
    }
    Ok(())
}

/// 边偏移的逐样本慢路径（C# `SaoEdge`）：有 noFilter 块或邻居不全可用时逐样本检查切片边界条件。
/// 读 `strip`（去块后），写 `plane`。
#[allow(clippy::too_many_arguments)]
fn sao_edge(
    strip: &[u16],
    strip_offset: i32,
    plane: &mut [u16],
    width: i32,
    height: i32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    c: usize,
    p: &SaoParams,
    bit_depth: i32,
    ctb_addr: i32,
    ctb_log2: i32,
    pic_width_in_ctbs: i32,
    no_filter: &[u8],
    w4: i32,
    ctb_slice_addr: &[i32],
    ctb_slice: &[HevcSliceHeader],
    luma_shift: i32,
) -> HeifResult<()> {
    // 表 8-13。
    let (h_pos0, v_pos0, h_pos1, v_pos1) = match p.class_(c) {
        0 => (-1, 0, 1, 0),
        1 => (0, -1, 0, 1),
        2 => (-1, -1, 1, 1),
        _ => (1, -1, -1, 1),
    };
    let mut offset_val = [0i32; 5];
    for k in 0..4 {
        offset_val[k + 1] = i32::from(p.offset(c, k));
    }
    let max = (1 << bit_depth) - 1;
    let pw = width as usize;
    let current_slice = &ctb_slice[ctb_addr as usize];
    let at = |x: i32, y: i32| -> HeifResult<usize> {
        let idx = strip_offset + y * width + x;
        if idx < 0 {
            return Err(HeifError::invalid("SAO 边偏移源越界。"));
        }
        Ok(idx as usize)
    };
    for y in y0..y1 {
        for x in x0..x1 {
            let block = (((y << luma_shift) >> 2) * w4 + ((x << luma_shift) >> 2)) as usize;
            if no_filter[block] != 0 {
                continue;
            }
            let (xa, ya) = (x + h_pos0, y + v_pos0);
            let (xb, yb) = (x + h_pos1, y + v_pos1);
            if xa < 0
                || ya < 0
                || xa >= width
                || ya >= height
                || xb < 0
                || yb < 0
                || xb >= width
                || yb >= height
            {
                continue;
            }
            if !sao_neighbour_usable(
                xa << luma_shift,
                ya << luma_shift,
                ctb_addr,
                current_slice,
                ctb_log2,
                pic_width_in_ctbs,
                ctb_slice_addr,
                ctb_slice,
            ) || !sao_neighbour_usable(
                xb << luma_shift,
                yb << luma_shift,
                ctb_addr,
                current_slice,
                ctb_log2,
                pic_width_in_ctbs,
                ctb_slice_addr,
                ctb_slice,
            ) {
                continue;
            }
            let v = i32::from(strip[at(x, y)?]);
            let va = i32::from(strip[at(xa, ya)?]);
            let vb = i32::from(strip[at(xb, yb)?]);
            let mut edge_idx = 2 + (v - va).signum() + (v - vb).signum();
            // (8-411 .. 8-413) 的重映射：局部极小取 offsetVal[1] … 局部极大取 offsetVal[4]。
            if edge_idx <= 2 {
                edge_idx = if edge_idx == 2 { 0 } else { edge_idx + 1 };
            }
            if edge_idx != 0 {
                let idx = y as usize * pw + x as usize;
                plane[idx] = (v + offset_val[edge_idx as usize]).clamp(0, max) as u16;
            }
        }
    }
    Ok(())
}

/// 8.7.3.2 对邻居样本（亮度坐标 (xN, yN)）的切片边界条件。
#[allow(clippy::too_many_arguments)]
fn sao_neighbour_usable(
    x_n: i32,
    y_n: i32,
    ctb_addr: i32,
    current_slice: &HevcSliceHeader,
    ctb_log2: i32,
    pic_width_in_ctbs: i32,
    ctb_slice_addr: &[i32],
    ctb_slice: &[HevcSliceHeader],
) -> bool {
    let neighbour_ctb = (y_n >> ctb_log2) * pic_width_in_ctbs + (x_n >> ctb_log2);
    if neighbour_ctb == ctb_addr {
        return true;
    }
    let neighbour_slice = &ctb_slice[neighbour_ctb as usize];
    if ctb_slice_addr[neighbour_ctb as usize] == ctb_slice_addr[ctb_addr as usize] {
        return true;
    }
    // 不同切片：由后解码那个切片的标志决定（无 tile 时按 CTB 栅格序）。
    if neighbour_ctb < ctb_addr {
        current_slice.loop_filter_across_slices
    } else {
        neighbour_slice.loop_filter_across_slices
    }
}

/// `ctbAddr` 周围存在的每个 CTB 都可用（边偏移读 `SaoNeighbourUsable` 只看邻居样本的 CTB）。
#[allow(clippy::too_many_arguments)]
fn sao_neighbours_usable(
    rx: i32,
    ry: i32,
    ctb_addr: i32,
    slice: &HevcSliceHeader,
    ctb_log2: i32,
    pic_width_in_ctbs: i32,
    pic_height_in_ctbs: i32,
    ctb_slice_addr: &[i32],
    ctb_slice: &[HevcSliceHeader],
) -> bool {
    for dy in -1..=1 {
        for dx in -1..=1 {
            let (nx, ny) = (rx + dx, ry + dy);
            if (dx | dy) == 0 || nx < 0 || ny < 0 || nx >= pic_width_in_ctbs || ny >= pic_height_in_ctbs
            {
                continue;
            }
            if !sao_neighbour_usable(
                nx << ctb_log2,
                ny << ctb_log2,
                ctb_addr,
                slice,
                ctb_log2,
                pic_width_in_ctbs,
                ctb_slice_addr,
                ctb_slice,
            ) {
                return false;
            }
        }
    }
    true
}

impl HevcFrameDecoder {
    /// 8.7.2.1：先全图竖直边，再全图水平边（`Deblock()`）。
    pub(crate) fn deblock(&mut self) -> HeifResult<()> {
        let ctb_size = self.sps.ctb_size_y();
        let height = self.sps.height;
        let rows = self.sps.pic_height_in_ctbs();
        for r in 0..rows {
            self.deblock_direction(true, r * ctb_size, ((r + 1) * ctb_size).min(height))?;
        }
        for r in 0..rows {
            self.deblock_direction(false, r * ctb_size, ((r + 1) * ctb_size).min(height))?;
        }
        Ok(())
    }

    /// 一个 CTB 带上的单方向去块（C# `DeblockDirection`）。
    fn deblock_direction(&mut self, vertical: bool, y_from: i32, y_to: i32) -> HeifResult<()> {
        let width = self.sps.width;
        let bit_depth_y = self.sps.bit_depth_y;
        let bit_depth_c = self.sps.bit_depth_c;
        let chroma_stride = self.picture.chroma_width;
        let cb_qp_offset = self.pps.cb_qp_offset;
        let cr_qp_offset = self.pps.cr_qp_offset;
        let ctb_log2 = self.sps.ctb_log2_size_y;
        let pic_width_in_ctbs = self.sps.pic_width_in_ctbs();
        let w4 = self.w4;
        let block = |x: i32, y: i32| -> usize { ((y >> 2) * w4 + (x >> 2)) as usize };

        let edges: &[u8] = if vertical { &self.edge_ver } else { &self.edge_hor };
        let qp_y = &self.qp_y;
        let no_filter = &self.no_filter;
        let ctb_slice = &self.ctb_slice;
        let planes = &mut self.picture.planes;
        let [ref mut luma, ref mut pcb, ref mut pcr] = *planes;

        let bit_scale = 1 << (bit_depth_y - 8);
        let max_luma = (1 << bit_depth_y) - 1;
        let chroma_scale = 1 << (bit_depth_c - 8);
        let max_c = (1 << bit_depth_c) - 1;

        let mut y = y_from;
        while y < y_to {
            let mut x = 0;
            while x < width {
                if (if vertical { x } else { y }) % 8 != 0 {
                    x += 4;
                    continue;
                }
                let b = block(x, y);
                let b_s = edges[b];
                if b_s == 0 {
                    x += 4;
                    continue;
                }
                let x_p = if vertical { x - 1 } else { x };
                let y_p = if vertical { y } else { y - 1 };
                let qp_q = i32::from(qp_y[b]);
                let qp_p = i32::from(qp_y[block(x_p, y_p)]);
                let slice = &ctb_slice
                    [((y >> ctb_log2) * pic_width_in_ctbs + (x >> ctb_log2)) as usize];
                let no_p = no_filter[block(x_p, y_p)] != 0;
                let no_q = no_filter[b] != 0;

                let qp_l = (qp_q + qp_p + 1) >> 1;
                let beta = i32::from(BETA_TABLE[(qp_l + (slice.beta_offset_div2 << 1)).clamp(0, 51) as usize])
                    * bit_scale;
                let tc = i32::from(TC_TABLE
                    [(qp_l + 2 * (b_s as i32 - 1) + (slice.tc_offset_div2 << 1)).clamp(0, 53) as usize])
                    * bit_scale;
                let stride = width;
                let across = if vertical { 1 } else { stride };
                let along = if vertical { stride } else { 1 };
                filter_luma_edge(luma, y * stride + x, across, along, beta, tc, no_p, no_q, max_luma)?;

                // 色度（4:2:0）：亮度 16 网格上的边，即亮度位置 8 样本一段；bS 必须 2。
                if b_s == 2
                    && (if vertical { x } else { y }) % 16 == 0
                    && (if vertical { y } else { x }) % 8 == 0
                {
                    let c_base = (y >> 1) * chroma_stride + (x >> 1);
                    let c_across = if vertical { 1 } else { chroma_stride };
                    let c_along = if vertical { chroma_stride } else { 1 };
                    for c in 1..=2usize {
                        let offset = if c == 1 { cb_qp_offset } else { cr_qp_offset };
                        let q_pi = ((qp_q + qp_p + 1) >> 1) + offset;
                        let qp_c = Self::chroma_qp_table(q_pi);
                        let tc_c = i32::from(TC_TABLE
                            [(qp_c + 2 * (b_s as i32 - 1) + (slice.tc_offset_div2 << 1)).clamp(0, 53) as usize])
                            * chroma_scale;
                        let plane_chroma = if c == 1 { &mut **pcb } else { &mut **pcr };
                        filter_chroma_edge(
                            plane_chroma,
                            c_base,
                            c_across,
                            c_along,
                            tc_c,
                            no_p,
                            no_q,
                            max_c,
                        )?;
                    }
                }
                x += 4;
            }
            y += 4;
        }
        Ok(())
    }

    /// 全图 SAO 应用（C# `ApplySao`）：每个分量独立，按 CTB 行从去块后的图画读出并重写该行
    /// （边偏移的行带用 `strip` 保留出没重写的上/下边界行）。
    pub(crate) fn apply_sao(&mut self) -> HeifResult<()> {
        let sao_enabled = self.sps.sao_enabled;
        if !sao_enabled {
            return Ok(());
        }
        if !self.ctb_slice.iter().any(|h| h.sao_luma || h.sao_chroma) {
            return Ok(());
        }
        let ctb_size_y = self.sps.ctb_size_y();
        let bit_depth_y = self.sps.bit_depth_y;
        let bit_depth_c = self.sps.bit_depth_c;
        let rows = self.sps.pic_height_in_ctbs();
        let any_no_filter = self.no_filter.contains(&1);
        for c in 0..3 {
            let width = self.picture.plane_width(c);
            let height = self.picture.plane_height(c);
            let ctb_size = if c == 0 { ctb_size_y } else { ctb_size_y >> 1 };
            let bit_depth = if c == 0 { bit_depth_y } else { bit_depth_c };
            let pw = width as usize;
            // [2r]：r 行上邻行；[2r + 1]：r 行下邻行。
            let mut borders = vec![0u16; (2 * rows as usize) * pw];
            {
                let plane = &self.picture.planes[c];
                for r in 0..rows {
                    let y0 = r * ctb_size;
                    if y0 > 0 {
                        let s = (y0 - 1) as usize * pw;
                        let d = (2 * r as usize) * pw;
                        borders[d..d + pw].copy_from_slice(&plane[s..s + pw]);
                    }
                    if y0 + ctb_size < height {
                        let s = (y0 + ctb_size) as usize * pw;
                        let d = (2 * r as usize + 1) * pw;
                        borders[d..d + pw].copy_from_slice(&plane[s..s + pw]);
                    }
                }
            }
            for r in 0..rows {
                self.sao_ctb_row(c, r, &borders, width, height, ctb_size, bit_depth, any_no_filter)?;
            }
        }
        Ok(())
    }

    /// 组件 `c` 的 CTB 行 `ry` 的 SAO（C# `SaoCtbRow`）。
    #[allow(clippy::too_many_arguments)]
    fn sao_ctb_row(
        &mut self,
        c: usize,
        ry: i32,
        borders: &[u16],
        width: i32,
        height: i32,
        ctb_size: i32,
        bit_depth: i32,
        any_no_filter: bool,
    ) -> HeifResult<()> {
        let y0 = ry * ctb_size;
        let y1 = (y0 + ctb_size).min(height);
        let pw = width as usize;
        // strip：行 y0 − 1 + k（上面已存行、本行自带行、下面已存行）。
        let mut strip = vec![0u16; ((ctb_size + 2) as usize) * pw];
        {
            let plane = &self.picture.planes[c];
            if y0 > 0 {
                let d = (2 * ry as usize) * pw;
                strip[..pw].copy_from_slice(&borders[d..d + pw]);
            }
            strip[pw..pw + ((y1 - y0) as usize) * pw]
                .copy_from_slice(&plane[y0 as usize * pw..y1 as usize * pw]);
            if y1 < height {
                let d = (2 * ry as usize + 1) * pw;
                let dd = ((y1 - y0) as usize + 1) * pw;
                strip[dd..dd + pw].copy_from_slice(&borders[d..d + pw]);
            }
        }
        // strip[stripOffset + y * width + x] = 去块后 (x, y)。
        let strip_offset = -(y0 - 1) * width;

        let w4 = self.w4;
        let ctb_log2 = self.sps.ctb_log2_size_y;
        let pic_width_in_ctbs = self.sps.pic_width_in_ctbs();
        let pic_height_in_ctbs = self.sps.pic_height_in_ctbs();
        let ctb_slice_addr = &self.ctb_slice_addr;
        let ctb_slice = &self.ctb_slice;
        let sao = &self.sao;
        let no_filter = &self.no_filter;
        let luma_shift = if c == 0 { 0 } else { 1 };
        let planes = &mut self.picture.planes;
        let plane = &mut planes[c];
        let mut offs = [0i32; 4];

        for rx in 0..pic_width_in_ctbs {
            let ctb_addr = ry * pic_width_in_ctbs + rx;
            let slice = &ctb_slice[ctb_addr as usize];
            let slice_sao = if c == 0 { slice.sao_luma } else { slice.sao_chroma };
            if !slice_sao {
                continue;
            }
            let p = &sao[ctb_addr as usize];
            let ty = p.ty(c);
            if ty == 0 {
                continue;
            }
            let x0 = rx * ctb_size;
            let x1 = (x0 + ctb_size).min(width);
            for (i, slot) in offs.iter_mut().enumerate() {
                *slot = i32::from(p.offset(c, i));
            }
            if ty == 1 {
                // 带偏移：只读它写的那一个样本（原地）。
                if any_no_filter {
                    sao_band(plane, width, x0, y0, x1, y1, c, p, bit_depth, no_filter, w4, luma_shift)?;
                    continue;
                }
                let band_pos = i32::from(p.band(c));
                let count = (x1 - x0) as usize;
                let mut buf = vec![0u16; count];
                for y in y0..y1 {
                    let s = y as usize * pw + x0 as usize;
                    HevcSao::band_row_scalar(
                        &plane[s..s + count],
                        &mut buf,
                        0,
                        count,
                        band_pos,
                        bit_depth,
                        &offs,
                    )?;
                    plane[s..s + count].copy_from_slice(&buf);
                }
            } else if any_no_filter || !sao_neighbours_usable(
                rx,
                ry,
                ctb_addr,
                slice,
                ctb_log2,
                pic_width_in_ctbs,
                pic_height_in_ctbs,
                ctb_slice_addr,
                ctb_slice,
            ) {
                sao_edge(
                    &strip,
                    strip_offset,
                    plane,
                    width,
                    height,
                    x0,
                    y0,
                    x1,
                    y1,
                    c,
                    p,
                    bit_depth,
                    ctb_addr,
                    ctb_log2,
                    pic_width_in_ctbs,
                    no_filter,
                    w4,
                    ctb_slice_addr,
                    ctb_slice,
                    luma_shift,
                )?;
            } else {
                sao_edge_fast(
                    &strip,
                    strip_offset,
                    plane,
                    width,
                    height,
                    x0,
                    y0,
                    x1,
                    y1,
                    i32::from(p.class_(c)),
                    bit_depth,
                    &offs,
                )?;
            }
        }
        Ok(())
    }
}
