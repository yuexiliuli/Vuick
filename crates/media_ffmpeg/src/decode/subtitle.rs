//! 字幕解码（ASS/SSA/SRT）子模块：从 `decode.rs` 按职责拆出。
//!
//! 组合关系：独立于视频/音频独立使用（`SubtitleReader`），FFMPEG_LOCK 串行纪律由模块根的
//! [`super::FFMPEG_LOCK`]、日志收敛 [`super::quiet_ffmpeg_log_once`] 在此复用；自由函数
//! [`active_subtitle_index`] 供 reader 读口与生产对拍（与 `video` crate 同口径）。

use super::{FFMPEG_LOCK, OPEN_DEFAULT_TIMEOUT, open_input_interruptible, quiet_ffmpeg_log_once};
use std::path::Path;

use anyhow::{Context as _, Result, anyhow};

use ffmpeg_next as ffmpeg;
use ffmpeg::media::Type;

// ---------------- 字幕（ASS/SSA）----------------

/// 字幕格式枚举。
/// 变体名沿用 ASS/SSA/SRT 标准缩写 → 放行。
#[allow(clippy::upper_case_acronyms)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SubtitleFormat {
    ASS,
    SSA,
    SRT,
    Unknown,
}

/// 字幕顶层参数。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SubtitleParams {
    /// 字幕流数量。
    pub stream_count: u32,
    /// 首选字幕格式。
    pub format: SubtitleFormat,
}

/// 一条解码后的字幕条目。
#[derive(Clone, Debug)]
pub struct SubtitleEntry {
    /// 开始时间（秒）。
    pub start_pts: f64,
    /// 结束时间（秒）。
    pub end_pts: f64,
    /// 字幕文本（ASS 格式可能含样式标记）。
    pub text: String,
}

/// 可 seek 的字幕解码器：提取 ASS/SSA/SRT 字幕。
///
/// 与 [`VideoReader`] / [`AudioReader`] 同款姿势：调用方搬进后台线程逐条拉取。
///
/// **释放纪律**：`Drop` 把 FFmpeg 成员（`Input`/decoder）手动 drop 并持有
/// [`FFMPEG_LOCK`] —— 同视频/音频的串行纪律。
pub struct SubtitleReader {
    ictx: std::mem::ManuallyDrop<ffmpeg::format::context::Input>,
    /// 已确定的输出参数（仅供 [`Self::params`] 返回）。
    params: SubtitleParams,
    /// 预加载的字幕清单（打开时一次性加载全部）。
    subtitles: Vec<SubtitleEntry>,
    /// 当前读取位置索引。
    current_index: usize,
}

impl Drop for SubtitleReader {
    fn drop(&mut self) {
        let _ffmpeg_guard = FFMPEG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::mem::ManuallyDrop::drop(&mut self.ictx);
        }
    }
}

impl SubtitleReader {
    /// 打开媒体并提取字幕流。
    ///
    /// **锁纪律**：`format::input`（avformat_open_input）与并发 open/decode 互占死
    /// （FFMPEG_LOCK 注释与 AudioReader 释放纪律同源），故打开/查流/定格式在全局锁内
    /// 完成；拿到 `ictx` 后**提前释放锁**再对全文件做 `packets()` 扫描——扫描是纯
    /// demux（不开解码器、逐包读独立文件句柄），与视频/音频 worker 的 open/decode/seek
    /// 并存（并发压力测试 `subtitle_scan_concurrent_with_video_seek_stress` 实证：多轮
    /// 不崩、不死锁、seek 不被拖慢）。若在锁内扫完整 415MB 文件（≈0.4-0.6s），期间
    /// **所有** A/V FFI 调用被闸阻塞 → 打开后窗口期点击进度条 seek 卡顿（复现测试
    /// `subtitle_scan_does_not_block_video_seek` 量化 delta≈385ms 即为该因）。
    pub fn open(path: &Path) -> Result<Self> {
        let (mut ictx, stream_index, time_base, format) = {
            let _ffmpeg_guard = FFMPEG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            ffmpeg::init().context("ffmpeg init 失败")?;
            quiet_ffmpeg_log_once();

            let ictx = open_input_interruptible(path, OPEN_DEFAULT_TIMEOUT)?;

            // 查找字幕流
            let stream = ictx
                .streams()
                .best(Type::Subtitle)
                .ok_or_else(|| anyhow!("媒体中没有字幕流：{}", path.display()))?;

            let stream_index = stream.index();
            let time_base = stream.time_base();

            // 检测字幕格式
            let codec_id = stream.parameters().id();
            let format = match codec_id {
                ffmpeg::codec::Id::ASS => SubtitleFormat::ASS,
                ffmpeg::codec::Id::SSA => SubtitleFormat::SSA,
                ffmpeg::codec::Id::SUBRIP => SubtitleFormat::SRT,
                ffmpeg::codec::Id::HDMV_PGS_SUBTITLE => SubtitleFormat::Unknown, // 图形字幕不支持
                _ => SubtitleFormat::Unknown,
            };

            (ictx, stream_index, time_base, format)
        };
        // ← 全局锁已提前释放：以下全文件扫描为无锁并发纯 demux

        let params = SubtitleParams {
            stream_count: 1,
            format,
        };
        
        // 一次性加载所有字幕（`ictx.packets()` 产出 `(Stream, Packet)` 元组，迭代无失败分支）；
        // `stream_index/time_base/format` 只在 open 期扫描解析用，不作暂存（无流级重定位消费者）
        let mut subtitles = Vec::new();
        for (stream, pkt) in ictx.packets() {
            if stream.index() == stream_index {
                // 解析字幕包
                if let Some(entry) = Self::parse_subtitle_packet(&pkt, time_base, format) {
                    subtitles.push(entry);
                }
            }
        }

        // 按开始时间排序
        subtitles.sort_by(|a, b| a.start_pts.partial_cmp(&b.start_pts).unwrap_or(std::cmp::Ordering::Equal));

        // 兜底封闭窗口：包 duration 缺失/为 0 的事件会把 `end_pts` 留成 `start_pts`（永不结束）。
        // 以「下一条的开始」封闭；末条给默认窗口。本仓 `视频测试字幕.mkv` 各包都带 duration，
        // 此兜底只护其它源。
        for i in 0..subtitles.len() {
            if subtitles[i].end_pts <= subtitles[i].start_pts {
                let next_start = subtitles.get(i + 1).map(|n| n.start_pts);
                subtitles[i].end_pts = next_start
                    .unwrap_or(subtitles[i].start_pts + Self::guess_subtitle_duration(format));
            }
        }
        
        Ok(Self {
            ictx: std::mem::ManuallyDrop::new(ictx),
            params,
            subtitles,
            current_index: 0,
        })
    }
    
    /// 解析字幕包为 `SubtitleEntry`。
    ///
    /// ASS/SSA 事件在容器里有两种封存姿势，这里都兼容：
    /// 1. **老式「Dialogue: 」整行**（行内自带 `Layer,Start,End,Style,…` 时间）——
    ///    [`Self::parse_ass_dialogue`]；
    /// 2. **Matroska 新块格式**（`Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Text`，
    ///    开始/结束时间不在行内，由**包 PTS + duration** 提供）。本仓 `视频测试字幕.mkv`
    ///    即此姿势：首包数据 `0,0,Default,,0,0,0,,<字幕文本>`，pts=20050ms、
    ///    duration=1980ms → start≈20.05s、end≈22.03s。
    pub(crate) fn parse_subtitle_packet(
        pkt: &ffmpeg::Packet,
        time_base: ffmpeg::Rational,
        format: SubtitleFormat,
    ) -> Option<SubtitleEntry> {
        let data = pkt.data()?;
        let Ok(mut text) = std::str::from_utf8(data) else {
            return None;
        };
        // F6：剥 UTF-8 BOM（\u{FEFF}）：SRT/MKV 带 BOM 时 `trim()` 不去 BOM（BOM 不是
        // 空白），首字符会残留 BOM；且 ASS 老式 `Dialogue:` 行因 BOM 前缀 `starts_with`
        // 不命中 → 错误走包 PTS 分支、时间错位。在解析入口剥一次，`Dialogue:` 与 8/9
        // 字段检查都在剥之后进行。
        text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let text = text.trim();
        if text.is_empty() {
            return None;
        }

        if format == SubtitleFormat::ASS || format == SubtitleFormat::SSA {
            // 老式整行：行内时间优先（以字幕作者写入的时间为准）
            if text.starts_with("Dialogue:") {
                return Self::parse_ass_dialogue(text);
            }
            // Matroska 新块格式（mkvmerge 惯例，实测 `视频测试字幕.mkv` 首包即此布局）：
            //   `readorder,layer,style,name,marginL,marginR,marginV,effect,text`
            // 前 8 个字段 + 文本；文本字段可含逗号 → `splitn(9, ',')`。
            // 少数 mux 不计 readorder（7 前导）时退到 `splitn(8, ',')` 兜底。
            let dialogue = {
                let pieces8: Vec<&str> = text.splitn(9, ',').collect();
                if pieces8.len() >= 9 {
                    pieces8[8].trim()
                } else {
                    let pieces7: Vec<&str> = text.splitn(8, ',').collect();
                    if pieces7.len() >= 8 {
                        pieces7[7].trim()
                    } else {
                        return None;
                    }
                }
            };
            if dialogue.is_empty() {
                return None;
            }
            let (start_pts, end_pts) = Self::packet_window(pkt, time_base);
            return Some(SubtitleEntry {
                start_pts,
                end_pts,
                text: dialogue.to_string(),
            });
        } else if format == SubtitleFormat::SRT {
            // SRT 无内嵌时间字段：开始/结束由包 PTS + duration 提供
            let (start_pts, end_pts) = Self::packet_window(pkt, time_base);
            return Some(SubtitleEntry {
                start_pts,
                end_pts,
                text: text.to_string(),
            });
        }

        None
    }

    /// 字幕时间窗口：包 PTS * 时间基 = 开始秒；PTS + 包 duration * 时间基 = 结束秒。
    /// `duration` 缺失（为 0）时结束 = 开始（由 open 后置兜底按「下一条开始/默认窗」封闭）。
    fn packet_window(pkt: &ffmpeg::Packet, time_base: ffmpeg::Rational) -> (f64, f64) {
        let tb = f64::from(time_base);
        let start = pkt.pts().unwrap_or(0) as f64 * tb;
        let end = start + pkt.duration() as f64 * tb;
        (start, end)
    }

    /// 解析「Dialogue: …」整行（行内 `Layer,Start,End,Style,Name,MarginL,MarginR,MarginV,
    /// Effect,Text`，时间格式 `H:MM:SS.cc`）。
    fn parse_ass_dialogue(text: &str) -> Option<SubtitleEntry> {
        // ASS Dialogue 格式：Dialogue: Layer,Start,End,Style,Name,MarginL,MarginR,MarginV,Effect,Text
        if !text.starts_with("Dialogue:") {
            return None;
        }

        // `Dialogue:` 之后是 ` Layer,Start,End,...`（9 个字段 + 文本；文本可含逗号 → splitn(10, ','))
        let parts: Vec<&str> = text["Dialogue:".len()..].splitn(10, ',').collect();
        if parts.len() < 10 {
            return None;
        }

        let start_secs = Self::parse_ass_time(parts[1])?;
        let end_secs = Self::parse_ass_time(parts[2])?;
        // 提取层保留原始文本（样式标记由 viewer 渲染层统一清理，见 video::subtitle::parse_ass_text）
        let text_content = parts[9].trim();

        Some(SubtitleEntry {
            start_pts: start_secs,
            end_pts: end_secs,
            text: text_content.to_string(),
        })
    }

    /// 解析 ASS 时间戳（格式：H:MM:SS.cc 或 H:MM:SS.ccc）。
    fn parse_ass_time(s: &str) -> Option<f64> {
        let s = s.trim();
        // 格式：H:MM:SS.cc 或 H:MM:SS.ccc
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 3 {
            return None;
        }

        let hours: f64 = parts[0].parse().ok()?;
        let minutes: f64 = parts[1].parse().ok()?;
        let seconds: f64 = parts[2].parse().ok()?;

        Some(hours * 3600.0 + minutes * 60.0 + seconds)
    }
    
    /// 猜测字幕持续时间（用于 SRT 等无明确结束时间的格式）。
    fn guess_subtitle_duration(format: SubtitleFormat) -> f64 {
        match format {
            SubtitleFormat::ASS | SubtitleFormat::SSA => 2.0, // ASS 有明确结束时间
            SubtitleFormat::SRT => 2.0,
            SubtitleFormat::Unknown => 2.0,
        }
    }
    
    /// 已确定的输出参数。
    pub fn params(&self) -> SubtitleParams {
        self.params
    }
    
    /// 获取所有字幕条目。
    pub fn subtitles(&self) -> &[SubtitleEntry] {
        &self.subtitles
    }
    
    /// 跳转到指定时间（秒），返回该时间之后的第一个字幕索引。
    /// **R2-5 dormant**：原 FFI 入口 `vff_subtitle_seek` 及其主侧解析已随死导出清理删除，
    /// 本方法（连同 `current_index`/`next_subtitle`）保留为「逐条拉取」预留 API。
    #[allow(dead_code)] // 预留「逐条拉取」遍历 API（配合 [`Self::seek`] 定位起点）
    pub fn seek(&mut self, seconds: f64) -> usize {
        // 二分查找第一个 start_pts >= seconds 的字幕
        let idx = self.subtitles.partition_point(|s| s.start_pts < seconds);
        self.current_index = idx;
        idx
    }
    
    /// 取下一条字幕（按时间顺序）。
    #[allow(dead_code)] // 预留「逐条拉取」遍历 API（配合 [`Self::seek`] 定位起点）；
    /// 当前 FFI/上层走预加载清单 `subtitles()` + `subtitle_at()`，未消费本方法。
    pub fn next_subtitle(&mut self) -> Option<&SubtitleEntry> {
        if self.current_index < self.subtitles.len() {
            let entry = &self.subtitles[self.current_index];
            self.current_index += 1;
            Some(entry)
        } else {
            None
        }
    }
    
    /// 当前时间应显示的字幕下标——`subtitles` 按 `start_pts` 升序（open 已 sort）：
    /// 二分定位最后一个 `start_pts ≤ seconds`，从该处**往前回扫**找命中。重叠窗口
    /// （同一时间多条活跃）取「最近开始」那条，与 `video` crate 生产路径
    /// `subtitle::find_active_subtitle` **同口径**（显示最新对白）。dormancy：显示已走
    /// `find_active_subtitle`；本方法供本 crate 单测对拍（原 FFI `vff_subtitle_at` 已随
    /// R2-5 死导出清理删除），保持与生产一致以消除三份实现的口径漂移。
    pub fn active_subtitle_index(&self, seconds: f64) -> Option<usize> {
        active_subtitle_index(&self.subtitles, seconds)
    }

    /// 当前时间应显示的字幕（同 [`Self::active_subtitle_index`] 语义）。
    #[allow(dead_code)] // 仅供本 crate 单测对拍（显示已走 find_active_subtitle）
    pub fn subtitle_at(&self, seconds: f64) -> Option<&SubtitleEntry> {
        self.active_subtitle_index(seconds).map(|i| &self.subtitles[i])
    }
}

/// 与 [`SubtitleReader::active_subtitle_index`] 同语义的纯函数（独立于 reader 构造，可直测）。
pub(crate) fn active_subtitle_index(subtitles: &[SubtitleEntry], seconds: f64) -> Option<usize> {
    let i = subtitles.partition_point(|s| s.start_pts <= seconds);
    subtitles[..i].iter().rposition(|s| seconds <= s.end_pts)
}
