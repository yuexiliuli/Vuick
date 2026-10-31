//! GIF 自研后端 oracle：对语料/夹具「gif crate（dev-dep）↔ 自研」双解全部帧 RGBA8 逐字节比对；
//! 编码回环：自研 encoder → 自研 decoder 与 → gif crate decoder 两路一致。
//!
//! gif crate 只给原始帧（不合成），故这里在其上复刻 kimg 既有 `GifFrameIterator` 的
//! `blend_and_dispose_pixel` 合成规则得到参照画布（该规则即旧路径被替换的语义）。

use std::io::Cursor;

use kimg::codecs::gif::{GifDecoder, GifEncoder};
use kimg::{AnimationDecoder, Delay, Frame, ImageDecoder, LoopCount, Rgba, RgbaImage};
use kimg::buffer::ImageBuffer;

use gif::{ColorOutput, DecodeOptions, DisposalMethod};

// ---------------------------------------------------------------------------
// gif crate 参照端
// ---------------------------------------------------------------------------

/// gif crate 的一帧原始数据（palette 展开、透明索引 → [0,0,0,0]）与元数据。
struct CrateFrame {
    rgba: Vec<u8>,
    left: u32,
    top: u32,
    width: u32,
    height: u32,
    dispose: DisposalMethod,
    transparent: Option<u8>,
    delay: u16,
}

/// gif crate 读全部原始帧；截断等处返回 Err。
fn gif_crate_read(bytes: &[u8]) -> Result<(u32, u32, gif::Repeat, Vec<CrateFrame>), String> {
    let mut opts = DecodeOptions::new();
    opts.set_color_output(ColorOutput::RGBA);
    let mut r = opts
        .read_info(Cursor::new(bytes))
        .map_err(|e| format!("gif crate 头部失败: {e:?}"))?;
    let (w, h, repeat) = (u32::from(r.width()), u32::from(r.height()), r.repeat());
    let mut out = Vec::new();
    loop {
        let fr = match r.next_frame_info() {
            Ok(Some(fr)) => fr,
            Ok(None) => break,
            Err(e) => return Err(format!("gif crate 迭代失败: {e:?}")),
        };
        // next_frame_info 返回的帧借用内部缓冲，先取出元数据（借用随之结束）再读像素。
        let meta = (
            u32::from(fr.left),
            u32::from(fr.top),
            u32::from(fr.width),
            u32::from(fr.height),
            fr.dispose,
            fr.transparent,
            fr.delay,
        );
        let sz = r.buffer_size();
        let mut buf = vec![0u8; sz];
        r.read_into_buffer(&mut buf)
            .map_err(|e| format!("gif crate 帧读失败: {e:?}"))?;
        out.push(CrateFrame {
            rgba: buf,
            left: meta.0,
            top: meta.1,
            width: meta.2,
            height: meta.3,
            dispose: meta.4,
            transparent: meta.5,
            delay: meta.6,
        });
    }
    Ok((w, h, repeat, out))
}

fn blend_c(dispose: DisposalMethod, previous: &mut [u8], current: &mut [u8]) {
    if current[3] == 0 {
        current.copy_from_slice(previous);
    }
    match dispose {
        DisposalMethod::Any | DisposalMethod::Keep => previous.copy_from_slice(current),
        DisposalMethod::Background => previous.copy_from_slice(&[0, 0, 0, 0]),
        DisposalMethod::Previous => {}
    }
}

/// 在参照画布上合成第 `f` 帧，返回该帧全画布 RGBA（复刻 kimg 旧 GifFrameIterator）。
fn crate_composite(canvas: &mut [u8], f: &CrateFrame, w: usize, h: usize) -> Vec<u8> {
    let fw = f.width as usize;
    let fh = f.height as usize;
    let full = f.left as usize == 0
        && f.top as usize == 0
        && fw == w
        && fh == h;
    if full {
        let mut cur = f.rgba.clone();
        for y in 0..fh {
            for x in 0..fw {
                let i = (y * fw + x) * 4;
                blend_c(f.dispose, &mut canvas[i..i + 4], &mut cur[i..i + 4]);
            }
        }
        cur
    } else {
        let mut out = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                let fx = x as i64 - i64::from(f.left);
                let fy = y as i64 - i64::from(f.top);
                let prev = canvas[i..i + 4].to_vec();
                if fx >= 0 && fx < fw as i64 && fy >= 0 && fy < fh as i64 {
                    let si = (fy as usize * fw + fx as usize) * 4;
                    let mut p = f.rgba[si..si + 4].to_vec();
                    blend_c(f.dispose, &mut canvas[i..i + 4], &mut p);
                    out[i..i + 4].copy_from_slice(&p);
                } else {
                    out[i..i + 4].copy_from_slice(&prev);
                }
            }
        }
        out
    }
}

/// gif crate 原始帧 → 参照合成帧序列（全画布 RGBA）。
fn crate_frames(bytes: &[u8]) -> Result<(u32, u32, gif::Repeat, Vec<Vec<u8>>, Vec<u16>), String> {
    let (w, h, repeat, frames) = gif_crate_read(bytes)?;
    let mut canvas = vec![0u8; (w as usize) * (h as usize) * 4];
    let del = frames.iter().map(|f| f.delay).collect::<Vec<_>>();
    let out = frames
        .iter()
        .map(|f| crate_composite(&mut canvas, f, w as usize, h as usize))
        .collect();
    Ok((w, h, repeat, out, del))
}

// ---------------------------------------------------------------------------
// 自研端便捷读
// ---------------------------------------------------------------------------

struct SelfRead {
    w: u32,
    h: u32,
    loop_count: LoopCount,
    frames: Vec<Vec<u8>>,
    delays_ms: Vec<u32>,
}

fn self_read(bytes: &[u8]) -> Result<SelfRead, String> {
    let dec = GifDecoder::new(Cursor::new(bytes)).map_err(|e| format!("自研 new 失败: {e:?}"))?;
    let loop_count = dec.loop_count();
    let (w, h) = dec.dimensions();
    let mut frames = Vec::new();
    let mut delays = Vec::new();
    for fr in dec.into_frames() {
        let fr = fr.map_err(|e| format!("自研帧失败: {e:?}"))?;
        delays.push(fr.delay().numer_denom_ms().0);
        frames.push(fr.buffer().as_raw().clone());
    }
    Ok(SelfRead { w, h, loop_count, frames, delays_ms: delays })
}

fn loop_equiv(a: &LoopCount, b: &LoopCount) -> bool {
    match (a, b) {
        (LoopCount::Infinite, LoopCount::Infinite) => true,
        (LoopCount::Finite(x), LoopCount::Finite(y)) => x == y,
        _ => false,
    }
}

fn loop_desc(l: &LoopCount) -> String {
    match l {
        LoopCount::Infinite => "Infinite".to_string(),
        LoopCount::Finite(n) => format!("Finite({})", n.get()),
    }
}

/// 由 gif crate `Repeat` 按 kimg 旧映射推期望 `LoopCount`。
fn expect_loop_from_crate(r: gif::Repeat) -> LoopCount {
    match r {
        gif::Repeat::Infinite => LoopCount::Infinite,
        gif::Repeat::Finite(0) => LoopCount::Infinite,
        gif::Repeat::Finite(n) => {
            LoopCount::Finite(std::num::NonZeroU32::new(u32::from(n)).expect("非零"))
        }
    }
}

/// 双解比对：全部帧逐字节 + 尺寸 + 帧数 + delay + loop。
fn assert_dual_parity(bytes: &[u8], label: &str) {
    let (cw, ch, crepeat, cframes, cdel) = crate_frames(bytes)
        .unwrap_or_else(|e| panic!("[{label}] gif crate 参照读失败: {e}"));
    let mine = self_read(bytes).unwrap_or_else(|e| panic!("[{label}] 自研读失败: {e}"));
    assert_eq!(mine.w, cw, "[{label}] 逻辑屏宽不一致");
    assert_eq!(mine.h, ch, "[{label}] 逻辑屏高不一致");
    assert_eq!(
        mine.frames.len(),
        cframes.len(),
        "[{label}] 帧数不一致：自研 {} vs 参照 {}",
        mine.frames.len(),
        cframes.len()
    );
    let exp = expect_loop_from_crate(crepeat);
    assert!(
        loop_equiv(&mine.loop_count, &exp),
        "[{label}] loop 不一致：自研 {} vs 参照 {crepeat:?}",
        loop_desc(&mine.loop_count)
    );
    for (i, (a, b)) in mine.frames.iter().zip(cframes.iter()).enumerate() {
        assert_eq!(
            a,
            b,
            "[{label}] 第 {i} 帧 RGBA 不一致（total {} 像素，前 16 字节 自研 {:?} 参照 {:?}）",
            a.len() / 4,
            &a[..16.min(a.len())],
            &b[..16.min(b.len())]
        );
        assert_eq!(
            mine.delays_ms[i],
            u32::from(cdel[i]) * 10,
            "[{label}] 第 {i} 帧 delay(ms) 不一致"
        );
    }
}

/// 静态首帧（read_image）与合成首帧一致。
fn assert_static_first_frame(bytes: &[u8], label: &str) {
    let dec = GifDecoder::new(Cursor::new(bytes)).unwrap();
    let (w, h) = dec.dimensions();
    let mut buf = vec![0u8; (w * h * 4) as usize];
    if dec.total_bytes() as usize != buf.len() {
        return;
    }
    if dec.read_image(&mut buf).is_ok() {
        let (_, _, _, first, _) = crate_frames(bytes).unwrap();
        if let Some(f0) = first.first() {
            assert_eq!(&buf, f0, "[{label}] read_image 与合成首帧不一致");
        }
    }
}

// ---------------------------------------------------------------------------
// 夹具构造（gif crate 编码器 → 自研/gif crate 双解）
// ---------------------------------------------------------------------------

fn build_gif(
    w: u16,
    h: u16,
    frames: &[(u16, u16, u16, u16, RgbaImage, u8)],
    repeat: Option<gif::Repeat>,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut enc = gif::Encoder::new(&mut bytes, w, h, &[]).unwrap();
        if let Some(r) = repeat {
            enc.set_repeat(r).unwrap();
        }
        for (left, top, fw, fh, img, dispose) in frames {
            let mut px = img.as_raw().clone();
            let mut fr = gif::Frame::from_rgba(*fw, *fh, &mut px);
            fr.left = *left;
            fr.top = *top;
            fr.dispose = match dispose {
                0 => DisposalMethod::Any,
                2 => DisposalMethod::Background,
                3 => DisposalMethod::Previous,
                _ => DisposalMethod::Keep,
            };
            enc.write_frame(&fr).unwrap();
        }
    }
    bytes
}

/// 全不透明色块。
fn solid(w: u32, h: u32, r: u8, g: u8, b: u8) -> RgbaImage {
    RgbaImage::from_pixel(w, h, Rgba([r, g, b, 255]))
}

/// 条纹（周期重复 → 触发 KwKwK 型字典复用）。
fn stripes(w: u32, h: u32) -> RgbaImage {
    ImageBuffer::from_fn(w, h, |x, _y| {
        if x % 4 < 2 {
            Rgba([255, 0, 0, 255])
        } else {
            Rgba([0, 255, 0, 255])
        }
    })
}

#[test]
fn decode_oracle_mini_fixtures() {
    // 1) 单帧纯色。
    let one = [(
        0,
        0,
        8,
        8,
        solid(8, 8, 0, 0, 255),
        1u8,
    )];
    let b = build_gif(8, 8, &one, None);
    assert_dual_parity(&b, "still_solid");

    // 2) 两帧动画（延迟不同）。
    let f0 = solid(8, 8, 255, 0, 0);
    let f1 = solid(8, 8, 0, 255, 0);
    let two = [
        (0, 0, 8, 8, f0, 1u8),
        (0, 0, 8, 8, f1, 1u8),
    ];
    let b = build_gif(8, 8, &two, Some(gif::Repeat::Finite(4)));
    assert_dual_parity(&b, "two_frame_anim");

    // 3) 偏置 bbox（不全覆盖）动画。
    let b3 = [
        (0, 0, 16, 16, stripes(16, 16), 1u8),
        (4, 4, 8, 8, solid(8, 8, 255, 255, 0), 0u8),
        (4, 4, 8, 8, solid(8, 8, 0, 255, 255), 3u8),
    ];
    let b = build_gif(16, 16, &b3, Some(gif::Repeat::Infinite));
    assert_dual_parity(&b, "partial_bbox_disposal_chain");

    // 4) 大尺寸渐变（字典增长/可能 4096 冻结），条纹重复段。
    let grad = ImageBuffer::from_fn(512, 64, |x, y| {
        Rgba([(x * 7) as u8, (y * 9) as u8, ((x + y) * 3) as u8, 255])
    });
    let g4 = [(0, 0, 512, 64, grad, 1u8)];
    let b = build_gif(512, 64, &g4, None);
    assert_dual_parity(&b, "large_gradient");

    // 5) 透明索引帧（gif crate 编的透明 → 读回两路一致）。
    let mut timg = RgbaImage::from_pixel(8, 8, Rgba([0, 0, 0, 0]));
    let t = build_gif(8, 8, &[(0, 0, 8, 8, timg.clone(), 1u8)], None);
    let _ = t;
    // 半透明打底 + 透明像素：用手写 gif crate Frame 透明索引。
    let mut bytes = Vec::new();
    {
        let mut enc = gif::Encoder::new(&mut bytes, 8, 8, &[255, 0, 0, 0, 255, 0]).unwrap();
        let mut px = vec![0u8; 8 * 8 * 4];
        for (i, c) in px.chunks_exact_mut(4).enumerate() {
            if i % 3 == 0 {
                c.copy_from_slice(&[0, 0, 0, 0]);
            } else {
                c.copy_from_slice(&[0, 128, 255, 255]);
            }
        }
        let mut fr = gif::Frame::from_rgba(8, 8, &mut px);
        fr.transparent = Some(0);
        enc.write_frame(&fr).unwrap();
    }
    assert_dual_parity(&bytes, "transparent_frame");
}

#[test]
fn decode_oracle_corpus() {
    // 语料：target/imgbench-corpus/GIF/*.gif（缺则 SKIP）。
    let dir = {
        let manifest = env!("CARGO_MANIFEST_DIR");
        std::path::Path::new(manifest)
            .join("..")
            .join("..")
            .join("target")
            .join("imgbench-corpus")
            .join("GIF")
    };
    let mut paths: Vec<_> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "gif").unwrap_or(false))
            .map(|e| e.path())
            .collect(),
        Err(_) => {
            eprintln!("SKIP: 缺少语料目录 {dir:?}，跳过 GIF 语料 oracle");
            return;
        }
    };
    paths.sort();
    if paths.is_empty() {
        eprintln!("SKIP: 语料目录 {dir:?} 无 .gif 文件");
        return;
    }
    for p in &paths {
        let bytes = std::fs::read(p).unwrap();
        let label = format!("corpus:{}", p.file_name().unwrap().to_string_lossy());
        assert_dual_parity(&bytes, &label);
        assert_static_first_frame(&bytes, &label);
        eprintln!("ok {label}: {} bytes, {} 帧", bytes.len(), {
            self_read(&bytes).map(|r| r.frames.len()).unwrap_or(0)
        });
    }
}

#[test]
fn truncated_gif_both_error() {
    let f0 = solid(8, 8, 255, 0, 0);
    let f1 = solid(8, 8, 0, 255, 0);
    let full = build_gif(8, 8, &[(0, 0, 8, 8, f0, 1u8), (0, 0, 8, 8, f1, 1u8)], None);
    // 截断点：头部内 / 帧数据中部（尾块前几个字节）。
    let n = full.len();
    for cut in [10usize, n - 4, n - 3, n - 2, n - 1] {
        let bytes = &full[..cut];
        let mine_err = self_read(bytes).is_err();
        let crate_err = gif_crate_read(bytes).is_err();
        assert_eq!(
            mine_err, crate_err,
            "cut={cut}/{n}: 两路成败不一致 mine_err={mine_err} crate_err={crate_err}"
        );
        eprintln!("truncated cut={cut}/{n}: mine_err={mine_err} crate_err={crate_err}");
    }
}

// ---------------------------------------------------------------------------
// 编码回环
// ---------------------------------------------------------------------------

#[test]
fn encode_roundtrip_self_and_crate() {
    // 小调色板源（精确无损），含透明与偏置帧。
    let f0 = RgbaImage::from_fn(16, 16, |x, y| {
        if (x + y) % 5 == 0 {
            Rgba([0, 0, 0, 0])
        } else {
            Rgba([255, 0, 0, 255])
        }
    });
    let f1 = RgbaImage::from_pixel(16, 16, Rgba([0, 255, 0, 255]));
    let f2 = RgbaImage::from_fn(16, 16, |x, _y| {
        if x < 8 {
            Rgba([0, 0, 255, 255])
        } else {
            Rgba([255, 255, 255, 255])
        }
    });

    let mut bytes = Vec::new();
    {
        let mut enc = GifEncoder::new(&mut bytes);
        enc.set_repeat(kimg::codecs::gif::Repeat::Finite(3)).unwrap();
        for (f, d) in [(f0, 120u32), (f1, 240), (f2, 360)] {
            enc.encode_frame(Frame::from_parts(
                f,
                0,
                0,
                Delay::from_numer_denom_ms(d, 1),
            ))
            .unwrap();
        }
    }

    // 自研解码。
    let mine = self_read(&bytes).expect("自研读回自研输出失败");
    assert_eq!(mine.frames.len(), 3);
    assert_eq!(mine.delays_ms, [120, 240, 360]);
    assert!(
        matches!(mine.loop_count, LoopCount::Finite(n) if n.get() == 3),
        "自研读回 loop 应 Finite(3)，实际 {}",
        loop_desc(&mine.loop_count)
    );

    // gif crate 反向回环（输出规范可读）。
    let (cw, ch, _, cframes, cdel) =
        crate_frames(&bytes).unwrap_or_else(|e| panic!("gif crate 读回自研输出失败: {e}"));
    assert_eq!((cw, ch), (16, 16));
    assert_eq!(cframes.len(), 3);
    for (i, (a, b)) in mine.frames.iter().zip(cframes.iter()).enumerate() {
        assert_eq!(a, b, "第 {i} 帧两路解码不一致");
        assert_eq!(mine.delays_ms[i], u32::from(cdel[i]) * 10, "第 {i} 帧 delay 不一致");
    }

    // 与源像素一致（小调色板精确路径）。
    let src: Vec<Vec<u8>> = vec![
        RgbaImage::from_fn(16, 16, |x, y| {
            if (x + y) % 5 == 0 { Rgba([0, 0, 0, 0]) } else { Rgba([255, 0, 0, 255]) }
        }),
        RgbaImage::from_pixel(16, 16, Rgba([0, 255, 0, 255])),
        RgbaImage::from_fn(16, 16, |x, _y| {
            if x < 8 { Rgba([0, 0, 255, 255]) } else { Rgba([255, 255, 255, 255]) }
        }),
    ]
    .into_iter()
    .map(|im| im.into_raw())
    .collect();
    for (i, (a, s)) in mine.frames.iter().zip(src.iter()).enumerate() {
        let hit = a.iter().zip(s.iter()).position(|(x, y)| x != y);
        match hit {
            Some(p) => {
                panic!(
                    "第 {i} 帧未精确还原：首差异 @{p}（自研 {:?} vs 源 {:?}）",
                    &a[p..(p + 8).min(a.len())],
                    &s[p..(p + 8).min(s.len())]
                )
            }
            None => {}
        }
    }
}
