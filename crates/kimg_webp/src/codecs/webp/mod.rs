//! WebP 解码器：自研容器 + VP8L 无损 + VP8 有损 + ALPH + 动画内核（自包含）。
//!
//! 对外 API（`WebPDecoder<R>` 的 `ImageDecoder`+`AnimationDecoder`）与 image 0.25 一致。
//! 后端开关（M3 演化）：`WEBP_BACKEND` 原子位 AUTO/OURS，环境变量 `KIMG_WEBP_BACKEND`
//! 仅首次惰性解析一次（防每次解码 `env::var` 的 std::env 并发问题）；`pub(crate) set_webp_backend`
//! 供测试直设。家族判定（VP8/VP8L/动画/ALPH）在容器解析后按 VP8X features byte + 是否含 ANIM
//! chunk 判定（P1 起），不按首 4 字节盲判。
//!
//! 自包含（P7，2026-11）：image-webp 0.2.4 已从 `[dependencies]` 移入 `[dev-dependencies]`
//! （仅测试/基准 oracle 参照）。坏输入 / 超限（动画 `KIMG_WEBP_ANIM_CAP_MB`，默认 256MB）不再
//! 静默降级 image-webp，一律显式 `ImageError::Decoding`。

use std::io::{BufRead, Seek};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU8, Ordering};

use kimg_core::animation::{Frames, LoopCount};
use kimg_core::color::ColorType;
use kimg_core::error::{DecodingError, ImageError, ImageResult};
use kimg_core::format::ImageFormat;
use kimg_core::metadata::Orientation;
use kimg_core::pixel::Rgba;
use crate::{AnimationDecoder, ImageDecoder};

pub(crate) mod animation;
pub(crate) mod container;
pub(crate) mod simd;
pub(crate) mod vp8;
pub(crate) mod vp8l;

// 阶段分账探针公共入口（仅 `stage_profile` feature 编译；供 imgbench 探针调用，K59 统一基座）。
// feature 关闭时零代码影响（正常构建路径不变）。
#[cfg(feature = "stage_profile")]
pub use vp8l::Vp8lStageProfile;

/// 从完整 WEBP 文件抽出 VP8L 位流载荷（`stage_profile` feature 门控；imgbench 探针用）。
#[cfg(feature = "stage_profile")]
pub fn vp8l_extract_bitstream(data: &[u8]) -> Option<Vec<u8>> {
    let c = container::parse(data).ok()?;
    c.bitstream.map(|b| b.to_vec())
}

/// 阶段分账解码（`stage_profile` feature 门控；`bitstream` 为 VP8L chunk 载荷，首字节 0x2f）。
/// 出错返回 `Err(String)`（畸形输入不 panic）。
#[cfg(feature = "stage_profile")]
pub fn vp8l_stage_decode(bitstream: &[u8]) -> std::result::Result<Vp8lStageProfile, String> {
    vp8l::stage_decode(bitstream).map_err(|e| e.0)
}

// VP8 有损阶帧分账探针公共入口（`stage_profile` feature 门控；供 imgbench `vp8_stage_probe`，
// K59 统一基座）。feature 关闭时零代码影响（正常构建路径不变）。镜像上方 vp8l 探针姿势。
#[cfg(feature = "stage_profile")]
pub use vp8::Vp8StageProfile;

/// 从完整 WEBP 文件抽出 VP8 有损帧（`stage_profile` feature 门控；imgbench 探针用）。
/// 返回 (VP8 帧载荷, 宽, 高, 是否 LossyAlpha)；非静态有损/解析失败返回 `None`。
#[cfg(feature = "stage_profile")]
pub fn vp8_extract_frame(data: &[u8]) -> Option<(Vec<u8>, u32, u32, bool)> {
    let c = container::parse(data).ok()?;
    let lossy_alpha = c.family == container::Family::LossyAlpha;
    if c.family != container::Family::StaticLossy && !lossy_alpha {
        return None;
    }
    let frame = c.bitstream?.to_vec();
    Some((frame, c.width, c.height, lossy_alpha))
}

/// 阶段分账解码（`stage_profile` feature 门控；`frame` 为 VP8 帧载荷，`width/height` 来自容器）。
/// 出错返回 `Err(String)`（畸形输入不 panic）。
#[cfg(feature = "stage_profile")]
pub fn vp8_stage_decode(
    frame: &[u8],
    width: u32,
    height: u32,
) -> std::result::Result<Vp8StageProfile, String> {
    vp8::stage_decode(frame, width, height).map_err(|e| e.0)
}

/// 熵内部三类操作的单位耗时探针（`stage_profile` feature 门控；imgbench `vp8_stage_probe` 用，
/// 把 bool 读/记账/去量化 计数换算成时间占比）。返回每操作 ns：`(bool 读, 记账·游标, 去量化+zigzag)`。
#[cfg(feature = "stage_profile")]
pub fn vp8_probe_entropy_op_costs(reps: u32) -> (f64, f64, f64) {
    vp8::probe_entropy_op_costs(reps)
}

/// i4 段内五类操作的单位耗时探针（`stage_profile` feature 门控；imgbench `vp8_stage_probe` 用，
/// K83 修正方案：外层粗钟 + 原子计数折算）。返回各操作 ns，见 `I4OpCosts`。
#[cfg(feature = "stage_profile")]
pub use vp8::I4OpCosts;

/// `probe_i4_op_costs` 的模块级入口（供 imgbench `vp8_stage_probe`）。
#[cfg(feature = "stage_profile")]
pub fn vp8_probe_i4_op_costs(reps: u32) -> I4OpCosts {
    vp8::probe_i4_op_costs(reps)
}

/// 探针自身开销校准入口（`stage_profile` feature 门控）：`(原子计数 ns/次, Instant::now() ns/次)`。
#[cfg(feature = "stage_profile")]
pub fn vp8_probe_clock_costs(reps: u32) -> (f64, f64) {
    vp8::probe_clock_costs(reps)
}

// 外部语料定位助手（与集成测试共享，见 tests/fixtures/webp/mod.rs）。
#[cfg(test)]
#[path = "../../../tests/fixtures/webp/mod.rs"]
mod webp_fixtures_support;

// ---------------------------------------------------------------------------
// 后端开关（M3 演化 → 自包含，2026-11）
// ---------------------------------------------------------------------------

/// 自动（默认）：自研核（自包含后与 OURS 同义，无其它后端可选）。
pub const BACKEND_AUTO: u8 = 0;
/// 强制自研（全部已实现家族：VP8/VP8L/ALPH/动画）。
pub const BACKEND_OURS: u8 = 1;

static WEBP_BACKEND: AtomicU8 = AtomicU8::new(BACKEND_AUTO);

/// 直设后端（测试/对照用；不触发环境变量解析）。
#[cfg_attr(not(test), allow(dead_code))]
pub fn set_webp_backend(v: u8) {
    debug_assert!(v <= BACKEND_OURS);
    WEBP_BACKEND.store(v, Ordering::SeqCst);
}

/// 解析后端：首次调用（状态为 AUTO 且从未被直设）才读一次环境变量 `KIMG_WEBP_BACKEND`
/// （`auto|ours|imagewebp`），并发下 CAS 保证只由先到者写入。此后恒定。
/// 自包含后 `imagewebp` 值不再切换后端：接受 + 忽略（等同 ours），仅 eprintln 提示一次。
pub(crate) fn resolved_backend() -> u8 {
    let cur = WEBP_BACKEND.load(Ordering::Relaxed);
    if cur != BACKEND_AUTO {
        return cur;
    }
    let resolved = match std::env::var("KIMG_WEBP_BACKEND") {
        Ok(v) => match v.to_ascii_lowercase().as_str() {
            "ours" => BACKEND_OURS,
            // image-webp 运行时兜底已随自包含移除：接受但忽略，等同 ours（仅提示一次）。
            "imagewebp" | "image-webp" => {
                eprintln!(
                    "KIMG_WEBP_BACKEND=imagewebp 已被忽略：WebP 已自包含，运行时等同自研（ours）"
                );
                BACKEND_OURS
            }
            // `auto` / 其它未知名 / 未设：默认自研核（2026-11）。
            _ => BACKEND_OURS,
        },
        Err(_) => BACKEND_OURS,
    };
    match WEBP_BACKEND.compare_exchange(
        BACKEND_AUTO,
        resolved,
        Ordering::SeqCst,
        Ordering::SeqCst,
    ) {
        Ok(_) => resolved,
        Err(already_set) => already_set,
    }
}

/// 是否允许自研路径接管：自包含后恒为 true（已无 image-webp 运行时兜底）。保留调用以维持
/// `KIMG_WEBP_BACKEND` 解析语义（`imagewebp` 值仅打印提示，不再切换后端）。
fn ours_allowed() -> bool {
    let _ = resolved_backend();
    true
}

// ---------------------------------------------------------------------------
// 解码器包装
// ---------------------------------------------------------------------------

/// 容器解析 + 自研解码产物（P1/P2 填充）。
#[derive(Default)]
pub(crate) struct OursStatic {
    /// 解码后的 RGBA8 像素（长度 = w*h*4；零拷贝封装，见 `vp8l::RgbaBytes`）。
    /// 全部静态路径持有：alpha/无损路径（真实 A 通道），以及 opaque 有损（α 恒 0xFF、
    /// 单遍 YUV→RGBA4 直出，消 RGB3→RGBA4 展开+分配税，见 `decode_static_lossy`）。
    pub rgba: vp8l::RgbaBytes,
    /// 遗留 RGB3 直出字段（长度 = w*h*3）：被 opaque RGBA4 直出取代后恒空（K24 dormancy，
    /// 见 `vp8::decode_vp8_rgb3`）。非空时像素在 `rgba`。
    pub rgb: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// 输出像素是否为 RGBA8（决定 `color_type`/`read_image` 的字节/像素口径）。opaque 有损
    /// 现恒为 true：输出 RGBA8 α=255，`to_rgba8`/`into_rgba8` 零拷贝；容器/文件层无 alpha
    /// 概念（与 image-webp 的 Rgb8 口径有意分道，T1 生产消费姿态 RGBA8 直出）。
    pub has_alpha: bool,
    pub exif: Option<Vec<u8>>,
    pub icc: Option<Vec<u8>>,
    pub xmp: Option<Vec<u8>>,
    /// 直写惰性载荷：有损静态（opaque / LossyAlpha）时持原始压缩字节（Box）、像素未预解；
    /// 无损 / 急切路径恒 None（像素在 `rgba`）。`read_image` 见 `raw_data` 即走
    /// `decode_static_lossy_rgba8_into` 直写调用方缓冲——不经中间 `Vp8Yuv.rgba` 整帧分配 +
    /// `read_image` 整帧拷贝（镜像无损 `decode_static_lossless_rgba8_into` 的直写姿势）。
    pub raw_data: Option<Box<[u8]>>,
}

enum WebPDecoderInner {
    /// 自研静态路径：容器 + VP8L/VP8 已解码缓冲（P2 起启用）。
    Ours(OursStatic),
    /// 自研动画路径：ANMF 帧原始解码 + 画布合成（P6 起启用）。
    OursAnimated(animation::OursAnimated),
}

/// WebP 解码器（支持有损/无损/带 alpha/动画；自包含，全部家族走自研内核）。
pub struct WebPDecoder<R> {
    inner: WebPDecoderInner,
    orientation: Option<Orientation>,
    _r: PhantomData<R>,
}

impl<R: BufRead + Seek> WebPDecoder<R> {
    /// 构造：读全量字节 → 容器解析判定家族 → 自研内核解码（VP8/VP8L/ALPH/动画）。
    /// 自包含（P7）：坏输入 / 超限（动画 `KIMG_WEBP_ANIM_CAP_MB`，默认 256MB）/ 解码失败一律
    /// 显式 `ImageError::Decoding`，不再静默降级 image-webp。
    pub fn new(r: R) -> ImageResult<Self> {
        let mut data = Vec::new();
        {
            let mut br = r;
            br.read_to_end(&mut data).map_err(ImageError::IoError)?;
        }
        // 自包含后恒允许自研路径；此调用仅为维持 `KIMG_WEBP_BACKEND` 解析语义
        // （`imagewebp` 值只打印提示，不再切换后端）。
        let _ = ours_allowed();

        let c = match container::parse(&data) {
            Ok(c) => c,
            Err(_) => {
                return Err(ImageError::Decoding(DecodingError::new(
                    ImageFormat::WebP.into(),
                    "WebP 容器解析失败（RIFF/WEBP 结构无效）",
                )));
            }
        };
        if c.family == container::Family::Animated {
            return match crate::codecs::webp::animation::decode_animation(&data) {
                Ok(Some(ours)) => Ok(Self {
                    inner: WebPDecoderInner::OursAnimated(ours),
                    orientation: None,
                    _r: PhantomData,
                }),
                Ok(None) => Err(ImageError::Decoding(DecodingError::new(
                    ImageFormat::WebP.into(),
                    "WebP 动画解码失败或超出 KIMG_WEBP_ANIM_CAP_MB 内存护栏（默认 256MB；已自包含，无 image-webp 兜底）",
                ))),
                Err(e) => Err(e),
            };
        }
        if c.family.is_static_lossless() {
            return match crate::codecs::webp::decode_static_lossless(&data)? {
                Some(ours) => Ok(Self {
                    inner: WebPDecoderInner::Ours(ours),
                    orientation: None,
                    _r: PhantomData,
                }),
                None => Err(ImageError::Decoding(DecodingError::new(
                    ImageFormat::WebP.into(),
                    "WebP 无损静态解码失败（已自包含，无 image-webp 兜底）",
                ))),
            };
        }
        if c.family == container::Family::StaticLossy
            || c.family == container::Family::LossyAlpha
        {
            // 有损静态：不再急切整帧解码。持压缩载荷（`raw_data`），`read_image` 经
            // `decode_static_lossy_rgba8_into` 直写调用方缓冲——消「内部 `Vp8Yuv.rgba` 整帧分配 +
            // `read_image` 整帧拷贝」两笔 w*h*4 税（字节恒等：直写核与既有 `decode_static_lossy`
            // 同一 `decode_from_header` 发射路径，仅输出目标不同）。尺寸/元数据由容器解析即得，
            // 无需像素。有损输出恒 RGBA8（opaque 有损 α 恒 255、LossyAlpha 真实 A 通道），
            // 与既有 `decode_static_lossy`/`color_type` 语义一致（`has_alpha=true`）。
            let (lossy_w, lossy_h) = (c.width, c.height);
            let lossy_exif = c.exif.map(|e| e.to_vec());
            let lossy_icc = c.icc_profile.map(|e| e.to_vec());
            let lossy_xmp = c.xmp.map(|e| e.to_vec());
            // 元数据/尺寸已抽到本地，`c` 不再借用 `data` → 可安全整体移入惰性载荷。
            let raw = data.into_boxed_slice();
            return Ok(Self {
                inner: WebPDecoderInner::Ours(OursStatic {
                    rgba: vp8l::RgbaBytes::default(),
                    rgb: Vec::new(),
                    width: lossy_w,
                    height: lossy_h,
                    has_alpha: true,
                    exif: lossy_exif,
                    icc: lossy_icc,
                    xmp: lossy_xmp,
                    raw_data: Some(raw),
                }),
                orientation: None,
                _r: PhantomData,
            });
        }
        Err(ImageError::Decoding(DecodingError::new(
            ImageFormat::WebP.into(),
            "WebP 家族不受支持（已自包含，无 image-webp 兜底）",
        )))
    }

    /// 是否为动画。
    pub fn has_animation(&self) -> bool {
        match &self.inner {
            WebPDecoderInner::Ours(_) => false,
            WebPDecoderInner::OursAnimated(_) => true,
        }
    }

    /// 设置动画背景色。
    pub fn set_background_color(&mut self, color: Rgba<u8>) -> ImageResult<()> {
        match &mut self.inner {
            WebPDecoderInner::Ours(_) => Ok(()),
            // 记录背景色，`into_frames` 合成时按 image-webp 语义使用：
            // Some(0,0,0,0)=透明画布 + dispose 清矩形为透明黑；None=dispose 不清矩形。
            WebPDecoderInner::OursAnimated(anim) => {
                anim.background_color = Some(color.0);
                Ok(())
            }
        }
    }
}

impl<R: BufRead + Seek> ImageDecoder for WebPDecoder<R> {
    fn dimensions(&self) -> (u32, u32) {
        match &self.inner {
            WebPDecoderInner::Ours(o) => (o.width, o.height),
            WebPDecoderInner::OursAnimated(a) => (a.width, a.height),
        }
    }

    fn color_type(&self) -> ColorType {
        match &self.inner {
            WebPDecoderInner::Ours(o) => {
                if o.has_alpha {
                    ColorType::Rgba8
                } else {
                    ColorType::Rgb8
                }
            }
            // 与 image-webp `has_alpha()` 口径一致（VP8X alpha flag）。
            WebPDecoderInner::OursAnimated(a) => {
                if a.has_alpha_flag {
                    ColorType::Rgba8
                } else {
                    ColorType::Rgb8
                }
            }
        }
    }

    fn exif_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        let exif = match &mut self.inner {
            WebPDecoderInner::Ours(o) => o.exif.as_ref().cloned(),
            WebPDecoderInner::OursAnimated(a) => a.exif.as_ref().cloned(),
        };
        self.orientation = Some(
            exif.as_ref()
                .and_then(|exif| Orientation::from_exif_chunk(exif))
                .unwrap_or(Orientation::NoTransforms),
        );
        Ok(exif)
    }

    fn icc_profile(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(match &mut self.inner {
            WebPDecoderInner::Ours(o) => o.icc.as_ref().cloned(),
            WebPDecoderInner::OursAnimated(a) => a.icc.as_ref().cloned(),
        })
    }

    fn xmp_metadata(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(match &mut self.inner {
            WebPDecoderInner::Ours(o) => o.xmp.clone(),
            WebPDecoderInner::OursAnimated(a) => a.xmp.as_ref().cloned(),
        })
    }

    fn orientation(&mut self) -> ImageResult<Orientation> {
        if self.orientation.is_none() {
            let _ = self.exif_metadata()?;
        }
        Ok(self.orientation.unwrap())
    }

    fn read_image(self, buf: &mut [u8]) -> ImageResult<()> {
        assert_eq!(u64::try_from(buf.len()), Ok(self.total_bytes()));
        match self.inner {
            WebPDecoderInner::Ours(o) => {
                // 有损静态直写进口：`OursStatic` 持原始压缩字节（惰性 `raw_data`），
                // `decode_static_lossy_rgba8_into` 把 VP8 行定稿直出直接写调用方 `buf`，不经
                // 中间 `Vp8Yuv.rgba` 整帧分配 + 本函数整帧拷贝（opaque 与 LossyAlpha 两族同走，
                // 镜像无损 `decode_into_rgba8` 直写姿势；输出恒 RGBA8 缓冲）。
                if let Some(data) = o.raw_data.as_deref() {
                    let target = (o.width as usize) * (o.height as usize) * 4;
                    if buf.len() != target {
                        return Err(ImageError::Decoding(DecodingError::new(
                            ImageFormat::WebP.into(),
                            "Ours 解码缓冲长度与像素缓冲不一致",
                        )));
                    }
                    return match crate::codecs::webp::decode_static_lossy_rgba8_into(data, buf) {
                        Ok(true) => Ok(()),
                        Ok(false) => Err(ImageError::Decoding(DecodingError::new(
                            ImageFormat::WebP.into(),
                            "有损静态直写接管失败（家族已核，不应发生）",
                        ))),
                        Err(e) => Err(e),
                    };
                }
                if o.has_alpha {
                    let target = (o.width as usize) * (o.height as usize) * 4;
                    if buf.len() != target {
                        return Err(ImageError::Decoding(DecodingError::new(
                            ImageFormat::WebP.into(),
                            "Ours 解码缓冲长度与像素缓冲不一致",
                        )));
                    }
                    buf.copy_from_slice(o.rgba.as_bytes());
                } else {
                    // RGB8 目标：buf.len()==w*h*3。opaque 有损自 T1 起走 RGBA 分支（`has_alpha`
                    // true，`rgba` 直拷，零拷贝）；本分支现仅为「无损-无 alpha（VP8L 不透明）」
                    // 服务：`o.rgb` 遗留恒空（K24 dormancy）→ rgba4 抽条（既有语义不变）。
                    let target = (o.width as usize) * (o.height as usize) * 3;
                    if buf.len() != target {
                        return Err(ImageError::Decoding(DecodingError::new(
                            ImageFormat::WebP.into(),
                            "Ours 解码缓冲长度与像素缓冲不一致",
                        )));
                    }
                    if !o.rgb.is_empty() {
                        buf.copy_from_slice(&o.rgb);
                    } else {
                        simd::rgba8_to_rgb8(o.rgba.as_bytes(), buf);
                    }
                }
                Ok(())
            }
            // 动画整图读取 = 首帧（image-webp 语义）。
            WebPDecoderInner::OursAnimated(a) => {
                let (canvas, w, h) = match animation::first_frame_rgba(&a) {
                    Some(v) => v,
                    None => {
                        return Err(ImageError::Decoding(DecodingError::new(
                            ImageFormat::WebP.into(),
                            "Ours 动画无帧可读",
                        )))
                    }
                };
                if a.has_alpha_flag {
                    let target = (w as usize) * (h as usize) * 4;
                    if buf.len() != target {
                        return Err(ImageError::Decoding(DecodingError::new(
                            ImageFormat::WebP.into(),
                            "Ours 动画解码缓冲长度与像素缓冲不一致",
                        )));
                    }
                    buf.copy_from_slice(&canvas);
                } else {
                    let target = (w as usize) * (h as usize) * 3;
                    if buf.len() != target {
                        return Err(ImageError::Decoding(DecodingError::new(
                            ImageFormat::WebP.into(),
                            "Ours 动画解码缓冲长度与像素缓冲不一致",
                        )));
                    }
                    simd::rgba8_to_rgb8(&canvas, buf);
                }
                Ok(())
            }
        }
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}

impl<'a, R: 'a + BufRead + Seek> AnimationDecoder<'a> for WebPDecoder<R> {
    fn loop_count(&self) -> LoopCount {
        match &self.inner {
            WebPDecoderInner::Ours(_) => LoopCount::Finite(1.try_into().unwrap()),
            WebPDecoderInner::OursAnimated(a) => {
                if a.loop_count == 0 {
                    LoopCount::Infinite
                } else {
                    // image-webp 口径：非 0 存值直传（0 才映射 Forever）。
                    LoopCount::Finite(a.loop_count.try_into().unwrap())
                }
            }
        }
    }

    fn into_frames(self) -> Frames<'a> {
        match self.inner {
            // 自研动画：逐帧合成迭代器（背景色/合成语义已在构造时定稿）。
            WebPDecoderInner::OursAnimated(anim) => {
                Frames::new(Box::new(animation::OursFramesIterator::new(anim)))
            }
            // 自研静态：非动画，无帧。
            WebPDecoderInner::Ours(_) => Frames::new(Box::new(std::iter::empty())),
        }
    }
}

/// 自研 VP8L 静态无损解码入口（P2）：家族=StaticLossless 时调用，解析容器并解码。
/// 返回 `Some(ours)` 表示自研接管成功；`None` 表示不可接管（解码失败；调用方转为显式错误，
/// 自包含后无 image-webp 兜底）。
pub(crate) fn decode_static_lossless(
    data: &[u8],
) -> ImageResult<Option<crate::codecs::webp::OursStatic>> {
    let c = match container::parse(data) {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };
    if !c.family.is_static_lossless() {
        return Ok(None);
    }
    let bitstream = match c.bitstream {
        Some(b) => b,
        None => return Ok(None),
    };
    let img = match vp8l::decode(bitstream) {
        Ok(i) => i,
        Err(e) => {
            // 自研解码失败 → Ok(None)（调用方转为显式错误）。
            let _ = e;
            return Ok(None);
        }
    };
    Ok(Some(crate::codecs::webp::OursStatic {
        rgba: img.rgba,
        rgb: Vec::new(),
        width: img.width,
        height: img.height,
        has_alpha: img.has_alpha,
        exif: c.exif.map(|e| e.to_vec()),
        icc: c.icc_profile.map(|e| e.to_vec()),
        xmp: c.xmp.map(|e| e.to_vec()),
        raw_data: None,
    }))
}

/// 自研 VP8L 静态直写调用方 RGBA8 缓冲（`C# DecodeTo` 镜像：暖态零分配；`data` 为完整
/// WEBP 文件，`out` 长度须 == w*h*4）。仅 VP8L 静态族（无损 / 无损-alpha，含不带 alpha 的
/// 无损）且自研可解时接管直写并返回 `Ok(true)`（`out` 已写满 RGBA8）；非可接管家族（VP8
/// 有损等）返回 `Ok(false)`，调用方自回退既有路径；畸形/自研解码失败返回 `Err`。
/// 供复用缓冲零分配解码（kimg & imgbench 探针），不改既有 `WebPDecoder` 默认语义。
pub fn decode_static_lossless_rgba8_into(data: &[u8], out: &mut [u8]) -> ImageResult<bool> {
    let c = match container::parse(data) {
        Ok(c) => c,
        Err(_) => return Ok(false),
    };
    if !c.family.is_static_lossless() {
        return Ok(false);
    }
    let bitstream = match c.bitstream {
        Some(b) => b,
        None => return Ok(false),
    };
    match vp8l::decode_into_rgba8(bitstream, out) {
        Ok(()) => Ok(true),
        Err(e) => Err(ImageError::Decoding(DecodingError::new(
            ImageFormat::WebP.into(),
            e.0,
        ))),
    }
}

/// 自研 VP8 静态直写调用方 RGBA8 缓冲（`C# DecodeTo` 镜像：暖态零分配；`data` 为完整 WEBP
/// 文件，`out` 长度须 == w*h*4）。仅 VP8 静态族（opaque 有损 / 有损-alpha）且自研可解时接管
/// 直写并返回 `Ok(true)`（`out` 已写满 RGBA8——行定稿直出直写，不经整帧 `Vp8Yuv.rgba` 中间
/// 缓冲，消 w*h*4 分配 + 整帧拷贝）；非可接管家族（VP8L 无损 / 动画等）返回 `Ok(false)`，
/// 调用方自回退既有路径；畸形/自研解码失败返回 `Err`。
/// 供 `WebPDecoder::read_image` 的有损静态直写出口使用（镜像
/// [`decode_static_lossless_rgba8_into`] 的直写姿势），不改既有 `decode_static_lossy`
/// /`WebPDecoder` 公共语义。
pub fn decode_static_lossy_rgba8_into(data: &[u8], out: &mut [u8]) -> ImageResult<bool> {
    let c = match container::parse(data) {
        Ok(c) => c,
        Err(_) => return Ok(false),
    };
    if c.family != container::Family::StaticLossy && c.family != container::Family::LossyAlpha {
        return Ok(false);
    }
    let bitstream = match c.bitstream {
        Some(b) => b,
        None => return Ok(false),
    };
    let result = if c.family == container::Family::LossyAlpha {
        // ALPH 反滤波平面先整帧解出（独立于 VP8 解码），随后把 alpha 行并入 VP8 发射内层、
        // 直接写 `out`——与 `decode_static_lossy` 同语义，仅输出目标由内部 rgba 变为调用方
        // 缓冲（alpha 平面 w*h 沿用既有形态，非本次优化对象）。
        let alpha_plane = match c.frames.first().and_then(|f| f.alpha.clone()) {
            Some(range) => match decode_and_unfilter_alpha(&data[range], c.width, c.height) {
                Some(p) => Some(p),
                // ALPH chunk 存在但不可解码：与急切 `decode_static_lossy` 的「保守降级显式错」
                // 语义对齐——拒绝接通直写（`Ok(false)` → read_image 回显式 Decoding 接管失败），
                // 禁用 α=255 静默错像素输出。
                None => return Ok(false),
            },
            None => None,
        };
        vp8::decode_vp8_with_alpha_into(bitstream, c.width, c.height, alpha_plane.as_deref(), out)
    } else {
        vp8::decode_vp8_into(bitstream, c.width, c.height, out)
    };
    match result {
        Ok(()) => Ok(true),
        Err(e) => Err(ImageError::Decoding(DecodingError::new(
            ImageFormat::WebP.into(),
            e.0,
        ))),
    }
}

/// VP8L 静态族「生产直写」预检：解析容器并确认可被 [`decode_static_lossless_rgba8_into`]
/// 直写接管（静态无损 / 无损-alpha）。返回 `(宽, 高, 目标缓冲字节数)`，其中缓冲字节数
/// 沿用既有 `WebPDecoder::total_bytes` 口径（has_alpha → w*h*4；无 alpha → w*h*3，
/// 与 `color_type()` 的 Rgb8 对齐），供调用方先按与现状一致的体积做 `Limits::reserve`。
///
/// 不可接管（VP8 有损 / 动画 / 容器畸形 / 无 VP8L 位流）或尺寸乘法上溢返回 `None`，
/// 调用方回退既有解码路径（错误口径与现状一致，不改变 VP8L 之外任何家族行为）。
/// 供 `ImageReader::decode` / `load_from_memory*` 的 VP8L 直写出口使用：先预检拿尺寸，
/// 再以 [`decode_static_lossless_rgba8_into`] 直写调用方分配的 RGBA8 缓冲（单一全帧写，
/// 消「内部解一次 + read_image 4→3 + to_rgba8 3→4」两次整帧冗余写）。
pub fn lossless_static_size(data: &[u8]) -> Option<(u32, u32, usize)> {
    let c = container::parse(data).ok()?;
    if !c.family.is_static_lossless() {
        return None;
    }
    // 与 `decode_static_lossless_rgba8_into` 同一取位流姿势（`c.bitstream` 为 VP8L 载荷）。
    let bitstream = c.bitstream?;
    let (width, height, has_alpha) = container::read_vp8l_dimensions(bitstream).ok()?;
    let bytes_per_px: usize = if has_alpha { 4 } else { 3 };
    let total = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(bytes_per_px)?;
    Some((width, height, total))
}

/// 未初始化 RGBA8 输出缓冲（跳过 `vec![0u8; n]` 的整帧清零——直写核随后全量覆写 = 纯浪费）。
/// sound：`u8` 无 Drop glue，`decode_into_rgba8` 在返回 Ok 前按「全覆写不变式」写满
/// `out`（对齐快道 `decode_image_into` 顺序全写 + 原地字节交换；未对齐兜底整段拷贝）；
/// 中途 Err 时缓冲由调用方丢弃（回退常规解码路径），无未初始化字节泄漏到图像之外。
pub fn uninit_rgba8_vec(len: usize) -> Vec<u8> {
    simd::raw::uninit_vec_u8(len)
}

/// RGB8 → RGBA8 扩写快道（SSSE3，alpha 恒 255），供 kimg 门面 `DynamicImage::to_rgba8` 的
/// ImageRgb8 分支调用（`simd` 模块为 crate 私有，这里单独重导出一个入口）。
pub use self::simd::rgb8_to_rgba8;

/// 自研 VP8 静态有损解码入口（P3 有损族，V4+ 接管）：家族=StaticLossy/LossyAlpha 时
/// 调用。解析容器 → VP8 YUV → fancy RGBA 直出（单遍）。opaque（StaticLossy，无 alpha）：
/// `decode_vp8`（α=255 熔入发射内层）直出 **RGBA4**（`OursStatic.rgba`，w*h*4，
/// `to_rgba8`/`into_rgba8` 零拷贝，消 RGB3→RGBA4 展开+零值分配税）；
/// LossyAlpha：`decode_vp8_with_alpha` 把 ALPH 反滤波 alpha 行并入发射内层。出错/不可接管
/// 回 `Ok(None)`（调用方转为显式错误，自包含后无 image-webp 兜底）。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn decode_static_lossy(
    data: &[u8],
) -> ImageResult<Option<crate::codecs::webp::OursStatic>> {
    let c = match container::parse(data) {
        Ok(c) => c,
        Err(_) => return Ok(None),
    };
    if c.family != container::Family::StaticLossy && c.family != container::Family::LossyAlpha {
        return Ok(None);
    }
    let bitstream = match c.bitstream {
        Some(b) => b,
        None => return Ok(None),
    };
    if c.family == container::Family::LossyAlpha {
        // 先把整帧 ALPH 反滤波平面解出（独立于 VP8 解码），随后把 alpha 行并入 VP8 发射内层
        // （YUV→RGB 结果 R/G/B 与 alpha 同拍单遍写 RGBA，消灭「RGBA4 全帧发射 + 事后独立
        // 标量逐像素写 alpha」两遍合成）；语义/字节与既有完全一致。
        let alpha_plane = match c.frames.first().and_then(|f| f.alpha.clone()) {
            Some(range) => match decode_and_unfilter_alpha(&data[range], c.width, c.height) {
                Some(plane) => Some(plane),
                // ALPH 解析失败 → 保守降级。
                None => return Ok(None),
            },
            // 无 ALPH chunk（malformed）→ alpha=255，与旧 `yuv_to_rgba_fancy` 发射语义一致。
            None => None,
        };
        let mut yuv = match vp8::decode_vp8_with_alpha(
            bitstream,
            c.width,
            c.height,
            alpha_plane.as_deref(),
        ) {
            Ok(y) => y,
            Err(e) => {
                let _ = e;
                return Ok(None);
            }
        };
        let rgba = vp8::yuv_to_rgba_fancy(&mut yuv);
        return Ok(Some(crate::codecs::webp::OursStatic {
            rgba: vp8l::RgbaBytes::from_vec_u8(rgba),
            rgb: Vec::new(),
            width: c.width,
            height: c.height,
            has_alpha: true,
            exif: c.exif.map(|e| e.to_vec()),
            icc: c.icc_profile.map(|e| e.to_vec()),
            xmp: c.xmp.map(|e| e.to_vec()),
            raw_data: None,
        }));
    }
    // opaque 静态有损：单遍 YUV→RGBA4 直出（α 恒 0xFF 熔入发射内层、无 alpha 平面融合，镜像
    // lossy-alpha 姿势去掉 alpha 行并入）。`decode_vp8`（`alpha=None` → α=255，行定稿直出
    // RGBA4 w*h*4）零重算，`yuv_to_rgba_fancy` 零拷贝取走。合并后 opaque/有损-alpha/无损
    // 统一持 `rgba` 直出：`to_rgba8`/`into_rgba8` 零拷贝移交（消旧 RGB3 直出→`to_rgba8`
    // SIMD 3→4 扩写 + w*h*4 零值分配税，K52 同窗 -9.9% 全链摊）。
    let mut yuv = match vp8::decode_vp8(bitstream, c.width, c.height) {
        Ok(y) => y,
        Err(e) => {
            let _ = e;
            return Ok(None);
        }
    };
    let rgba = vp8::yuv_to_rgba_fancy(&mut yuv);
    Ok(Some(crate::codecs::webp::OursStatic {
        rgba: vp8l::RgbaBytes::from_vec_u8(rgba),
        rgb: Vec::new(),
        width: c.width,
        height: c.height,
        has_alpha: true,
        exif: c.exif.map(|e| e.to_vec()),
        icc: c.icc_profile.map(|e| e.to_vec()),
        xmp: c.xmp.map(|e| e.to_vec()),
        raw_data: None,
    }))
}

/// 解码并反滤波 ALPH chunk（`alpha_chunk[0]` 为头字节）为 alpha 平面（w*h 字节）。
/// 与 C# `WebpDecoder.DecodeAndUnfilterAlpha` 同款（压缩 0/1 + 滤波 0-3 全支持）。
/// 供动画帧（VP8+ALPH）与静态 LossyAlpha 共用。
pub(crate) fn decode_and_unfilter_alpha(alpha_chunk: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    if alpha_chunk.is_empty() {
        return None;
    }
    let header = alpha_chunk[0];
    let compression = header & 3;
    let filter = (header >> 2) & 3;
    let pixel_count = (width as usize) * (height as usize);
    let mut values = vec![0u8; pixel_count];
    match compression {
        0 => {
            if alpha_chunk.len() - 1 != pixel_count {
                return None;
            }
            values.copy_from_slice(&alpha_chunk[1..]);
        }
        1 => {
            values = match vp8l::decode_alpha(&alpha_chunk[1..], width, height) {
                Ok(v) => v,
                Err(_) => return None,
            };
        }
        _ => return None,
    }
    let width = width as usize;
    match filter {
        1 => {
            // 首个像素取上邻，其余取左邻。
            for row in 0..height as usize {
                let off = row * width;
                let above = if row == 0 { 0 } else { values[off - width] };
                values[off] = values[off].wrapping_add(above);
                for col in 1..width {
                    values[off + col] = values[off + col].wrapping_add(values[off + col - 1]);
                }
            }
        }
        2 => {
            // 首行水平铺，其后各像素取上邻。
            for row in 0..height as usize {
                let off = row * width;
                if row == 0 {
                    for col in 1..width {
                        values[off + col] = values[off + col].wrapping_add(values[off + col - 1]);
                    }
                } else {
                    let above_off = off - width;
                    for col in 0..width {
                        values[off + col] = values[off + col].wrapping_add(values[above_off + col]);
                    }
                }
            }
        }
        3 => {
            // 梯度：clamp(左 + 上 - 左上)。
            for row in 0..height as usize {
                let off = row * width;
                if row == 0 {
                    for col in 1..width {
                        values[off + col] = values[off + col].wrapping_add(values[off + col - 1]);
                    }
                } else {
                    let above_off = off - width;
                    let mut above = values[above_off];
                    values[off] = values[off].wrapping_add(above);
                    for col in 1..width {
                        let left = values[off + col - 1];
                        let top = values[above_off + col];
                        let predictor = (i32::from(left) + i32::from(top) - i32::from(above)).clamp(0, 255);
                        values[off + col] = values[off + col].wrapping_add(predictor as u8);
                        above = top;
                    }
                }
            }
        }
        _ => {}
    }
    Some(values)
}

#[cfg(test)]
mod tests {
    // backend 开关单测（不读环境变量路径：直设后断言 resolve）。
    use super::{resolved_backend, set_webp_backend, BACKEND_AUTO, BACKEND_OURS};

    #[test]
    fn backend_setter_and_resolve() {
        set_webp_backend(BACKEND_OURS);
        assert_eq!(resolved_backend(), BACKEND_OURS);
        // 复位 AUTO：环境变量未设时解析回自研核（自包含后无 IMAGEWEBP 后端）。
        set_webp_backend(BACKEND_AUTO);
        let rb = resolved_backend();
        assert!(rb == BACKEND_AUTO || rb == BACKEND_OURS);
    }
}

#[cfg(test)]
mod metadata_tests {
    //! P1 容器元数据对照：自研 `container::parse` vs 自研 `WebPDecoder` 视图在全部 fixture 上
    //! 一致；畸形输入不 panic 不 OOM。放在 crate 内测试（container 为 pub(crate)）。

    use std::io::Cursor;

    use super::container;
    use super::webp_fixtures_support;
    use crate::{AnimationDecoder, ImageDecoder};

    // golden TSV 保留在仓内；图片取外部语料（tests/fixtures/webp/mod.rs 定位）。

    fn all_fixtures() -> Vec<std::path::PathBuf> {
        // 从外部语料目录按名收集（含用户语料 / imgbench-corpus / SimdCodecs 夹具）。
        let mut names = webp_fixtures_support::webp_fixture_names();
        names.sort();
        names
            .into_iter()
            .filter_map(|n| webp_fixtures_support::webp_fixture_path(&n))
            .collect()
    }

    /// kimg 解码器视图；解析失败返回 None（供 SKIP 分流）。
    fn decoder_view(data: &[u8]) -> Option<(u32, u32, bool, bool, u32, bool, bool, bool)> {
        let mut dec = crate::codecs::webp::WebPDecoder::new(Cursor::new(data)).ok()?;
        let (w, h) = dec.dimensions();
        let alpha = dec.color_type() == crate::ColorType::Rgba8;
        let animated = dec.has_animation();
        let loop_count = if animated {
            match dec.loop_count() {
                crate::LoopCount::Infinite => 0,
                crate::LoopCount::Finite(n) => n.get(),
            }
        } else {
            0
        };
        let exif = dec.exif_metadata().ok().flatten().is_some();
        let icc = dec.icc_profile().ok().flatten().is_some();
        let xmp = dec.xmp_metadata().ok().flatten().is_some();
        Some((w, h, alpha, animated, loop_count, exif, icc, xmp))
    }

    #[test]
    fn container_metadata_matches_imagewebp_across_corpus() {
        let mut checked = 0usize;
        let mut skipped = 0usize;
        let mut mismatches = Vec::new();
        for path in all_fixtures() {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let data = std::fs::read(&path).unwrap();
            let c = match container::parse(&data) {
                Ok(c) => c,
                Err(e) => {
                    // 外部语料可能含本实现容器不接的文件 → SKIP 不判失败。
                    eprintln!("SKIP {name} 容器解析失败 {e:?}");
                    skipped += 1;
                    continue;
                }
            };
            let Some((dw, dh, dalpha, danim, dloop, dexif, dicc, dxmp)) =
                decoder_view(&data)
            else {
                eprintln!("SKIP {name} kimg 解码失败");
                skipped += 1;
                continue;
            };
            if (c.width, c.height) != (dw, dh) {
                mismatches.push(format!(
                    "{name}: dims 容器 {}x{} vs kimg {}x{}",
                    c.width, c.height, dw, dh
                ));
            }
            if (c.family == container::Family::Animated) != danim {
                mismatches.push(format!(
                    "{name}: animated 容器 {} vs kimg {danim}",
                    c.family == container::Family::Animated
                ));
            }
            // 动画 has_alpha 是启发式且引擎各异（SimdCodecs 按首帧覆盖/不透明/背景置位推导，
            // 自研解码器按自身帧序）→ 动画仅比对静态元数据；静态严格比对。
            // 例外：StaticLossy（opaque 有损）自 T1 起经 `decode_static_lossy` 恒输出 RGBA8
            // （α=0xFF 直出）——容器层无 alpha 概念（has_alpha=false），与 kimg 的 Rgba8 口径
            // 有意分道（生产消费姿态 to_rgba8/into_rgba8 零拷贝），故不比对 has_alpha。
            let opaque_lossy = c.family == container::Family::StaticLossy;
            if !danim && !opaque_lossy && c.has_alpha != dalpha {
                mismatches.push(format!(
                    "{name}: has_alpha 容器 {} vs kimg {}",
                    c.has_alpha, dalpha
                ));
            }
            if danim {
                let clo = if c.loop_count == 0 { 0 } else { c.loop_count };
                if clo != dloop {
                    mismatches.push(format!(
                        "{name}: loop_count 容器 {clo} vs kimg {dloop}"
                    ));
                }
            }
            let e1 = c.exif.is_some();
            if e1 != dexif {
                mismatches.push(format!("{name}: exif 存在性 容器 {e1} vs kimg {dexif}"));
            }
            if c.icc_profile.is_some() != dicc {
                mismatches.push(format!(
                    "{name}: icc 存在性 容器 {} vs kimg {dicc}",
                    c.icc_profile.is_some()
                ));
            }
            if c.xmp.is_some() != dxmp {
                mismatches.push(format!("{name}: xmp 存在性 容器 {} vs kimg {dxmp}", c.xmp.is_some()));
            }
            checked += 1;
        }
        for m in &mismatches {
            eprintln!("WEBP_CONTAINER_MISMATCH\t{m}");
        }
        assert!(
            mismatches.is_empty(),
            "{} 处容器元数据与 kimg 解码器不一致（见上）",
            mismatches.len()
        );
        eprintln!("WEBP_CONTAINER_CHECKED\t{checked}, skipped={skipped}");
        // 金标准（SimdCodecs 60 张）几乎全覆；外部语料缺失属常态。
        assert!(checked >= 59, "corpus 应几乎全覆（SimdCodecs 60 张 + 额外语料），当前 {checked}");
    }

    #[test]
    fn container_rejects_malformed_without_panic() {
        for c in [
            &[][..],
            b"RIFF".as_slice(),
            b"RIFF\x00\x00\x00\x00WEBP".as_slice(),
            b"RIFF\xff\xff\xff\x0fWEBPVP8L".as_slice(),
            &[0x52, 0x49, 0x46, 0x46, 0x04, 0x00, 0x00, 0x00, b'W', b'E', b'B', b'P', 0xff, 0xff, 0xff, 0xff][..],
        ] {
            let _ = container::parse(c); // 不 panic 即通过
            let _ = crate::codecs::webp::WebPDecoder::new(Cursor::new(c)); // 坏输入显式 Err 不 panic
        }
    }

    #[test]
    fn container_family_classification_on_real_fixtures() {
        let mut counts: std::collections::BTreeMap<&'static str, usize> = Default::default();
        for path in all_fixtures() {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let data = std::fs::read(&path).unwrap();
            let label = match container::family_of(&data) {
                Some(container::Family::StaticLossy) => "static-lossy",
                Some(container::Family::StaticLossless) => "static-lossless",
                Some(container::Family::StaticLosslessAlpha) => "static-lossless-alpha",
                Some(container::Family::LossyAlpha) => "lossy-alpha",
                Some(container::Family::Animated) => "animated",
                None => "NONE",
            };
            *counts.entry(label).or_insert(0) += 1;
            eprintln!("WEBP_CONTAINER_FAMILY\t{name}\t{label}");
        }
        for (k, v) in &counts {
            eprintln!("WEBP_CONTAINER_FAMILY_SUMMARY\t{k}={v}");
        }
        // 8 个动画 fixture 必须全部判为动画（外部语料若含更多动画则只加不减）。
        assert!(
            counts.get("animated").copied().unwrap_or(0) >= 8,
            "动画夹具家族判定失败"
        );
    }
}

/// ALPH 滤波 1/2/3 parity（V6 收口补盲）：corpus 的 7 个有损-alpha fixture 全部 filter=0
/// （含 comp=0/1），滤波 1（水平）/2（垂直）/3（梯度）路径从未被覆盖——此测试以合成
/// VP8X+ALPH+VP8 容器（compression=0）+ 编码侧滤波字节，对自研 / image-webp（dev oracle）
/// 双通道解码逐字节对拍 + alpha 通道与真实 alpha 平面回比（K27：证明性实验落仓可复跑）。
#[cfg(test)]
mod alpha_filter_tests {
    use std::io::Cursor;

    use super::{
        container, decode_and_unfilter_alpha, set_webp_backend, WebPDecoder, BACKEND_AUTO,
        BACKEND_OURS,
    };
    use super::webp_fixtures_support;
    use crate::{ColorType, ImageDecoder};

    fn le24(v: u32) -> [u8; 3] {
        [v as u8, (v >> 8) as u8, (v >> 16) as u8]
    }

    /// libwebp 编码侧滤波（`DoHorizontalFilter_C` / `DoVerticalFilter_C` / `DoGradientFilter_C`）
    /// 的 wrap 减法复刻：`filtered = src - predictor`（模 256）。
    fn filter_encode(src: &[u8], w: usize, filter: u8) -> Vec<u8> {
        let h = src.len() / w;
        let mut out = src.to_vec();
        match filter {
            1 => {
                for c in 1..w {
                    out[c] = src[c].wrapping_sub(src[c - 1]);
                }
                for r in 1..h {
                    let off = r * w;
                    out[off] = src[off].wrapping_sub(src[off - w]);
                    for c in 1..w {
                        out[off + c] = src[off + c].wrapping_sub(src[off + c - 1]);
                    }
                }
            }
            2 => {
                for c in 1..w {
                    out[c] = src[c].wrapping_sub(src[c - 1]);
                }
                for r in 1..h {
                    let off = r * w;
                    for c in 0..w {
                        out[off + c] = src[off + c].wrapping_sub(src[off - w + c]);
                    }
                }
            }
            3 => {
                for c in 1..w {
                    out[c] = src[c].wrapping_sub(src[c - 1]);
                }
                for r in 1..h {
                    let off = r * w;
                    out[off] = src[off].wrapping_sub(src[off - w]);
                    for c in 1..w {
                        let pred =
                            (i32::from(src[off + c - 1]) + i32::from(src[off - w + c])
                                - i32::from(src[off - w + c - 1]))
                            .clamp(0, 255) as u8;
                        out[off + c] = src[off + c].wrapping_sub(pred);
                    }
                }
            }
            _ => {}
        }
        out
    }

    fn build_container(vp8: &[u8], w: u32, h: u32, alpha: &[u8], filter: u8) -> Vec<u8> {
        let mut vp8x = vec![0x10u8, 0, 0, 0]; // alpha flag
        vp8x.extend_from_slice(&le24(w - 1));
        vp8x.extend_from_slice(&le24(h - 1));
        // compression=0 + filter（低 2 位压缩、2-3 位滤波），后接 w*h 原始滤波字节。
        let mut alph = vec![filter << 2];
        alph.extend_from_slice(alpha);
        let mut chunks = Vec::new();
        let mut push_chunk = |tag: &[u8; 4], payload: &[u8], chunks: &mut Vec<u8>| {
            chunks.extend_from_slice(tag);
            chunks.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            chunks.extend_from_slice(payload);
            if payload.len() % 2 == 1 {
                chunks.push(0);
            }
        };
        push_chunk(b"VP8X", &vp8x, &mut chunks);
        push_chunk(b"ALPH", &alph, &mut chunks);
        push_chunk(b"VP8 ", vp8, &mut chunks);
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&((chunks.len() + 4) as u32).to_le_bytes());
        out.extend_from_slice(b"WEBP");
        out.extend_from_slice(&chunks);
        out
    }

    /// 解码为 RGBA：ours=true 走自研 `WebPDecoder`；false 直连 `image_webp::WebPDecoder`
    /// （dev oracle；自研已自包含，运行时零依赖）。
    fn decode_rgba(data: &[u8], ours: bool) -> (Vec<u8>, bool) {
        if ours {
            set_webp_backend(BACKEND_OURS);
            let dec = WebPDecoder::new(Cursor::new(data)).expect("WebPDecoder::new 应成功");
            let has_a = dec.color_type() == ColorType::Rgba8;
            let mut buf = vec![0u8; dec.total_bytes() as usize];
            dec.read_image(&mut buf).expect("read_image 应成功");
            (buf, has_a)
        } else {
            let mut dec = image_webp::WebPDecoder::new(Cursor::new(data))
                .expect("image_webp::new 应成功");
            let has_a = dec.has_alpha();
            let n = dec.output_buffer_size().expect("image_webp 缓冲大小");
            let mut buf = vec![0u8; n];
            dec.read_image(&mut buf).expect("image_webp read_image 应成功");
            (buf, has_a)
        }
    }

    #[test]
    fn lossy_alpha_filters_1_to_3_match_imagewebp() {
        let Some(p) = webp_fixtures_support::webp_fixture_path("lossy-alpha-q50.webp") else {
            eprintln!("SKIP lossy-alpha-q50.webp 外部语料缺失");
            return;
        };
        let data = std::fs::read(p).expect("读取夹具");
        let c = container::parse(&data).expect("容器解析");
        assert_eq!(c.family, container::Family::LossyAlpha);
        let vp8 = c.bitstream.unwrap().to_vec();
        let (w, h) = (c.width, c.height);
        let alpha_range = c.frames[0].alpha.clone().expect("alpha 范围");
        // 参考 alpha 平面：q50 夹具为 comp=1 + filter=0，`decode_and_unfilter_alpha` 直解。
        let true_alpha = decode_and_unfilter_alpha(&data[alpha_range], w, h).expect("参考 alpha 解码");
        assert_eq!(true_alpha.len(), (w as usize) * (h as usize));

        for filter in 1..=3 {
            let filtered = filter_encode(&true_alpha, w as usize, filter);
            let bytes = build_container(&vp8, w, h, &filtered, filter);
            let (ours, a1) = decode_rgba(&bytes, true);
            let (img, a2) = decode_rgba(&bytes, false);
            assert!(a1 && a2, "filter={filter} 双后端均应判为 RGBA8");
            assert_eq!(ours.len(), img.len(), "filter={filter} 缓冲长");
            assert_eq!(ours, img, "filter={filter} OURS vs image-webp 逐字节");
            for (i, &a) in true_alpha.iter().enumerate() {
                assert_eq!(ours[i * 4 + 3], a, "filter={filter} alpha 通道 {i}");
            }
            eprintln!("VP8_ALPHA_FILTER_PARITY_OK\tfilter={filter}\t({w}x{h})");
        }
        set_webp_backend(BACKEND_AUTO);
    }
}

/// opaque 静态有损 RGBA4 直出硬闸（T1）：`OursStatic` 对 opaque 持 `rgba`（w*h*4，α 恒 0xFF）、
/// `rgb` 恒空、`has_alpha=true`；`rgba` 逐字节 ≡ 「旧路径 RGBA4 直出」（`decode_vp8`+
/// `yuv_to_rgba_fancy`）；经公开 `WebPDecoder::read_image` RGBA 目标直拷同字节。RGB 前 3 字节
/// 恒等旧 RGB3 直出路径（`decode_vp8_rgb3` 遗留 dormant，字节契约不变）。
/// 全部 opaque（StaticLossy 非 LossyAlpha）fixture 对拍；缺失外部语料则 SKIP（AGENTS §5）。
#[cfg(test)]
mod opaque_rgba_path_tests {
    use std::io::Cursor;

    use super::webp_fixtures_support;
    use super::{container, decode_static_lossy, set_webp_backend, vp8, WebPDecoder};
    use super::{BACKEND_AUTO, BACKEND_OURS};
    use crate::{ColorType, ImageDecoder};

    /// 旧路径参考：`decode_vp8` + `yuv_to_rgba_fancy`（RGBA4 直出，改动前生产路径字节形态）。
    fn old_path_rgba(payload: &[u8], w: u32, h: u32) -> Vec<u8> {
        let mut yuv = vp8::decode_vp8(payload, w, h).expect("decode_vp8");
        vp8::yuv_to_rgba_fancy(&mut yuv)
    }

    /// 遗留旧 RGB3 直出参考：`decode_vp8_rgb3` + `yuv_to_rgb_fancy`（dormant，T1 前 opaque 形态）。
    fn old_path_rgb3(payload: &[u8], w: u32, h: u32) -> Vec<u8> {
        let mut yuv = vp8::decode_vp8_rgb3(payload, w, h).expect("decode_vp8_rgb3");
        vp8::yuv_to_rgb_fancy(&mut yuv)
    }

    #[test]
    fn opaque_lossy_rgba4_direct_matches_old_paths() {
        let mut checked = 0usize;
        for name in webp_fixtures_support::webp_fixture_names() {
            let Some(p) = webp_fixtures_support::webp_fixture_path(&name) else {
                continue;
            };
            let data = std::fs::read(&p).expect("read fixture");
            let Ok(c) = container::parse(&data) else {
                continue;
            };
            if c.family != container::Family::StaticLossy {
                continue;
            }
            let payload = c.bitstream.unwrap();
            // 新不变式：opaque 应持 `rgba`（RGBA4，w*h*4）+ `rgb` 恒空 + has_alpha（RGBA8 输出）。
            let ours = decode_static_lossy(&data)
                .ok()
                .expect("decode_static_lossy 应 Ok")
                .expect("opaque 有损应被自研接管");
            let n_px = (c.width as usize) * (c.height as usize);
            assert!(ours.has_alpha, "{name}: opaque 输出应为 RGBA8（has_alpha=true）");
            assert!(ours.rgb.is_empty(), "{name}: opaque 不应持 RGB3 缓冲（rgb 恒空）");
            assert_eq!(ours.rgba.len(), n_px * 4, "{name}: rgba 应为 w*h*4");
            // 逐字节：RGBA4 直出 ≡ 旧 RGBA4 路径（含 α=0xFF），且 RGB 前 3 字节 ≡ 旧 RGB3 直出。
            let old_rgba = old_path_rgba(payload, c.width, c.height);
            let old_rgb3 = old_path_rgb3(payload, c.width, c.height);
            assert_eq!(old_rgba.len(), n_px * 4, "{name}: 旧路径缓冲长");
            assert_eq!(old_rgb3.len(), n_px * 3, "{name}: 旧 RGB3 路径缓冲长");
            let ours_rgba = ours.rgba.as_bytes();
            for i in 0..n_px {
                assert_eq!(
                    &ours_rgba[i * 4..i * 4 + 4],
                    &old_rgba[i * 4..i * 4 + 4],
                    "{name} px={i}: RGBA4 直出应等于旧 RGBA 路径"
                );
                assert_eq!(ours_rgba[i * 4 + 3], 0xFF, "{name} px={i}: opaque α 恒 255");
                assert_eq!(
                    &ours_rgba[i * 4..i * 4 + 3],
                    &old_rgb3[i * 3..i * 3 + 3],
                    "{name} px={i}: RGB 前 3 字节应等于旧 RGB3 直出"
                );
            }
            // 经公开 API：color_type=Rgba8、read_image RGBA 目标直写（`WebPDecoder` 有损现走
            // 惰性 `decode_static_lossy_rgba8_into` 直写 `buf`，不经中间 `Vp8Yuv.rgba`）。
            set_webp_backend(BACKEND_OURS);
            let dec = WebPDecoder::new(Cursor::new(&data)).expect("WebPDecoder");
            assert_eq!(dec.color_type(), ColorType::Rgba8, "{name}: opaque 动态分类应 Rgba8");
            let mut buf = vec![0u8; dec.total_bytes() as usize];
            dec.read_image(&mut buf).expect("read_image");
            assert_eq!(buf.len(), n_px * 4, "{name}: read_image 目标应为 w*h*4");
            assert_eq!(buf, ours_rgba, "{name}: read_image 直写应与 eager decode_static_lossy 逐字节一致");
            checked += 1;
        }
        set_webp_backend(BACKEND_AUTO);
        eprintln!("WEBP_OPAQUE_RGBA4_PARITY\t{checked}");
        assert!(
            checked >= 1,
            "应至少对拍 1 个 opaque 有损 fixture（当前 {checked}；缺外部语料）"
        );
    }
}
