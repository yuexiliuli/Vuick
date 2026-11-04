/// VP8 解码产物：`rgba` 为「行定稿直出」产物（fancy 上采样 + YUV→RGBA4），由
/// `decode_from_header` 在滤波每宏块行后即时写出并缓存——`yuv_to_rgba_fancy`
/// 取用后置空，避免二次全帧 pass。opaque 有损路径（经 `decode_vp8`，T1 起）与
/// alpha/兼容路径一样填 `rgba`（w*h*4，α=255 熔入发射内层、单遍直出）；`rgb`（RGB3 直出，
/// T1 前 opaque 形态）恒空（dormancy，K24）。`y/u/v` 紧密 4:2:0 平面（环路滤波后）在生产
/// 路径默认为空（直出为输出，紧密平面纯浪费，见 `decode_vp8` 的 `need_tight`），仅供
/// `decode_vp8_tight`（测试/兼容 API）与手工构造回退
/// `yuv_to_rgba_fancy`/`yuv_to_rgb_fancy` 使用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Vp8Yuv {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub rgba: Vec<u8>,
    /// opaque 有损直出 RGB3（w*h*3）；非 RGB3 路径恒空。
    pub rgb: Vec<u8>,
}
