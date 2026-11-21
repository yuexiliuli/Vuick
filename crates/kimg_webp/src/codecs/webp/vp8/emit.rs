use super::*;

// ---------------------------------------------------------------------------
// V4：YUV→RGBA（fancy 上采样 + 定点转换；C# `WebpDecoder.cs` 有损路径标量同款）
// ---------------------------------------------------------------------------

/// fancy 上采样一条 chroma 行（`current` 当前 chroma 行，`adjacent` 相邻行，各 `cw` 字节）。
/// 输出 `width` 字节。算术 = `3*cur+adj` 的 t 行 + 偶数/奇数对的 `(m*t ± 邻居 + 8)>>4`。
/// `pub(crate)`：SIMD 版（`webp::simd::upsample_fancy_row`）以本标量为参考并逐字节对拍；
/// 向量核与标量一致性见 `simd.rs` 单测。
pub(crate) fn upsample_fancy(current: &[u8], adjacent: &[u8], width: usize) -> Vec<u8> {
    let cw = current.len();
    let mut t = vec![0u16; cw];
    for col in 0..cw {
        t[col] = 3 * u16::from(current[col]) + u16::from(adjacent[col]);
    }
    let mut out = vec![0u8; width];
    let first = t[0];
    out[0] = ((4 * first + 8) >> 4) as u8;
    if width > 1 {
        let next = if cw > 1 { t[1] } else { first };
        out[1] = ((3 * first + next + 8) >> 4) as u8;
    }
    for col in 1..cw.saturating_sub(1) {
        let c = t[col];
        let oc = col * 2;
        out[oc] = ((3 * c + t[col - 1] + 8) >> 4) as u8;
        out[oc + 1] = ((3 * c + t[col + 1] + 8) >> 4) as u8;
    }
    if cw > 1 {
        let last_col = cw - 1;
        let c = t[last_col];
        let oc = last_col * 2;
        out[oc] = ((3 * c + t[last_col - 1] + 8) >> 4) as u8;
        if oc + 1 < width {
            out[oc + 1] = ((4 * c + 8) >> 4) as u8;
        }
    }
    out
}

/// 定点 YUV→RGB（与 C# `WritePixel` 完全同序：`(s*k)>>8` 各项、bias 减后再 `>>6`，无重结合）。
/// `pub(crate)`：`webp::simd::yuv_row_to_rgba` 的标量回退/尾列直接调用本函数（单一算术源）。
#[inline]
pub(crate) fn yuv_to_rgb(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    let y_term = (i32::from(y) * 19077) >> 8;
    let red = (y_term + ((i32::from(v) * 26149) >> 8) - 14234) >> 6;
    let green = (y_term - ((i32::from(u) * 6419) >> 8) - ((i32::from(v) * 13320) >> 8) + 8708) >> 6;
    let blue = (y_term + ((i32::from(u) * 33050) >> 8) - 17685) >> 6;
    (clamp_byte(red), clamp_byte(green), clamp_byte(blue))
}

/// 行定稿直出 RGBA（复用工作区）。
pub(crate) struct EmitScratch {
    u_up: Vec<u8>,
    v_up: Vec<u8>,
    // 行对熔合（P2）：偶行第二输出行缓冲（`u_up`/`v_up` 存奇行），避免逐行分配。
    u_up2: Vec<u8>,
    v_up2: Vec<u8>,
    t_ws: Vec<u16>,
}

impl EmitScratch {
    pub(crate) fn new(width: usize) -> Self {
        let cw = (width + 1) >> 1;
        EmitScratch {
            u_up: vec![0u8; width],
            v_up: vec![0u8; width],
            u_up2: vec![0u8; width],
            v_up2: vec![0u8; width],
            // U/V × 奇/偶 四行 t 工作区共存（`upsample_fancy_into_uv_pair` 各用 cw+8；单行路径
            // `upsample_fancy_into_uv` 只需 2*(cw+8)，多倍缓冲仅容量富余，语义不变）。
            t_ws: vec![0u16; 4 * (cw + 8)],
        }
    }

    /// 池化复用（`subbuffers` 池 take 路径）：把先前宽度的缓冲 resize 到当前 `width`（容量
    /// 保留、grow 不 shrink）。各字段每次 emit 调用前均由 `upsample_fancy_into_uv(_pair)` 全写
    /// 先于读（输出行 u_up/v_up 及 t_ws 逐元素覆写，读只取本次已写段；见 simd::upsample），
    /// 且消费方按当前 `width` 切片（`yuv_row_to_rgba_alpha` debug_assert 长度相等）——故仅需
    /// 保证 `len` 与当前 width/cw 一致，无需重新清零（残值不可读）。
    pub(crate) fn resize_to(mut self, width: usize) -> Self {
        let cw = (width + 1) >> 1;
        // `resize` 收缩时容量保留；扩容时仅新段置零（少见，grow 语义）——旧段内容不参与任何读。
        self.u_up.resize(width, 0);
        self.v_up.resize(width, 0);
        self.u_up2.resize(width, 0);
        self.v_up2.resize(width, 0);
        self.t_ws.resize(4 * (cw + 8), 0);
        self
    }
}

/// 从**滤波后** padded 平面把已定稿行 `[first, end)` fancy 上采样 + YUV→RGBA 直出到 `rgba`。
/// 算术与 `yuv_to_rgba_fancy`（旧二趟）完全一致，只是读 padded 平面（跳过左侧 129 列）而非
/// tight 平面（tight 只是顺序拷贝，字节相同）。行 `end` 的选取保证 `[first,end)` 各行及其
/// 上采样相邻 chroma 行均已环路滤波定稿（C# `RgbaSink.Emit` 的 `end=mbY*16+8` 语义，注释于
/// `decode_from_header`）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_rows_into_rgba(
    y: &[u8],
    y_pitch: usize,
    u: &[u8],
    v: &[u8],
    uv_pitch: usize,
    width: usize,
    cw: usize,
    ch: usize,
    first: usize,
    end: usize,
    rgba: &mut [u8],
    ws: &mut EmitScratch,
    alpha: Option<&[u8]>,
) {
    stage_clock::yuv(|| {
    let mut row = first;
    while row < end {
        // 行对熔合（P2，libwebp `UpsampleLinePair` 同趟 top/bottom 语义）：奇行 2k+1 与紧随
        // 偶行 2k+2 共享同一对 adjacent chroma 行 (C_k, C_{k+1})，`upsample_fancy_into_uv_pair`
        // 一次装入两 chroma 行 + 构建 4 条 t 行 + 两输出行 fancy（与逐行两次
        // `upsample_fancy_into_uv` 逐字节恒等，`upsample_pair_matches_twice_into_uv` 对拍 0 差）。
        // 行 0 / 尾奇行（镜像邻）/ 任意无配对余行回落单行路径（原逻辑逐字节）。
        if row & 1 == 1 && row + 1 < end {
            let k = row >> 1;
            let u0 = k * uv_pitch + 1;
            let u1 = (k + 1) * uv_pitch + 1;
            super::simd::upsample_fancy_into_uv_pair(
                &u[u0..u0 + cw], &u[u1..u1 + cw],
                &v[u0..u0 + cw], &v[u1..u1 + cw],
                &mut ws.u_up, &mut ws.v_up,   // 奇行 2k+1 上采样 U/V
                &mut ws.u_up2, &mut ws.v_up2, // 偶行 2k+2 上采样 U/V
                &mut ws.t_ws,
            );
            let yb = row * y_pitch + 1;
            let yb2 = (row + 1) * y_pitch + 1;
            // lossy-alpha：alpha 行随各行直出并入发射内层（单遍 RGBA；`None` 时 α 恒 255）。
            super::simd::yuv_row_to_rgba_alpha(
                &y[yb..yb + width], &ws.u_up, &ws.v_up,
                alpha.map(|a| &a[row * width..(row + 1) * width]),
                &mut rgba[row * width * 4..(row + 1) * width * 4],
            );
            super::simd::yuv_row_to_rgba_alpha(
                &y[yb2..yb2 + width], &ws.u_up2, &ws.v_up2,
                alpha.map(|a| &a[(row + 1) * width..(row + 2) * width]),
                &mut rgba[(row + 1) * width * 4..(row + 2) * width * 4],
            );
            row += 2;
            continue;
        }
        let chroma_row = row >> 1;
        let adjacent = if row.is_multiple_of(2) {
            if chroma_row == 0 {
                0
            } else {
                chroma_row - 1
            }
        } else if chroma_row == ch - 1 {
            chroma_row
        } else {
            chroma_row + 1
        };
        let yba = row * y_pitch + 1;
        let u0 = chroma_row * uv_pitch + 1;
        let u1 = adjacent * uv_pitch + 1;
        super::simd::upsample_fancy_into_uv(
            &u[u0..u0 + cw], &u[u1..u1 + cw],
            &v[u0..u0 + cw], &v[u1..u1 + cw],
            &mut ws.u_up, &mut ws.v_up, &mut ws.t_ws,
        );
        // lossy-alpha：alpha 行随本行直出并入发射内层（单遍 RGBA；`None` 时 α 恒 255）。
        super::simd::yuv_row_to_rgba_alpha(
            &y[yba..yba + width], &ws.u_up, &ws.v_up,
            alpha.map(|a| &a[row * width..(row + 1) * width]),
            &mut rgba[row * width * 4..(row + 1) * width * 4],
        );
        row += 1;
    }
    });
}

/// 从**滤波后** padded 平面把已定稿行 `[first, end)` fancy 上采样 + YUV→RGB3 直出（3 字节/像素，
/// alpha 不写）。算术与 [`emit_rows_into_rgba`] 完全一致，仅末段走 `yuv_row_to_rgb3`（3 步长）；
/// 每像素前 3 字节恒等于 RGBA 路径 → opaque 有损 RGB3 直出与旧「RGBA4→4→3」同像素（硬闸）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_rows_into_rgb3(
    y: &[u8],
    y_pitch: usize,
    u: &[u8],
    v: &[u8],
    uv_pitch: usize,
    width: usize,
    cw: usize,
    ch: usize,
    first: usize,
    end: usize,
    rgb: &mut [u8],
    ws: &mut EmitScratch,
) {
    stage_clock::yuv(|| {
    let mut row = first;
    while row < end {
        // 行对熔合（P2）：同 `emit_rows_into_rgba` 的奇偶行对共享语义，仅末段走 `yuv_row_to_rgb3`
        // （3 步长，alpha 不写）；每像素前 3 字节恒等于 RGBA 路径（硬闸）。
        if row & 1 == 1 && row + 1 < end {
            let k = row >> 1;
            let u0 = k * uv_pitch + 1;
            let u1 = (k + 1) * uv_pitch + 1;
            super::simd::upsample_fancy_into_uv_pair(
                &u[u0..u0 + cw], &u[u1..u1 + cw],
                &v[u0..u0 + cw], &v[u1..u1 + cw],
                &mut ws.u_up, &mut ws.v_up,
                &mut ws.u_up2, &mut ws.v_up2,
                &mut ws.t_ws,
            );
            let yb = row * y_pitch + 1;
            let yb2 = (row + 1) * y_pitch + 1;
            super::simd::yuv_row_to_rgb3(
                &y[yb..yb + width], &ws.u_up, &ws.v_up,
                &mut rgb[row * width * 3..(row + 1) * width * 3],
            );
            super::simd::yuv_row_to_rgb3(
                &y[yb2..yb2 + width], &ws.u_up2, &ws.v_up2,
                &mut rgb[(row + 1) * width * 3..(row + 2) * width * 3],
            );
            row += 2;
            continue;
        }
        let chroma_row = row >> 1;
        let adjacent = if row.is_multiple_of(2) {
            if chroma_row == 0 {
                0
            } else {
                chroma_row - 1
            }
        } else if chroma_row == ch - 1 {
            chroma_row
        } else {
            chroma_row + 1
        };
        let yba = row * y_pitch + 1;
        let u0 = chroma_row * uv_pitch + 1;
        let u1 = adjacent * uv_pitch + 1;
        super::simd::upsample_fancy_into_uv(
            &u[u0..u0 + cw], &u[u1..u1 + cw],
            &v[u0..u0 + cw], &v[u1..u1 + cw],
            &mut ws.u_up, &mut ws.v_up, &mut ws.t_ws,
        );
        super::simd::yuv_row_to_rgb3(&y[yba..yba + width], &ws.u_up, &ws.v_up, &mut rgb[row * width * 3..(row + 1) * width * 3]);
        row += 1;
    }
    });
}

/// 串行路径行窗步进：滤波单个已重建宏块行 `mby`（读其 top 邻行 mby-1、bottom 邻行 mby+1，
/// 两者均须已重建），然后发射已定稿行 `[emitted, end)`（RGBA4 当 `channels=4`；RGB3 当
/// `channels=3`），返回新的 `emitted` 水位。
///
/// `end = mby==mbh-1 ? height : mby*16+8`（等价 C# `RgbaSink.Emit` 的 `end=mbY*16+8` 语义）：
/// 行 `[emitted, mby*16+8)` 只被第 mby 行的滤波读写定稿——下一行滤波只触及本行 13..15 及
/// chroma 5..7，故滤完即出；上采样相邻 chroma 行最远取 `mby*8+4`，同属本 MB 行、已在本次
/// 滤波定稿。本函数的滤波行序与旧 `filter_and_emit_serial` 完全一致：第 mby 行滤波恰排在
/// 上一行滤波之后（调用方按 mby=0..mbh-1 逐行调用，末行在整帧重建毕后调用），故发射的
/// 字节段序与旧路径恒等。
#[allow(clippy::too_many_arguments)]
pub(crate) fn filter_emit_serial_row(
    y: &mut [u8],
    y_pitch: usize,
    u: &mut [u8],
    v: &mut [u8],
    uv_pitch: usize,
    mbw: usize,
    mby: usize,
    mbh: usize,
    filter: &FilterSettings,
    row_filters: &[(u8, u8)],
    width: usize,
    height: usize,
    rgba: &mut [u8],
    ws: &mut EmitScratch,
    channels: usize,
    alpha: Option<&[u8]>,
    mut emitted: usize,
) -> usize {
    filter_macroblock_row(y, y_pitch, u, v, uv_pitch, mbw, mby, filter, row_filters);
    let end = if mby == mbh - 1 { height } else { mby * 16 + 8 };
    if end > emitted {
        let cw = (width + 1) >> 1;
        let ch = (height + 1) >> 1;
        if channels == 3 {
            emit_rows_into_rgb3(y, y_pitch, u, v, uv_pitch, width, cw, ch, emitted, end, rgba, ws);
        } else {
            emit_rows_into_rgba(y, y_pitch, u, v, uv_pitch, width, cw, ch, emitted, end, rgba, ws, alpha);
        }
        emitted = end;
    }
    emitted
}

/// YUV 4:2:0 → RGBA8（fancy 上采样默认，等价 image-webp Bilinear / dwebp fancy 列）。
/// `decode_vp8(_with_alpha)` 已把行定稿直出的 RGBA（lossy-alpha 已含真实 A 通道）缓存在
/// `yuv.rgba`，此处直接取走（`mem::take`，零重算、零拷贝）；若为空（手工构造的 `Vp8Yuv`）
/// 回退旧整帧二趟路径，回退路径 alpha 恒 255，输出逐字节一致。
pub(crate) fn yuv_to_rgba_fancy(yuv: &mut Vp8Yuv) -> Vec<u8> {
    if !yuv.rgba.is_empty() {
        return std::mem::take(&mut yuv.rgba);
    }
    let w = yuv.width as usize;
    let h = yuv.height as usize;
    let cw = (w + 1) >> 1;
    let ch = (h + 1) >> 1;
    let mut rgba = vec![0u8; w * h * 4];
    let mut u_up = vec![0u8; w];
    let mut v_up = vec![0u8; w];
    // 复用上采样工作区（U/V 两行 t 缓冲共存，`upsample_fancy_into_uv` 各用 cw+8），避免逐行分配。
    let mut t_ws = vec![0u16; 2 * (cw + 8)];
    for row in 0..h {
        let chroma_row = row >> 1;
        let adjacent = if row % 2 == 0 {
            if chroma_row == 0 {
                0
            } else {
                chroma_row - 1
            }
        } else if chroma_row == ch - 1 {
            chroma_row
        } else {
            chroma_row + 1
        };
        let u_row = &yuv.u[chroma_row * cw..(chroma_row + 1) * cw];
        let u_adj = &yuv.u[adjacent * cw..(adjacent + 1) * cw];
        let v_row = &yuv.v[chroma_row * cw..(chroma_row + 1) * cw];
        let v_adj = &yuv.v[adjacent * cw..(adjacent + 1) * cw];
        super::simd::upsample_fancy_into_uv(u_row, u_adj, v_row, v_adj, &mut u_up, &mut v_up, &mut t_ws);
        let y_row = &yuv.y[row * w..(row + 1) * w];
        let out = &mut rgba[row * w * 4..(row + 1) * w * 4];
        super::simd::yuv_row_to_rgba(y_row, &u_up, &v_up, out);
    }
    rgba
}

/// 【dormancy，K24】YUV 4:2:0 → RGB3（w*h*3，alpha 不写）：T1 前 opaque 静态有损直出入口。
/// [`decode_vp8_rgb3`] 已把行定稿直出的 RGB3 缓存在 `yuv.rgb`，此处直接取走（`mem::take`，
/// 零重算、零拷贝）；若为空（手工构造的 `Vp8Yuv`）回退旧整帧二趟路径（走 `yuv_row_to_rgb3`，
/// 逐字节与 RGBA 前 3 字节一致）。opaque 有损现走 `yuv_to_rgba_fancy`（RGBA4 直出），保留
/// 作硬闸/测试参考与手工构造回退（K24）。
#[allow(dead_code)]
pub(crate) fn yuv_to_rgb_fancy(yuv: &mut Vp8Yuv) -> Vec<u8> {
    if !yuv.rgb.is_empty() {
        return std::mem::take(&mut yuv.rgb);
    }
    let w = yuv.width as usize;
    let h = yuv.height as usize;
    let cw = (w + 1) >> 1;
    let ch = (h + 1) >> 1;
    let mut rgb = vec![0u8; w * h * 3];
    let mut u_up = vec![0u8; w];
    let mut v_up = vec![0u8; w];
    let mut t_ws = vec![0u16; 2 * (cw + 8)];
    for row in 0..h {
        let chroma_row = row >> 1;
        let adjacent = if row % 2 == 0 {
            if chroma_row == 0 {
                0
            } else {
                chroma_row - 1
            }
        } else if chroma_row == ch - 1 {
            chroma_row
        } else {
            chroma_row + 1
        };
        let u_row = &yuv.u[chroma_row * cw..(chroma_row + 1) * cw];
        let u_adj = &yuv.u[adjacent * cw..(adjacent + 1) * cw];
        let v_row = &yuv.v[chroma_row * cw..(chroma_row + 1) * cw];
        let v_adj = &yuv.v[adjacent * cw..(adjacent + 1) * cw];
        super::simd::upsample_fancy_into_uv(u_row, u_adj, v_row, v_adj, &mut u_up, &mut v_up, &mut t_ws);
        let y_row = &yuv.y[row * w..(row + 1) * w];
        let out = &mut rgb[row * w * 3..(row + 1) * w * 3];
        super::simd::yuv_row_to_rgb3(y_row, &u_up, &v_up, out);
    }
    rgb
}

