//! 音乐库扫描与分组：音频扩展名过滤、递归扫描（手写栈）、按艺术家分组。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::model::{MixGroup, Track, gradient_pair, grad_from_hash, parse_meta};

/// 可识别的音频扩展名（小写）。
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "ogg", "flac", "mp3", "m4a", "aac", "wav", "wma", "opus", "ape",
];

/// 某路径是否音频文件（扩展名大小写不敏感）。
pub fn is_audio_extension(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)))
        .unwrap_or(false)
}

/// 递归扫描 `root` 下的音频文件：手写栈遍历；跳过以 `.` 开头的隐藏目录与
/// `target`/`node_modules`/`.git`/`vendor`/`session` 构建/缓存目录；结果按路径自然排序。
/// `scan` 只按扩展名收集，**不读文件内容**（时长先置 0，由 [`probe_duration`] 后台补）。
pub fn scan_dir(root: &Path) -> Vec<Track> {
    let mut stack = vec![root.to_path_buf()];
    let mut files: Vec<PathBuf> = Vec::new();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_dir() {
                let name = entry.file_name();
                let n = name.to_str().unwrap_or("");
                if n.starts_with('.')
                    || matches!(n, "target" | "node_modules" | ".git" | "vendor" | "session")
                {
                    continue;
                }
                stack.push(p);
            } else if is_audio_extension(&p) {
                files.push(p);
            }
        }
    }
    // 文件名自然排序：路径字典序（Windows 大小写不敏感，跨平台稳定用逐字节比较）
    files.sort();
    files.into_iter().map(track_from_path).collect()
}

fn track_from_path(path: PathBuf) -> Track {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let (artist, title) = parse_meta(&stem);
    let grad = grad_from_hash(&format!("{artist} - {title}"));
    Track {
        path,
        title,
        artist,
        album: String::new(),
        duration: 0.0,
        grad,
    }
}

/// 按艺术家稳定分组（首现顺序），每组记录 `library` 下标与 `"N首"` 副标题；
/// 组渐变双色由艺术家名哈希派生。
pub fn build_mix_groups(tracks: &[Track]) -> Vec<MixGroup> {
    let mut order: Vec<&str> = Vec::new();
    let mut map: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, t) in tracks.iter().enumerate() {
        if !map.contains_key(t.artist.as_str()) {
            order.push(t.artist.as_str());
        }
        map.entry(t.artist.as_str()).or_default().push(i);
    }
    let mut groups = Vec::with_capacity(order.len());
    for artist in order {
        let tracks = map.remove(artist).unwrap_or_default();
        let subtitle = format!("{}首", tracks.len());
        let grad = gradient_pair(artist);
        groups.push(MixGroup {
            title: artist.to_string(),
            subtitle,
            tracks,
            grad,
        });
    }
    groups
}

/// 探测单个文件时长（open + params + drop）：`0.0` 或 Err 都可由调用方按失败处理。
pub fn probe_duration(path: &Path, sample_rate: u32) -> Result<f64> {
    let codec = crate::decode::open_audio(path, sample_rate)?;
    Ok(codec.params().duration)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建含中文文件名/子目录/隐藏目录/构建目录的临时扫描树，返回根目录。
    fn tmp_scan_tree() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gpui-music-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sub = dir.join("子目录");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.join("周杰伦 - 晴天.ogg"), b"ogg").unwrap();
        std::fs::write(dir.join("林俊杰 - 江南.mp3"), b"id3").unwrap();
        std::fs::write(sub.join("陈奕迅 - 十年 (Live).flac"), b"flac").unwrap();
        std::fs::write(sub.join("只有歌名.wav"), b"riff").unwrap();
        // 非音频：应被过滤
        std::fs::write(dir.join("readme.md"), b"# readme").unwrap();
        std::fs::write(dir.join("曲目清单.txt"), b"list").unwrap();
        // 隐藏目录与构建目录：应被跳过
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git").join("阿伟.m4a"), b"git").unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("target").join("构建产物.mp3"), b"t").unwrap();
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("node_modules").join("混入.ogg"), b"n").unwrap();
        dir
    }

    #[test]
    fn scan_dir_collects_audio_and_filters_non_audio() {
        let dir = tmp_scan_tree();
        let tracks = scan_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(tracks.len(), 4, "应收 4 个音频；实得 {tracks:?}");
        let names: Vec<&str> = tracks.iter().map(|t| t.title.as_str()).collect();
        assert!(names.contains(&"晴天"));
        assert!(names.contains(&"江南"));
        assert!(names.contains(&"十年 (Live)"));
        assert!(names.contains(&"只有歌名"));
        // 扩展名过滤 + 隐藏/构建目录跳过
        for t in &tracks {
            assert!(is_audio_extension(&t.path), "非音频混入: {}", t.path.display());
            assert!(!t.path.to_string_lossy().contains(".git"), "隐藏目录混入");
            assert!(!t.path.to_string_lossy().contains("target"), "构建目录混入");
            assert!(!t.path.to_string_lossy().contains("node_modules"), "vendor 目录混入");
        }
        // 时长先置 0（scan 不读内容）
        assert!(tracks.iter().all(|t| t.duration == 0.0));
    }

    #[test]
    fn is_audio_extension_case_insensitive() {
        assert!(is_audio_extension(Path::new("a.MP3")));
        assert!(is_audio_extension(Path::new("b.Ogg")));
        assert!(!is_audio_extension(Path::new("c.oga")));
        assert!(!is_audio_extension(Path::new("d.txt")));
        assert!(!is_audio_extension(Path::new("e")));
    }

    #[test]
    fn build_mix_groups_groups_by_artist_in_first_seen_order() {
        // 构造 3 首：周杰伦×2、林俊杰×1（下标 0/2 同艺术家）
        let mk = |i: usize, artist: &str, title: &str| Track {
            path: PathBuf::from(format!("p{i}.mp3")),
            title: title.to_string(),
            artist: artist.to_string(),
            album: String::new(),
            duration: 0.0,
            grad: (0, 0, 0),
        };
        let tracks = vec![
            mk(0, "周杰伦", "晴天"),
            mk(1, "林俊杰", "江南"),
            mk(2, "周杰伦", "七里香"),
        ];
        let groups = build_mix_groups(&tracks);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].title, "周杰伦");
        assert_eq!(groups[0].tracks, vec![0, 2]);
        assert_eq!(groups[0].subtitle, "2首");
        assert_eq!(groups[1].title, "林俊杰");
        assert_eq!(groups[1].tracks, vec![1]);
        assert_eq!(groups[1].subtitle, "1首");
        // 渐变两色均为 #rrggbb
        for g in &groups {
            assert!(g.grad.0.starts_with('#') && g.grad.0.len() == 7);
            assert!(g.grad.1.starts_with('#') && g.grad.1.len() == 7);
        }
    }

    #[test]
    fn build_mix_groups_empty_and_single() {
        assert!(build_mix_groups(&[]).is_empty());
        let tracks = vec![Track {
            path: PathBuf::from("x.flac"),
            title: "x".into(),
            artist: "未知歌手".into(),
            album: String::new(),
            duration: 0.0,
            grad: (0, 0, 0),
        }];
        let g = build_mix_groups(&tracks);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].title, "未知歌手");
        assert_eq!(g[0].subtitle, "1首");
    }
}
