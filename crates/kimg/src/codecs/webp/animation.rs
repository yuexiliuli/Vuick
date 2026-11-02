//! WebP 动画解码（P6）：ANMF 容器帧 → 帧级并行静态解码 → 顺序画布合成。
//!
//! 结构（对照 C# `WebpAnimationDecoder` / `WebpDecoder.DecodeFrameRgba`）：
//! 1. `decode_animation`：`container::parse` 已产出 ANMF 帧表（x/y/w/h/duration/blend/dispose
//!    + 帧内 VP8/VP8L/ALPH 载荷绝对范围）→ 各帧**独立**解码成本帧 RGBA（帧间无依赖，可并行；
//!    静态内核 `vp8::decode_vp8`/`vp8l::decode`/`decode_and_unfilter_alpha` 保证与静态族字节一致）。
//! 2. 合成阶段串行：`CanvasCompositor` 维护全画布 RGBA 工作缓冲，按显示序逐帧
//!    执行 dispose（清上一帧矩形）/ blend / 覆盖，**合成语义逐字节对齐 image-webp**
//!    （`extended::composite_frame` + `alpha_blending::do_alpha_blending` 的 libwebp 近似非预乘
//!    混合公式；不是 SimdCodecs 的精确先乘公式——SimdCodecs 与 image-webp 在部分 fixture 上
//!    合成语义不同，以 image-webp 为准，即本仓现 gpui 渲染口径）。
//! 3. 画布背景：`set_background_color(0,0,0,0)`（gpui 强制）= 透明画布。image-webp 语义记入
//!    `background_color: Option<[u8;4]>`——**当未调用 set_background_color（=None）时 dispose 不
//!    清矩形、画布透明黑**；调用后被置 Some([0,0,0,0])，dispose 清矩形为透明黑。两者都必须复刻
//!    （None 态是 image-webp 的默认行为，oracle 测试即按 None 对拍）。
//!
//! 帧模型：`Frame`（delay=毫秒）+ `loop_count`（ANIM loop）。帧尺寸 0,0（全画布）由容器
//! 解析产出 x/y=0、w/h=画布全尺寸，blend 默认 ALPHA、dispose 默认 NONE（RFC 9649）。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use super::container;
use super::vp8;
use super::vp8l;
use crate::animation::{Delay, Frame};
use crate::buffer::RgbaImage;
use crate::error::ImageResult;

/// 动画原始帧：帧内（非画布）解码 RGBA + ANMF 元数据。合成前所有帧解码完成、互不依赖。
#[derive(Debug)]
pub(crate) struct AnimatedRawFrame {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub duration_ms: u32,
    /// ANMF BLEND 位：true=alpha 混合；false=直接覆盖。
    pub blend: bool,
    /// ANMF DISPOSE 位：true=下一帧绘制前清本帧矩形。
    pub dispose_to_background: bool,
    /// 帧内 RGBA8（`width*height*4`）。无 alpha 帧（VP8 无 ALPH）Alpha 恒 255，
    /// 但 `has_alpha=false`——合成按 image-webp 的 3 通道帧缓冲语义处理。
    pub rgba: Vec<u8>,
    /// VP8L（恒）/ALPH 帧 = true；VP8 无 ALPH = false（与 image-webp `frame_has_alpha` 同口径）。
    pub has_alpha: bool,
}

/// 自研动画解码产物：帧原始数据 + 画布/循环元数据 + 背景色（可经 `set_background_color` 改写）。
#[derive(Debug)]
pub(crate) struct OursAnimated {
    pub width: u32,
    pub height: u32,
    /// image-webp `has_alpha()` = VP8X alpha flag（决定输出 RGBA8 还是 RGB8→A=255）。
    pub has_alpha_flag: bool,
    /// ANIM loop count（0=无限）。与 image-webp `LoopCount` 同口径（0→Forever）。
    pub loop_count: u32,
    /// 背景色（None = 未设置，等同 image-webp 默认：画布透明黑、dispose 不清矩形）。
    pub background_color: Option<[u8; 4]>,
    pub exif: Option<Vec<u8>>,
    pub icc: Option<Vec<u8>>,
    pub xmp: Option<Vec<u8>>,
    pub frames: Vec<AnimatedRawFrame>,
}

/// 解码单个 ANMF 帧（复用已验证静态内核，保证与静态族逐字节一致）。
fn decode_frame(data: &[u8], f: &container::WebpContainerFrame) -> Option<AnimatedRawFrame> {
    let bitstream = &data[f.bitstream.clone()];
    let (rgba, has_alpha) = match f.format {
        container::WebpFormat::Lossy => {
            let mut yuv = vp8::decode_vp8(bitstream, f.width, f.height).ok()?;
            let rgb = vp8::yuv_to_rgba_fancy(&mut yuv);
            match &f.alpha {
                // VP8 + ALPH：反滤波 alpha 平面合并进 alpha 通道。
                Some(a) => {
                    let plane =
                        super::decode_and_unfilter_alpha(&data[a.clone()], f.width, f.height)?;
                    let mut rgba = rgb;
                    for (i, &v) in plane.iter().enumerate() {
                        rgba[i * 4 + 3] = v;
                    }
                    (rgba, true)
                }
                None => (rgb, false),
            }
        }
        container::WebpFormat::Lossless => {
            let img = vp8l::decode(bitstream).ok()?;
            // 与 image-webp 同口径：VP8L 帧恒视为带 alpha（`frame_has_alpha=true`）。
            // 动画帧缓冲为自有 `Vec<u8>`：此处一次性拷贝（动画非静态无损热路径）。
            (img.rgba.as_bytes().to_vec(), true)
        }
        container::WebpFormat::Animated => return None, // ANMF 子块内不会出现该格式
    };
    Some(AnimatedRawFrame {
        x: f.x,
        y: f.y,
        width: f.width,
        height: f.height,
        duration_ms: f.duration_ms,
        blend: f.blend,
        dispose_to_background: f.dispose_to_background,
        rgba,
        has_alpha,
    })
}

/// 帧级并行门控：`KIMG_WEBP_THREADS` 语义沿用 vp8 滤波门控（`0|1|off|serial`=强制串行、
/// `2|on|two`=强制并行、其余/缺省=auto）。auto 下仅当聚合像素足够大（避免小动画的线程
/// 开销>收益）才开并行；帧数 <2 恒串行。
fn parallel_enabled(frames: &[container::WebpContainerFrame]) -> bool {
    if frames.len() < 2 {
        return false;
    }
    let mode = std::env::var("KIMG_WEBP_THREADS").unwrap_or_default();
    let mode = mode.trim();
    match mode {
        "0" | "1" | "off" | "serial" => return false,
        "2" | "on" | "two" => return true,
        _ => {}
    }
    let total_px = frames
        .iter()
        .map(|f| u64::from(f.width) * u64::from(f.height))
        .sum::<u64>();
    // ≈0.25MP 聚合像素下限（小动画串行更快；宽动画（多帧大图）开并行显著提速）。
    total_px >= (1 << 18)
}

/// 全部帧独立解码（先解码并行 → 后合成串行管道）。任一帧失败返回 None（调用方转显式错误，
/// 自包含后无 image-webp 兜底）。
fn decode_frames_parallel(
    data: &[u8],
    frames: &[container::WebpContainerFrame],
) -> Option<Vec<AnimatedRawFrame>> {
    let n = frames.len();
    let results: Vec<Option<AnimatedRawFrame>> = if parallel_enabled(frames) {
        let workers = std::thread::available_parallelism()
            .map(|x| x.get())
            .unwrap_or(2)
            .min(n);
        // 引用（Copy）move 进作用域线程：只移动「比作用域长寿」的引用本身，绕开
        // 「闭包借局部值可能比函数活更久」的 E0373。
        let next = &AtomicUsize::new(0);
        let merged = &Mutex::new(Vec::<(usize, Option<AnimatedRawFrame>)>::new());
        let data = data;
        let frames = frames;
        std::thread::scope(|s| {
            let mut handles = Vec::new();
            for _ in 0..workers {
                handles.push(s.spawn(move || {
                    let mut local = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= n {
                            break;
                        }
                        local.push((i, decode_frame(data, &frames[i])));
                    }
                    local
                }));
            }
            for h in handles {
                merged.lock().unwrap().extend(h.join().unwrap());
            }
        });
        let mut merged = std::mem::take(&mut *merged.lock().unwrap());
        merged.sort_by_key(|(i, _)| *i);
        merged.into_iter().map(|(_, r)| r).collect()
    } else {
        frames.iter().map(|f| decode_frame(data, f)).collect()
    };
    if results.iter().any(|r| r.is_none()) {
        return None;
    }
    Some(results.into_iter().map(|r| r.unwrap()).collect())
}

/// #2 内存护栏：自研动画构建期全量解码全部帧（`OursAnimated.frames` 各持一帧 RGBA，
/// n×画布峰值，K76），对真尺寸大动画该峰值可能与画布×帧数同量级。按保守上界
/// `帧数×画布宽×高×4 + 画布宽×高×4`（每帧最多全画布）估算；超 `KIMG_WEBP_ANIM_CAP_MB`
/// （默认 256MB）→ `decode_animation` 返回 None，`WebPDecoder::new` 转为**显式错误**
/// （自包含后无 image-webp 流式兜底；需求方可调大该 env 换取完整自研解码）。小 fixture
/// 远低于阈值，行为不变仍自研接管。
fn anim_mem_estimate_exceeds_cap(c: &super::container::WebpContainer) -> bool {
    let canvas_bytes = u64::from(c.width) * u64::from(c.height) * 4;
    let estimate = canvas_bytes.saturating_mul(c.frames.len() as u64 + 1);
    let cap_mb = std::env::var("KIMG_WEBP_ANIM_CAP_MB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(256);
    estimate > cap_mb.saturating_mul(1024 * 1024)
}

/// 自研动画解码入口：家族=Animated 时由 `WebPDecoder::new` 调用。
/// 返回 `Some(ours)` 表示接管成功；`None` 表示不可接管（超内存护栏 / 解码失败；调用方转为
/// 显式错误，自包含后无 image-webp 兜底）。
/// 入口先走 #2 内存护栏：预估内存超阈值 → 直接 `Ok(None)` 放弃自研接管（不付解码成本）。
pub(crate) fn decode_animation(data: &[u8]) -> ImageResult<Option<OursAnimated>> {
    let c = match container::parse(data) {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };
    if c.family != container::Family::Animated {
        return Ok(None);
    }
    // #2 内存护栏：预估超阈值 → `Ok(None)`，`WebPDecoder::new` 转为显式错误（带 env 提示）。
    if anim_mem_estimate_exceeds_cap(&c) {
        return Ok(None);
    }
    let frames = match decode_frames_parallel(data, &c.frames) {
        Some(f) => f,
        None => return Ok(None),
    };
    Ok(Some(OursAnimated {
        width: c.width,
        height: c.height,
        has_alpha_flag: c.alpha_flag,
        loop_count: c.loop_count,
        background_color: None,
        exif: c.exif.map(|e| e.to_vec()),
        icc: c.icc_profile.map(|e| e.to_vec()),
        xmp: c.xmp.map(|e| e.to_vec()),
        frames,
    }))
}

/// `div_by_255`：除以 255 且四舍五入到最近（image-webp `alpha_blending.rs` 移植，
/// libwebp 近似；`//` 向下取整不可用，字节级对齐必须逐位复刻）。
#[inline]
fn div_by_255(v: u32) -> u32 {
    (((v + 0x80) >> 8) + v + 0x80) >> 8
}

/// 单通道非预乘混合（image-webp `blend_channel_nonpremult`）。
#[inline]
fn blend_channel_nonpremult(src: u32, src_a: u8, dst: u32, dst_a: u8, scale: u32, shift: u32) -> u8 {
    let src_channel = ((src >> shift) & 0xff) as u8;
    let dst_channel = ((dst >> shift) & 0xff) as u8;
    let blend_unscaled =
        (u32::from(src_channel) * u32::from(src_a)) + (u32::from(dst_channel) * u32::from(dst_a));
    debug_assert!(u64::from(blend_unscaled) < (1u64 << 32) / u64::from(scale));
    ((blend_unscaled * scale) >> 24) as u8
}

/// 逐像素非预乘 alpha 混合（image-webp `blend_pixel_nonpremult` 逐字节复刻）。
#[inline]
fn blend_pixel_nonpremult(src: u32, dst: u32) -> u32 {
    let src_a = ((src >> 24) & 0xff) as u8;
    if src_a == 0 {
        dst
    } else {
        let dst_a = ((dst >> 24) & 0xff) as u8;
        let dst_factor_a = div_by_255(u32::from(dst_a) * (255 - u32::from(src_a)));
        let blend_a = u32::from(src_a) + dst_factor_a;
        let scale = (1u32 << 24) / blend_a;
        let blend_r = blend_channel_nonpremult(src, src_a, dst, dst_factor_a as u8, scale, 0);
        let blend_g = blend_channel_nonpremult(src, src_a, dst, dst_factor_a as u8, scale, 8);
        let blend_b = blend_channel_nonpremult(src, src_a, dst, dst_factor_a as u8, scale, 16);
        (u32::from(blend_r) << 0)
            | (u32::from(blend_g) << 8)
            | (u32::from(blend_b) << 16)
            | (blend_a << 24)
    }
}

/// 两 [u8;4] 混合（image-webp `do_alpha_blending`；buffer=帧像素、canvas=画布像素）。
#[inline]
fn do_alpha_blending(buffer: [u8; 4], canvas: [u8; 4]) -> [u8; 4] {
    blend_pixel_nonpremult(u32::from_le_bytes(buffer), u32::from_le_bytes(canvas)).to_le_bytes()
}

/// 帧合成（image-webp `extended::composite_frame` 逐行移植；`frame` 为帧像素缓冲——
/// `frame_has_alpha=true` 时 RGBA 4 通道/像素，`false` 时 RGB 3 通道/像素）。
#[allow(clippy::too_many_arguments)]
fn composite_frame(
    canvas: &mut [u8],
    canvas_width: u32,
    canvas_height: u32,
    clear_color: Option<[u8; 4]>,
    frame: &[u8],
    frame_offset_x: u32,
    frame_offset_y: u32,
    frame_width: u32,
    frame_height: u32,
    frame_has_alpha: bool,
    frame_use_alpha_blending: bool,
    previous_frame_width: u32,
    previous_frame_height: u32,
    previous_frame_offset_x: u32,
    previous_frame_offset_y: u32,
) {
    let frame_is_full_size = frame_offset_x == 0
        && frame_offset_y == 0
        && frame_width == canvas_width
        && frame_height == canvas_height;

    // 全尺寸 + 不混合 → 直接整画布覆盖（无 alpha 帧带 A=255；清矩形步骤被跳过——
    // 与 image-webp 一致，重建把 dispose 场景交给下一帧的 clear）。
    if frame_is_full_size && !frame_use_alpha_blending {
        if frame_has_alpha {
            canvas.copy_from_slice(frame);
        } else {
            for (input, output) in frame.chunks_exact(3).zip(canvas.chunks_exact_mut(4)) {
                output[..3].copy_from_slice(input);
                output[3] = 255;
            }
        }
        return;
    }

    // 上一帧 DISPOSE → 用背景色清上一帧矩形（clear_color=None 时不清，与 image-webp 一致）。
    if let Some(clear_color) = clear_color {
        match (frame_is_full_size, frame_has_alpha) {
            (true, true) => {
                for pixel in canvas.chunks_exact_mut(4) {
                    pixel.copy_from_slice(&clear_color);
                }
            }
            // 注：以下两张表按 image-webp 原样移植，含其「以 3 字节视画布」的奇行——
            // 本实现目标是与 image-webp 逐字节一致（含这些分支），故不「修正」。
            (true, false) => {
                for pixel in canvas.chunks_exact_mut(3) {
                    pixel.copy_from_slice(&clear_color[..3]);
                }
            }
            (false, true) => {
                for y in 0..previous_frame_height as usize {
                    for x in 0..previous_frame_width as usize {
                        let canvas_index = ((x + previous_frame_offset_x as usize)
                            + (y + previous_frame_offset_y as usize) * canvas_width as usize)
                            * 4;
                        canvas[canvas_index..][..4].copy_from_slice(&clear_color);
                    }
                }
            }
            (false, false) => {
                for y in 0..previous_frame_height as usize {
                    for x in 0..previous_frame_width as usize {
                        let canvas_index = ((x + previous_frame_offset_x as usize)
                            + (y + previous_frame_offset_y as usize) * canvas_width as usize)
                            * 3;
                        canvas[canvas_index..][..3].copy_from_slice(&clear_color[..3]);
                    }
                }
            }
        }
    }

    let width = frame_width.min(canvas_width.saturating_sub(frame_offset_x)) as usize;
    let height = frame_height.min(canvas_height.saturating_sub(frame_offset_y)) as usize;

    if frame_has_alpha && frame_use_alpha_blending {
        for y in 0..height {
            for x in 0..width {
                let frame_index = (x + y * frame_width as usize) * 4;
                let canvas_index = ((x + frame_offset_x as usize)
                    + (y + frame_offset_y as usize) * canvas_width as usize)
                    * 4;
                let input = &frame[frame_index..][..4];
                let output = &mut canvas[canvas_index..][..4];
                let blended = do_alpha_blending(
                    input.try_into().unwrap(),
                    output.try_into().unwrap(),
                );
                output.copy_from_slice(&blended);
            }
        }
    } else if frame_has_alpha {
        // 带 alpha 帧 + 不混合 → 直拷 RGBA 行。
        for y in 0..height {
            let frame_index = (y * frame_width as usize) * 4;
            let canvas_index = (frame_offset_x as usize
                + (y + frame_offset_y as usize) * canvas_width as usize)
                * 4;
            canvas[canvas_index..][..width * 4]
                .copy_from_slice(&frame[frame_index..][..width * 4]);
        }
    } else {
        // 无 alpha 帧 → RGB 行 + A=255。
        for y in 0..height {
            let frame_index = (y * frame_width as usize) * 3;
            let canvas_index = (frame_offset_x as usize
                + (y + frame_offset_y as usize) * canvas_width as usize)
                * 4;
            let input = &frame[frame_index..][..width * 3];
            let output = &mut canvas[canvas_index..][..width * 4];
            for (input, output) in input.chunks_exact(3).zip(output.chunks_exact_mut(4)) {
                output[..3].copy_from_slice(input);
                output[3] = 255;
            }
        }
    }
}

/// 顺序合成器：维护全画布 RGBA 工作缓冲 + dispose/blend 帧间状态。
#[derive(Debug)]
pub(crate) struct CanvasCompositor {
    canvas: Vec<u8>,
    canvas_width: u32,
    canvas_height: u32,
    background_color: Option<[u8; 4]>,
    /// 上一帧是否要求 DISPOSE（决定本帧绘制前是否清上一帧矩形；首帧初始 true 但
    /// 上一帧矩形为 0 尺寸，实际无效果——与 image-webp `AnimationState` 同款）。
    dispose_next_frame: bool,
    previous_x: u32,
    previous_y: u32,
    previous_width: u32,
    previous_height: u32,
}

impl CanvasCompositor {
    pub(crate) fn new(width: u32, height: u32, background_color: Option<[u8; 4]>) -> Self {
        let mut canvas = vec![0u8; (width as usize) * (height as usize) * 4];
        if let Some(color) = background_color {
            for pixel in canvas.chunks_exact_mut(4) {
                pixel.copy_from_slice(&color);
            }
        }
        Self {
            canvas,
            canvas_width: width,
            canvas_height: height,
            background_color,
            dispose_next_frame: true,
            previous_x: 0,
            previous_y: 0,
            previous_width: 0,
            previous_height: 0,
        }
    }

    /// 把一帧合成进工作缓冲并推进 dispose/blend 帧间状态（`compose`/`compose_to` 共用；
    /// dispose/blend 语义与 image-webp 对齐，见 `composite_frame`）。
    fn composite_into_canvas(&mut self, f: &AnimatedRawFrame) {
        let clear_color = if self.dispose_next_frame {
            self.background_color
        } else {
            None
        };
        // 无 alpha 帧抽 RGB 视图（3 通道/像素），对齐 image-webp 的 RGB 帧缓冲语义。
        let rgb_view;
        let frame: &[u8] = if f.has_alpha {
            &f.rgba
        } else {
            rgb_view = f
                .rgba
                .chunks_exact(4)
                .flat_map(|px| px[..3].iter().copied())
                .collect::<Vec<u8>>();
            &rgb_view
        };
        composite_frame(
            &mut self.canvas,
            self.canvas_width,
            self.canvas_height,
            clear_color,
            frame,
            f.x,
            f.y,
            f.width,
            f.height,
            f.has_alpha,
            f.blend,
            self.previous_width,
            self.previous_height,
            self.previous_x,
            self.previous_y,
        );
        self.previous_x = f.x;
        self.previous_y = f.y;
        self.previous_width = f.width;
        self.previous_height = f.height;
        self.dispose_next_frame = f.dispose_to_background;
    }

    /// 合成一帧（dispose → 覆盖/混合），返回合成后全画布 RGBA 快照（`first_frame_rgba` 用：
    /// 整图读取仅取首帧，非播放热路径，直接 clone）。
    pub(crate) fn compose(&mut self, f: &AnimatedRawFrame) -> Vec<u8> {
        self.composite_into_canvas(f);
        self.canvas.clone()
    }

    /// 合成一帧并把全画布快照**单次直出**到 `out`（#4 优化）。
    /// 合并旧的「`compose` 返回 `self.canvas.clone()` → 非 alpha 再整画布 repack 重建新 Vec」
    /// 两遍整画布拷贝为一遍：`rgba_out=true` 直拷 RGBA；`false` 逐像素单遍 RGB+255 直写
    /// （输出尺寸同为 w*h*4，逐字节与旧的「clone 后再 repack」完全一致）。dispose/blend
    /// 仍先在 RGBA 工作缓冲完成（跨帧状态依赖真 alpha），快照才在末了单遍产出。
    pub(crate) fn compose_to(&mut self, f: &AnimatedRawFrame, out: &mut Vec<u8>, rgba_out: bool) {
        self.composite_into_canvas(f);
        out.clear();
        if rgba_out {
            out.extend_from_slice(&self.canvas);
        } else {
            out.resize(self.canvas.len(), 0);
            for (dst, src) in out.chunks_exact_mut(4).zip(self.canvas.chunks_exact(4)) {
                dst[..3].copy_from_slice(&src[..3]);
                dst[3] = 255;
            }
        }
    }
}

/// `AnimationDecoder::into_frames` 迭代器：逐帧合成并把全画布转成 `Frame`（RGBA8）。
pub(crate) struct OursFramesIterator {
    composer: CanvasCompositor,
    width: u32,
    height: u32,
    /// image-webp `has_alpha()` 口径：决定输出 RGBA8 直接透传，还是 RGB8→A=255。
    has_alpha_flag: bool,
    frames: std::vec::IntoIter<AnimatedRawFrame>,
}

impl OursFramesIterator {
    pub(crate) fn new(anim: OursAnimated) -> Self {
        Self {
            width: anim.width,
            height: anim.height,
            has_alpha_flag: anim.has_alpha_flag,
            composer: CanvasCompositor::new(anim.width, anim.height, anim.background_color),
            frames: anim.frames.into_iter(),
        }
    }
}

impl Iterator for OursFramesIterator {
    type Item = ImageResult<Frame>;

    fn next(&mut self) -> Option<Self::Item> {
        let f = self.frames.next()?;
        let delay = f.duration_ms;
        // #4：合成时直出快照到临时缓冲（alpha 直拷 / 非 alpha RGB+255 单遍直写），
        // 替代旧「compose 返回 clone + 非 alpha 再 repack」两遍整画布拷贝。
        let mut canvas = Vec::new();
        self.composer.compose_to(&f, &mut canvas, self.has_alpha_flag);
        let img = RgbaImage::from_raw(self.width, self.height, canvas).expect("画布尺寸与帧快照一致");
        Some(Ok(Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(delay, 1))))
    }
}

/// 把 `OursAnimated` 合成得到首帧 RGBA（供 `ImageDecoder::read_image` 用；image-webp
/// 对动画的整图读取 = 首帧）。
pub(crate) fn first_frame_rgba(anim: &OursAnimated) -> Option<(Vec<u8>, u32, u32)> {
    let mut composer = CanvasCompositor::new(anim.width, anim.height, anim.background_color);
    let f = anim.frames.first()?;
    let canvas = composer.compose(f);
    Some((canvas, anim.width, anim.height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::webp_fixtures_support;
    use std::io::Cursor;
    use crate::codecs::webp::set_webp_backend;
    use crate::codecs::webp::{BACKEND_AUTO, BACKEND_OURS};
    use crate::{AnimationDecoder, ImageDecoder};

    fn loop_label(l: crate::LoopCount) -> &'static str {
        match l {
            crate::LoopCount::Infinite => "infinite",
            crate::LoopCount::Finite(_) => "finite",
        }
    }

    /// image-webp dev oracle：直连 `image_webp::WebPDecoder` 逐帧解码动画为
    /// `((w,h), loop, [(RGBA, delay_ms)…])`（RGB 帧补 alpha=255，与 kimg `into_frames` RGBA 同口径）。
    fn imagewebp_anim_reference(
        data: &[u8],
    ) -> ((u32, u32), crate::LoopCount, Vec<(Vec<u8>, u32)>) {
        let mut dec =
            image_webp::WebPDecoder::new(Cursor::new(data)).expect("image_webp::new 应成功");
        let dims = dec.dimensions();
        let loop_count = match dec.loop_count() {
            image_webp::LoopCount::Forever => crate::LoopCount::Infinite,
            image_webp::LoopCount::Times(n) => crate::LoopCount::Finite(n.into()),
        };
        let has_a = dec.has_alpha();
        let n = dec.output_buffer_size().expect("image_webp 缓冲大小");
        let mut buf = vec![0u8; n];
        let mut out = Vec::new();
        loop {
            match dec.read_frame(&mut buf) {
                Ok(delay_ms) => {
                    let rgba = if has_a {
                        buf.clone()
                    } else {
                        let mut rgba = Vec::with_capacity(n / 3 * 4);
                        for px in buf.chunks_exact(3) {
                            rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
                        }
                        rgba
                    };
                    out.push((rgba, delay_ms));
                }
                Err(image_webp::DecodingError::NoMoreFrames) => break,
                Err(e) => panic!("image_webp read_frame 失败: {e}"),
            }
        }
        (dims, loop_count, out)
    }

    /// 全部动画 fixture（SimdCodecs 8 个 anim-* + 用户语料）必须被自研动画解码接管
    /// （Some），且与 image-webp 逐帧 RGBA/delay 全等（自研经 kimg `into_frames`，
    /// 参照经直连 `image_webp::WebPDecoder`）。
    /// 帧数 < image-webp 或多帧不符 → 失败；缺外部语料 SKIP。
    #[test]
    fn animated_fixtures_ours_matches_imagewebp() {
        set_webp_backend(BACKEND_AUTO);
        let mut names = webp_fixtures_support::webp_fixture_names();
        names.sort();
        let mut checked = 0usize;
        let mut skipped = 0usize;
        let mut errors = Vec::new();
        for name in &names {
            let Some(path) = webp_fixtures_support::webp_fixture_path(name) else {
                continue;
            };
            let data = std::fs::read(&path).unwrap();
            // 是否动画以自研接管为判据（OURS 后端 + has_animation）。
            set_webp_backend(BACKEND_OURS);
            let ours_dec = match crate::codecs::webp::WebPDecoder::new(Cursor::new(data.clone())) {
                Ok(d) => d,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            if !ours_dec.has_animation() {
                continue; // 静态图，非本测试对象
            }
            // 自研接管硬证：直接调 `decode_animation` 必须产出 Some，否则本测试只是空转。
            let direct = crate::codecs::webp::animation::decode_animation(&data)
                .expect("decode_animation 不应 Err");
            assert!(
                direct.is_some(),
                "{name}: OURS 动画解码应接管而非显式报错"
            );
            let ours_loop = ours_dec.loop_count();
            let mk = |d: crate::codecs::webp::WebPDecoder<Cursor<Vec<u8>>>| {
                let n = d.dimensions();
                let frames = d
                    .into_frames()
                    .collect_frames()
                    .expect("帧读取应成功");
                let out: Vec<(Vec<u8>, u32)> = frames
                    .into_iter()
                    .map(|f| {
                        let d = f.delay().numer_denom_ms().0;
                        (f.into_buffer().into_raw(), d)
                    })
                    .collect();
                (n, out)
            };
            let (ours_dims, ours_frames) = mk(ours_dec);
            let (img_dims, img_loop, img_frames) = imagewebp_anim_reference(&data);

            let loop_same = match (ours_loop, img_loop) {
                (crate::LoopCount::Infinite, crate::LoopCount::Infinite) => true,
                (crate::LoopCount::Finite(a), crate::LoopCount::Finite(b)) => a == b,
                _ => false,
            };
            if !loop_same {
                errors.push(format!(
                    "{name}: loop 不一致 ours={:?} vs imgwebp={:?}",
                    loop_label(ours_loop),
                    loop_label(img_loop)
                ));
            }
            if ours_dims != img_dims {
                errors.push(format!(
                    "{name}: 尺寸不一致 {:?} vs {:?}",
                    ours_dims, img_dims
                ));
            }
            if ours_frames.len() != img_frames.len() {
                errors.push(format!(
                    "{name}: 帧数不一致 {} vs {}",
                    ours_frames.len(),
                    img_frames.len()
                ));
            } else {
                for (i, (ours_f, img_f)) in ours_frames.iter().zip(&img_frames).enumerate() {
                    if ours_f.0 != img_f.0 {
                        let d = ours_f
                            .0
                            .iter()
                            .zip(&img_f.0)
                            .position(|(a, b)| a != b)
                            .map(|p| {
                                (ours_f.0[p], ours_f.0[p + 3], img_f.0[p], img_f.0[p + 3])
                            });
                        errors.push(format!(
                            "{name}: 帧 {i} RGBA 不一致（首失配 {d:?}）"
                        ));
                    }
                    if ours_f.1 != img_f.1 {
                        errors.push(format!(
                            "{name}: 帧 {i} delay 不一致 {} vs {}",
                            ours_f.1, img_f.1
                        ));
                    }
                }
            }
            checked += 1;
        }
        set_webp_backend(BACKEND_AUTO);
        for e in &errors {
            eprintln!("WEBP_ANIM_MISMATCH\t{e}");
        }
        eprintln!("WEBP_ANIM_ORACLE\tchecked={checked} skipped={skipped}");
        assert!(
            errors.is_empty(),
            "{} 处动画逐帧与 image-webp 不一致（见上）",
            errors.len()
        );
        assert!(checked >= 8, "至少 8 个动画 fixture 应对拍，实际 {checked}");
    }

    /// #2 内存护栏（自包含语义）：预估内存超 `KIMG_WEBP_ANIM_CAP_MB` → `decode_animation`
    /// 返回 None，`WebPDecoder::new` 将其转为**显式错误**（自研已自包含，无 image-webp 兜底）；
    /// 复位 cap 后小 fixture 仍自研接管。
    /// （env 读写沿用既有 `KIMG_WEBP_THREADS` 测试姿势：本模块按 `--test-threads=1` 串行跑。）
    #[test]
    fn anim_cap_is_explicit_error_no_fallback() {
        set_webp_backend(BACKEND_AUTO);
        let Some(data) = webp_fixtures_support::webp_fixture_path("anim-webpmux-combinations.webp")
            .map(|p| std::fs::read(p).unwrap())
        else {
            eprintln!("SKIP anim-webpmux-combinations.webp 外部语料缺失");
            return;
        };
        // 默认 cap（256MB）：小 fixture 必须被自研接管。
        assert!(
            decode_animation(&data)
                .expect("decode_animation 不应 Err")
                .is_some(),
            "默认 cap 下小动画 fixture 应被自研接管"
        );
        // cap=0：预估必超阈值 → decode_animation 放弃（None）+ WebPDecoder::new 显式 Err。
        unsafe { std::env::set_var("KIMG_WEBP_ANIM_CAP_MB", "0") };
        let guarded = decode_animation(&data).expect("decode_animation 不应 Err");
        let new_err = crate::codecs::webp::WebPDecoder::new(Cursor::new(data.clone())).is_err();
        unsafe { std::env::remove_var("KIMG_WEBP_ANIM_CAP_MB") };
        assert!(guarded.is_none(), "cap=0 时应放弃自研动画解码");
        assert!(new_err, "cap=0 时 WebPDecoder::new 应显式报错（无 image-webp 兜底）");
    }

    /// 帧级并行解码与串行逐字节一致（先解码并行、后合成串行管道的硬闸）。
    #[test]
    fn parallel_equals_serial() {
        set_webp_backend(BACKEND_AUTO);
        let Some(data) = webp_fixtures_support::webp_fixture_path("anim-webpmux-combinations.webp")
            .map(|p| std::fs::read(p).unwrap())
        else {
            eprintln!("SKIP anim-webpmux-combinations.webp 外部语料缺失");
            return;
        };
        let c = container::parse(&data).expect("容器解析");
        assert_eq!(c.family, container::Family::Animated);
        // Rust 2024 下 env 读写标记 unsafe；测试内已知无并发竞争（串行测试，K42-style）。
        unsafe { std::env::set_var("KIMG_WEBP_THREADS", "0") };
        let serial = decode_frames_parallel(&data, &c.frames).expect("串行解码");
        unsafe { std::env::set_var("KIMG_WEBP_THREADS", "2") };
        let para = decode_frames_parallel(&data, &c.frames).expect("并行解码");
        unsafe { std::env::remove_var("KIMG_WEBP_THREADS") };
        assert_eq!(serial.len(), para.len());
        for (a, b) in serial.iter().zip(&para) {
            assert_eq!(a.rgba, b.rgba, "帧级并行解码输出应逐字节一致");
            assert_eq!(a.blend, b.blend);
            assert_eq!(a.dispose_to_background, b.dispose_to_background);
            assert_eq!(a.has_alpha, b.has_alpha);
        }
    }

    /// 合成器与 SimdCodecs `WebpAnimationDecoder` 差异专项：anim-vp8l-alpha-hint（RFC-blend +
    /// IsIndependent）应可被自研接管且与 image-webp 帧数一致。
    #[test]
    fn vp8l_alpha_hint_takeover() {
        set_webp_backend(BACKEND_OURS);
        let Some(data) = webp_fixtures_support::webp_fixture_path("anim-vp8l-alpha-hint.webp")
            .map(|p| std::fs::read(p).unwrap())
        else {
            eprintln!("SKIP anim-vp8l-alpha-hint.webp 外部语料缺失");
            return;
        };
        let dec = crate::codecs::webp::WebPDecoder::new(Cursor::new(data.clone()))
            .expect("自研动画接管");
        assert!(dec.has_animation(), "alpha-hint 动画应走 OURS");
        let ours_frames = dec.into_frames().collect_frames().unwrap();
        set_webp_backend(BACKEND_AUTO);
        let (_, _, img_frames) = imagewebp_anim_reference(&data);
        // 帧数一致 + 尺寸一致（逐字节在 animated_fixtures_ours_matches_imagewebp 已覆盖）。
        assert_eq!(
            ours_frames.len(),
            img_frames.len(),
            "alpha-hint 帧数应与 image-webp 一致"
        );
        for (o, (i, _)) in ours_frames.iter().zip(&img_frames) {
            assert_eq!(
                o.buffer().as_raw(),
                i,
                "alpha-hint 逐帧 RGBA 应与 image-webp 一致"
            );
        }
    }
}
