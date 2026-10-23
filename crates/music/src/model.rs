//! 音乐播放器数据模型：曲目 / 混排分组 / 播放模式 / 元数据解析与渐变配色语义。

/// 一首曲目。
#[derive(Clone, Debug)]
pub struct Track {
    /// 音频文件绝对/相对路径。
    pub path: std::path::PathBuf,
    /// 显示标题（枚举 `parse_meta`）。
    pub title: String,
    /// 艺术家（未知时 `"未知歌手"`）。
    pub artist: String,
    /// 专辑（Phase A 不解析标签，保持空串）。
    pub album: String,
    /// 时长（秒）；未探测为 0。
    pub duration: f64,
    /// 渐变封面「肤色」主色（由 `artist - title` 稳定哈希派生）。
    pub grad: (u32, u32, u32),
}

/// 混排分组（按艺术家聚合）。
#[derive(Clone, Debug)]
pub struct MixGroup {
    /// 组标题（艺术家名）。
    pub title: String,
    /// 副标题（如「N首」）。
    pub subtitle: String,
    /// 组内曲目的 `library` 下标（首现顺序）。
    pub tracks: Vec<usize>,
    /// CSS 渐变两色（`#rrggbb`，由组标题哈希派生）。
    pub grad: (String, String),
}

/// 播放模式。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RepeatMode {
    /// 不循环（到队尾停）。
    Off,
    /// 列表循环（到队尾回 0）。
    All,
    /// 单曲循环（重播当前）。
    One,
}

/// 解析文件名主干的 `(artist, title)`：按第一个 `" - "` 切分；无分隔 → 未知歌手 + 主干。
/// 修剪空白；title 切分后为空用整体主干。
pub fn parse_meta(stem: &str) -> (String, String) {
    match stem.trim().find(" - ") {
        Some(pos) => {
            let artist = stem[..pos].trim().to_string();
            let title = stem[pos + 3..].trim().to_string();
            let artist = if artist.is_empty() { "未知歌手".to_string() } else { artist };
            let title = if title.is_empty() { stem.trim().to_string() } else { title };
            (artist, title)
        }
        None => ("未知歌手".to_string(), stem.trim().to_string()),
    }
}

/// 时长（秒）→ `"m:ss"`（秒两位补零）。
pub fn duration_str(secs: f64) -> String {
    let secs = if secs.is_finite() && secs > 0.0 { secs } else { 0.0 };
    let total = secs as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

/// 预设渐变主色调色板（对齐 Orbitrum 原型深紫暗调衍生色，主色用于渐变封面底）。
///
/// 每个元素是 `(r, g, b)`；从主色派生的深/浅变体由 [`gradient_pair`] 计算。
const GRAD_PALETTE: [(u32, u32, u32); 8] = [
    (0x7c, 0x5c, 0xfc), // 紫（accent）
    (0xf4, 0x72, 0xb6), // 粉
    (0x34, 0xd3, 0x99), // 绿
    (0xfb, 0x92, 0x3c), // 橙
    (0x60, 0xa5, 0xfa), // 蓝
    (0xa7, 0x8b, 0xfa), // 浅紫
    (0x2d, 0xd4, 0xbf), // 青
    (0xf8, 0x71, 0x71), // 红
];

/// 由 `seed` 稳定哈希出一个主色（FNV-1a 字节哈希 → 调色板取模）；同一 seed 恒同色。
pub fn grad_from_hash(seed: &str) -> (u32, u32, u32) {
    let mut h: u32 = 0x811c_9dc5;
    for b in seed.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    GRAD_PALETTE[(h as usize) % GRAD_PALETTE.len()]
}

fn to_hex(c: (u32, u32, u32)) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
}

/// 由 `seed` 派生 CSS 渐变两色 `(主色, 加深变体)`（`#rrggbb`）。
pub fn gradient_pair(seed: &str) -> (String, String) {
    let (r, g, b) = grad_from_hash(seed);
    let dark = (
        ((r as f32 * 0.55) as u32).min(255),
        ((g as f32 * 0.55) as u32).min(255),
        ((b as f32 * 0.55) as u32).min(255),
    );
    (to_hex((r, g, b)), to_hex(dark))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_meta_artist_dash_title() {
        let (a, t) = parse_meta("周杰伦 - 晴天");
        assert_eq!((a.as_str(), t.as_str()), ("周杰伦", "晴天"));
        let (a, t) = parse_meta(" 林俊杰  -  江南 ");
        assert_eq!((a.as_str(), t.as_str()), ("林俊杰", "江南"));
        // 只按第一个 " - " 切
        let (a, t) = parse_meta("A - B - C");
        assert_eq!((a.as_str(), t.as_str()), ("A", "B - C"));
    }

    #[test]
    fn parse_meta_unknown_artist_and_fallbacks() {
        let (a, t) = parse_meta("只有歌名");
        assert_eq!(a, "未知歌手");
        assert_eq!(t, "只有歌名");
        // 带风格后缀 (Live) 保留在 title
        let (a, t) = parse_meta("陈奕迅 - 十年 (Live)");
        assert_eq!(a, "陈奕迅");
        assert_eq!(t, "十年 (Live)");
        // 空 title / 空 artist 回退
        let (a, t) = parse_meta(" - ");
        assert_eq!(a, "未知歌手");
        assert_eq!(t, "-");
    }

    #[test]
    fn duration_str_formats_mm_ss() {
        assert_eq!(duration_str(0.0), "0:00");
        assert_eq!(duration_str(59.4), "0:59");
        assert_eq!(duration_str(61.0), "1:01");
        assert_eq!(duration_str(125.7), "2:05");
        assert_eq!(duration_str(-3.0), "0:00");
        assert_eq!(duration_str(f64::NAN), "0:00");
    }

    #[test]
    fn grad_hash_stable_and_bounded() {
        let a = grad_from_hash("周杰伦 - 晴天");
        let b = grad_from_hash("周杰伦 - 晴天");
        assert_eq!(a, b, "同 seed 必须稳定同色");
        assert!(a.0 < 256 && a.1 < 256 && a.2 < 256, "颜色分量须在 0..=255");
        // 不同 seed 大概率不同（哈希扩散）
        let c = grad_from_hash("林俊杰 - 江南");
        assert_ne!(grad_from_hash("周杰伦 - 晴天"), c);
        let d = grad_from_hash("周杰伦 - 晴天 ");
        assert_ne!(grad_from_hash("周杰伦 - 晴天"), d, "seed 变化应改变颜色");
    }

    #[test]
    fn gradient_pair_is_hex_and_darkened() {
        let (light, dark) = gradient_pair("周杰伦 - 晴天");
        assert!(light.starts_with('#') && light.len() == 7, "light={light}");
        assert!(dark.starts_with('#') && dark.len() == 7, "dark={dark}");
        assert_ne!(light, dark, "两色应是主色与变体（不同）");
        assert_eq!(gradient_pair("周杰伦 - 晴天"), gradient_pair("周杰伦 - 晴天"));
    }
}
