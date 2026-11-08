//! M0 杀探针（K109）：baseline MCU 行带并行「串行预扫」成本 W + 12 线程 spawn 开销 + 内存墙 sanity。
//!
//! 背景：MCU 行带并行需要串行预扫（discard 走位：只消费位流、不装系数、不反量化），
//! 产出每 band 起始 bit 偏移 + 跨行 dc_pred 状态。成败闸：W = 预扫墙钟 / 熵段墙钟 ≤ 0.50
//! 且 12 线程 spawn ≤ ~0.2ms → GO；否则 NO-GO（架构硬顶 ~1.9×，W>0.5 时 S<1.6×）。
//!
//! 探针只读，零生产路径改动：`#![cfg(test)]`，release 下跑（§3 bench/探针 release 例外）。
//! 本机 RDTSC 不可信（K104）→ 全部只下墙钟（`Instant`）。
//!
//! 测量口径：
//! - `t_full`：生产完整解码墙钟（`JpegDecoder::read_image`，含 header/idct/post）。
//! - `t_ent`：熵段墙钟——本模块 `prescan_walk::<false>` 逐 MCU 驱动 `decode_mcu_block`
//!   （同 mcu.rs `inner_decode_mcu_width` 的迭代次序/refill/marker 断点；不含 IDCT/post）。
//! - `t_disc`：预扫（discard 走位）墙钟——`prescan_walk::<true>` 逐 MCU 驱动
//!   `discard_mcu_block`（只消费位流；`decode_block_inner` 的 block=None 分支本身不读
//!   qt/zq，落系数段被跳过，见 bitstream.rs:1271/1320）。
//! - `W = t_disc / t_ent`（自洽主口径）；`W_055 = t_disc / (0.55 × t_full)`（任务批准近似口径）。
//! - `E = t_ent / t_full`：实测熵段占比，与既有消融 0.55 交叉核对。
//!
//! dc_pred 追踪：`prescan_walk::<true>` 用每分量持久 `dc_pred_state`（镜像 M1 pre-scan 的
//! 跨行状态；discard 位消耗本身与 dc_pred 值无关，追踪只是一次 add，对墙钟可忽略）。
//!
//! 用法（前台阻塞到结束，K86）：
//! ```text
//! $env:KIMG_JPEG_CORPUS = 'Benchmarks/corpus/JPEG'
//! cargo test -p kimg_jpeg --release --lib prescan_probe -- --ignored --nocapture
//! ```

#![cfg(test)]

use std::hint::black_box;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::bitstream::{BitStream, HotStream};
use super::error::DecodeError;
use super::huffman::HuffmanTable;
use super::io::Marker;
use super::segment::{
    build_zigzag_quant, ColorSpace, Decoder, DCT_BLOCK, MAX_COMPONENTS, ZIGZAG_QUANT_LEN,
};
use super::JpegDecoder;
use kimg_core::ImageDecoder as _;

/// 语料目录：`KIMG_JPEG_CORPUS` 覆盖（任务：Benchmarks/corpus/JPEG），
/// 缺省主语料 `<workspace>/Benchmarks/corpus/JPEG`（446 矩阵，fixtures 同口径）。
fn corpus_dir() -> PathBuf {
    if let Ok(p) = std::env::var("KIMG_JPEG_CORPUS") {
        return PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("Benchmarks")
        .join("corpus")
        .join("JPEG")
}

fn collect_jpegs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension()
                .map(|x| x.eq_ignore_ascii_case("jpg"))
                .unwrap_or(false)
            {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn median_ms<F: FnMut()>(mut f: F, rounds: usize) -> f64 {
    f(); // 暖机
    let mut acc: Vec<f64> = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let t = Instant::now();
        f();
        acc.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    acc.sort_by(|a, b| a.partial_cmp(b).unwrap());
    acc[acc.len() / 2]
}

/// baseline 几何（镜像 `decode_mcu_ycbcr_baseline` mcu.rs:189-209 的 MCU 行列数）。
#[cfg(test)]
fn baseline_geometry(dec: &Decoder<'_>) -> (usize, usize) {
    let h_max = dec
        .components
        .iter()
        .map(|c| c.horizontal_sample)
        .max()
        .unwrap_or(1)
        .max(1);
    let v_max = dec
        .components
        .iter()
        .map(|c| c.vertical_sample)
        .max()
        .unwrap_or(1)
        .max(1);
    let interleaved = h_max != 1 || v_max != 1;
    let (mw, mh) = if interleaved {
        (dec.width.div_ceil(h_max * 8), dec.height.div_ceil(v_max * 8))
    } else {
        ((dec.width + 7) / 8, (dec.height + 7) / 8)
    };
    if dec.input_colorspace == ColorSpace::Luma && interleaved {
        // 灰度下采样图像：重置为非交织形态（mcu.rs:203-209 / reset_params）
        ((dec.width + 7) / 8, (dec.height + 7) / 8)
    } else {
        (mw, mh)
    }
}

/// 对当前 SOS 的熵段做一次逐 MCU 走位（镜像 mcu.rs `inner_decode_mcu_width`
/// 的熵驱动部分：同一迭代次序 / HotStream / refill / marker 断点 / restart）。
///
/// - `DISCARD=true`：每块 `discard_mcu_block`（M1 pre-scan 位走），每分量持久 dc_pred 追踪。
/// - `DISCARD=false`：每块 `decode_mcu_block`（熵段本体：huffman + 反量化 + 落系数 + tmp.fill）。
/// - 终止语义对齐生产：每 MCU 检查 `marker && bits_left==0`（mcu.rs:624），行末 write_back 后按
///   `stream.marker` 的 EOI/RST/SOS 收尾；首个 SOS 即止（`t_disc`/`t_ent` 同窗自洽，
///   多 SOS 图像两数同比例缩减，W 不变）。
///
/// 返回 `(走位块数, 墙钟 ms)`。
#[cfg(test)]
#[inline(never)] // 防 release DCE：调用点不透明（镜像生产 decode_block_inner 的 #[inline(never)]）
#[allow(clippy::cast_precision_loss)]
fn prescan_walk<const DISCARD: bool>(dec: &mut Decoder<'_>) -> Result<(u64, f64), DecodeError> {
    let start = Instant::now();
    let (mw, mh) = baseline_geometry(dec);

    let mut stream = BitStream::new();
    let mut hot = HotStream::from_stream(&stream);
    let (mut ab, mut bl) = hot.ab_bl();
    let mut dc_pred_state = [0i32; MAX_COMPONENTS];
    let mut tmp = [0i16; DCT_BLOCK];
    let mut blocks: u64 = 0;

    let nc = dec.components.len().min(MAX_COMPONENTS);
    // raw 指针快照（镜像 mcu.rs:456-477 的表外提；表解码期不变，解引用安全）。
    let mut dc_ptr: [*const HuffmanTable; MAX_COMPONENTS] = [core::ptr::null(); MAX_COMPONENTS];
    let mut ac_ptr: [*const HuffmanTable; MAX_COMPONENTS] = [core::ptr::null(); MAX_COMPONENTS];
    let mut qt_ptr: [*const [i32; DCT_BLOCK]; MAX_COMPONENTS] =
        [core::ptr::null(); MAX_COMPONENTS];
    let mut zq_store: [[u32; ZIGZAG_QUANT_LEN]; MAX_COMPONENTS] =
        [[0; ZIGZAG_QUANT_LEN]; MAX_COMPONENTS];
    let mut vs = [0usize; MAX_COMPONENTS];
    let mut hs = [0usize; MAX_COMPONENTS];
    for k in 0..nc {
        let c = &dec.components[k];
        match dec.dc_huffman_tables[c.dc_huff_table % MAX_COMPONENTS].as_ref() {
            Some(t) => dc_ptr[k] = t as *const HuffmanTable,
            None => {
                return Err(DecodeError::Format(
                    "prescan: DC table missing".to_string(),
                ))
            }
        }
        match dec.ac_huffman_tables[c.ac_huff_table % MAX_COMPONENTS].as_ref() {
            Some(t) => ac_ptr[k] = t as *const HuffmanTable,
            None => {
                return Err(DecodeError::Format(
                    "prescan: AC table missing".to_string(),
                ))
            }
        }
        match dec.qt_tables[c.quantization_table_number as usize].as_ref() {
            Some(q) => {
                qt_ptr[k] = q as *const [i32; DCT_BLOCK];
                zq_store[k] = build_zigzag_quant(q);
            }
            None => {
                return Err(DecodeError::Format(
                    "prescan: quantization table missing".to_string(),
                ))
            }
        }
        vs[k] = c.vertical_sample;
        hs[k] = c.horizontal_sample;
    }

    let z_order = dec.z_order; // 拷到局部，避免 &mut dec 调和借用（同 mcu.rs:423）
    let z_scans = &z_order[..usize::from(dec.num_scans)];
    let restart = dec.restart_interval != 0;

    'outer: for _row in 0..mh {
        for _j in 0..mw {
            for &k in z_scans {
                // SAFETY: k<nc 且指针由上方非空 as_ref 填充；解码期表不变（同 mcu.rs:495）。
                let dc: &HuffmanTable = unsafe { &*dc_ptr[k] };
                let ac: &HuffmanTable = unsafe { &*ac_ptr[k] };
                let qt: &[i32; DCT_BLOCK] = unsafe { &*qt_ptr[k] };
                let zq: &[u32; ZIGZAG_QUANT_LEN] = &zq_store[k];
                // 每分量每 MCU 的块数 = vs×hs（scan_subsampled=false 时全为 1×1，等价）
                    for _v in 0..vs[k] {
                    for _h in 0..hs[k] {
                        blocks += 1;
                        let r = if DISCARD {
                            match hot.discard_mcu_block(&mut dec.reader, dc, ac, ab, bl) {
                                Ok((_, a, b)) => {
                                    ab = a;
                                    bl = b;
                                    Ok(0u16)
                                }
                                Err(e) => Err(e),
                            }
                        } else {
                            tmp.fill(0);
                            match hot.decode_mcu_block(
                                &mut dec.reader,
                                dc,
                                ac,
                                qt,
                                zq,
                                &mut tmp,
                                &mut dc_pred_state[k],
                                ab,
                                bl,
                            ) {
                                Ok((len, a, b)) => {
                                    ab = a;
                                    bl = b;
                                    Ok(len)
                                }
                                Err(e) => Err(e),
                            }
                        };
                        if r.is_err() {
                            break 'outer; // 与生产的 Terminate 一致：停止走位
                        }
                    }
                }
            }
            dec.todo = dec.todo.wrapping_sub(1);
            if restart {
                hot.set_ab_bl(ab, bl);
                hot.write_back(&mut stream);
                if dec.todo == 0 {
                    if dec.handle_rst_main(&mut stream).is_err() {
                        break 'outer;
                    }
                    dc_pred_state.iter_mut().for_each(|x| *x = 0);
                    hot.reload(&stream);
                    let (a, b) = hot.ab_bl();
                    ab = a;
                    bl = b;
                }
            }
            let bl_zero = bl == 0;
            if hot.marker().is_some() && bl_zero {
                break 'outer; // 生产 mcu.rs:624 同款断点
            }
        }
        hot.set_ab_bl(ab, bl);
        hot.write_back(&mut stream);
        if let Some(m) = stream.marker {
            match m {
                Marker::EOI => break 'outer,
                Marker::SOS => break 'outer, // 非交织多 scan：本探针只预扫首个 scan
                Marker::RST(_) if dec.todo == 0 => {
                    if dec.handle_rst(&mut stream).is_err() {
                        break 'outer;
                    }
                    dc_pred_state.iter_mut().for_each(|x| *x = 0);
                }
                _ => break 'outer,
            }
        }
    }
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    Ok((blocks, ms))
}

/// 期望块数（几何：首 SOS 各分量 vs×hs × MCU 网格；供核对走位完整度）。
#[cfg(test)]
fn expected_blocks(dec: &Decoder<'_>) -> u64 {
    let (mw, mh) = baseline_geometry(dec);
    let z_order = dec.z_order;
    (0..usize::from(dec.num_scans))
        .map(|i| {
            let k = z_order[i];
            if k < dec.components.len() {
                dec.components[k].vertical_sample * dec.components[k].horizontal_sample * mw * mh
            } else {
                0
            }
        })
        .sum::<usize>() as u64
}

const ROUNDS: usize = 5;

/// 走位墙钟中位：每轮**新建** Decoder（header 解析不入计时窗——上一轮走位已把 reader 消耗到
/// EOF/EOI，复用同一 Decoder 会导致后续轮立即终止，测得 ~0ms，见 K43/本探针 M0 复盘）。
/// 防 release DCE：`#[inline(never)]` + `black_box` 走位结果。
/// 返回 None = header 失败 / 走位 Err（该文件跳过）。
#[cfg(test)]
fn median_walk_ms<const DISCARD: bool>(bytes: &[u8], rounds: usize) -> Option<f64> {
    let run = |force_bb: bool| -> Result<(), DecodeError> {
        let mut dec = Decoder::new(bytes);
        dec.decode_headers_internal()?;
        let r = black_box(prescan_walk::<DISCARD>(&mut dec).unwrap());
        if force_bb {
            black_box(r);
        }
        Ok(())
    };
    if run(true).is_err() {
        return None;
    }
    let mut acc: Vec<f64> = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let mut dec = Decoder::new(bytes);
        if dec.decode_headers_internal().is_err() {
            return None;
        }
        let t = Instant::now();
        match prescan_walk::<DISCARD>(&mut dec) {
            Ok(r) => {
                black_box(r);
            }
            Err(_) => return None,
        }
        acc.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    acc.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some(acc[acc.len() / 2])
}

#[test]
#[ignore]
fn m0_prescan_probe() {
    let dir = corpus_dir();
    if !dir.is_dir() {
        println!("SKIP: corpus dir missing/unreadable: {}", dir.display());
        return;
    }
    let files = collect_jpegs(&dir);
    if files.is_empty() {
        println!("SKIP: empty corpus: {}", dir.display());
        return;
    }

    println!("== M0 prescan kill-probe (K109) corpus={} files={} ==", dir.display(), files.len());
    println!(
        "{:<8} {:>5} {:>5} {:>9} {:>9} {:>9} {:>8} {:>8} {:>7}",
        "file", "w", "h", "full_ms", "ent_ms", "disc_ms", "W_ent", "W_055", "E"
    );

    let mut ws: Vec<f64> = Vec::new();
    let mut w055s: Vec<f64> = Vec::new();
    let mut es: Vec<f64> = Vec::new();

    for p in &files {
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let bytes = match std::fs::read(p) {
            Ok(b) => b,
            Err(e) => {
                println!("WARN {name}: read failed {e}");
                continue;
            }
        };

        // 1) 生产完整解码（头 + 熵 + idct + post）
        let (w, h, n) = match JpegDecoder::new(Cursor::new(bytes.as_slice())) {
            Ok(d) => {
                let (w, h) = d.dimensions();
                (w, h, d.total_bytes() as usize)
            }
            Err(_) => {
                println!("WARN {name}: JpegDecoder::new failed; skip");
                continue;
            }
        };
        let mut full_buf = vec![0u8; n];
        let t_full = median_ms(
            || {
                let dec = JpegDecoder::new(Cursor::new(bytes.as_slice())).unwrap();
                dec.read_image(&mut full_buf).unwrap();
            },
            ROUNDS,
        );

        // 2) 熵段 / 预扫走位墙钟（每轮新 Decoder；header 不计时；结果 black_box 防 DCE）
        let t_ent = match median_walk_ms::<false>(&bytes, ROUNDS) {
            Some(v) => v,
            None => {
                println!("WARN {name}: baseline walk ent failed; skip");
                continue;
            }
        };
        let t_disc = match median_walk_ms::<true>(&bytes, ROUNDS) {
            Some(v) => v,
            None => {
                println!("WARN {name}: baseline walk disc failed; skip");
                continue;
            }
        };

        // 3) 块数核对（独立一次真实走位）
        let mut blocks = 0u64;
        let mut exp = 0u64;
        {
            let mut dec = Decoder::new(&bytes);
            if dec.decode_headers_internal().is_ok() {
                if let Ok((b, _)) = prescan_walk::<false>(&mut dec) {
                    blocks = b;
                    exp = expected_blocks(&dec);
                }
            }
        }
        if blocks != exp {
            println!("NOTE {name}: blocks walked {blocks} != expected {exp} (多 SOS/截断?)");
        }

        let w_ent = t_disc / t_ent;
        let w_055 = t_disc / (0.55 * t_full);
        let e = t_ent / t_full;
        ws.push(w_ent);
        w055s.push(w_055);
        es.push(e);

        println!(
            "{:<8} {:>5} {:>5} {:>9.3} {:>9.3} {:>9.3} {:>8.3} {:>8.3} {:>7.3}",
            name.trim_end_matches(".jpg"),
            w,
            h,
            t_full,
            t_ent,
            t_disc,
            w_ent,
            w_055,
            e,
        );
    }

    if ws.is_empty() {
        println!("M0: 无可测文件，返回");
        return;
    }
    let md = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let m_w = md(ws.clone());
    let m_w055 = md(w055s.clone());
    let m_e = md(es.clone());
    println!(
        "M0 MEDIAN: W_ent={m_w:.3} W_055={m_w055:.3} E(entropy fraction)={m_e:.3} (跨文件)"
    );
    let go = m_w <= 0.50;
    println!("M0 W 判据: W_ent med={m_w:.3} (GO 需 ≤0.50) -> {}", if go { "候选 GO" } else { "NO-GO" });
}

#[test]
#[ignore]
fn m0_spawn_probe() {
    const THREADS: usize = 12;
    const ROUNDS: usize = 20;
    let spin = || {
        let mut x = 0u32;
        for _ in 0..8 {
            x = x.wrapping_add(1);
        }
        black_box(x); // <1µs 空转防消除
    };
    std::thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(spin);
        }
    });
    let mut acc: Vec<f64> = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let t = Instant::now();
        std::thread::scope(|s| {
            for _ in 0..THREADS {
                s.spawn(spin);
            }
        });
        acc.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    acc.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = acc[acc.len() / 2];
    println!(
        "M0 spawn: {THREADS} threads scope spawn+join x{ROUNDS} median={med:.4} ms (GO 需 ≤0.2ms)"
    );
}

#[test]
#[ignore]
fn m0_memwall_probe() {
    const W: usize = 2048;
    const H: usize = 1360;
    const BPP: usize = 3;
    const N: usize = W * H * BPP;
    let mut buf = vec![0u8; N];
    // 数据依赖写（每字节值依赖 seed，LLVM 无法常量折叠 → memset 不可被 DCE 消除），
    // 末字节读数依赖全部前序写 → 整段 8.3MB 写必须物化。口径：输出直写带宽下界。
    let seed: u64 = black_box(0x9E37_79B9_7F4A_7C15);
    let ms = median_ms(
        || {
            for (i, b) in buf.iter_mut().enumerate() {
                *b = ((seed ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)) >> 8) as u8;
            }
            black_box(buf[N - 1]);
        },
        ROUNDS,
    );
    let mbps = N as f64 / (ms / 1000.0) / 1e6;
    println!(
        "M0 memwall: {W}x{H}x{BPP} = {N}B 数据依赖写 median={ms:.3} ms -> {mbps:.0} MB/s (输出直写下界)"
    );
}
