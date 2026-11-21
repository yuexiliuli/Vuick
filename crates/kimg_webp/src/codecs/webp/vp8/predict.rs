#![forbid(unsafe_code)]
// 预测内核参数多、blocks 按下标循环逐块分派（字节恒等硬约束）→ 放行。
#![allow(clippy::too_many_arguments, clippy::needless_range_loop)]
// 本文件零裸操作：`subblock_above` 的预测行裸读下沉 `simd::raw::unchecked_index`
// （`#[inline(always)]` 安全门面，内联后与原裸指针对代码生成逐指令一致）。

// ---------------------------------------------------------------------------
// V2：宏块重建（帧内预测 / IDCT / WHT / 解量化 / 写回 YUV）
// ---------------------------------------------------------------------------

/// 平面存储：每行开头 1 字节左边界（列 -1，=129），随后是编码列 0..cw。帧行 r 列 c 的
/// 存储下标 = `r * pitch + 1 + c`；列 -1 = `r * pitch + 0`。
/// 帧内预测的「上一行」「上一行右侧」均取自平面未滤波重建值（V3 环路滤波延后到整帧之后，
/// 与 C# 增量滤波结果完全一致——预测只读未滤波行，滤波只读已重建行）。
#[inline]
fn plane_at(r: usize, pitch: usize, c: usize) -> usize {
    r * pitch + 1 + c
}

#[inline]
fn plane_at_l(r: usize, pitch: usize) -> usize {
    r * pitch
}

/// RFC 6386 §14.4 一维变换（p0=p0,p2=p2,p1=p1,p3=p3 的按名实参顺序固定）。
#[inline]
fn transform_1d(p0: i32, p2: i32, p1: i32, p3: i32) -> (i32, i32, i32, i32) {
    let a0 = p0 + p2;
    let b0 = p0 - p2;
    let c0 = ((p1 * 35468) >> 16) - (p3 + ((p3 * 20091) >> 16));
    let d0 = p1 + ((p1 * 20091) >> 16) + ((p3 * 35468) >> 16);
    (a0 + d0, b0 + c0, b0 - c0, a0 - d0)
}

/// RFC 6386 §14.4 逆 DCT：纵过（结果回绕 i16）→ 横过（(x+4)>>3 舍入并回绕）。
fn inverse_dct_short(input: &[i16; 16], output: &mut [i16; 16], has_ac_coefficients: bool) {
    if !has_ac_coefficients {
        let dc = ((i32::from(input[0]) + 4) >> 3) as i16;
        output.fill(dc);
        return;
    }
    let mut horizontal = [0i16; 16];
    for column in 0..4 {
        // 实参顺序：p0=input[c], p2=input[8+c], p1=input[4+c], p3=input[12+c]（C# 同款）。
        let (a, b, c, d) = transform_1d(
            i32::from(input[column]),
            i32::from(input[8 + column]),
            i32::from(input[4 + column]),
            i32::from(input[12 + column]),
        );
        horizontal[column] = a as i16;
        horizontal[4 + column] = b as i16;
        horizontal[8 + column] = c as i16;
        horizontal[12 + column] = d as i16;
    }
    for row in 0..4 {
        let i = row * 4;
        let (a, b, c, d) = transform_1d(
            i32::from(horizontal[i]),
            i32::from(horizontal[i + 2]),
            i32::from(horizontal[i + 1]),
            i32::from(horizontal[i + 3]),
        );
        output[i] = ((a + 4) >> 3) as i16;
        output[i + 1] = ((b + 4) >> 3) as i16;
        output[i + 2] = ((c + 4) >> 3) as i16;
        output[i + 3] = ((d + 4) >> 3) as i16;
    }
}

/// 逆变换并把残差加到 4x4 像素（注意残差为 i16，逐像素 clamp [0,255]）。
/// `pub(crate)`：`webp::simd` 的 `inverse_dct_add_two` 标量回退复用（恒 has_ac=true，
/// 与既有短路语义逐字节一致）。
pub(crate) fn inverse_dct_add(
    coefficients: &[i16; 16],
    plane: &mut [u8],
    pitch: usize,
    row0: usize,
    col0: usize,
    has_ac: bool,
) {
    let mut residual = [0i16; 16];
    inverse_dct_short(coefficients, &mut residual, has_ac);
    for row in 0..4 {
        for col in 0..4 {
            let idx = plane_at(row0 + row, pitch, col0 + col);
            plane[idx] = (i32::from(plane[idx]) + i32::from(residual[row * 4 + col])).clamp(0, 255) as u8;
        }
    }
}

/// RFC 6386 §14.3 逆 Walsh-Hadamard：Y2 块输出写为 16 个 luma 块的 DC。
pub(crate) fn inverse_walsh_to_blocks(input: &[i16; 16], blocks: &mut [[i16; 16]; 25], has_ac: bool) {
    if !has_ac {
        let dc = ((i32::from(input[0]) + 3) >> 3) as i16;
        for i in 0..16 {
            blocks[i][0] = dc;
        }
        return;
    }
    let mut horizontal = [0i16; 16];
    for column in 0..4 {
        let a = i32::from(input[column]) + i32::from(input[12 + column]);
        let b = i32::from(input[4 + column]) + i32::from(input[8 + column]);
        let c = i32::from(input[4 + column]) - i32::from(input[8 + column]);
        let d = i32::from(input[column]) - i32::from(input[12 + column]);
        horizontal[column] = (a + b) as i16;
        horizontal[4 + column] = (c + d) as i16;
        horizontal[8 + column] = (a - b) as i16;
        horizontal[12 + column] = (d - c) as i16;
    }
    for row in 0..4 {
        let i = row * 4;
        let a = i32::from(horizontal[i]) + i32::from(horizontal[i + 3]);
        let b = i32::from(horizontal[i + 1]) + i32::from(horizontal[i + 2]);
        let c = i32::from(horizontal[i + 1]) - i32::from(horizontal[i + 2]);
        let d = i32::from(horizontal[i]) - i32::from(horizontal[i + 3]);
        blocks[i][0] = ((a + b + 3) >> 3) as i16;
        blocks[i + 1][0] = ((c + d + 3) >> 3) as i16;
        blocks[i + 2][0] = ((a - b + 3) >> 3) as i16;
        blocks[i + 3][0] = ((d - c + 3) >> 3) as i16;
    }
}

// ---------------------------------------------------------------------------
// 帧内预测（RFC 6386 §12.2 / §12.3，标量）
// ---------------------------------------------------------------------------

#[inline]
pub(crate) fn clamp_byte(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

#[inline]
fn avg2(a: u8, b: u8) -> u8 {
    ((i32::from(a) + i32::from(b) + 1) >> 1) as u8
}

#[inline]
fn avg3(a: u8, b: u8, c: u8) -> u8 {
    ((i32::from(a) + 2 * i32::from(b) + i32::from(c) + 2) >> 2) as u8
}

/// 16x16 luma / 8x8 chroma 预测（mode 0 DC / 1 V / 2 H / 3 TM）。
/// `above[0]` = 左上角，`above[1 + i]` = 上一行列 i（共 size 个）。左列读平面（列 -1 为边界）。
/// `pub(crate)`：`webp::simd` 的向量版以此为标量回退/对拍基准（D1）。
pub(crate) fn predict_large(
    plane: &mut [u8],
    pitch: usize,
    row0: usize,
    col0: usize,
    size: usize,
    mode: u8,
    has_top: bool,
    has_left: bool,
    above: &[u8],
) {
    match mode {
        1 => {
            // V：整块 = 上一行。
            for r in 0..size {
                for c in 0..size {
                    let idx = plane_at(row0 + r, pitch, col0 + c);
                    plane[idx] = above[1 + c];
                }
            }
        }
        2 => {
            // H：整行 = 左列。
            for r in 0..size {
                let l = plane[plane_at_l(row0 + r, pitch) + col0];
                for c in 0..size {
                    let idx = plane_at(row0 + r, pitch, col0 + c);
                    plane[idx] = l;
                }
            }
        }
        3 => {
            // TM：clamp(左 + 上 - 左上)。
            let top_left = i32::from(above[0]);
            for r in 0..size {
                let left = i32::from(plane[plane_at_l(row0 + r, pitch) + col0]);
                for c in 0..size {
                    let idx = plane_at(row0 + r, pitch, col0 + c);
                    plane[idx] = clamp_byte(left + i32::from(above[1 + c]) - top_left);
                }
            }
        }
        _ => {
            // DC：仅累计界内边缘。
            let mut sum = 0i32;
            for i in 0..size {
                if has_top {
                    sum += i32::from(above[1 + i]);
                }
                if has_left {
                    sum += i32::from(plane[plane_at_l(row0 + i, pitch) + col0]);
                }
            }
            let value = if has_top && has_left {
                (sum + size as i32) / (2 * size as i32)
            } else if has_top || has_left {
                (sum + (size as i32) / 2) / size as i32
            } else {
                128
            } as u8;
            for r in 0..size {
                for c in 0..size {
                    let idx = plane_at(row0 + r, pitch, col0 + c);
                    plane[idx] = value;
                }
            }
        }
    }
}

/// 构造 4x4 块的 above 行（`above[0]`=左上，`above[1..9]`=上 8 像素）：
/// 顶行块取宏块 above 缓冲；下方块取已重建上一行；右列块的 above-right 取自宏块 above 缓冲
/// 第 16..19（RFC 6386 §12.3 的 16×8 边界不可重排语义）。
/// I4 热路径逐块调用（16×/宏块 × ~80% 宏块）：读下沉 `simd::raw::unchecked_index`
/// 免边界检查（内联后与原裸指针同指令）。
/// 越界不变量：`block_y>0` 时读行 `row_above=row0-1`，列范围
/// `col0-1 ..= col0+7`——`col0=dx+bx*4`，bx≤3 → 最大列 `dx+15 < cw`，最小 `dx-1` 为左边界
/// 129 列（`pitch` 行首格）；故最大平面下标 `(ch-1)*pitch+1+cw-1 < ch*pitch`（=y 平面长度）
/// 恒不越界（`plane_at`/`plane_at_l` 行布局见 645 注释；`debug_assert` 守形状边界）。
pub(crate) fn subblock_above(
    plane: &[u8],
    pitch: usize,
    row0: usize,
    col0: usize,
    block_x: usize,
    block_y: usize,
    above_y: &[u8; 21],
) -> [u8; 9] {
    if block_y == 0 {
        let base = 4 * block_x;
        let mut a = [0u8; 9];
        a[0] = above_y[base];
        a[1..9].copy_from_slice(&above_y[base + 1..base + 9]);
        return a;
    }
    let row_above = row0 - 1;
    let base = row_above * pitch;
    let mut a = [0u8; 9];
    debug_assert!(base + col0 + 8 < plane.len());
    if block_x < 3 {
        a[0] = super::simd::raw::unchecked_index(plane, base + col0);
        for k in 0..8 {
            a[1 + k] = super::simd::raw::unchecked_index(plane, base + col0 + 1 + k);
        }
    } else {
        // block_x == 3：右侧 4 像素取宏块 aboveY[16..19]。
        a[0] = super::simd::raw::unchecked_index(plane, base + col0);
        for k in 0..4 {
            a[1 + k] = super::simd::raw::unchecked_index(plane, base + col0 + 1 + k);
        }
        a[5..9].copy_from_slice(&above_y[17..21]);
    }
    a
}

/// `subblock_above` 的 by>0 分支行缓存变体：`row` 为本子块行上方 17 字节平面行（raw 偏移
/// `(row0-1)*pitch + dx .. dx+16`，即帧列 dx..dx+16），按 `block_x` 切出与 `subblock_above`
/// by>0 分支逐字节相同的 `[u8;9]`——同一平面同源读取，只是把 4 个子块各自重叠读升级为一次
/// 行缓存复用；右列块（block_x==3）的 above-right 仍取宏块 `above_y[17..21]`（RFC §12.3
/// 16×8 边界不可重排语义，与 `subblock_above` 完全一致）。
pub(crate) fn subblock_above_row(row: &[u8; 17], block_x: usize, above_y: &[u8; 21]) -> [u8; 9] {
    let base = 4 * block_x;
    let mut a = [0u8; 9];
    a[0] = row[base];
    a[1..5].copy_from_slice(&row[base + 1..base + 5]);
    if block_x < 3 {
        a[5..9].copy_from_slice(&row[base + 5..base + 9]);
    } else {
        a[5..9].copy_from_slice(&above_y[17..21]);
    }
    a
}

/// 4x4 luma 预测（10 模式）。`above[0]`=左上，`above[1..9]`=上 8 像素；左列读平面。
/// `pub(crate)`：`webp::simd` 的向量版以此为标量回退/对拍基准（D1）。
#[allow(clippy::too_many_lines)]
pub(crate) fn predict_subblock(
    plane: &mut [u8],
    pitch: usize,
    row0: usize,
    col0: usize,
    mode: u8,
    above: &[u8; 9],
) {
    let mut left = [0u8; 4];
    for i in 0..4 {
        left[i] = plane[plane_at_l(row0 + i, pitch) + col0];
    }
    let p = above[0];
    // edge = [L3 L3'? 不：edge[0..9]]：edge = [L3 L2 L1 L0 P A0 A1 A2 A3]。
    let edge = [
        left[3],
        left[2],
        left[1],
        left[0],
        p,
        above[1],
        above[2],
        above[3],
        above[4],
    ];
    let at = |r: usize, c: usize| plane_at(row0 + r, pitch, col0 + c);
    match mode {
        0 => {
            let sum = 4u32 + (0..4).map(|i| u32::from(above[1 + i])).sum::<u32>()
                + (0..4).map(|i| u32::from(left[i])).sum::<u32>();
            let v = (sum >> 3) as u8;
            for r in 0..4 {
                for c in 0..4 {
                    plane[at(r, c)] = v;
                }
            }
        }
        1 => {
            // TM。
            let p = i32::from(above[0]);
            for r in 0..4 {
                let lv = i32::from(left[r]);
                for c in 0..4 {
                    plane[at(r, c)] = clamp_byte(lv + i32::from(above[1 + c]) - p);
                }
            }
        }
        2 => {
            // VE。
            for r in 0..4 {
                for c in 0..4 {
                    let a = if c == 0 { p } else { above[c] };
                    plane[at(r, c)] = avg3(a, above[1 + c], above[2 + c]);
                }
            }
        }
        3 => {
            // HE。
            for r in 0..4 {
                let prev = if r == 0 { p } else { left[r - 1] };
                let next = left[(r + 1).min(3)];
                let v = avg3(prev, left[r], next);
                for c in 0..4 {
                    plane[at(r, c)] = v;
                }
            }
        }
        4 => {
            // LD。
            for r in 0..4 {
                for c in 0..4 {
                    let idx = r + c + 1;
                    plane[at(r, c)] = if idx < 7 {
                        avg3(above[idx], above[1 + idx], above[2 + idx])
                    } else {
                        avg3(above[7], above[8], above[8])
                    };
                }
            }
        }
        5 => {
            // RD。
            for r in 0..4 {
                for c in 0..4 {
                    let base = 4 + c as i32 - r as i32;
                    let e = |off: i32| edge[(base + off) as usize];
                    plane[at(r, c)] = avg3(e(-1), e(0), e(1));
                }
            }
        }
        6 => {
            // VR。
            for r in 0..4 {
                for c in 0..4 {
                    let v = match r {
                        0 => avg2(edge[4 + c], edge[5 + c]),
                        1 => avg3(edge[3 + c], edge[4 + c], edge[5 + c]),
                        2 => {
                            if c == 0 {
                                avg3(edge[2], edge[3], edge[4])
                            } else {
                                avg2(edge[3 + c], edge[4 + c])
                            }
                        }
                        _ => {
                            let (a, b, d) = if c == 0 {
                                (edge[1], edge[2], edge[3])
                            } else {
                                (edge[2 + c], edge[3 + c], edge[4 + c])
                            };
                            avg3(a, b, d)
                        }
                    };
                    plane[at(r, c)] = v;
                }
            }
        }
        7 => {
            // VL。
            for r in 0..4 {
                for c in 0..4 {
                    let v = match r {
                        0 => avg2(above[1 + c], above[2 + c]),
                        1 => avg3(above[1 + c], above[2 + c], above[3 + c]),
                        2 => {
                            if c < 3 {
                                avg2(above[2 + c], above[3 + c])
                            } else {
                                avg3(above[5], above[6], above[7])
                            }
                        }
                        _ => {
                            if c == 3 {
                                avg3(above[6], above[7], above[8])
                            } else {
                                avg3(above[2 + c], above[3 + c], above[4 + c])
                            }
                        }
                    };
                    plane[at(r, c)] = v;
                }
            }
        }
        8 => {
            // HD。
            for r in 0..4 {
                for c in 0..4 {
                    let index = match r {
                        0 => match c {
                            0 => 3usize,
                            1 => 4,
                            2 => 5,
                            _ => 6,
                        },
                        1 => match c {
                            0 => 2usize,
                            1 => 3,
                            2 => 3,
                            _ => 4,
                        },
                        2 => match c {
                            0 => 1usize,
                            1 => 2,
                            2 => 2,
                            _ => 3,
                        },
                        _ => match c {
                            0 => 0usize,
                            1 => 1,
                            2 => 1,
                            _ => 2,
                        },
                    };
                    let half = (r == 0 && c == 0)
                        || (r == 1 && (c == 0 || c == 2))
                        || (r == 2 && (c == 0 || c == 2))
                        || (r == 3 && (c == 0 || c == 2));
                    plane[at(r, c)] = if half {
                        avg2(edge[index], edge[index + 1])
                    } else {
                        avg3(edge[index - 1], edge[index], edge[index + 1])
                    };
                }
            }
        }
        _ => {
            // HU。
            for r in 0..4 {
                for c in 0..4 {
                    let v = if r == 3 || (r == 2 && c >= 2) {
                        left[3]
                    } else {
                        match r {
                            0 => match c {
                                0 => avg2(left[0], left[1]),
                                1 => avg3(left[0], left[1], left[2]),
                                2 => avg2(left[1], left[2]),
                                _ => avg3(left[1], left[2], left[3]),
                            },
                            1 => match c {
                                0 => avg2(left[1], left[2]),
                                1 => avg3(left[1], left[2], left[3]),
                                2 => avg2(left[2], left[3]),
                                _ => avg3(left[2], left[3], left[3]),
                            },
                            _ => {
                                if c == 0 {
                                    avg2(left[2], left[3])
                                } else {
                                    avg3(left[2], left[3], left[3])
                                }
                            }
                        }
                    };
                    plane[at(r, c)] = v;
                }
            }
        }
    }
}

