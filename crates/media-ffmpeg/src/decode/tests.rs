//! 解码器测试（视频 / 音频 / 硬解 / 字幕 / bench）——原 `decode.rs` 内 `mod tests` 与
//! `mod subtitle_tests` 两段 `#[cfg(test)]` 测试整体平移至此（`use super::*;` 关系保持：
//! 本文件即 `decode::tests`，`super` 即 decode 根，与旧内联等价）。夹具路径、bench 与
//! 断言逐字保留。

use super::*;
use anyhow::Context as _;
use std::path::PathBuf;

/// 视频夹具：§5 手动装配（不入库），320×240 / 2s@30fps / H.264(yuv420p)+AAC 2ch/48k，
/// 关键帧间隔 15 帧（seek(1.0) 贴近落点满足 `|pts-1.0|<0.15`）。
fn fixture() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target")
        .join("video测试.mp4")
}

/// 缺夹具时 SKIP 不 FAIL（媒体夹具不入库，同 viewer 约定）；返回已存在的路径。
macro_rules! fixture_or_skip {
    () => {{
        let p = fixture();
        if !p.exists() {
            eprintln!("SKIP: missing target/video测试.mp4（媒体夹具不入库，手动装配）");
            return;
        }
        p
    }};
}

/// P0-A 看门狗回归：损坏/畸形媒体探测**不再永久卡死**（无媒体夹具依赖——临时目录写假
/// mp4）。短 deadline（300ms）→ [`open_input_interruptible`] 快速 Err；已过期 deadline →
/// 确定性验证中断回调立即使探测 abort；最后证明锁/武装无残留（不存在路径快速 Err +
/// 真视频夹具正常打开）。全部在持 [`FFMPEG_LOCK`] 下调用（看门狗契约）。
#[test]
fn open_watchdog_interrupts_corrupt_file() {
    let dir = std::env::temp_dir().join(format!(
        "gpui-editor-mff-corrupt-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let bad = dir.join("bad.mp4");
    std::fs::write(&bad, b"not a real video").unwrap();

    // 1) 短 deadline：真实挂起（无看门狗）会永久卡死；有限 deadline 必须快速失败
    let err = {
        let _g = FFMPEG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let t0 = std::time::Instant::now();
        let r = open_input_interruptible(&bad, std::time::Duration::from_millis(300));
        let elapsed = t0.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "损坏媒体应在 <2s 内快速失败（实际 {elapsed:?}，疑似仍卡死）"
        );
        match r {
            Ok(_) => panic!("损坏媒体应失败而非打开成功"),
            Err(e) => e,
        }
    };
    assert!(
        format!("{err:#}").contains("打开媒体"),
        "错误信息应可辨媒体打开失败/超时：{err:#}"
    );

    // 2) 已过期 deadline：确定性验证中断回调立即生效（probe 首查即 abort → AVERROR_EXIT）
    {
        let _g = FFMPEG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let t0 = std::time::Instant::now();
        let r = open_input_interruptible(&bad, std::time::Duration::ZERO);
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(2),
            "已过期 deadline 应立即中断探测"
        );
        let msg = match r {
            Ok(_) => panic!("已过期 deadline 的打开不应成功"),
            Err(e) => format!("{e:#}"),
        };
        assert!(
            msg.contains("超时/被取消"),
            "被中断错误应标识「打开超时/被取消」：{msg}"
        );
    }

    // 3) 无残留：中断后可再开（不存在路径快速 Err + 真视频夹具正常打开）
    {
        let _g = FFMPEG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let t0 = std::time::Instant::now();
        let r = open_input_interruptible(&dir.join("missing.mp4"), OPEN_DEFAULT_TIMEOUT);
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(2),
            "不存在文件应快速 Err（锁未卡死）"
        );
        assert!(r.is_err(), "不存在文件应失败");
        let fixture = fixture();
        if fixture.exists() {
            assert!(
                open_input_interruptible(&fixture, OPEN_DEFAULT_TIMEOUT).is_ok(),
                "中断后真视频应仍可正常打开（锁/deadline 无残留）"
            );
        } else {
            eprintln!("SKIP: missing target/video测试.mp4, 跳过真视频打开断言");
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_reads_params() {
    let r = VideoReader::open(&fixture_or_skip!()).unwrap();
    let p = r.params();
    assert_eq!((p.width, p.height), (320, 240));
    assert!((1.9..=2.5).contains(&p.duration), "duration={}", p.duration);
    assert!(p.frame_rate.is_some(), "应识别出帧率");
}

/// decode-to-display 下采样：长边封顶后保持宽高比（320×240 → 160×120），
/// 逐帧输出与内存按输出尺寸走；`params()` 仍报源宽高供显示适配。
#[test]
fn open_scaled_caps_longest_side_and_keeps_source_params() {
    let mut r = VideoReader::open_scaled(&fixture_or_skip!(), 160).unwrap();
    let p = r.params();
    assert_eq!((p.width, p.height), (320, 240), "params 保持源尺寸");
    let (pts, w, h, bgra) = r.next_frame_bgra().unwrap().expect("缩放后应出帧");
    assert!(pts >= 0.0);
    assert_eq!((w, h), (160, 120), "输出按长边封顶下采样");
    assert_eq!(bgra.len(), (160 * 120 * 4) as usize, "内存按输出尺寸分配");
}

/// 封顶 ≥ 源长边时与全尺寸一致（320×240 原样）。
#[test]
fn open_scaled_with_huge_cap_is_identity() {
    let r = VideoReader::open_scaled(&fixture_or_skip!(), 4096).unwrap();
    assert_eq!(r.params(), VideoReader::open(&fixture_or_skip!()).unwrap().params());
    let mut r = VideoReader::open_scaled(&fixture_or_skip!(), 4096).unwrap();
    let (w, h) = { let f = r.next_frame_bgra().unwrap().unwrap(); (f.1, f.2) };
    assert_eq!((w, h), (320, 240), "封顶超源应按源尺寸输出");
}

#[test]
fn scaled_dims_preserves_aspect_and_floor() {
    assert_eq!(scaled_dims(320, 240, 0), (320, 240), "max_dim=0 不限制");
    assert_eq!(scaled_dims(320, 240, u32::MAX), (320, 240));
    assert_eq!(scaled_dims(320, 240, 160), (160, 120), "等比减半");
    assert_eq!(scaled_dims(200, 100, 50), (50, 25), "等比 1/4");
    assert_eq!(scaled_dims(3, 2, 6), (3, 2), "界内原样");
    assert!((scaled_dims(1, 1, 0)).0 >= 1, "输出至少 1×1");
}

#[test]
fn next_frame_pts_are_monotonic() {
    let mut r = VideoReader::open(&fixture_or_skip!()).unwrap();
    let p = r.params();
    let mut last = -1.0f64;
    for i in 0..24 {
        let (pts, w, h, bgra) = r.next_frame_bgra().unwrap().expect("帧应可解码");
        assert!(pts >= last - 1e-9, "pts 单调（i={i}）");
        assert!((0.0..=p.duration + 0.1).contains(&pts), "pts 在时长内 i={i}");
        last = pts;
        assert_eq!((w, h), (320, 240));
        assert_eq!(bgra.len(), (320 * 240 * 4) as usize);
    }
}

#[test]
fn seek_lands_near_target() {
    let mut r = VideoReader::open(&fixture_or_skip!()).unwrap();
    r.seek(1.0).unwrap();
    let (pts, ..) = r.next_frame_bgra().unwrap().expect("seek 后应出帧");
    // 回拉关键帧最多落后一点；testsrc 全 intraframe 应贴近
    assert!((pts - 1.0).abs() < 0.15, "seek 落点偏差过大 pts={pts}");
}

#[test]
fn reaches_eof_then_seek_reuses() {
    let mut r = VideoReader::open(&fixture_or_skip!()).unwrap();
    let mut n = 0;
    while r.next_frame_bgra().unwrap().is_some() {
        n += 1;
        assert!(n < 200, "不该无限出帧");
    }
    assert!(n >= 50, "2s@30fps 应解出约 60 帧, 实得 {n}");
    // EOF 后 seek 仍可复用
    r.seek(0.0).unwrap();
    assert!(r.next_frame_bgra().unwrap().is_some(), "seek 回 0 应再出帧");
}

#[test]
fn rejects_missing_file() {
    let bad = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("no-such-file.mp4");
    assert!(VideoReader::open(&bad).is_err());
}

fn audio_fixture() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target")
        .join("video测试有声.mp4")
}

/// 缺音频夹具时 SKIP 不 FAIL（不入库，同 viewer 约定）；返回已存在的路径。
macro_rules! audio_fixture_or_skip {
    () => {{
        let p = audio_fixture();
        if !p.exists() {
            eprintln!("SKIP: missing target/video测试有声.mp4（音频夹具不入库，手动装配）");
            return;
        }
        p
    }};
}

#[test]
fn audio_open_and_decode() {
    let mut r = AudioReader::open_pcm_f32(&audio_fixture_or_skip!(), 48000).unwrap();
    let p = r.params();
    assert_eq!(p.channels, 2);
    assert_eq!(p.sample_rate, 48000);
    assert!((1.5..=3.0).contains(&p.duration), "duration={}", p.duration);
    let mut n = 0;
    let mut last = -1.0f64;
    let mut all: Vec<f32> = Vec::new();
    while let Some((pts, samples)) = r.next_pcm_f32().unwrap() {
        assert!(pts >= last - 1e-9, "音频 pts 单调");
        last = pts;
        assert_eq!(samples.len() % 2, 0, "立体声交错样本");
        assert!(
            samples.iter().all(|&s| s.is_finite() && s.abs() <= 2.0),
            "解码样本应有限且有界"
        );
        all.extend_from_slice(&samples);
        n += 1;
        assert!(n < 1000, "不该无限出音频块");
    }
    assert!(n > 5, "2s 音频应产出多块, 实得 {n}");
    // 全段 440Hz 正弦「形状」断言（AAC 编码会把电平压低，故不断言绝对幅度，
    // 只断言它是音调而非静态噪音）：过零率 ≈ 2×440/秒（静噪≈48000/秒），
    // 峰值与 RMS 有正弦特征（peak ≥ 1.4×rms）。
    let len = all.len();
    let rms = (all.iter().map(|s| s * s).sum::<f32>() / len as f32).sqrt();
    let peak = all.iter().fold(0.0f32, |a, &s| a.max(s.abs()));
    let zc = all.windows(2).filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0)).count();
    let secs = len as f32 / (2.0 * 48000.0);
    let zc_s = zc as f32 / secs;
    eprintln!("audio stats: rms={rms:.4} peak={peak:.4} zc/s={zc_s:.0}");
    assert!(
        (400.0..=1400.0).contains(&zc_s),
        "440Hz 音调过零率≈880/s，实得 {zc_s:.0}（静噪≈48000/s）"
    );
    assert!(
        peak > rms * 1.2 && rms > peak * 0.2,
        "正弦峰值/均方根应具正弦特征 (peak={peak:.4}, rms={rms:.4})"
    );
}

#[test]
fn audio_seek_lands_near_target() {
    let mut r = AudioReader::open_pcm_f32(&audio_fixture_or_skip!(), 48000).unwrap();
    r.seek(1.0).unwrap();
    let (pts, _) = r.next_pcm_f32().unwrap().expect("seek 后应出音频");
    assert!((pts - 1.0).abs() < 0.25, "音频 seek 落点偏差过大 pts={pts}");
}

/// 音频 reader 常驻复用（查看器音频 worker 复用姿势）：open 一次后连续 seek 回不同
/// 落点，每次重新起播首块 pts 落在目标附近、不重开文件；`AudioReader::seek` 在
/// 复位 demux/decoder 的同时冲掉 swr 内部残留（不残留旧段）。缺夹具 SKIP（音频
/// 夹具不入库，同 viewer 约定）。
#[test]
fn audio_seek_reuses_reader_across_targets() {
    let path = audio_fixture_or_skip!();
    let mut r = AudioReader::open_pcm_f32(&path, 48000).unwrap();
    for target in [0.1, 1.0, 0.35, 1.7] {
        r.seek(target).unwrap();
        let mut got: Option<(f64, Vec<f32>)> = None;
        for _ in 0..200 {
            match r.next_pcm_f32() {
                Ok(Some(x)) => {
                    got = Some(x);
                    break;
                }
                _ => break, // EOF / 错误
            }
        }
        let (pts, samples) = got.expect("seek 后应出音频（复用不重开）");
        assert!(
            (pts - target).abs() < 0.30,
            "音频复用 seek 首块落点偏离 target={target} pts={pts}"
        );
        assert!(!samples.is_empty(), "首块应有样本");
    }
}

#[test]
fn audio_rejects_missing() {
    let bad = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("no-such.mp4");
    assert!(AudioReader::open_pcm_f32(&bad, 48000).is_err());
}

/// 回归：重采样器内部残留必须被**真实排空**（复用 reader seek 前不清残留会把旧段
/// 夹进新起播）。ffmpeg-next `Context::flush` 对全空 out 帧会因 `swr_convert_frame`
/// 的 `config_changed` 直接报 `Output changed`、一次都排不出——按输出规格预分配帧
/// 后（[`AudioReader::flush_resampler`]）才排出；终止判据用「产出样本数」，不能用
/// `delay()`（排空后 `swr_get_delay` 仍非零，按它判死循环）。用 out_rate≠源率强制
/// 真实重采样制造残留。缺夹具 SKIP（同 viewer 约定）。
#[test]
fn audio_drain_clears_resampler_residue() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/视频测试.mp4");
    if !path.exists() {
        eprintln!("SKIP: missing target/视频测试.mp4（夹具不入库）");
        return;
    }
    // 44100 ≠ 源率（48k）→ 重力采样，读几块必有内部残留
    let mut r = AudioReader::open_pcm_f32(&path, 44100).unwrap();
    let mut got_block = false;
    for _ in 0..5 {
        if r.next_pcm_f32().unwrap().is_none() {
            break;
        }
        got_block = true;
    }
    assert!(got_block, "夹具应能解码出音频块");
    let delay_before = r.resampler.delay();
    assert!(delay_before.is_some(), "真实重采样必然积累内部残留 delay={delay_before:?}");
    let drained = r.flush_resampler();
    assert!(
        !drained.is_empty(),
        "残留应被真实排空（空帧 flush 会 Err 落空，drain 长度应为 0 之前）len={}",
        drained.len()
    );
    assert!(
        drained.len() <= 2 * 44100,
        "返回长度应截到实际产出（planes 全容量未定段不得带出），len={}",
        drained.len()
    );
    let delay_after = r.resampler.delay();
    // 排空后不应再有余量可排（下一次 flush 产出 0）——以再排一次产出为判据
    let mut out = ffmpeg::frame::Audio::new(
        r.resampler.output().format,
        r.resampler.output().rate as usize,
        r.resampler.output().channel_layout,
    );
    let more = match r.resampler.flush(&mut out) {
        Ok(Some(_)) if out.samples() > 0 => out.samples(),
        _ => 0,
    };
    assert_eq!(more, 0, "排空后再 flush 不应有产出");
    let _ = delay_after; // 提示值：排空后 delay 仍可能非零（引擎内插延迟），不作断言
    // 排空后读取仍正常（不破坏后续解码）
    assert!(r.next_pcm_f32().is_ok(), "排空后读取应当正常");
}

/// 回归（上采样容量截断）：ffmpeg-next `resampler.run` 对空输出帧按 `input.samples()`
/// 分配容量（resampling/context.rs `run`），上采样（out_rate>in_rate）时 swr 每块最多
/// 写出输入样本数 → 丢尾 → 播放偏快 + 块边界截断杂音（44.1k 源 -> 48k 设备实测复现，
/// 音乐播放器 / 高采样率设备的视频共用此路径）。修复：`next_pcm_f32` 预先按速率比例
/// 配足输出帧容量。本测试把夹具 48k 音轨按 96k 解码数秒，断言隐含输出率≈96k
/// （修复前恒 ≈48k → FAIL；缺夹具 SKIP，同 viewer 约定）。
#[test]
fn audio_upsample_outputs_requested_rate() {
    const OUT_RATE: u32 = 96_000;
    let path = audio_fixture_or_skip!();
    let mut r = AudioReader::open_pcm_f32(&path, OUT_RATE).unwrap();
    let p = r.params();
    assert_eq!(p.channels, 2, "应统一立体声");
    // 解到 ≥4 秒即停（全曲解码对夹具太慢，测速足够）
    let mut total_out = 0usize;
    let mut blocks = 0usize;
    let mut last_pts = 0.0f64;
    loop {
        match r.next_pcm_f32() {
            Ok(Some((pts, samples))) => {
                total_out += samples.len();
                blocks += 1;
                last_pts = pts;
                if pts >= 4.0 {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => panic!("解码失败: {e}"),
        }
    }
    assert!(blocks > 0, "应能解码出音频块");
    assert!(last_pts >= 1.0, "应至少解码到 ≥1s（实际 {last_pts:.2}s）");
    let implied = (total_out / 2) as f64 / last_pts;
    let rel = (implied - f64::from(OUT_RATE)).abs() / f64::from(OUT_RATE);
    eprintln!(
        "audio {OUT_RATE}: blocks={blocks} total_samples={total_out} reached={last_pts:.2}s implied_rate={implied:.1} rel={rel:.4}"
    );
    assert!(
        rel < 0.02,
        "上采样输出率应≈{OUT_RATE}，实测 {implied:.1}（上采样截断未修，播放偏快+杂音）"
    );
}

/// 双路径（硬件优先 / 软件回退）都必须产出尺寸正确、alpha 不透明的 BGRA 帧。
#[test]
fn hw_or_sw_path_both_output_valid_bgra() {
    let mut r = VideoReader::open_scaled(&fixture_or_skip!(), 160).unwrap();
    eprintln!(
        "media-ffmpeg 解码路径: {}",
        if r.hw.is_some() { "硬件" } else { "软件回退" }
    );
    let (pts, w, h, bgra) = r.next_frame_bgra().unwrap().expect("帧应可解码");
    assert!(pts >= 0.0);
    assert_eq!((w, h), (160, 120));
    assert_eq!(bgra.len(), 160 * 120 * 4);
    assert!(
        bgra.as_chunks::<4>().0.iter().all(|px| px[3] == 255),
        "sws 输出应全不透明"
    );
}

/// 仅硬件路径（无软件回退）：本机支持硬件解码时逐帧解码 + seek 全通过；
/// 不支持（无 GPU / 驱动 / 编解码器）则打印跳过，避免 CI 误红。
#[test]
fn hw_only_path_decodes_when_available() {
    match VideoReader::open_hw_only(&fixture_or_skip!(), u32::MAX) {
        Ok(mut r) => {
            assert!(r.hw.is_some(), "open_hw_only 必须启用硬件");
            for _ in 0..16 {
                let (pts, w, h, bgra) =
                    r.next_frame_bgra().unwrap().expect("硬件帧应可解码");
                assert!(pts >= 0.0);
                assert_eq!((w, h), (320, 240), "硬件路径输出应按源尺寸");
                assert_eq!(bgra.len(), 320 * 240 * 4);
            }
            r.seek(1.0).unwrap();
            assert!(
                r.next_frame_bgra().unwrap().is_some(),
                "硬件路径 seek 后应出帧"
            );
        }
        Err(e) => eprintln!("本机硬件解码不可用，跳过: {e:#}"),
    }
}

// ---------------- 解码吞吐 / 拆账基准（VFF_BENCH_* 环境, `--ignored` 手动跑） ----------------

const ST_DEMUX: usize = 0;
const ST_DECODE: usize = 1;
const ST_TRANSFER: usize = 2;
const ST_SWS: usize = 3;
const ST_COPY: usize = 4;

/// 基准文件：`VFF_BENCH_VIDEO` 指定，缺省仓库 `target/视频测试.mp4`（1280x720 h264@30）。
fn bench_candidate() -> std::path::PathBuf {
    std::env::var("VFF_BENCH_VIDEO")
        .ok()
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_file())
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/视频测试.mp4")
        })
}

/// 生产路径逐帧墙钟（`next_frame_bgra_into` 原样调用）：
/// 报 min / 中位 / 均值 / p95 ms·帧 + 统计窗口 fps + 「忙核@30fps」估算（均值 ms×30/1000）。
/// 前 `warmup` 帧剔除（scaler / 硬件设备预热）；文件短于目标帧数时 EOF→seek(0) 复跑凑数。
fn bench_leg_canonical(name: &str, mut r: VideoReader, frames: u32, warmup: u32) {
    let cap = (r.out_width as usize) * (r.out_height as usize) * 4;
    let mut out = vec![0u8; cap];
    let mut times: Vec<f64> = Vec::with_capacity(frames as usize);
    let mut n: u32 = 0;
    let mut win_start: Option<std::time::Instant> = None;
    while n < frames {
        let t = std::time::Instant::now();
        match r.next_frame_bgra_into(&mut out) {
            Ok(Some(_)) => {
                let dt = t.elapsed().as_secs_f64() * 1e3;
                n += 1;
                if n > warmup {
                    if win_start.is_none() {
                        win_start = Some(std::time::Instant::now());
                    }
                    times.push(dt);
                }
            }
            Ok(None) => r.seek(0.0).unwrap(),
            Err(e) => panic!("{name} 帧解码失败: {e:#}"),
        }
    }
    match win_start {
        Some(w0) => {
            let win = (std::time::Instant::now().duration_since(w0).as_secs_f64()).max(1e-6);
            let keep = times.len();
            let mut sorted = times.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let (min, med, p95) = (sorted[0], sorted[keep / 2], sorted[(keep as f64 * 0.95) as usize]);
            let mean = times.iter().sum::<f64>() / keep as f64;
            eprintln!(
                "[BENCH] {name}: 采样={keep}帧 min={min:.3} 中位={med:.3} 均值={mean:.3} p95={p95:.3} ms/帧 | {:.0} fps | 忙核@30fps≈{:.2}",
                keep as f64 / win,
                mean * 30.0 / 1000.0
            );
        }
        None => eprintln!("[BENCH] {name}: 采样帧数不足（frames={frames} warmup={warmup}）"),
    }
}

/// 逐帧拆账：镜像 [`VideoReader::next_frame_bgra_into`]+[`VideoReader::convert_bgra_into`] 的
/// 主循环（测试模块可访问私有字段），把每帧 CPU 墙钟按 demux / decode / transfer / sws / copy
/// 累加后除以「产出帧数」报均值。软件路径 transfer 恒 0。
///
/// ⚠️ 硬解 decode 墙钟常被 GPU 异步缓冲吸收（D3D11VA 提交即返回，真实成本滞后到回读
/// `av_hwframe_transfer_data` 的等待上）——故 decode/transfer 分开看，净值看 canonical 全帧。
fn bench_leg_breakdown(name: &str, mut r: VideoReader, frames: u32) -> anyhow::Result<()> {
    let cap = (r.out_width as usize) * (r.out_height as usize) * 4;
    let mut out = vec![0u8; cap];
    // 常驻 BGRA 输出帧（镜像生产 `VideoReader::bgra_out` 的懒建一次姿势，R2-1——拆账的
    // sws 阶段不再含每帧 av_frame_get_buffer 分配，与生产路径逐字一致）
    let mut bgra = ffmpeg::frame::Video::new(
        ffmpeg::format::Pixel::BGRA,
        r.out_width,
        r.out_height,
    );
    let mut stage = [0.0f64; 5];
    let mut n: u32 = 0;
    while n < frames {
        let t = std::time::Instant::now();
        let packet = r.next_video_packet();
        stage[ST_DEMUX] += t.elapsed().as_secs_f64();
        let packet = match packet {
            Some(p) => p,
            None => {
                if !r.drained {
                    r.decoder.send_eof().ok();
                    r.drained = true;
                }
                let mut f = ffmpeg::frame::Video::empty();
                loop {
                    let t = std::time::Instant::now();
                    let ok = r.decoder.receive_frame(&mut f).is_ok();
                    stage[ST_DECODE] += t.elapsed().as_secs_f64();
                    if !ok {
                        break;
                    }
                    if bench_convert_stage(&mut r, &f, &mut out, &mut bgra, &mut stage)?.is_some() {
                        n += 1;
                    }
                }
                r.seek(0.0).unwrap();
                continue;
            }
        };
        let t = std::time::Instant::now();
        r.decoder
            .send_packet(&packet)
            .context("send_packet 失败")?;
        stage[ST_DECODE] += t.elapsed().as_secs_f64();
        let mut f = ffmpeg::frame::Video::empty();
        loop {
            let t = std::time::Instant::now();
            let ok = r.decoder.receive_frame(&mut f).is_ok();
            stage[ST_DECODE] += t.elapsed().as_secs_f64();
            if !ok {
                break;
            }
            if bench_convert_stage(&mut r, &f, &mut out, &mut bgra, &mut stage)?.is_some() {
                n += 1;
            }
        }
    }
    let per = |s: f64| s / n as f64 * 1e3;
    eprintln!(
        "[BENCH] {name} 拆账 ({n}帧): demux={:.3} decode={:.3} transfer={:.3} sws={:.3} copy={:.3} ms/帧",
        per(stage[ST_DEMUX]),
        per(stage[ST_DECODE]),
        per(stage[ST_TRANSFER]),
        per(stage[ST_SWS]),
        per(stage[ST_COPY])
    );
    Ok(())
}

/// 拆账用的转换阶段（镜像 [`VideoReader::convert_bgra_into`] 计时）：HW opaque 帧先回读
/// （`av_hwframe_transfer_data`），再 sws 转 BGRA，再拷入输出缓冲。
fn bench_convert_stage(
    r: &mut VideoReader,
    f: &ffmpeg::frame::Video,
    out: &mut [u8],
    bgra: &mut ffmpeg::frame::Video,
    stage: &mut [f64; 5],
) -> anyhow::Result<Option<(f64, u32, u32)>> {
    let Some(pts) = f.pts() else {
        return Ok(None);
    };
    if r.hw.is_some() && is_hw_frame(f) {
        let mut sw = ffmpeg::frame::Video::empty();
        let t = std::time::Instant::now();
        let rc =
            unsafe { ffmpeg::ffi::av_hwframe_transfer_data(sw.as_mut_ptr(), f.as_ptr(), 0) };
        if rc < 0 {
            return Err(anyhow!("硬件帧回读失败 (err={rc})"));
        }
        stage[ST_TRANSFER] += t.elapsed().as_secs_f64();
        let fmt = sw.format();
        let (w, h) = (sw.width(), sw.height());
        let t = std::time::Instant::now();
        scaler_for(
            &mut r.scaler,
            fmt,
            w,
            h,
            r.out_width,
            r.out_height,
        )?
        .run(&sw, bgra)
        .context("sws 转换失败")?;
        stage[ST_SWS] += t.elapsed().as_secs_f64();
    } else {
        let fmt = f.format();
        let (w, h) = (f.width(), f.height());
        let t = std::time::Instant::now();
        scaler_for(
            &mut r.scaler,
            fmt,
            w,
            h,
            r.out_width,
            r.out_height,
        )?
        .run(f, bgra)
        .context("sws 转换失败")?;
        stage[ST_SWS] += t.elapsed().as_secs_f64();
    }
    let need = (r.out_width as usize) * (r.out_height as usize) * 4;
    let t = std::time::Instant::now();
    out[..need].copy_from_slice(&bgra.data(0)[..need]);
    stage[ST_COPY] += t.elapsed().as_secs_f64();
    let secs = r.pts_seconds(pts);
    Ok(Some((secs, r.out_width, r.out_height)))
}

/// 本机硬件解码设备可用性探针（`--ignored` 手动跑）：逐个尝试建 D3D11VA / DXVA2 设备上下文，
/// 打印哪个成功（打开媒体逻辑按此顺序优先启用）。输出即证据。
#[test]
#[ignore = "手动测量用"]
fn probe_hw_device() {
    use ffmpeg::ffi::{
        AVHWDeviceType, av_buffer_unref, av_hwdevice_ctx_create,
    };
    let names = [
        (AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA, "D3D11VA"),
        (AVHWDeviceType::AV_HWDEVICE_TYPE_DXVA2, "DXVA2"),
    ];
    for (t, name) in names {
        let mut p: *mut ffmpeg::ffi::AVBufferRef = std::ptr::null_mut();
        let rc = unsafe {
            av_hwdevice_ctx_create(&mut p, t, std::ptr::null(), std::ptr::null_mut(), 0)
        };
        eprintln!("[HWPROBE] {name}: av_hwdevice_ctx_create rc={rc} ok={}", !p.is_null());
        if !p.is_null() {
            unsafe { av_buffer_unref(&mut p); }
        }
    }
}

/// 解码吞吐 / 拆账测量（非回归，默认忽略手动跑）。
///
/// 软硬件对照：软路径强制软件（`open_scaled_impl(.., false)`，无视 `VFF_VIDEO_HW` 环境）；
/// 硬路径走 `open_hw_only`（force，无软回退，不可用即打印证据并跳过硬解对照）。
///
/// 环境：
/// - `VFF_BENCH_VIDEO` 基准文件（缺省 `target/视频测试.mp4`）
/// - `VFF_BENCH_FRAMES` 采样帧数（默认 240，剔除前 20 帧预热；文件短→EOF seek 复跑）
/// - `VFF_BENCH_PATH=sw|hw` 限定单路径；缺省软硬都跑（软先硬后，按外部进程采样内存用）
#[test]
#[ignore = "手动测量用"]
fn bench_decode_throughput() -> anyhow::Result<()> {
    let candidate = bench_candidate();
    let frames: u32 = std::env::var("VFF_BENCH_FRAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(240);
    let warmup: u32 = 20.min(frames.saturating_sub(1));
    let mode = std::env::var("VFF_BENCH_PATH").unwrap_or_default();
    let info = |r: &VideoReader| {
        eprintln!(
            "[BENCH] 文件={} 源尺寸={}x{} 输出={}x{} 帧率={:?} 目标帧数={frames} warmup={warmup}",
            candidate.display(),
            r.params().width,
            r.params().height,
            r.out_width,
            r.out_height,
            r.params().frame_rate,
        );
    };
    if mode.is_empty() || mode == "sw" {
        // 软路径确定性强制软解（无视环境里可能残留的 VFF_VIDEO_HW；ignore 手动跑，进程内自控）
        // SAFETY: `--ignored` 手动/串行诊断用，无并行测试竞争该 env。
        unsafe { std::env::set_var("VFF_VIDEO_SW", "1") };
        let r = VideoReader::open_scaled_impl(&candidate, 1920, false)
            .context("软路径打开失败")?;
        unsafe { std::env::remove_var("VFF_VIDEO_SW") };
        assert!(r.hw.is_none(), "软路径必须无硬件（VFF_VIDEO_SW=1 强制软解）");
        info(&r);
        bench_leg_canonical("软解", r, frames, warmup);
        unsafe { std::env::set_var("VFF_VIDEO_SW", "1") };
        let r2 = VideoReader::open_scaled_impl(&candidate, 1920, false)
            .context("软路径拆账重开失败")?;
        unsafe { std::env::remove_var("VFF_VIDEO_SW") };
        bench_leg_breakdown("软解", r2, frames)?;
    }
    if mode.is_empty() || mode == "hw" {
        match VideoReader::open_hw_only(&candidate, 1920) {
            Ok(r) => {
                assert!(r.hw.is_some(), "open_hw_only 必须启用硬件");
                info(&r);
                bench_leg_canonical("硬解", r, frames, warmup);
                match VideoReader::open_hw_only(&candidate, 1920) {
                    Ok(r2) => bench_leg_breakdown("硬解", r2, frames)?,
                    Err(e) => eprintln!("硬解拆账重开失败: {e:#}"),
                }
            }
            Err(e) => eprintln!(
                "本机硬件解码不可用，跳过硬解对照: {e:#}\n证据：open_hw_only(path,1920) 返回 Err"
            ),
        }
    }
    Ok(())
}

/// B1 瘦包接管性能探针（`--ignored` 手动跑，不进常规测试计数）：gyan 全量 ↔ slim 瘦包
/// 的纯解码吞吐对比。完整走一遍真实解码链路——`VideoReader::open_scaled`（默认软解，
/// `HW_DEFAULT=false` 确定性，正是不受 GPU 影响的 CPU 解码成本）→ 全流
/// `next_frame_bgra_into`（demux→decode→sws→BGRA）到 EOF，`std::time::Instant` 记
/// 整段墙钟，输出帧数 + fps + 源信息；同一文件连续复跑 3 次取中位（规避首开热缓存 /
/// 偶发抖动），供 gyan↔slim 对比——重点量化 `--enable-small` 是否带来可感代价。
///
/// 夹具：`target\视频测试.mp4`（1280x720 h264@30，≈311 帧 / 10.2s）；缺失回退
/// `target\video测试.mp4`（320x240 短样，信号弱）；再缺失打 SKIP 不硬造。
#[test]
#[ignore = "B1 手动性能探针"]
fn bench_b1_decode_gyan_vs_slim() -> anyhow::Result<()> {
    use std::time::Instant;

    let primary = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/视频测试.mp4");
    let fallback = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/video测试.mp4");
    let candidate = if primary.exists() {
        primary
    } else if fallback.exists() {
        eprintln!("[B1PROBE] 视频测试.mp4 缺失，回退 video测试.mp4（帧数少、对比信号弱）");
        fallback
    } else {
        eprintln!("SKIP: missing target/视频测试.mp4 与 target/video测试.mp4（媒体夹具不入库）");
        return Ok(());
    };

    // 单次完整解码：open → streams → video frames → swscale 转 BGRA（贴近真实链路）。
    let run = |i: usize| -> anyhow::Result<(u64, f64, f64)> {
        let t0 = Instant::now();
        let mut r = VideoReader::open_scaled(&candidate, 1920).context("打开失败")?;
        let open_ms = t0.elapsed().as_secs_f64() * 1e3;
        let p = r.params();
        let cap = (r.out_width as usize * r.out_height as usize * 4).max(1);
        let mut out = vec![0u8; cap];
        let t1 = Instant::now();
        let mut n: u64 = 0;
        while r.next_frame_bgra_into(&mut out)?.is_some() {
            n += 1;
        }
        let wall_s = t1.elapsed().as_secs_f64();
        let fps = n as f64 / wall_s;
        eprintln!(
            "[B1PROBE] run{i}: 帧数={n} wall={wall_s:.3}s fps={fps:.1} open={open_ms:.1}ms {} {}x{} hw={}",
            candidate.file_name().unwrap().to_string_lossy(),
            p.width,
            p.height,
            if r.hw.is_some() { "是" } else { "否" }
        );
        Ok((n, wall_s, fps))
    };

    let (mut frames, mut walls, mut fps) = (Vec::new(), Vec::new(), Vec::new());
    for i in 0..3 {
        let (n, w, f) = run(i)?;
        frames.push(n);
        walls.push(w);
        fps.push(f);
    }
    fps.sort_by(|a, b| a.partial_cmp(b).unwrap());
    walls.sort_by(|a, b| a.partial_cmp(b).unwrap());
    eprintln!(
        "[B1PROBE] MEDIAN: 帧数={} wall={:.3}s fps={:.1}（3 次取中位）",
        frames[1], walls[1], fps[1]
    );
    Ok(())
}

/// seek 预滚成本对照（SW vs HW，`--ignored` 手动跑）：长 GOP 中段落点 seek 后解码到
/// 「首个 pts ≥ 落点」帧的 wall clock 与预滚帧数——即「点击进度条后 seek2first」的
/// 解码阶段子项。软路径经 `VFF_VIDEO_SW=1` 强制（默认硬解的机器上也确定性走软解）；
/// 硬路径走 `open_hw_only`（force，无软回退）。断言首个命中帧输出有效（非全黑/纯色，
/// 只测能量不写内容——夹具敏感）。缺夹具 SKIP（`target\视频测试字幕.mkv` 不入库）。
#[test]
#[ignore = "手动测量用"]
fn bench_seek_preroll_hw_vs_sw() -> anyhow::Result<()> {
    use std::time::Instant;

    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/视频测试字幕.mkv");
    if !path.exists() {
        eprintln!("SKIP: missing target/视频测试字幕.mkv（长 GOP 夹具不入库）");
        return Ok(());
    }
    // 落点：开头近距 / 长 GOP 中段最坏档（≈停下 6-15s 长帧距）/ 两处中段 / 近结尾
    let targets = [12.0, 90.0, 333.3, 601.7, 900.1, 1200.5, 1500.3, 1766.0];
    // SAFETY: 探针为 `--ignored` 手动/串行诊断用，无并行测试竞争该 env；跑完还原。
    unsafe { std::env::set_var("VFF_VIDEO_SW", "1") };
    let t0 = Instant::now();
    let sw = VideoReader::open_scaled_impl(&path, 1920, false)
        .context("软路径打开失败")?;
    let sw_open_ms = t0.elapsed().as_secs_f64() * 1e3;
    unsafe { std::env::remove_var("VFF_VIDEO_SW") };
    assert!(sw.hw.is_none(), "VFF_VIDEO_SW=1 软路径必须无硬件");

    let hw = match VideoReader::open_hw_only(&path, 1920) {
        Ok(r) => {
            assert!(r.hw.is_some(), "open_hw_only 必须启用硬件");
            Some(r)
        }
        Err(e) => {
            eprintln!("[SEEKPREROLL] 硬解不可用，仅软解对照: {e:#}");
            None
        }
    };

    eprintln!(
        "[SEEKPREROLL] 文件={} 硬解可用={}",
        path.display(),
        hw.is_some()
    );

    // 单腿测量：8 落点 seek→首帧 ≥ 落点。返回每个落点的 (首帧 pts, 预滚帧数, wall, energy)。
    let measure = |mut r: VideoReader, name: &str| -> anyhow::Result<Vec<(f64, f64, f64, f64)>> {
        let mut rows = Vec::with_capacity(targets.len());
        for &t in &targets {
            r.seek(t)?;
            let cap = (r.out_width as usize) * (r.out_height as usize) * 4;
            let mut out = vec![0u8; cap];
            let t1 = Instant::now();
            let mut n: u64 = 0;
            let mut first: f64 = -1.0;
            let mut energy = 0.0;
            loop {
                n += 1;
                match r.next_frame_bgra_into(&mut out)? {
                    Some((pts, ..)) if pts >= t => {
                        first = pts;
                        energy = frame_energy(&out, r.out_width, r.out_height);
                        break;
                    }
                    Some(_) => continue, // 预滚帧只解不取
                    None => break,       // 近结尾 EOF 且无 ≥ 落点帧：认首帧缺席
                }
            }
            let wall_ms = t1.elapsed().as_secs_f64() * 1e3;
            eprintln!(
                "[SEEKPREROLL] {name} target={t:.1}s → 首帧 pts={first:.2}s 预滚帧={n} seek2first={wall_ms:.1}ms energy={energy:.1}",
            );
            rows.push((t, first, wall_ms, energy));
        }
        Ok(rows)
    };

    let t0 = Instant::now();
    let sw_rows = measure(sw, "SW")?;
    eprintln!(
        "[SEEKPREROLL] SW 整体 wall={:.1}ms（open={sw_open_ms:.1}ms + 8 落点 seek+预滚）",
        t0.elapsed().as_secs_f64() * 1e3
    );
    if let Some(hw) = hw {
        let t0 = Instant::now();
        let hw_rows = measure(hw, "HW")?;
        eprintln!(
            "[SEEKPREROLL] HW 整体 wall={:.1}ms（含 8 落点 seek+预滚）",
            t0.elapsed().as_secs_f64() * 1e3
        );
        // 内容有效性用「软硬同落点 energy 对照」判：真实暗场两路都低（相互印证），
        // 任一路独高另一路独低 → 可疑花屏/黑帧（GPU 回读垃圾）。
        for (i, swt) in sw_rows.iter().enumerate() {
            let (t, sw_e, hw_e) = (swt.0, swt.3, hw_rows[i].3);
            let both_low = sw_e < 3.0 && hw_e < 3.0; // 双方都判暗场 → 一致，可信
            if !both_low {
                assert!(
                    (sw_e - hw_e).abs() < 30.0 || (sw_e > 3.0 && hw_e > 3.0),
                    "target={t}s 软硬 energy 背离（SW={sw_e:.1} HW={hw_e:.1}）疑似花屏/黑帧"
                );
            }
        }
    }
    Ok(())
}

/// 探针：拖拽快扫「只解关键帧（skip_frame=NONKEY）」vs「整段预滚精确解」的解码帧数与
/// 墙钟——视频解码引擎负载与解码帧数成正比；长 GOP 文件一次精确 seek 解近百帧，NONKEY
/// 只需 1 帧（对应 VLC 快扫只解关键帧）。断 NONKEY 恰好 1 帧，同时是机制回归测试。
#[test]
fn bench_scrub_preview_skip_nonkey_vs_precise() -> anyhow::Result<()> {
    use std::time::Instant;

    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/视频测试字幕.mkv");
    if !path.exists() {
        eprintln!("SKIP: missing target/视频测试字幕.mkv（长 GOP 夹具不入库）");
        return Ok(());
    }
    const TARGET: f64 = 176.0; // 中段长帧距档（预滚帧数最大档位之一）
    // SAFETY: 探针为串行诊断用，无并行测试竞争该 env；跑完还原。
    unsafe { std::env::set_var("VFF_VIDEO_SW", "1") };
    let mut sw =
        VideoReader::open_scaled_impl(&path, 1920, false).context("软路径打开失败")?;
    unsafe { std::env::remove_var("VFF_VIDEO_SW") };
    let cap = (sw.out_width as usize) * (sw.out_height as usize) * 4;
    let mut out = vec![0u8; cap];

    let mut measure = |sw: &mut VideoReader, nonkey: bool| -> anyhow::Result<(u64, f64)> {
        sw.set_skip_nonkey(nonkey);
        sw.seek(TARGET)?;
        let t1 = Instant::now();
        let mut n: u64 = 0;
        loop {
            n += 1;
            match sw.next_frame_bgra_into(&mut out)? {
                // 精确：首帧 ≥ TARGET 落点；快扫：唯一 I 帧（pts ≤ TARGET）即落点
                Some((pts, ..)) if pts >= TARGET || nonkey => break,
                Some(_) => continue, // 预滚帧只解不取
                None => break,       // 片尾 EOF 无 I 帧
            }
        }
        Ok((n, t1.elapsed().as_secs_f64() * 1e3))
    };

    let (p_n, p_ms) = measure(&mut sw, false)?;
    let (k_n, k_ms) = measure(&mut sw, true)?;
    eprintln!(
        "[SCRUBPREV] target={}s 精确(全帧): {p_n}帧 {p_ms:.1}ms ｜ 快扫(仅关键帧): {k_n}帧 {k_ms:.1}ms ｜ 帧数比≈{:.1}× 墙钟比≈{:.1}×",
        TARGET,
        p_n as f64 / k_n as f64,
        p_ms / k_ms
    );
    assert_eq!(k_n, 1, "skip_frame=NONKEY 应只解 1 个关键帧（got {k_n}）");
    Ok(())
}

/// BGRA 帧内容能量（粗判非黑/非花屏）：等距抽样像素算平均绝对偏差；真实画面远高于
/// 纯黑/纯色的 ~0。只判「帧有效」，不含具体内容断言（夹具敏感）。
fn frame_energy(bgra: &[u8], w: u32, h: u32) -> f64 {
    let px = w as usize * h as usize;
    if px == 0 {
        return 0.0;
    }
    const STEP: usize = 8;
    let n = (px / STEP).max(1);
    let mut samples: Vec<i64> = Vec::with_capacity(n);
    for i in (0..px).step_by(STEP) {
        let b = i * 4;
        samples.push((bgra[b] as i64 + bgra[b + 1] as i64 + bgra[b + 2] as i64) / 3);
    }
    let mean = samples.iter().sum::<i64>() as f64 / samples.len() as f64;
    samples
        .iter()
        .map(|s| (*s as f64 - mean).abs())
        .sum::<f64>()
        / samples.len() as f64
}

/// 回归：HE-AAC(MKV) 反复 seek（等价反复点进度条）——seek 落点非 AAC 帧对齐 + MKV
/// demux 落点可大幅回退（本夹具实测首块可落后目标 ≈-12s）+ HE-AAC SBR 断链坏帧
///（`env_facs_q ... is invalid`）。修复前：起播音在落点前（预滚整段被播放/越位），
/// 边界坏帧刷屏且可能断流。断言：seek 后首块必须 ≥ 落点且落在落点后一帧内
///（[`AudioReader::skip_until`] 丢弃落点前包/坏帧）、样本有限非空、逐块 pts 单调
/// 不回头。缺夹具 SKIP（`target\视频测试字幕.mkv` 不入库）。
#[test]
fn he_aac_mkv_seek_skips_preroll_and_starts_clean() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/视频测试字幕.mkv");
    if !path.exists() {
        eprintln!("SKIP: missing target/视频测试字幕.mkv（夹具不入库，本机无 HE-AAC MKA 样例）");
        return;
    }
    let mut r = AudioReader::open_pcm_f32(&path, 48000).unwrap();
    // 覆盖：开头 / 中部 / 小数字 / 近结尾（避开 EOF 边缘），含来回跳
    let targets = [5.0, 123.456, 500.0, 999.9, 1300.5, 1650.0];
    for &t in &targets {
        r.seek(t).unwrap();
        let mut first: Option<f64> = None;
        let mut last = -1.0f64;
        let mut n = 0u32;
        for _ in 0..60 {
            match r.next_pcm_f32().unwrap_or_else(|e| panic!("seek {t} 读取失败: {e:#}")) {
                Some((pts, samples)) => {
                    assert!(
                        pts >= last - 1e-9,
                        "seek {t} 后逐块 pts 应单调不回头: pts={pts} last={last}"
                    );
                    last = pts;
                    if first.is_none() {
                        first = Some(pts);
                        assert!(
                            pts >= t - 1e-6,
                            "seek {t} 后首块必须 ≥ 落点（否则落点前预滚被播放/越位），实得 {pts}"
                        );
                        assert!(
                            (pts - t).abs() < 0.1,
                            "seek {t} 后首块应落在落点后一帧内（warm 起播），实得 {pts}"
                        );
                        assert!(
                            !samples.is_empty() && samples.iter().all(|s| s.is_finite()),
                            "seek {t} 后首块样本应非空且有限（clean 帧，非 SBR 坏帧）len={}",
                            samples.len()
                        );
                    }
                    n += 1;
                    if n >= 6 {
                        break;
                    }
                }
                None => break, // 近结尾：落点后已无块，接受
            }
        }
        assert!(n >= 3, "seek {t} 后应连续读出多块，实得 {n}");
    }
}

/// 回归（A2）：**普通打开从头播放**（不经 seek）也套用 skip 口径——流头若带 `pts<0`
/// 包/首个坏 AAC 帧，须在输出前丢弃到第一个 clean 帧，正常打开 stderr 不刷
/// `env_facs_q`。断言：open 后首块 pts ≥ 0、样本有限非空、逐块 pts 单调。缺夹具 SKIP。
#[test]
fn open_from_start_skips_head_noise_first_frame_clean() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/视频测试字幕.mkv");
    if !path.exists() {
        eprintln!("SKIP: missing target/视频测试字幕.mkv（夹具不入库）");
        return;
    }
    let mut r = AudioReader::open_pcm_f32(&path, 48000).unwrap();
    let mut last = -1.0f64;
    let mut first: Option<f64> = None;
    let mut n = 0u32;
    for _ in 0..12 {
        match r.next_pcm_f32().unwrap() {
            Some((pts, samples)) => {
                assert!(pts >= last - 1e-9, "open 从头播放逐块 pts 应单调: pts={pts} last={last}");
                last = pts;
                if first.is_none() {
                    first = Some(pts);
                    assert!(pts >= -1e-6, "open 首块 pts 不应为负（负时间戳包被丢弃）: {pts}");
                    assert!(
                        !samples.is_empty() && samples.iter().all(|s| s.is_finite()),
                        "open 首块样本应非空且有限（clean 帧）"
                    );
                }
                n += 1;
            }
            None => break,
        }
    }
    assert!(n >= 3, "open 后应连续读出多块，实得 {n}");
}

/// 回归（A3）：真实 MKV 内封 ASS 提取。[`SubtitleReader::open`] 应从
/// `target/视频测试字幕.mkv`（ASS 流 index=2，codec=ass）按「Matroska 新块格式 +
/// 包 PTS/duration」提取字幕：首条 start≈20.05s、end≈22.03s、文本非空且无 ASS 标签。
/// 缺夹具 SKIP（夹具不入库，与 A1/A2 同口径，SKIP 是常态不是回归）。
#[test]
fn subtitle_reader_extracts_ass_mkv() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/视频测试字幕.mkv");
    if !path.exists() {
        eprintln!("SKIP: missing target/视频测试字幕.mkv（夹具不入库）");
        return;
    }
    let r = SubtitleReader::open(&path).unwrap();
    let subs = r.subtitles();
    assert!(
        !subs.is_empty(),
        "应从 MKV 提取到字幕，实得 {} 条（txt 应非空）",
        subs.len()
    );
    let first = &subs[0];
    assert!(
        (first.start_pts - 20.05).abs() < 0.05,
        "首条 start_pts 应≈20.05s，实得 {:.4}",
        first.start_pts
    );
    assert!(
        (first.end_pts - 22.03).abs() < 0.1,
        "首条 end_pts 应≈22.03s，实得 {:.4}",
        first.end_pts
    );
    assert!(
        !first.text.is_empty(),
        "首条文本应非空（纯文本已剥离 ASS 标签），实得 {:?}",
        first.text
    );
    assert!(
        !first.text.contains("<font") && !first.text.contains("<b"),
        "首条文本不应含 ASS 标签，实得 {:?}",
        first.text
    );
    // 活动字幕命中：位于 [start, end] 内应选中该条，之外不应命中
    assert!(
        r.subtitle_at(20.5).is_some(),
        "20.5s 应命中活动字幕（首条 [20.05, 22.03]）"
    );
}

/// 对拍（评审 SUGGESTION-5 修复）：重叠窗口（同一秒多条活跃）取「最近开始」——与
/// `video` crate 生产 `find_active_subtitle` 同口径（旧线性首命中 = 最早开始，不一致）。
/// 锁死该语义 + 纯函数与 reader 读口（`subtitle_at` 委托同一实现）不漂移。
#[test]
fn active_subtitle_index_prefers_latest_start_match() {
    let subs = vec![
        SubtitleEntry { start_pts: 0.0, end_pts: 4.0, text: "early".into() },
        SubtitleEntry {
            start_pts: 2.0,
            end_pts: 5.0,
            text: "late".into(),
        },
        SubtitleEntry {
            start_pts: 6.0,
            end_pts: 7.0,
            text: "after".into(),
        },
    ];
    assert_eq!(
        active_subtitle_index(&subs, 3.0),
        Some(1),
        "重叠时刻 3.0：应取「最近开始」的 late(2.0)，而非 early(0.0)"
    );
    assert_eq!(active_subtitle_index(&subs, 0.5), Some(0));
    assert_eq!(active_subtitle_index(&subs, 6.5), Some(2));
    assert_eq!(active_subtitle_index(&subs, 5.5), None, "[2,5] 与 [6,7] 之间无活跃");
    assert_eq!(active_subtitle_index(&subs, 99.0), None);
    assert_eq!(active_subtitle_index(&[], 1.0), None, "空清单");
}

/// 对拍（评审 SUGGESTION-8 补强）：与 `video` crate `subtitle::tests::
/// find_active_subtitle_edge_case_battery` **逐字同源**的样例集跑字幕骨干实现
/// `active_subtitle_index`（原 FFI `vff_subtitle_at` 的单一实现，R2-5 后仅内部用）——
/// 覆盖重叠窗口、相邻紧贴、0 时刻起点、窗口间隙、尾部越界、空集；相同采样点断言相同
/// 索引，锁死两份实现口径一致。
#[test]
fn active_subtitle_index_edge_case_battery_cross_check() {
    let subs = vec![
        SubtitleEntry { start_pts: 0.0, end_pts: 1.0, text: "at-zero".into() }, // 0 时刻起点
        SubtitleEntry { start_pts: 1.0, end_pts: 2.0, text: "touching".into() }, // 紧贴 [0,1]|[1,2]
        SubtitleEntry { start_pts: 1.5, end_pts: 3.0, text: "overlap".into() }, // 跨段包裹 touching 起点
        SubtitleEntry { start_pts: 5.0, end_pts: 6.0, text: "tail".into() },
    ];
    let cases: &[(f64, Option<usize>)] = &[
        (-1.0, None), // 全部之前
        (0.0, Some(0)), // 0 时刻闭区间命中
        (0.5, Some(0)),
        (1.0, Some(1)), // 紧贴边界：两端活跃，取「最近开始」（touching）
        (1.75, Some(2)), // 重叠：overlap 起于 1.5，覆盖 touching → 取最近开始
        (2.5, Some(2)), // overlap 独立活跃
        (3.0, Some(2)), // 含结束端点（闭区间）
        (3.5, None), // 窗口间隙
        (5.0, Some(3)),
        (6.0, Some(3)), // 含结束端点（闭区间）
        (6.5, None), // 尾部越界
        (99.0, None),
    ];
    assert_eq!(active_subtitle_index(&[], 0.0), None, "空清单");
    for &(t, want) in cases {
        assert_eq!(
            active_subtitle_index(&subs, t),
            want,
            "t={t}: active_subtitle_index 应选中索引 {want:?}"
        );
    }
}

/// 回归（K-锁）：字幕全文件扫描不得长期占全局 FFMPEG_LOCK 阻塞视频 seek。
///
/// 现象：打开 `目标/视频测试字幕.mkv`（415MB、511 条 ASS）后点击进度条卡顿——
/// 前序实现 `SubtitleReader::open` 持锁扫完整文件（≈0.6s），期间**所有** A/V FFI
/// 调用被闸阻塞。本测试量化：起字幕扫描线程并发跑同款视频 seek+出帧，断言
/// seek 相对增量 < 250ms（修复前 delta≈扫描持锁时长 → FAIL；修复后扫描在锁外
/// 纯 demux → delta≈0 → PASS）。
#[test]
fn subtitle_scan_does_not_block_video_seek() {
    let big = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/视频测试字幕.mkv");
    let small = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/video测试.mp4");
    if !big.exists() {
        eprintln!("SKIP: missing target/视频测试字幕.mkv（夹具不入库）");
        return;
    }
    if !small.exists() {
        eprintln!("SKIP: missing target/video测试.mp4（夹具不入库）");
        return;
    }

    // 小文件预开视频 reader（长边 160 封顶 → 固定小缓冲），seek 复用常驻 reader
    let mut v = VideoReader::open_scaled(&small, 160).unwrap();
    let mut buf = vec![0u8; 160 * 120 * 4];
    v.seek(0.5).unwrap();
    assert!(
        v.next_frame_bgra_into(&mut buf).unwrap().is_some(),
        "预热 seek 应出帧"
    );

    let timed_seeks = [0.3f64, 1.1f64];
    let base = {
        let t = std::time::Instant::now();
        for &s in &timed_seeks {
            v.seek(s).unwrap();
            assert!(
                v.next_frame_bgra_into(&mut buf).unwrap().is_some(),
                "基线 seek {s} 应出帧"
            );
        }
        t.elapsed()
    };

    // 并发：字幕扫描线程先起（当前实现抓全局锁扫完整 415MB），主线程随后跑同款 seek
    let path2 = big.clone();
    let h = std::thread::spawn(move || {
        let r = SubtitleReader::open(&path2);
        let n = r.as_ref().map(|s| s.subtitles().len()).unwrap_or(0);
        // Input 非 Send 时不能跨线程返回 → 线程内销毁（Drop 回锁关闭）
        drop(r);
        n
    });
    // 给扫描 ~80ms 抓锁（持锁态覆盖首次 seek；修复后纯放行不阻塞）
    std::thread::sleep(std::time::Duration::from_millis(80));
    let with = {
        let t = std::time::Instant::now();
        for &s in &timed_seeks {
            v.seek(s).unwrap();
            assert!(
                v.next_frame_bgra_into(&mut buf).unwrap().is_some(),
                "并发 seek {s} 应出帧"
            );
        }
        t.elapsed()
    };
    let count = h.join().expect("字幕扫描线程不应死锁");
    assert!(count > 0, "并发提取应得字幕，实得 {count} 条");

    eprintln!(
        "[sub-lock] base_seek={base:?} with_concurrent_scan={with:?} delta={:?}",
        with - base
    );
    // 相对增量红线：并发字幕扫描不得把视频 seek 拖慢超过 +250ms（修复前 ≈ 扫描持锁时长 → FAIL）
    assert!(
        with - base < std::time::Duration::from_millis(250),
        "字幕全文件扫描不应阻塞视频 seek：delta={:?}（应 <250ms）",
        with - base
    );
}

/// 并发压力（K-锁 实证纪律）：字幕 open 与视频 seek+解码并行多轮，断言
/// 不崩、不死锁、每轮 seek 都出帧、扫描都提到字幕——给「扫描移出锁（非锁并发
/// 纯 demux）安全」的实证依据（K14/K15：平台语义先实证再采信）。
#[test]
fn subtitle_scan_concurrent_with_video_seek_stress() {
    let big = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/视频测试字幕.mkv");
    let small = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/video测试.mp4");
    if !big.exists() {
        eprintln!("SKIP: missing target/视频测试字幕.mkv（夹具不入库）");
        return;
    }
    if !small.exists() {
        eprintln!("SKIP: missing target/video测试.mp4（夹具不入库）");
        return;
    }

    let mut v = VideoReader::open_scaled(&small, 160).unwrap();
    let mut buf = vec![0u8; 160 * 120 * 4];
    for round in 0..3 {
        let p = big.clone();
        let h = std::thread::spawn(move || {
            let r = SubtitleReader::open(&p);
            let n = r.as_ref().map(|s| s.subtitles().len()).unwrap_or(0);
            drop(r);
            n
        });
        // 边扫描边做 seek+解码（复用缓冲，让两路真正并行撞期）
        for s in [0.7f64, 1.4f64] {
            v.seek(s).unwrap();
            assert!(
                v.next_frame_bgra_into(&mut buf).unwrap().is_some(),
                "stress round {round} seek {s} 应出帧"
            );
        }
        let count = h.join().expect("round {round}: 扫描线程不应死锁");
        assert!(count > 0, "round {round} 应提取字幕，实得 {count} 条");
    }
}

/// 日志过滤器（env_facs_q 收敛）：只匹配 HE-AAC SBR 断链坏帧的良性报错，其余
/// 解码错误（如真实 decode error）必须照常透传。
#[test]
fn log_filter_predicate_only_matches_env_facsq() {
    assert!(ffmpeg_log_line_is_benign("[aac @ 0000] env_facs_q 253 is invalid"));
    assert!(ffmpeg_log_line_is_benign("Reinit context to ... env_facs_q 254 is invalid"));
    assert!(!ffmpeg_log_line_is_benign("[h264 @ 0000] decode error"));
    assert!(!ffmpeg_log_line_is_benign("udta parsing failed retrying raw"));
    assert!(!ffmpeg_log_line_is_benign(""));
}

/// 过滤器在真实 seek 路径上的落点：打开 415MB MKV 音轨、seek 落点、读到首个
/// clean 帧——必经解码坏帧丢弃路径（若本机构造触发 `env_facs_q`，计数递增证明
/// 过滤器挂载生效且被丢弃；不触发则计数不变，路径仍应不 panic、不解码错）。
#[test]
fn audio_seek_mkv_env_facsq_filter_path_runs() {
    let mkv = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/视频测试字幕.mkv");
    if !mkv.exists() {
        eprintln!("SKIP: missing target/视频测试字幕.mkv（夹具不入库）");
        return;
    }
    // 前置：挂入过滤回调（quiet 幂等）
    ffmpeg::init().unwrap();
    quiet_ffmpeg_log_once();

    let before = FFMPEG_LOG_FILTERED.load(Ordering::Relaxed);
    let mut a = AudioReader::open_pcm_f32(&mkv, 48000).unwrap();
    let mut decoded = 0usize;
    for target in [35.0f64, 95.0, 142.0] {
        a.seek(target).unwrap();
        // 读到落点后的首个 clean 块（≤ 0.5s 样本）即证明 seek+坏帧丢弃路径走通
        let mut got = 0usize;
        while got < 48000 / 2 {
            if let Ok(Some((_, samples))) = a.next_pcm_f32() {
                got += samples.len();
            } else {
                break;
            }
        }
        decoded += got;
    }
    let after = FFMPEG_LOG_FILTERED.load(Ordering::Relaxed);
    eprintln!(
        "[env-facsq] filtered_count before={before} after={after} 解码样本={decoded}"
    );
    assert!(decoded > 0, "MKV 音轨 seek 后应解出音频，实得 {decoded}");
    assert!(after >= before, "过滤计数不倒退");
}

/// F2 回归：BGRA 输出行步长（pitch）假定 → 斜纹错画。sws 输出帧 `linesize` 32 字节对齐
/// 且通常 > `w*4`（输出宽 `w%8≠0` 时必有行尾填充），旧整段 `copy_from_slice` 会把下一行
/// 起点错位成斜纹。`copy_bgra_rows` 逐行按 `w*4` 拷、丢弃行尾填充——构造
/// `linesize=64 > w*4=20` 的假源（行尾填 0xEE 模拟填充区），断言输出逐行逐像素无错位。
#[test]
fn copy_bgra_rows_drops_padding_no_skew() {
    let (w, h) = (5usize, 3usize); // w*4=20；linesize=64 → 行尾 44 字节填充
    let row = w * 4;
    let linesize = 64usize;
    let mut src = vec![0xEEu8; linesize * h];
    for r in 0..h {
        for (i, px) in src[r * linesize..r * linesize + row].chunks_mut(4).enumerate() {
            px.copy_from_slice(&[r as u8, i as u8, (r + i) as u8, 255]);
        }
    }
    let mut dst = vec![0u8; row * h];
    copy_bgra_rows(&mut dst, &src, linesize, w, h);
    for r in 0..h {
        for i in 0..w {
            let off = (r * w + i) * 4;
            assert_eq!(
                &dst[off..off + 4],
                &[r as u8, i as u8, (r + i) as u8, 255],
                "行 {r} 像素 {i} 错位（斜纹未修：整段拷贝把填充算进行距）"
            );
        }
    }
    // 行尾填充确实被丢弃：dst 恒为连续 row*h 字节（无 0xEE 泄漏）
    assert!(
        !dst.contains(&0xEE),
        "行尾填充不得泄漏进连续输出缓冲"
    );
}

/// F5 回归：`classify_receive` 把 `receive_frame` 返回分三类——`Ok` 出帧 / `AVERROR_EOF`
/// 优雅结束 / 其它负码 = 真实解码错误（不得静默当 EOF 吞掉）。无需 FFmpeg 实例，直接对
/// 假返回值测各分支。
#[test]
fn classify_receive_distinguishes_eof_from_real_errors() {
    match classify_receive(Ok(())) {
        ReceiveStatus::Ok => {}
        other => panic!("Ok 应分类为 Ok，实得 {other:?}"),
    }
    match classify_receive(Err(ffmpeg::Error::Eof)) {
        ReceiveStatus::Eof => {}
        other => panic!("AVERROR_EOF 应分类为 Eof，实得 {other:?}"),
    }
    match classify_receive(Err(ffmpeg::Error::InvalidData)) {
        ReceiveStatus::Error(_) => {}
        other => panic!("真实解码错误应分类为 Error，实得 {other:?}"),
    }
    // GFP 分配失败（AVERROR(ENOMEM) → Other{errno}）必须归 Error 而非 Eof
    match classify_receive(Err(ffmpeg::Error::Other { errno: 12 })) {
        ReceiveStatus::Error(_) => {}
        other => panic!("GFP ENOMEM 应分类为 Error，实得 {other:?}"),
    }
}

/// F6 回归：字幕文本 UTF-8 BOM（\u{FEFF}）剥除。带 BOM 的 SRT/ASS 包 → 首字符无 BOM；
/// 旧式 ASS `Dialogue:` 整行带 BOM 也能被识别（走行内时间解析，而非错误地落包 PTS 分支
/// 导致时间错位）；MKV 新块格式带 BOM 同样取正文无 BOM。
#[test]
fn subtitle_bom_is_stripped_and_ass_dialogue_detected() {
    use ffmpeg::Rational;

    // SRT 带 BOM：首字符不应残留 \u{feff}
    let srt = "\u{feff}第一句字幕";
    let pkt = ffmpeg::packet::Packet::copy(srt.as_bytes());
    let e = SubtitleReader::parse_subtitle_packet(&pkt, Rational::new(1, 1000), SubtitleFormat::SRT)
        .expect("SRT 带 BOM 应解析出条目");
    assert!(
        !e.text.starts_with('\u{feff}'),
        "SRT 首字符不应残留 BOM，实得 {:?}",
        e.text
    );
    assert_eq!(e.text, "第一句字幕");

    // 旧式 ASS `Dialogue:` 整行带 BOM：必须命中 Dialogue 分支（行内时间生效）
    let ass = "\u{feff}Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,你好世界";
    let pkt = ffmpeg::packet::Packet::copy(ass.as_bytes());
    let e = SubtitleReader::parse_subtitle_packet(&pkt, Rational::new(1, 1000), SubtitleFormat::ASS)
        .expect("ASS Dialogue 带 BOM 应解析出条目");
    assert!(!e.text.starts_with('\u{feff}'), "ASS 文本不应残留 BOM");
    assert_eq!(e.text, "你好世界", "Dialogue 行应走行内时间解析提取正文");
    assert!(
        (e.start_pts - 1.0).abs() < 1e-9,
        "Dialogue 行内开始时间 1.0s 应生效，实得 {:.4}",
        e.start_pts
    );
    assert!(
        (e.end_pts - 2.0).abs() < 1e-9,
        "Dialogue 行内结束时间 2.0s 应生效，实得 {:.4}",
        e.end_pts
    );

    // Matroska 新块格式（无 Dialogue 前缀）带 BOM：剥后仍按包 PTS 分支取第 9 字段正文
    let mkv_block = "\u{feff}0,0,Default,,0,0,0,,第二句字幕";
    let pkt = ffmpeg::packet::Packet::copy(mkv_block.as_bytes());
    let e = SubtitleReader::parse_subtitle_packet(&pkt, Rational::new(1, 1000), SubtitleFormat::ASS)
        .expect("MKV 新块带 BOM 应解析出条目");
    assert_eq!(e.text, "第二句字幕", "MKV 新块应取正文且无 BOM");
}

// ---------------- R2-4：FFI 打开失败错误分类 ----------------

/// 读 dll 侧 `vff_last_error_message` 当前串（保底非空指针；NUL 结尾转 String）。
fn ffi_last_error_str() -> String {
    let p = unsafe { crate::vff_last_error_message() };
    assert!(!p.is_null(), "vff_last_error_message 不应返回空指针");
    unsafe { std::ffi::CStr::from_ptr(p) }
        .to_string_lossy()
        .into_owned()
}

/// 临时目录写一个内容损坏的假 mp4（同 `open_watchdog_interrupts_corrupt_file` 姿势；
/// 不入库，PID 后缀防并行测试撞目录）。
fn corrupt_fixture_path() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "gpui-editor-mff-classify-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("corrupt.mp4");
    std::fs::write(&p, b"not a real video at all").unwrap();
    p
}

/// R2-4 打开失败分类（`vff_last_error_message` thread-local）：
/// 事件「打不开（不存在/损坏）→ 无媒体流文件 → 缺解码器」返回**不同**分类串；成功
/// open 后清空；读口恒非 null。夹具：`target/audio_only_test.mp4`（仅音轨，无视频流）
/// 与 `target/av1_only_test.mp4`（AV1 视频，slim 无 AV1 解码器）手动装配（§5，缺则
/// SKIP 对应段；损坏文件就地生成，无夹具依赖）。
#[test]
fn ffi_open_failure_classification_distinct() {
    // 基准：无失败状态 → 空串
    assert_eq!(ffi_last_error_str(), "", "无失败时分类串应为空");

    // CString 辅助
    let to_c = |s: &str| std::ffi::CString::new(s).unwrap();

    // ① 不存在的路径：分类「打开媒体失败」
    let mut h: crate::Handle = std::ptr::null_mut();
    let missing = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("no-such-ffi.mp4");
    let rc = unsafe {
        crate::vff_open_video(
            to_c(&missing.to_string_lossy()).as_ptr(),
            u32::MAX,
            &mut h,
        )
    };
    assert_eq!(rc, -1, "不存在路径应打开失败");
    let e1 = ffi_last_error_str();
    assert!(
        e1.contains("打开媒体失败"),
        "打不开（不存在）应分类「打开媒体失败」，实得 {e1:?}"
    );

    // ② 损坏文件：同样「打开媒体失败」（probe 失败），但 root 原因串与 ① 不同
    let bad = corrupt_fixture_path();
    let mut h2: crate::Handle = std::ptr::null_mut();
    let rc = unsafe {
        crate::vff_open_video(to_c(&bad.to_string_lossy()).as_ptr(), u32::MAX, &mut h2)
    };
    assert_eq!(rc, -1, "损坏文件应打开失败");
    let e2 = ffi_last_error_str();
    assert!(e2.contains("打开媒体失败"), "打不开（损坏）应分类同词根，实得 {e2:?}");

    // ③ 无视频流文件（仅音轨）：分类「媒体中没有视频流」
    let only_audio = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/audio_only_test.mp4");
    if only_audio.exists() {
        let mut h3: crate::Handle = std::ptr::null_mut();
        let rc = unsafe {
            crate::vff_open_video(
                to_c(&only_audio.to_string_lossy()).as_ptr(),
                u32::MAX,
                &mut h3,
            )
        };
        assert_eq!(rc, -1, "无视频流文件打开为视频应失败");
        let e3 = ffi_last_error_str();
        assert!(
            e3.contains("媒体中没有视频流"),
            "无媒体流文件应分类「媒体中没有视频流」，实得 {e3:?}"
        );
        assert_ne!(e3, e1, "「无视频流」应与「打不开」分类不同，实得 {e3:?}");
    } else {
        eprintln!("SKIP: missing target/audio_only_test.mp4（夹具不入库）");
    }

    // ④ 缺解码器：引擎在 `open` 期的解码器查找是惰性的（`Context::decoder()` 不校验
    // 解码器存在，`send_packet`/`receive_frame` 才报）→ **缺解码器不在 open 分类窗口**：
    // 验证该文件 open 仍成功（与「打不开/无视频流」不同的自然边界），不错误归到
    // 打开失败分类。真正的中途解码错误不进 open 分类串（R2-4 保持最小范围）。
    let av1 = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/av1_only_test.mp4");
    if av1.exists() {
        let mut h4: crate::Handle = std::ptr::null_mut();
        let rc = unsafe {
            crate::vff_open_video(to_c(&av1.to_string_lossy()).as_ptr(), u32::MAX, &mut h4)
        };
        assert!(
            !h4.is_null() || rc != 0,
            "AV1 文件 open 应为成功（惰性解码器）或明确的打开失败"
        );
        if !h4.is_null() {
            unsafe { crate::vff_close_video(h4) };
            eprintln!(
                "[R2-4] 缺解码器文件 open 成功（解码器惰性），打开失败分类不覆盖解码期错误——符合设计"
            );
        }
    } else {
        eprintln!("SKIP: missing target/av1_only_test.mp4（夹具不入库）");
    }

    // ⑤ 成功 open 清空分类串：旧错误不残留误导诊断
    let video = fixture();
    if video.exists() {
        let mut h5: crate::Handle = std::ptr::null_mut();
        let rc = unsafe {
            crate::vff_open_video(to_c(&video.to_string_lossy()).as_ptr(), u32::MAX, &mut h5)
        };
        assert_eq!(rc, 0, "真视频应打开成功");
        assert!(!h5.is_null());
        unsafe { crate::vff_close_video(h5) };
        assert_eq!(
            ffi_last_error_str(),
            "",
            "成功 open 后分类串应清空（不残留上一个失败）"
        );
    } else {
        eprintln!("SKIP: missing target/video测试.mp4（夹具不入库）");
    }

    // ⑥ 音频侧同样接分类：不存在路径 → 「打开媒体失败」
    let mut ha: crate::Handle = std::ptr::null_mut();
    let rc = unsafe {
        crate::vff_open_audio(
            to_c(&missing.to_string_lossy()).as_ptr(),
            48000,
            &mut ha,
        )
    };
    assert_eq!(rc, -1, "音频打开不存在路径应失败");
    assert!(
        ffi_last_error_str().contains("打开媒体失败"),
        "音频打开失败应分类「打开媒体失败」，实得 {:?}",
        ffi_last_error_str()
    );

    let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!(
        "gpui-editor-mff-classify-{}",
        std::process::id()
    )));
}
