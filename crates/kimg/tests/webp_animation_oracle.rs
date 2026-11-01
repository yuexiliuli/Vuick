//! WebP 动画自研（OURS） vs image-webp 逐帧对拍 oracle（integration）。
//!
//! 遍历全部动画 fixture（SimdCodecs 8 个 `anim-*.webp` + 用户/桥接语料），以
//! `KIMG_WEBP_BACKEND=ours`（直设）与 `imagewebp` 两个后端经 kimg `WebPDecoder::into_frames`
//! 分别解码，逐帧比对 RGBA 字节 / delay(ms) / loop_count / 尺寸。合成语义以 image-webp 为准
//! （image-webp 与 anim_dump 在 3 个 fixture 上合成语义不同——见交接文档 §3.2）。
//!
//! 说明：
//! - OURS 接管判据：`has_animation()` 为真 + `into_frames` 产出帧（OursAnimated 路径）；
//!   若某 fixture 自研降级回 image-webp（decode_animation 失败），本测试仍对拍通过，但
//!   crate 内 `animation::tests::animated_fixtures_ours_matches_imagewebp` 独立断言接管成功。
//! - 缺外部语料 → SKIP（AGENTS §5 惯例）。

use std::io::Cursor;

use kimg::codecs::webp::{set_webp_backend, WebPDecoder};
use kimg::{AnimationDecoder as _, ImageDecoder as _, LoopCount};

mod fixtures;

/// 视图：(尺寸, loop, [(帧 RGBA, delay_ms)…])。
fn decode_all(data: &[u8], backend: u8) -> ((u32, u32), LoopCount, Vec<(Vec<u8>, u32)>) {
    set_webp_backend(backend);
    let dec = WebPDecoder::new(Cursor::new(data)).expect("WebPDecoder::new 应成功");
    let dims = dec.dimensions();
    let loop_count = dec.loop_count();
    let frames = dec
        .into_frames()
        .collect_frames()
        .expect("into_frames 应成功");
    let frames = frames
        .into_iter()
        .map(|f| {
            let delay = f.delay().numer_denom_ms().0;
            (f.into_buffer().into_raw(), delay)
        })
        .collect();
    (dims, loop_count, frames)
}

fn loop_eq(a: LoopCount, b: LoopCount) -> bool {
    match (a, b) {
        (LoopCount::Infinite, LoopCount::Infinite) => true,
        (LoopCount::Finite(a), LoopCount::Finite(b)) => a == b,
        _ => false,
    }
}

fn loop_label(l: LoopCount) -> &'static str {
    match l {
        LoopCount::Infinite => "infinite",
        LoopCount::Finite(_) => "finite",
    }
}

/// 反输出当前后端配置。
fn backend_label(b: u8) -> &'static str {
    match b {
        1 => "ours",
        _ => "imagewebp",
    }
}

#[test]
fn animated_fixtures_ours_vs_imagewebp_per_frame() {
    let mut names = fixtures::webp::webp_fixture_names();
    names.sort();
    let mut checked = 0usize;
    let mut skipped = 0usize;
    let mut mismatches = Vec::new();
    let mut total_frames = 0usize;

    for name in &names {
        let Some(path) = fixtures::webp::webp_fixture_path(name) else {
            continue;
        };
        let data = std::fs::read(&path).expect("read fixture");
        // 仅动画（OURS 后端 has_animation 判据，顺带验证接管）。
        set_webp_backend(1);
        let probe = match WebPDecoder::new(Cursor::new(&data)) {
            Ok(d) => d,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if !probe.has_animation() {
            continue; // 静态图，非本测试对象
        }

        let ours = decode_all(&data, 1);
        let imgwb = decode_all(&data, 2);
        checked += 1;

        if ours.0 != imgwb.0 {
            mismatches.push(format!(
                "{name}: 尺寸 {:?} vs {:?}",
                ours.0, imgwb.0
            ));
        }
        if !loop_eq(ours.1, imgwb.1) {
            mismatches.push(format!(
                "{name}: loop 不一致 {} vs {}",
                loop_label(ours.1),
                loop_label(imgwb.1)
            ));
        }
        if ours.2.len() != imgwb.2.len() {
            mismatches.push(format!(
                "{name}: 帧数 {} vs {}",
                ours.2.len(),
                imgwb.2.len()
            ));
        }
        total_frames += ours.2.len();
        for (i, (of, iff)) in ours.2.iter().zip(&imgwb.2).enumerate() {
            if of.0 != iff.0 {
                let first_bad = of
                    .0
                    .iter()
                    .zip(&iff.0)
                    .position(|(a, b)| a != b)
                    .map(|p| (of.0[p], of.0[p + 3], iff.0[p], iff.0[p + 3]));
                mismatches.push(format!(
                    "{name}: 帧 {i} RGBA 不一致（首失配 {first_bad:?}, ours={}/imgwebp={} 字节）",
                    of.0.len(),
                    iff.0.len()
                ));
            }
            if of.1 != iff.1 {
                mismatches.push(format!(
                    "{name}: 帧 {i} delay {} vs {}",
                    of.1, iff.1
                ));
            }
        }
    }
    set_webp_backend(0); // 复位 AUTO，避免影响同进程其它测试

    for m in &mismatches {
        eprintln!("WEBP_ANIM_ORACLE_MISMATCH\t{m}");
    }
    eprintln!(
        "WEBP_ANIM_ORACLE_SUMMARY\tchecked={checked} skipped={skipped} frames={total_frames} mismatches={}",
        mismatches.len(),
    );
    assert!(
        mismatches.is_empty(),
        "{} 处动画逐帧与 image-webp 不一致（见上）",
        mismatches.len()
    );
    // 8 个挠 fixture（SimdCodecs 目录）至少全部进入对拍；外部语料越多越严格。
    assert!(
        checked >= 8,
        "动画 fixture 应覆盖 SimdCodecs 的 8 个 anim-*.webp，实际 checked={checked}"
    );
}

/// OURS 后端动画接管到 `read_image`（整图读取=首帧）不回归：与 image-webp 首帧 RGBA 一致。
#[test]
fn animated_first_frame_read_image_matches() {
    let Some(path) = fixtures::webp::webp_fixture_path("anim-img2webp-mixed.webp") else {
        eprintln!("SKIP anim-img2webp-mixed.webp 外部语料缺失");
        return;
    };
    let data = std::fs::read(&path).unwrap();
    let read_first = |backend: u8| -> (u32, u32, Vec<u8>) {
        set_webp_backend(backend);
        let dec = WebPDecoder::new(Cursor::new(&data)).expect("new");
        let (w, h) = dec.dimensions();
        let mut buf = vec![0u8; dec.total_bytes() as usize];
        dec.read_image(&mut buf).expect("read_image");
        (w, h, buf)
    };
    let ours = read_first(1);
    let img = read_first(2);
    set_webp_backend(0);
    assert_eq!(ours.0, img.0, "首帧尺寸");
    assert_eq!(ours.1, img.1, "首帧高度");
    assert_eq!(ours.2, img.2, "首帧 RGBA 逐字节");
}
