//! 页面显示层**纸色重着色**（paper-only shading）：只改渲染位图像素中的「纸面」色，
//! 内容（文字 / 图片 / 图形）像素保持原色，**绝不写回 PDF 文件、不改 pdfium 解析**。
//!
//! 语义：把近白（纸面）像素按亮度渐变着成纸色，亮度低于 ──[`PAPER_L_LO`] 的像素
//! （文字墨迹、深/中调图形、图片内容）一律原样不动。文本层（选区/搜索）由 view 独立
//! 叠加层绘制、不依赖位图像素，因此本重着色不破坏文本层机制。
//!
//! 执行位置：**worker 渲染线程内**对 bgra 缓冲就地改（每瓦片数 ms 级一次性开销），
//! 主线程 / 文档 / 文件零接触；参数为 `None` 时完全直通（零像素循环、零成本）。

/// 纸重着色参数：`bg` = 纸色（仅近白纸面像素映射至此）。RGB 通道（0..=255）；
/// alpha 一贯保持 255。内容像素（低于亮度阈值）不变，故不再需要「墨色」参数。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecolorParams {
    /// 纸色（R/G/B，0..=255）。
    pub bg: [u8; 3],
}

/// 纸面亮度下限（感知亮度 / 255）：`L ≥` 该值视为「纸面」，按亮度渐变成纸色；
/// `L <` 该值视为「内容」，原样不动。0.90 为**内容保护优先**阈值：纯白(1.0)→纸色、
/// 纸边抗锯齿浅灰→微染、大多数内容（文字 / 深/中调图形 / 图片）→ 不变。
pub const PAPER_L_LO: f32 = 0.90;

/// 纸面亮度权重 a（纸色占的份量）：在 [`PAPER_L_LO`]..＝1.0 内线性从 0→1，
/// 低于下限恒 0（内容不动）。纯函数。
#[inline]
fn paper_weight(lum: f32) -> f32 {
    ((lum - PAPER_L_LO) / (1.0 - PAPER_L_LO)).clamp(0.0, 1.0)
}

/// 纸色重着色（纯函数，就地改 `bgra` 像素）：每像素感知亮度
/// `L = (0.299R + 0.587G + 0.114B) / 255 ∈ [0,1]`（白 = 1），各通道
/// `out = orig + (bg - orig) × a`，其中 `a = paper_weight(L)`——近白纸面趋向纸色、
/// 亮度低于 [`PAPER_L_LO`] 的内容（文字/图形/图片）完全不变。A 恒 255。
///
/// 输入为 BGRA 紧凑缓冲（`bgra[i]=B, [i+1]=G, [i+2]=R, [i+3]=A`）；`len` 按 4 对齐取整，
/// 尾部不足 4 字节忽略。空 / 无参数直通由调用方保证（`None` 不调用本函数）。
///
/// 已知局限：像素级无法区分「纸面」与「内容里的近白高光」（白底截图/高光区），
/// 这类近白像素同样会被当成纸面微染——深/中调内容则绝不触碰。
pub fn apply_recolor(bgra: &mut [u8], params: RecolorParams) {
    let bg_r = f32::from(params.bg[0]);
    let bg_g = f32::from(params.bg[1]);
    let bg_b = f32::from(params.bg[2]);
    let n = bgra.len() / 4;
    let mut i = 0usize;
    for _ in 0..n {
        let lum =
            (0.299 * f32::from(bgra[i + 2]) + 0.587 * f32::from(bgra[i + 1]) + 0.114 * f32::from(bgra[i]))
                / 255.0;
        let a = paper_weight(lum);
        if a > 0.0 {
            bgra[i] = (f32::from(bgra[i]) + (bg_b - f32::from(bgra[i])) * a).round() as u8;
            bgra[i + 1] = (f32::from(bgra[i + 1]) + (bg_g - f32::from(bgra[i + 1])) * a).round() as u8;
            bgra[i + 2] = (f32::from(bgra[i + 2]) + (bg_r - f32::from(bgra[i + 2])) * a).round() as u8;
        }
        bgra[i + 3] = 255;
        i += 4;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 纯函数断言：纯白 → 纸色；内容（黑 / 中灰）原样不动；近白按亮度渐染；A 恒 255。
    /// 参数 bp：bg=[10,20,30]（RGB）。
    #[test]
    fn apply_recolor_paper_only_and_alpha() {
        let params = RecolorParams { bg: [10, 20, 30] };
        // 白 / 黑 / 50% 灰 / 近白 250 / 内容灰 100（A=0 强制 255）。
        let mut buf: Vec<u8> = vec![
            255, 255, 255, 255, // 白：B,G,R,A
            0, 0, 0, 255, // 黑
            128, 128, 128, 255, // 50% 灰（内容，应不动）
            250, 250, 250, 255, // 近白 250（纸面，应渐染）
            100, 100, 100, 0, // 内容灰，A=0
        ];
        apply_recolor(&mut buf, params);

        // 白 L=1.0 → a=1 → 全纸色（RGB[10,20,30] → 缓冲 B=30,G=20,R=10）。
        assert_eq!(&buf[0..4], &[30, 20, 10, 255], "纯白纸面应映射为纸色");
        // 黑 L=0 → a=0 → 原样（内容不变）。
        assert_eq!(&buf[4..8], &[0, 0, 0, 255], "纯黑内容应原样不动");
        // 50% 灰 L≈0.50 < 0.90 → a=0 → 原样（内容不变）。
        assert_eq!(&buf[8..12], &[128, 128, 128, 255], "中灰内容应原样不动");
        // 近白 250：L=250/255≈0.9804，a=(0.9804-0.90)/0.10≈0.8039
        //   R:250+(10-250)*0.8039≈57.1→57 G:250+(20-250)*0.8039≈65.1→65
        //   B:250+(30-250)*0.8039≈73.1→73 → 缓冲 [73,65,57,255]。
        assert_eq!(&buf[12..16], &[73, 65, 57, 255], "近白纸面按亮度渐染为纸色");
        // 内容灰 100（L≈0.39，a=0）→ 原样；输入 A=0 被强制 255。
        assert_eq!(&buf[16..20], &[100, 100, 100, 255], "内容灰原样不动且 A 恒 255");
    }

    /// 亮度上界另一头：L 恰在阈值之下（0.89 < 0.90）→ a=0 完全不动。
    #[test]
    fn apply_recolor_below_threshold_is_unchanged() {
        let params = RecolorParams { bg: [0x20, 0x20, 0x20] };
        // 0.89*255 ≈ 226.95 → 227；L=227/255≈0.8902 略高于 0.90？——取 225（L≈0.882）
        // 与 240（L≈0.941，a≈0.41，应部分染向纸色）。
        let mut buf = vec![225, 225, 225, 255, 240, 240, 240, 255];
        apply_recolor(&mut buf, params);
        // 225：L≈0.8824 < 0.90 → a=0 → 原样。
        assert_eq!(&buf[0..4], &[225, 225, 225, 255], "阈值下（0.882）内容不动");
        // 240：L≈0.9412，a=(0.9412-0.90)/0.10≈0.4118
        //   R:240+(0x20-240)*0.4118 = 240-85.65=154.35→154（B/G 同）。
        assert_eq!(&buf[4..8], &[154, 154, 154, 255], "阈值上近白渐染到纸色方向");
    }

    /// 空缓冲（len==0）不 panic；len 非 4 对齐时尾部不足像素忽略、不越界。
    #[test]
    fn apply_recolor_empty_and_unaligned() {
        let params = RecolorParams { bg: [10, 20, 30] };
        let mut empty: Vec<u8> = Vec::new();
        apply_recolor(&mut empty, params);
        assert!(empty.is_empty());
        // 5 字节（= 1 整像素 + 1 冗余字节）：只处理前 4 字节，尾字节原样。
        let mut buf = vec![255, 255, 255, 255, 7];
        apply_recolor(&mut buf, params);
        assert_eq!(buf[0..4], [30, 20, 10, 255], "整像素按纸色处理");
        assert_eq!(buf[4], 7, "尾部分字节不应被处理");
    }
}
