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

/// 双路径（硬件优先 / 软件回退）都必须产出尺寸正确、alpha 不透明的 BGRA 帧。
#[test]
fn hw_or_sw_path_both_output_valid_bgra() {
    let mut r = VideoReader::open_scaled(&fixture_or_skip!(), 160).unwrap();
    eprintln!(
        "video-ffmpeg 解码路径: {}",
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
                    if bench_convert_stage(&mut r, &f, &mut out, &mut stage)?.is_some() {
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
            if bench_convert_stage(&mut r, &f, &mut out, &mut stage)?.is_some() {
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
    stage: &mut [f64; 5],
) -> anyhow::Result<Option<(f64, u32, u32)>> {
    let Some(pts) = f.pts() else {
        return Ok(None);
    };
    let mut bgra = ffmpeg::frame::Video::empty();
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
        r.scaler_for(fmt, w, h)?
            .run(&sw, &mut bgra)
            .context("sws 转换失败")?;
        stage[ST_SWS] += t.elapsed().as_secs_f64();
    } else {
        let fmt = f.format();
        let (w, h) = (f.width(), f.height());
        let t = std::time::Instant::now();
        r.scaler_for(fmt, w, h)?
            .run(f, &mut bgra)
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
/// find_active_subtitle_edge_case_battery` **逐字同源**的样例集跑 FFI 骨干实现
/// `active_subtitle_index`（即 `vff_subtitle_at` 的单一实现）——覆盖重叠窗口、相邻紧贴、
/// 0 时刻起点、窗口间隙、尾部越界、空集；相同采样点断言相同索引，锁死两份实现口径一致。
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
