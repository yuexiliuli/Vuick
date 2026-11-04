use super::*;

/// 完整解码：解析帧头 + 重建全部宏块 → 紧密 YUV 4:2:0（V2 暂不含环路滤波，V3 接入）。
/// 生产路径（`need_tight=false`）跳过紧密 y/u/v 抽平面与分配——输出以 `Vp8Yuv.rgba`（行定稿
/// 直出 RGBA4，α=255）为准，`y/u/v` 为空 Vec；测试/兼容 API 用 [`decode_vp8_tight`] 取紧密平面。
pub(crate) fn decode_vp8(frame: &[u8], width: u32, height: u32) -> Result<Vp8Yuv> {
    let fh = stage_clock::header(|| parse_frame_header(frame))?;
    if fh.width != width || fh.height != height {
        return Err(Vp8Error::new("VP8 与容器尺寸不一致"));
    }
    decode_from_header(fh, false, 4, None, None)
}

/// lossy-alpha 专用：同 [`decode_vp8`]（`need_tight=false`），但行定稿直出时把 `alpha` 平面
/// （w*h 字节，`decode_and_unfilter_alpha` 的整帧结果）逐行并入 RGBA 发射内层——每像素 4 字节
/// 一次写好（YUV→RGB 结果 R/G/B 与 alpha 同拍），消灭「RGBA4 全帧发射 + 事后独立标量逐像素
/// 写 alpha」两遍合成。`alpha=None`（无 ALPH chunk 的 malformed 输入）时 α 恒 255，与
/// `decode_vp8` 逐字节一致。产物落 `Vp8Yuv.rgba`。
/// lossy-alpha 专用直写入口见 [`decode_vp8_with_alpha_into`]；本急切收缓冲版本保留供测试/
/// 参考（K24 dormancy，生产热路径已直写）。
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn decode_vp8_with_alpha(
    frame: &[u8],
    width: u32,
    height: u32,
    alpha: Option<&[u8]>,
) -> Result<Vp8Yuv> {
    let fh = stage_clock::header(|| parse_frame_header(frame))?;
    if fh.width != width || fh.height != height {
        return Err(Vp8Error::new("VP8 与容器尺寸不一致"));
    }
    if let Some(a) = alpha {
        debug_assert_eq!(a.len(), (width as usize) * (height as usize));
    }
    decode_from_header(fh, false, 4, alpha, None)
}

/// 直写有损解码（opaque）：同 [`decode_vp8`]（`need_tight=false`，α=255 熔入发射内层）但行定稿
/// 直出直接写调用方 `out`（RGBA4），不经内部整帧 `Vp8Yuv.rgba`——消 w*h*4 中间分配 + 事后整帧
/// 拷贝。`out` 长度须为 w*h*4。字节恒等：与 [`decode_vp8`] 同一 `decode_from_header` 发射路径，
/// 仅「输出写入目标」不同。供 `decode_static_lossy_rgba8_into` 直写热路径使用。
pub(crate) fn decode_vp8_into(frame: &[u8], width: u32, height: u32, out: &mut [u8]) -> Result<()> {
    let fh = stage_clock::header(|| parse_frame_header(frame))?;
    if fh.width != width || fh.height != height {
        return Err(Vp8Error::new("VP8 与容器尺寸不一致"));
    }
    decode_from_header(fh, false, 4, None, Some(out))?;
    Ok(())
}

/// 直写有损解码（lossy-alpha）：同 [`decode_vp8_with_alpha`] 但行定稿直出直接写调用方 `out`
/// （alpha 行并入发射内层，同 [`decode_vp8_into`] 的直写姿势）。`out` 长度须为 w*h*4；字节
/// 恒等 = 与 [`decode_vp8_with_alpha`] 同一 `decode_from_header` 发射路径。供
/// `decode_static_lossy_rgba8_into` 直写热路径使用。
pub(crate) fn decode_vp8_with_alpha_into(
    frame: &[u8],
    width: u32,
    height: u32,
    alpha: Option<&[u8]>,
    out: &mut [u8],
) -> Result<()> {
    let fh = stage_clock::header(|| parse_frame_header(frame))?;
    if fh.width != width || fh.height != height {
        return Err(Vp8Error::new("VP8 与容器尺寸不一致"));
    }
    if let Some(a) = alpha {
        debug_assert_eq!(a.len(), (width as usize) * (height as usize));
    }
    decode_from_header(fh, false, 4, alpha, Some(out))?;
    Ok(())
}

/// 【dormancy，K24】opaque 有损 RGB3 直出旧入口：同 [`decode_vp8`]（`need_tight=false`）且行定稿
/// 直出 **RGB3**（w*h*3，alpha 不写），产物落 `Vp8Yuv.rgb`。自 T1 起 opaque 有损改用
/// [`decode_vp8`]（RGBA4 α=255 单遍直出，`to_rgba8`/`into_rgba8` 零拷贝），本入口与
/// `emit_rows_into_rgb3`/`yuv_to_rgb_fancy` 保留作旧口径参考（`ours_backend_webpdecoder`/
/// 硬闸对拍）+ 手工构造回退，不删（K24）。每像素前 3 字节仍与 `decode_vp8` 的 RGBA 恒等。
#[allow(dead_code)]
pub(crate) fn decode_vp8_rgb3(frame: &[u8], width: u32, height: u32) -> Result<Vp8Yuv> {
    let fh = stage_clock::header(|| parse_frame_header(frame))?;
    if fh.width != width || fh.height != height {
        return Err(Vp8Error::new("VP8 与容器尺寸不一致"));
    }
    decode_from_header(fh, false, 3, None, None)
}

/// 测试/兼容路径：同 [`decode_vp8`] 且一并抽紧密 y/u/v 平面（V2/V3 YUV golden 对拍用）。
#[cfg(test)]
pub(crate) fn decode_vp8_tight(frame: &[u8], width: u32, height: u32) -> Result<Vp8Yuv> {
    let fh = stage_clock::header(|| parse_frame_header(frame))?;
    if fh.width != width || fh.height != height {
        return Err(Vp8Error::new("VP8 与容器尺寸不一致"));
    }
    decode_from_header(fh, true, 4, None, None)
}




/// 未初始化 `Vec<u8>`：调用方必须保证返回后 `len` 字节在被读取前全部写入（见各调用点
/// Safety 注释）。省去 `vec![0u8; len]` 的全量 memset——大缓冲（RGBA 输出/紧密平面）零化
/// 在主线程关键路径上（P-B 直出已覆盖全部字节），逐字节恒等。
#[inline]
pub(crate) unsafe fn uninit_vec_u8(len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    v.set_len(len);
    v
}
