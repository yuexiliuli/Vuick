//! 音乐库扫描与分组：音频扩展名过滤、递归扫描（手写栈）、按艺术家分组。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::model::{MixGroup, Track, gradient_pair, grad_from_hash, parse_meta};

/// 可识别的音频扩展名（小写）。
pub const AUDIO_EXTENSIONS: &[&str] = &[
    "ogg", "flac", "mp3", "m4a", "aac", "wav", "wma", "opus", "ape",
];

/// 可识别的封面扩展名（小写；优先顺序即匹配优先级）。
pub const COVER_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp"];

/// 某路径是否音频文件（扩展名大小写不敏感）。
pub fn is_audio_extension(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)))
        .unwrap_or(false)
}

/// 递归扫描 `root` 下的音频文件：手写栈遍历；跳过以 `.` 开头的隐藏目录与
/// `target`/`node_modules`/`.git`/`vendor`/`session` 构建/缓存目录；结果按路径自然排序。
/// `scan` 按扩展名收集（时长先置 0，由 [`probe_duration`] 后台补），并顺带挂每首歌的
/// 同名 `.lrc` 歌词与同名封面图（`.jpg/.jpeg/.png/.webp`）侧车（都失败静默 → None）。
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
    let lyrics = read_lrc_sidecar(&path);
    let cover_path = find_cover_sidecar(&path);
    let mtime = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok());
    Track {
        path,
        title,
        artist,
        album: String::new(),
        duration: 0.0,
        grad,
        cover_path,
        lyrics,
        mtime,
    }
}

/// 按文件修改时间倒序排列（最新在前）；`mtime` 为 `None`（未知）恒殿后；None 之间保持
/// 原相对顺序（`sort_by` 是稳定排序，等键元素不换序）。
pub fn sort_tracks_by_mtime_desc(tracks: &mut [Track]) {
    tracks.sort_by(|a, b| b.mtime.cmp(&a.mtime));
}

/// 找歌曲旁同名封面图（`.jpg/.jpeg/.png/.webp`，扩展名大小写不敏感，按 [`COVER_EXTENSIONS`]
/// 顺序优先）；无 → None。
fn find_cover_sidecar(audio: &Path) -> Option<PathBuf> {
    let dir = audio.parent()?;
    let stem_l = audio.file_stem()?.to_string_lossy().to_lowercase();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return None;
    };
    // 先做一次目录快照再按优先级挑
    let mut candidates: Vec<(usize, PathBuf)> = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        let Some(ext) = p.extension().and_then(|x| x.to_str()) else {
            continue;
        };
        let Some(rank) = COVER_EXTENSIONS
            .iter()
            .position(|x| x.eq_ignore_ascii_case(ext))
        else {
            continue;
        };
        let stem_ok = p
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .is_some_and(|s| s == stem_l);
        if stem_ok {
            candidates.push((rank, p));
        }
    }
    candidates.sort_by_key(|(rank, _)| *rank);
    candidates.into_iter().next().map(|(_, p)| p)
}

/// 找歌曲旁同名 `.lrc` 歌词侧车（扩展名大小写不敏感），读取解析；无/失败 → None。
fn read_lrc_sidecar(audio: &Path) -> Option<crate::model::Lyrics> {
    let lrc = find_lrc_sidecar(audio)?;
    let content = std::fs::read_to_string(lrc).ok()?;
    crate::model::parse_lrc(&content)
}

/// 在歌曲同目录找同名（stem）`.lrc`：精确优先，退化为大小写不敏感匹配。
fn find_lrc_sidecar(audio: &Path) -> Option<PathBuf> {
    let dir = audio.parent()?;
    let stem = audio.file_stem()?;
    let exact = dir.join(format!("{}.lrc", stem.to_string_lossy()));
    if exact.is_file() {
        return Some(exact);
    }
    let stem_l = stem.to_string_lossy().to_lowercase();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return None;
    };
    for e in entries.flatten() {
        let p = e.path();
        let ext_ok = p
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| x.eq_ignore_ascii_case("lrc"));
        let stem_ok = p
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .is_some_and(|s| s == stem_l);
        if ext_ok && stem_ok {
            return Some(p);
        }
    }
    None
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
    fn scan_loads_lrc_sidecar() {
        let dir = std::env::temp_dir().join(format!("gpui-music-lrc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("周杰伦 - 晴天.ogg"), b"ogg").unwrap();
        std::fs::write(
            dir.join("周杰伦 - 晴天.lrc"),
            "[00:01.00]歌词第一句\n[00:05]歌词第二句\n",
        )
        .unwrap();
        // 无歌词侧车的歌保持 None
        std::fs::write(dir.join("林俊杰 - 江南.MP3"), b"id3").unwrap();

        let tracks = scan_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(tracks.len(), 2);
        let with_lyric = tracks.iter().find(|t| t.title == "晴天").unwrap();
        let l = with_lyric.lyrics.as_ref().expect("同名 .lrc 应被装载");
        assert_eq!(l.lines.len(), 2);
        assert_eq!(l.lines[0].text, "歌词第一句");
        let without = tracks.iter().find(|t| t.title == "江南").unwrap();
        assert!(without.lyrics.is_none(), "无 .lrc 的歌 lyrics 应为 None");
    }

    #[test]
    fn scan_lrc_extension_case_insensitive() {
        let dir = std::env::temp_dir().join(format!("gpui-music-lrc-mixed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("陈奕迅 - 十年.ogg"), b"ogg").unwrap();
        std::fs::write(dir.join("陈奕迅 - 十年.LRC"), "[00:01]十年之前\n").unwrap();
        let tracks = scan_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        let t = &tracks[0];
        assert_eq!(
            t.lyrics.as_ref().map(|l| l.lines[0].text.as_str()),
            Some("十年之前"),
            "扩展名大小写不敏感也应装载"
        );
    }

    #[test]
    fn scan_finds_cover_sidecar_by_stem_priority() {
        let dir = std::env::temp_dir().join(format!("gpui-music-cover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("初音未来 (初音ミク) - 月西江.ogg"), b"ogg").unwrap();
        std::fs::write(dir.join("初音未来 (初音ミク) - 月西江.jpg"), b"jpg").unwrap();
        std::fs::write(dir.join("心华 - 音符成诗.ogg"), b"ogg").unwrap();
        std::fs::write(dir.join("心华 - 音符成诗.png"), b"png").unwrap();
        // 无封面歌保持 None
        std::fs::write(dir.join("蔡琴 - 我要如何不想他.ogg"), b"ogg").unwrap();

        let tracks = scan_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(tracks.len(), 3);
        let moon = tracks.iter().find(|t| t.title == "月西江").unwrap();
        assert_eq!(
            moon.cover_path.as_ref().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()),
            Some("初音未来 (初音ミク) - 月西江.jpg".to_string())
        );
        let song = tracks.iter().find(|t| t.title == "音符成诗").unwrap();
        assert!(song.cover_path.is_some());
        let no_cover = tracks.iter().find(|t| t.title == "我要如何不想他").unwrap();
        assert!(no_cover.cover_path.is_none(), "无封面图应 None");
    }

    #[test]
    fn cover_sidecar_extension_case_insensitive_and_priority() {
        let dir = std::env::temp_dir().join(format!("gpui-music-cover-pr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("歌.ogg"), b"ogg").unwrap();
        // png 与 jpeg 同名 → 按 COVER_EXTENSIONS 顺序优先 jpeg
        std::fs::write(dir.join("歌.png"), b"png").unwrap();
        std::fs::write(dir.join("歌.JPEG"), b"jpeg").unwrap();
        let tracks = scan_dir(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        let t = &tracks[0];
        assert_eq!(
            t.cover_path.as_ref().map(|p| p.extension().unwrap().to_string_lossy().to_ascii_lowercase()),
            Some("jpeg".to_string())
        );
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
            cover_path: None,
            lyrics: None,
            mtime: None,
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
            cover_path: None,
            lyrics: None,
            mtime: None,
        }];
        let g = build_mix_groups(&tracks);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].title, "未知歌手");
        assert_eq!(g[0].subtitle, "1首");
    }

    #[test]
    fn scan_fills_mtime_matches_fs() {
        let dir = tmp_scan_tree();
        let tracks = scan_dir(&dir);
        // 在清理前与文件系统对拍（清理后文件已删，metadata 取不到 None）。
        for t in &tracks {
            let fs_mtime = std::fs::metadata(&t.path).and_then(|m| m.modified()).ok();
            assert!(
                fs_mtime.is_some(),
                "文件应存在且可读 mtime: {}",
                t.path.display()
            );
            assert_eq!(
                t.mtime, fs_mtime,
                "曲目 {} 的 mtime 应与文件系统一致",
                t.path.display()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sort_tracks_by_mtime_desc_none_last() {
        use std::time::{Duration, SystemTime};
        let base = SystemTime::UNIX_EPOCH;
        let mk = |tag: &str, m: Option<SystemTime>| Track {
            path: PathBuf::from(tag),
            title: tag.to_string(),
            artist: String::new(),
            album: String::new(),
            duration: 0.0,
            grad: (0, 0, 0),
            cover_path: None,
            lyrics: None,
            mtime: m,
        };
        let mut tracks = vec![
            mk("none-b", None),
            mk("old", Some(base + Duration::from_secs(100))),
            mk("mid", Some(base + Duration::from_secs(200))),
            mk("none-a", None),
            mk("new", Some(base + Duration::from_secs(300))),
        ];
        sort_tracks_by_mtime_desc(&mut tracks);
        let keys: Vec<&str> = tracks.iter().map(|t| t.path.to_str().unwrap()).collect();
        assert_eq!(
            keys,
            vec!["new", "mid", "old", "none-b", "none-a"],
            "最新在前、None 殿后且保持原相对顺序（稳定）"
        );
        assert!(tracks[3].mtime.is_none() && tracks[4].mtime.is_none());
    }
}
