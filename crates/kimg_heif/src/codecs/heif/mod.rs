//! HEIF/HEIC 解码门面（对应 C# `HeifDecoder`/`HeifImageDecoder` 的 Rust 等价物）。
//!
//! Phase-3：容器/参数集/位读取/切片头/帧解码（含 in-loop 滤波）全部可用；`read_info`/`heif_probe`
//! （header-only）真实现；`decode_yuv`（canvas 级 YUV，transform 前）与 `decode_rgba`（含 transform
//! 显示朝向全链 RGBA）真实现；`HeicDecoder` 接入 kimg `ImageDecoder` trait，`read_image` 返回真 RGBA8。
//! transform 属性（clap/irot/imir）按文件关联顺序应用到 RGBA 画布（对齐 C# `HeifCanvas` 合成映射）。

use std::io::{BufRead, Seek};
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

use kimg_core::decoder::ImageDecoder;
use kimg_core::error::{DecodingError, ImageError, ImageResult};
use kimg_core::format::ImageFormat;
use kimg_core::ColorType;

pub mod canvas;
// 并行 grid 合成叶子（方案 A：worker 直贴共享画布；`#![allow(unsafe_code)]` 的 unsafe 下沉点，
// 供 `decode_grid_into` 预检/裸指针散贴，安全接口不外泄）。
pub(super) mod canvas_paste;
pub mod color;
// 色彩转换 AVX2 内核叶子（`#![allow(unsafe_code)]`；color.rs 运行时检出后分派到本模块）。
pub(super) mod color_simd;
// 显示变换 AVX2 内核叶子（`#![allow(unsafe_code)]`；transform.rs 运行时检出后分派到本模块）。
pub(super) mod transform_simd;
pub mod container;
pub mod error;
pub mod hevc;
pub mod isobmff;
pub mod metadata;
pub mod probe;
pub mod stage_clock;
pub mod transform;

pub use error::{HeifError, HeifResult};

/// 从 HEIF 容器读出的元数据（C# `HeifInfo` 等价物）。
#[derive(Debug, Clone)]
pub struct HeifInfo {
    /// 主图像像素宽（transform 之后）。
    pub width: i32,
    /// 主图像像素高。
    pub height: i32,
    /// HEVC 编码采样位深（Main / Main 10 为 8 / 10）。
    pub bit_depth: i32,
    /// 主图像由多少个 grid tile 组成；未平铺为 1。
    pub tile_count: usize,
    /// `colr` box 里的 ICC profile 字节，缺省 None。
    pub icc_profile: Option<Vec<u8>>,
    /// Exif Orientation 值（由 irot/imir 推导；Phase-1 恒 1 = 正常）。
    pub orientation: u8,
}

/// 解码出的 YUV 平面（canvas 级，transform 前；C# `DecodeYuv` 等价物签名）。
#[derive(Debug)]
pub struct YuvPlanes {
    pub y: Vec<u16>,
    pub cb: Vec<u16>,
    pub cr: Vec<u16>,
    pub width: i32,
    pub height: i32,
    pub bit_depth: i32,
}

/// 读容器元数据，不解码任何 HEVC payload；返回主图像尺寸/位深/tile 数等。
pub fn read_info(data: &[u8]) -> HeifResult<HeifInfo> {
    let container = container::HeifContainer::parse(data)?;
    let sps = container.tile_configs[0].parse_sps()?;
    let (w, h) = container.display_size()?;
    Ok(HeifInfo {
        width: w,
        height: h,
        bit_depth: sps.bit_depth_y,
        tile_count: container.tiles.len(),
        icc_profile: container.icc_profile.clone(),
        orientation: 1,
    })
}

/// 把容器里每个画面 item（grid 行主序）解码为 [`canvas::TileYuv`]（含 in-loop 滤波，
/// 对应 C# `DecodeCodedImages`；`decode_item_flags` 全滤波口径 = `_t0.yuv` oracle）。
fn decode_tiles(data: &[u8], container: &container::HeifContainer<'_>) -> HeifResult<Vec<canvas::TileYuv>> {
    if container.tiles.is_empty() {
        return Err(HeifError::invalid("容器没有可解码的图像 item。"));
    }
    let mut tiles = Vec::with_capacity(container.tiles.len());
    for i in 0..container.tiles.len() {
        let config = &container.tile_configs[i];
        let item = &container.items.items[container.tiles[i]];
        let length = isobmff::item_container::ItemContainer::get_item_length(item);
        // 先校验 payload 不超出文件再分配（镜像 C# `ResolvePayloads` 的 `length > data.Length` 拒绝）。
        if length < 0 || length > data.len() as i64 {
            return Err(HeifError::invalid("图像 item 大于文件。"));
        }
        let mut payload = vec![0u8; length as usize];
        stage_clock::parse(|| container.items.resolve(item, &mut payload))?;
        let pic = hevc::decode_item_flags(config, &payload, false, false)?;
        // D2a：移动 HevcPicture 的三平面进 TileYuv（picture 至此不再被消费），免整图量 clone。
        let [y, cb, cr] = pic.planes;
        tiles.push(canvas::TileYuv {
            y,
            cb,
            cr,
            y_w: pic.width,
            y_h: pic.height,
            c_w: pic.chroma_width,
            c_h: pic.chroma_height,
            crop_left: pic.crop_left,
            crop_right: pic.crop_right,
            crop_top: pic.crop_top,
            crop_bottom: pic.crop_bottom,
        });
    }
    Ok(tiles)
}

/// 并行解码 grid 的全部 tile，与 [`decode_tiles`] 逐字节恒等：
/// - tiles<=1 直接落既有 [`decode_tiles`]（避免线程开销）；tiles>=2 用 `std::thread::scope`
///   派有限 worker（`available_parallelism`，且不超过 tile 数），`AtomicUsize` 索引轮转 +
///   `mpsc` 回传 `(idx, HeifResult<TileYuv>)`，主线程按 index 归位。
/// - 线程安全：`HevcFrameDecoder` 每 tile 独立 new、字段全 owned（无 Rc/RefCell/static）；
///   `Rc<Config>` 本身非 Send，不送进线程——串行预 resolve 全部 payload、预把 `tile_configs`
///   逐 `&**rc` deref 成 `Vec<&HevcDecoderConfiguration>`，workers 只捕获借用。
/// - 错误语义与串行一致：先串行预检 payload 长度（防分配前越界），解码后按 index 顺序
///   返回第一个 Err；worker 解码失败不 panic、经 Result 回传。
/// - 内存增量：串行预 resolve 全量 payload 驻留（累计 ≈ 输入文件大小）+ `results`
///   全量 tile 平面驻留（各 tile 解码完即归位，过程占用与串行等价，平面总和 ==
///   画布平面 + grid 边缘 padding）——超大 grid（如 192-tile）相对输出画布量级增量有界。
/// - 错误优先级微差（已接受，有效 grid 不可观测）：病态输入下 payload 长度预检发生在
///   任何解码之前，并行路径可能先报**后 tile** 的长度/解析错，而串行先报**前 tile** 的
///   解码错；两者都只回传一个 Err，正常输入两者一致。
///
/// 串行全链强制开关（dev 测量工具，仿 intra_pred.rs K93 姿势）：`KIMG_HEIF_PARALLEL=0` →
/// [`decode_tiles_par`] 直接落串行 [`decode_tiles`]，用于「专攻单核」战役的单核基线测量；
/// 默认空/AUTO → 并行（行为不变）。一次读取缓存到 `OnceLock`，不进每 tile 开销。
/// 字节恒等：串行路径即原 [`decode_tiles`]，0 失配必然。
static FORCE_SERIAL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn force_serial() -> bool {
    *FORCE_SERIAL.get_or_init(|| {
        std::env::var("KIMG_HEIF_PARALLEL")
            .map(|v| v == "0")
            .unwrap_or(false)
    })
}

fn decode_tiles_par(
    data: &[u8],
    container: &container::HeifContainer<'_>,
) -> HeifResult<Vec<canvas::TileYuv>> {
    if container.tiles.is_empty() {
        return Err(HeifError::invalid("容器没有可解码的图像 item。"));
    }
    // dev 测量开关：KIMG_HEIF_PARALLEL=0 → 强制单核（含 tiles>=2 也走串行）。
    if force_serial() {
        return decode_tiles(data, container);
    }
    let n = container.tiles.len();
    if n <= 1 {
        return decode_tiles(data, container);
    }
    // 串行预 resolve 全部 payload（含与串行 :83-86 同款长度预检，防分配前越界）。
    let mut payloads: Vec<Vec<u8>> = Vec::with_capacity(n);
    for i in 0..n {
        let item = &container.items.items[container.tiles[i]];
        let length = isobmff::item_container::ItemContainer::get_item_length(item);
        if length < 0 || length > data.len() as i64 {
            return Err(HeifError::invalid("图像 item 大于文件。"));
        }
        let mut payload = vec![0u8; length as usize];
        stage_clock::parse(|| container.items.resolve(item, &mut payload))?;
        payloads.push(payload);
    }
    // `Rc<Config>` 非 Send，deref 成 `&Config`（Send+Sync）交给 worker 共享。
    let configs: Vec<&hevc::HevcDecoderConfiguration> =
        container.tile_configs.iter().map(|rc| &**rc).collect();

    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<(usize, HeifResult<canvas::TileYuv>)>();
    let worker_count = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(1)
        .min(n);
    let mut results: Vec<Option<HeifResult<canvas::TileYuv>>> = Vec::with_capacity(n);
    results.resize_with(n, || None);

    // 共享借用进 scope（& 是 Copy，每个 worker 复制一份；scope 保证全部 join 后才返回）。
    let next_ref = &next;
    let configs_ref = &configs;
    let payloads_ref = &payloads;
    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            let tx = tx.clone();
            scope.spawn(move || loop {
                let i = next_ref.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let tile = hevc::decode_item_flags(configs_ref[i], &payloads_ref[i], false, false)
                    .map(|pic| {
                        // D2a：移动 planes（worker 独占 pic，至此不再消费），免整图量 clone。
                        let [y, cb, cr] = pic.planes;
                        canvas::TileYuv {
                            y,
                            cb,
                            cr,
                            y_w: pic.width,
                            y_h: pic.height,
                            c_w: pic.chroma_width,
                            c_h: pic.chroma_height,
                            crop_left: pic.crop_left,
                            crop_right: pic.crop_right,
                            crop_top: pic.crop_top,
                            crop_bottom: pic.crop_bottom,
                        }
                    });
                let _ = tx.send((i, tile));
            });
        }
        drop(tx);
        // 收到全部回传：所有 sender 在 worker 收工后 drop，rx 迭代自然结束。
        for (i, tile) in rx.iter() {
            results[i] = Some(tile);
        }
    });

    // 按 index 顺序返回第一个 Err（与串行一致）。
    let mut tiles = Vec::with_capacity(n);
    for r in results {
        tiles.push(r.unwrap()?);
    }
    Ok(tiles)
}

/// grid compose 并行化（方案 A：worker 解完自己 tile 即直贴共享画布，只回传 ack）。
///
/// - 布局预检（`canvas_paste::GridLayout::build`，SPS 级零解码）：均匀栅格 + 亮度/色度逐平面
///   精确铺满画布 → `Some`；单 tile / `KIMG_HEIF_PARALLEL=0` 强制串行 / 布局不整 → `None`，
///   调用方落回既有 `decode_tiles_par + compose_grid_owned`（不崩、不改语义）。
/// - 成功路径：spawn 前以未初始化三平面共享画布（[`canvas_paste::allocate_planes`]），
///   30+MB 级清零并入各 worker 的「贴满即写」、不再占主线程；worker 解码后
///   [`canvas_paste::paste_tile`] 直贴**不相交**矩形（字节恒等：行事时序与 `compose_grid`
///   共用 `canvas::tile_plane_copies`），只回传 `(idx, HeifResult<()>)`；scope join 即栅栏；
///   主线程按 index 扫首 Err，错误语义与串行一致（首错即返）。
/// - 预检不过 / 任何 worker 出错 → 画布随 Err 返回被 drop，未初始化字节不外泄。
fn decode_grid_into(
    data: &[u8],
    container: &container::HeifContainer<'_>,
) -> HeifResult<Option<canvas::ComposedYuv>> {
    let n = container.tiles.len();
    if force_serial() || n <= 1 {
        return Ok(None);
    }
    let cols = container.grid_columns;
    let rows = container.grid_rows;
    let canvas_w = container.canvas_width;
    let canvas_h = container.canvas_height;

    // 布局预检：各 tile 输出尺寸取自 hvcC SPS（零解码；容器已保证一致，此处独立复核防漂移）。
    // 共享同一 Rc 的 tile 只 parse 一次。
    let mut infos: Vec<canvas_paste::TileInfo> = Vec::with_capacity(n);
    {
        let mut parsed: Option<(Rc<hevc::HevcDecoderConfiguration>, hevc::parameter_sets::HevcSps)> = None;
        for config in &container.tile_configs {
            let sps = match &parsed {
                Some((c, s)) if Rc::ptr_eq(c, config) => s.clone(),
                _ => match config.parse_sps() {
                    // SPS 解析失败 → 保守落回既有路径（其结果与现有 decode 报错一致）。
                    Err(_) => return Ok(None),
                    Ok(s) => {
                        parsed = Some((config.clone(), s.clone()));
                        s
                    }
                },
            };
            infos.push(canvas_paste::TileInfo {
                out_w: sps.width - sps.conf_left - sps.conf_right,
                out_h: sps.height - sps.conf_top - sps.conf_bottom,
            });
        }
    }
    let layout = match canvas_paste::GridLayout::build(&infos, cols, rows, canvas_w, canvas_h) {
        Some(l) => l,
        None => return Ok(None),
    };

    // 串行预 resolve 全部 payload（与 decode_tiles_par :159-170 同款长度预检，防分配前越界）。
    let mut payloads: Vec<Vec<u8>> = Vec::with_capacity(n);
    for i in 0..n {
        let item = &container.items.items[container.tiles[i]];
        let length = isobmff::item_container::ItemContainer::get_item_length(item);
        if length < 0 || length > data.len() as i64 {
            return Err(HeifError::invalid("图像 item 大于文件。"));
        }
        let mut payload = vec![0u8; length as usize];
        stage_clock::parse(|| container.items.resolve(item, &mut payload))?;
        payloads.push(payload);
    }
    let configs: Vec<&hevc::HevcDecoderConfiguration> =
        container.tile_configs.iter().map(|rc| &**rc).collect();

    // 三平面共享画布：未初始化，每元素由散贴恰好写一次（清零随各 worker 的贴满分摊，不在主线程）。
    let (mut y, mut cb, mut cr) =
        canvas_paste::allocate_planes(layout.y_len, layout.c_len);
    let cptrs = canvas_paste::canvas_ptrs(&mut y, &mut cb, &mut cr);

    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<(usize, HeifResult<()>)>();
    let worker_count = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(1)
        .min(n);
    let mut results: Vec<Option<HeifResult<()>>> = Vec::with_capacity(n);
    results.resize_with(n, || None);

    // 共享借用进 scope（& 是 Copy；scope 保证全部 join 后才返回；句柄只写互不相交矩形）。
    let next_ref = &next;
    let configs_ref = &configs;
    let payloads_ref = &payloads;
    let layout_ref = &layout;
    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            let tx = tx.clone();
            scope.spawn(move || loop {
                let i = next_ref.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let res = (|| -> HeifResult<()> {
                    let pic =
                        hevc::decode_item_flags(configs_ref[i], &payloads_ref[i], false, false)?;
                    // D2a：移动 planes（worker 独占 pic，至此不再消费），免整图量 clone。
                    let [y, cb, cr] = pic.planes;
                    let tile = canvas::TileYuv {
                        y,
                        cb,
                        cr,
                        y_w: pic.width,
                        y_h: pic.height,
                        c_w: pic.chroma_width,
                        c_h: pic.chroma_height,
                        crop_left: pic.crop_left,
                        crop_right: pic.crop_right,
                        crop_top: pic.crop_top,
                        crop_bottom: pic.crop_bottom,
                    };
                    canvas_paste::paste_tile(layout_ref, i, &tile, cptrs)
                })();
                let _ = tx.send((i, res));
            });
        }
        drop(tx);
        // 收到全部回传：所有 sender 在 worker 收工后 drop，rx 迭代自然结束。
        for (i, r) in rx.iter() {
            results[i] = Some(r);
        }
    });

    // 按 index 顺序返回第一个 Err（与串行一致）；全部 Ok → 共享画布即合成结果。
    for r in results {
        match r.unwrap_or_else(|| Err(HeifError::invalid("并行 worker 未回传 ack。"))) {
            Ok(()) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(Some(canvas::ComposedYuv {
        y,
        cb,
        cr,
        width: canvas_w,
        height: canvas_h,
    }))
}
/// conformance window 输出按 grid 位置贴到画布并裁剪到 grid 输出尺寸。transform 属相（clap/irot/
/// imir）不影响此步——它退回**变换前画布**（IMG_* 竖图返回横向画布，与 C# oracle `<md5>.yuv`
/// 逐字节一致，供 oracle 对照）。
pub fn decode_yuv(data: &[u8]) -> HeifResult<YuvPlanes> {
    let container = container::HeifContainer::parse(data)?;
    if container.tiles.is_empty() {
        return Err(HeifError::invalid("容器没有可解码的图像 item。"));
    }
    let sps = container.tile_configs[0].parse_sps()?;
    if sps.bit_depth_y != sps.bit_depth_c {
        return Err(HeifError::invalid("亮度与色度位深不同不受支持。"));
    }
    let tiles = decode_tiles(data, &container)?;
    let composed = canvas::compose_grid_owned(
        tiles,
        container.grid_columns,
        container.grid_rows,
        container.canvas_width,
        container.canvas_height,
    )?;
    Ok(YuvPlanes {
        y: composed.y,
        cb: composed.cb,
        cr: composed.cr,
        width: composed.width,
        height: composed.height,
        bit_depth: sps.bit_depth_y,
    })
}

/// 与 [`decode_yuv`] 语义相同的并行 tile 解码版本（逐行对照 :111-136）：唯一差别是 tile 解码
/// 走 [`decode_tiles_par`]（tiles>=2 并行、tiles<=1 自然落串行），grid 合成/返回口径完全相同。
/// 供产品主通道 [`decode_rgba`] 与「串≡并」parity 测试使用。
pub fn decode_yuv_par(data: &[u8]) -> HeifResult<YuvPlanes> {
    let container = container::HeifContainer::parse(data)?;
    if container.tiles.is_empty() {
        return Err(HeifError::invalid("容器没有可解码的图像 item。"));
    }
    let sps = container.tile_configs[0].parse_sps()?;
    if sps.bit_depth_y != sps.bit_depth_c {
        return Err(HeifError::invalid("亮度与色度位深不同不受支持。"));
    }
    let composed = match decode_grid_into(data, &container)? {
        Some(c) => c,
        None => {
            let tiles = decode_tiles_par(data, &container)?;
            canvas::compose_grid_owned(
                tiles,
                container.grid_columns,
                container.grid_rows,
                container.canvas_width,
                container.canvas_height,
            )?
        }
    };
    Ok(YuvPlanes {
        y: composed.y,
        cb: composed.cb,
        cr: composed.cr,
        width: composed.width,
        height: composed.height,
        bit_depth: sps.bit_depth_y,
    })
}
/// grid 图像（多 tile）不支持，返回错误。供单 tile oracle 对照与新解码调试使用。
pub fn decode_yuv_bytes(data: &[u8]) -> HeifResult<YuvPlanes> {
    let container = container::HeifContainer::parse(data)?;
    if container.tiles.len() != 1 {
        return Err(HeifError::invalid("decode_yuv_bytes 只支持单 tile 图像。"));
    }
    let config = container.tile_configs[0].clone();
    let items = &container.items;
    let item = &items.items[container.tiles[0]];
    let length = isobmff::item_container::ItemContainer::get_item_length(item);
    // 先校验 payload 不超出文件再分配（镜像 C# Tiles.Extract 的 `length > data.Length` 拒绝）
    if length < 0 || length > data.len() as i64 {
        return Err(HeifError::invalid("图像 item 大于文件。"));
    }
    let mut payload = vec![0u8; length as usize];
    items.resolve(item, &mut payload)?;
    let pic = hevc::decode_item(&config, &payload)?;
    Ok(YuvPlanes {
        y: pic.planes[0].clone(),
        cb: pic.planes[1].clone(),
        cr: pic.planes[2].clone(),
        width: pic.width,
        height: pic.height,
        bit_depth: pic.bit_depth_y,
    })
}

/// 解码显示朝向的 RGBA。**真实现**：容器解析 → 逐 tile 解码（`decode_item_flags` 全滤波）→
/// `compose_grid` 合成整图 YUV → 色转（矩阵/full_range 取自容器 `nclx`，否则 SPS VUI；8/10 位都走，
/// 产出 RGBA8）。无 transform 时 `color::yuv_to_rgba_into` 直写 `destination`（免中间缓冲）；
/// 有 transform 时经 `color::yuv_to_rgba` 出整图 rgba 再按关联顺序 `transform::apply_rgba`
/// （clap/irot/imir，对准 C# `HeifCanvas` 合成映射）写 `destination`。返回 `(宽, 高)` =
/// 变换后显示尺寸（与 `ReadInfo().DisplaySize` 一致）。
pub fn decode_rgba(data: &[u8], destination: &mut [u8]) -> HeifResult<(i32, i32)> {
    let container = stage_clock::parse(|| container::HeifContainer::parse(data))?;
    if container.tiles.is_empty() {
        return Err(HeifError::invalid("容器没有可解码的图像 item。"));
    }
    let sps = container.tile_configs[0].parse_sps()?;
    if sps.bit_depth_y != sps.bit_depth_c {
        return Err(HeifError::invalid("亮度与色度位深不同不受支持。"));
    }
    let (matrix, full_range) = match container.nclx_colour {
        Some(n) => (n.matrix_coefficients, n.full_range),
        None => (sps.matrix_coefficients, sps.video_full_range),
    };
    let (cw, ch) = (container.canvas_width, container.canvas_height);
    if cw <= 0 || ch <= 0 {
        return Err(HeifError::invalid("画布尺寸非法。"));
    }
    let map = stage_clock::transform(|| transform::compose(cw, ch, &container.transforms))?;
    let (ow, oh) = (map.width, map.height);
    if ow <= 0 || oh <= 0 {
        return Err(HeifError::invalid("显示尺寸非法。"));
    }
    // 尺寸上限护栏：输出像素 × 4 字节不溢出（容器已限画布 ≤ 1<<28 像素；clap 只减 irot/imir 不增）。
    let need = (ow as usize)
        .checked_mul(oh as usize)
        .and_then(|p| p.checked_mul(4))
        .ok_or_else(|| HeifError::invalid("显示尺寸溢出。"))?;
    if destination.len() < need {
        return Err(HeifError::invalid(format!(
            "目标缓冲不足：需要 {need} 字节，实际 {} 字节。",
            destination.len()
        )));
    }
    let composed = match stage_clock::grid(|| decode_grid_into(data, &container))? {
        Some(c) => c,
        None => {
            let tiles = decode_tiles_par(data, &container)?;
            stage_clock::grid(|| {
                canvas::compose_grid_owned(tiles, container.grid_columns, container.grid_rows, cw, ch)
            })?
        }
    };
    // 色彩转换行带并行：tiles>=2 且非强制单核时，`yuv_to_rgba_*_par` 按输出行带
    // `std::thread::scope` 均分给 worker（每 worker 连续行带、dest split_at_mut 独立段，
    // 逐字节恒等）；`KIMG_HEIF_PARALLEL=0`（force_serial）或单 tile（color 量级小，
    // 不值得线程开销）→ 落[tiles 与 color 都]单线程整图，与既有串行路径逐字节恒等。
    // par 函数内另有行数/核数兜底门控，双重保证单核语义不受污染。
    let color_par = !force_serial() && container.tiles.len() >= 2;
    if container.transforms.is_empty() {
        // 无 transform：直写 destination（消除中间 out 分配与一次整图 copy_from_slice，
        // 与 yuv_to_rgba 路径逐字节恒等；identity 映射下 need == cw*ch*4，dest[..need]
        // 长度恰为整图 RGBA8，直写逐字节覆盖，无旧值残留）。
        stage_clock::color(|| {
            if color_par {
                color::yuv_to_rgba_into_par(
                    &composed.y,
                    &composed.cb,
                    &composed.cr,
                    cw as usize,
                    ch as usize,
                    sps.bit_depth_y as u8,
                    matrix,
                    full_range,
                    &mut destination[..need],
                )
            } else {
                color::yuv_to_rgba_into(
                    &composed.y,
                    &composed.cb,
                    &composed.cr,
                    cw as usize,
                    ch as usize,
                    sps.bit_depth_y as u8,
                    matrix,
                    full_range,
                    &mut destination[..need],
                )
            }
        })?;
    } else {
        // 有 transform：transform::apply_rgba 需要整图 rgba 作源（画布 → 显示朝向映射），
        // color 段同样行带并行填 rgba，其余保持原路径。
        let rgba = stage_clock::color(|| {
            if color_par {
                color::yuv_to_rgba_par(
                    &composed.y,
                    &composed.cb,
                    &composed.cr,
                    cw as usize,
                    ch as usize,
                    sps.bit_depth_y as u8,
                    matrix,
                    full_range,
                )
            } else {
                color::yuv_to_rgba(
                    &composed.y,
                    &composed.cb,
                    &composed.cr,
                    cw as usize,
                    ch as usize,
                    sps.bit_depth_y as u8,
                    matrix,
                    full_range,
                )
            }
        })?;
        stage_clock::transform(|| {
            transform::apply_rgba_par(&rgba, cw, ch, &map, &mut destination[..need], container.tiles.len())
        })?;
    }
    Ok((ow, oh))
}

/// HEIF 解码器（接入 kimg `ImageDecoder` trait）：new 时读全量字节 → 容器解析拿尺寸/位深；
/// `read_image` 真解码 RGBA8（对齐 C# `HeifDecoder.Decode` 输出 = `PixelFormat.Rgba32`）。
pub struct HeicDecoder<R> {
    width: u32,
    height: u32,
    color_type: ColorType,
    icc_profile: Option<Vec<u8>>,
    // 保存输入字节（供 read_image 解码）。
    _data: Option<Vec<u8>>,
    _r: PhantomData<R>,
}

impl<R: BufRead + Seek> HeicDecoder<R> {
    pub fn new(r: R) -> ImageResult<Self> {
        let mut data = Vec::new();
        {
            let mut br = r;
            br.read_to_end(&mut data).map_err(ImageError::IoError)?;
        }
        let info = read_info(&data).map_err(|e| {
            ImageError::Decoding(DecodingError::new(ImageFormat::Heic.into(), e))
        })?;
        Ok(HeicDecoder {
            width: info.width as u32,
            height: info.height as u32,
            // 8/10 位都出 RGBA8（`decode_rgba` 经 `yuv_to_rgba` 把 10 位采样直接映射到 8 位输出，
            // 与 C# `HeifDecoder.Decode` 的 `Rgba32` 口径一致）。
            color_type: ColorType::Rgba8,
            icc_profile: info.icc_profile,
            _data: Some(data),
            _r: PhantomData,
        })
    }
}

impl<R: BufRead + Seek> ImageDecoder for HeicDecoder<R> {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn color_type(&self) -> ColorType {
        self.color_type
    }

    fn icc_profile(&mut self) -> ImageResult<Option<Vec<u8>>> {
        Ok(self.icc_profile.clone())
    }

    fn read_image(self, buf: &mut [u8]) -> ImageResult<()> {
        let data = self._data.as_deref().ok_or_else(|| {
            ImageError::Decoding(DecodingError::new(
                ImageFormat::Heic.into(),
                "HEIF 输入数据缺失。",
            ))
        })?;
        decode_rgba(data, buf).map_err(|e| {
            ImageError::Decoding(DecodingError::new(ImageFormat::Heic.into(), e))
        })?;
        Ok(())
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 门禁：grid 并行贴入分支必须对典型 48-tile 手机照**真被选中**（Some），防预检被改严后
    /// 静默回退到串行 compose 而丢性能（评审 NIT；oracle 全绿不覆盖此通道）。
    #[test]
    fn grid_parallel_branch_is_taken_for_photo_grid() {
        let corpus =
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../Benchmarks/corpus/heif/IMG_0083.HEIC");
        let data = match std::fs::read(corpus) {
            Ok(d) => d,
            Err(e) => {
                println!("SKIP: missing {corpus} ({e})");
                return;
            }
        };
        let container = container::HeifContainer::parse(&data).expect("容器应可解析");
        let out = decode_grid_into(&data, &container).expect("并行贴入不应 Err");
        assert!(out.is_some(), "IMG_0083 48-tile 均匀 grid 应命中并行贴入分支");
    }
}

