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
    /// 封面图片（同名 `.jpg/.jpeg/.png/.webp` 侧车；无则 None，回退渐变）。
    pub cover_path: Option<std::path::PathBuf>,
    /// 歌词（同名 `.lrc` 侧车解析；无则 None）。
    pub lyrics: Option<Lyrics>,
    /// 文件修改时间（`std::fs::metadata::modified`；None=未知/读取失败）。
    pub mtime: Option<std::time::SystemTime>,
}

/// 一句带时间戳的歌词。
#[derive(Clone, Debug)]
pub struct LyricLine {
    /// 句首时间（毫秒）。
    pub time_ms: i64,
    /// 歌词文本（去前后空白）。
    pub text: String,
}

/// 歌曲歌词（解析自同名 `.lrc` 侧车）。无时间戳行被忽略。
#[derive(Clone, Debug)]
pub struct Lyrics {
    /// 按时间升序的时间戳行。
    pub lines: Vec<LyricLine>,
}

impl Lyrics {
    /// `pos_ms` 时刻「当前唱到哪一行」：最后一个 `time_ms <= pos_ms` 的行下标；
    /// 尚未到首句 / 无歌词 → None。
    pub fn active_index(&self, pos_ms: i64) -> Option<usize> {
        let mut idx = None;
        for (i, l) in self.lines.iter().enumerate() {
            if l.time_ms <= pos_ms {
                idx = Some(i);
            } else {
                break;
            }
        }
        idx
    }
}

/// 解析 LRC 文本（RFC 惯例）：
/// - `[mm:ss(.xx)]文本` 时间戳行；同一行可挂多个时间戳 → 展开为多句；
/// - 元数据 `[ti:]/[ar:]/[al:]` 忽略；`[offset:±毫秒]` 整体平移时间；
/// - 无可解析时间戳行 / 全文空 → None。
pub fn parse_lrc(content: &str) -> Option<Lyrics> {
    let mut offset: i64 = 0;
    let mut out: Vec<LyricLine> = Vec::new();
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let mut rest = line;
        let mut times: Vec<i64> = Vec::new();
        while let Some(closed) = rest.find(']') {
            if !rest.starts_with('[') {
                break;
            }
            let tag = &rest[1..closed];
            rest = &rest[closed + 1..];
            if let Some(ms) = parse_lrc_time(tag) {
                times.push(ms);
            } else if let Some(o) = tag.strip_prefix("offset:")
                && let Ok(v) = o.trim().parse::<i64>() {
                    offset = v;
                }
            // 其它 [..] 元数据标签（ti:/ar:/al:）直接忽略
        }
        let text = rest.trim().to_string();
        if times.is_empty() || text.is_empty() {
            continue;
        }
        for ms in times {
            out.push(LyricLine {
                time_ms: ms + offset,
                text: text.clone(),
            });
        }
    }
    if out.is_empty() {
        return None;
    }
    out.sort_by_key(|l| l.time_ms);
    Some(Lyrics { lines: out })
}

/// 解析单个 `mm:ss(.xx)` 时间戳 → 毫秒；非法 → None。
fn parse_lrc_time(tag: &str) -> Option<i64> {
    let (mm, ss) = tag.split_once(':')?;
    let mm: i64 = mm.trim().parse().ok()?;
    let sec = ss.trim();
    let (sec, ms) = match sec.split_once('.') {
        Some((s, f)) => {
            let frac = format!("{:0<3}", f);
            let ms = frac.chars().take(3).collect::<String>();
            (s.trim().parse::<i64>().ok()?, ms.parse::<i64>().ok()?)
        }
        None => (sec.parse::<i64>().ok()?, 0),
    };
    Some(mm * 60_000 + sec * 1000 + ms)
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

/// 时长（秒）→ 时钟字符串。逻辑收敛在 [`base::util::format_duration`]（与 video `fmt_time`
/// 共用一份实现）；**显式语义统一**：原来 ≥1h 显示 m:ss（如 `61:23`，隐含 bug），
/// 现按共享 util 走 h:mm:ss（如 `1:01:23`）。
pub fn duration_str(secs: f64) -> String {
    base::util::format_duration(secs)
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
        // **显式语义统一**（修隐含 bug）：原 ≥1h 显示 m:ss（3661 → "61:01"），
        // 统一走共享 util 后与 video 一致显示 h:mm:ss。
        assert_eq!(duration_str(3661.0), "1:01:01");
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

    #[test]
    fn parse_lrc_timestamps_and_meta() {
        let content = "\
[ti:晴天]
[ar:周杰伦]
[00:01.50]前奏响起
[00:05]第一句歌词
[01:02.250]带毫秒的句
[mm:ss]纯文本行被忽略
";
        let l = parse_lrc(content).expect("应解析出歌词");
        let t: Vec<(i64, &str)> = l.lines.iter().map(|x| (x.time_ms, x.text.as_str())).collect();
        assert_eq!(
            t,
            vec![(1500, "前奏响起"), (5000, "第一句歌词"), (62250, "带毫秒的句")]
        );
    }

    #[test]
    fn parse_lrc_multiple_timestamps_expand_and_offset() {
        // 双时间戳同一句 → 展开两行；[offset:+] 整体平移
        let content = "\
[offset:+500]
[00:01][01:02]同一句
[00:02.5]另一句
";
        let l = parse_lrc(content).expect("应解析出歌词");
        let t: Vec<(i64, &str)> = l.lines.iter().map(|x| (x.time_ms, x.text.as_str())).collect();
        assert_eq!(
            t,
            vec![(1500, "同一句"), (3000, "另一句"), (62500, "同一句")]
        );
    }

    #[test]
    fn parse_lrc_none_and_invalid() {
        assert!(parse_lrc("没有时间戳的歌词\n也不会有解析结果").is_none());
        assert!(parse_lrc("").is_none());
        assert!(parse_lrc("[00:01]").is_none(), "有时间戳但无文本 → 忽略");
        // 非法时间戳被跳过
        let l = parse_lrc("[00:xx]坏时间戳\n[01:00]好句").expect("至少保留合法句");
        assert_eq!(l.lines.len(), 1);
        assert_eq!(l.lines[0].text, "好句");
    }

    #[test]
    fn lyrics_active_index() {
        let l = Lyrics {
            lines: vec![
                LyricLine { time_ms: 1000, text: "a".into() },
                LyricLine { time_ms: 5000, text: "b".into() },
                LyricLine { time_ms: 9000, text: "c".into() },
            ],
        };
        assert_eq!(l.active_index(0), None, "首句之前无活动行");
        assert_eq!(l.active_index(1000), Some(0));
        assert_eq!(l.active_index(4999), Some(0));
        assert_eq!(l.active_index(5000), Some(1));
        assert_eq!(l.active_index(99999), Some(2));
    }
}
