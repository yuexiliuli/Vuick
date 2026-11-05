//! HEVC 帧解码器 oracle 测试：把 `crates/kimg_heif` 的 HEVC 帧内解码（`hevc::decode_item` 等）输出
//! 与 C# 参考解码器（`Sdcb.SimdCodecs.Heif`）逐字节对照：
//! - 默认带 in-loop 滤波（Deblock + SAO）= `_t0.yuv`（main8 全图 Oracle 全绿 = 目标）；
//! - `decode_item_flags(.., true, true)` 消融 = `_t0_nofilter.yuv`（保留，防拆东墙）。
//!
//! 语料：`target/heif/corpus/<file>.HEIC/.heic`；参考 dump：`target/heif/oracle/<md5>_t0?[_nofilter].yuv`。
//! dump 格式 = 32 字节头（yW,yH,cW,cH,cropL,cropR,cropT,cropB 各 i32LE）+ Y(u16LE×yW*yH) +
//! Cb(cW*cH) + Cr(cW*cH)，行主序无跨步；yW/yH 是 SPS 编码尺寸（含一致性 padding：SPS 尺寸本身
//! 就是 MinCb 对齐的编码尺寸，平面的 stride = 平面宽）。Rust 输出（`HevcPicture`）必须同尺寸。
//!
//! 缺失语料/dump → 打印 `SKIP: missing …` 后 return（SKIP 是常态不是失败）。
//! 失配 → 打印首差坐标/行列/两侧值/maxDelta 并 panic。
//! 运行：`cargo test -p kimg_heif --test oracle_decode -- --test-threads=1`

use std::path::PathBuf;

use kimg_heif::codecs::heif::container::HeifContainer;
use kimg_heif::codecs::heif::hevc::decode_item_flags;
use kimg_heif::codecs::heif::isobmff::item_container::ItemContainer;

const ORACLE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/heif/oracle");
const CORPUS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/heif/corpus");

/// 单一 main8 单 tile 图片：「md5（dump 前缀）, 语料文件名」。SPS 编码尺寸即 dump 尺寸
/// （含一致性 padding：SPS 尺寸本身是编码尺寸；输出画布对被 `crop` 裁剪，但 dump 是整图）。
const SINGLE_TILES: &[(&str, &str)] = &[
    ("25e8249964d4f8b08e022919ff07d852", "synth-screen-testsrc2-1280x720-main8.heic"),
    ("86d06d35c1f171952b534b498aca55cf", "synth-photo-fuji-1600x900-main8.heic"),
    ("f7afbdc3ad72918719d767ed0c7fb762", "synth-photo-mangrove-1599x1199-main8.heic"),
    ("0446df12bc6fb4bc4d6e13f7c7b14439", "synth-photo-fuji-1600x900-alpha.heic"),
    ("84e4d9493f325b938c3e2f849f9b69fc", "synth-screen-testsrc2-640x360-hlg-nclx.heic"),
    ("bb0c8511e1e2db5995f3e58480ad6af6", "synth-screen-testsrc2-640x360-pq-vui.heic"),
];

/// 手机 grid 图的第一个 tile（512×512 小 tile）单独解码对照。
const PHONE_TILE_T0: (&str, &str) = ("134614e62e9ef0f952de2261ec9beeee", "IMG_0083.HEIC");

fn read_u16s(data: &[u8]) -> Vec<u16> {
    data.chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect()
}

fn i32_at(data: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}

/// 解码语料文件的第 `tile_idx` 个 tile item，返回图画（`skip_deblock`/`skip_sao` 消融 in-loop 滤波）。
fn decode_tile(
    corpus: &PathBuf,
    tile_idx: usize,
    skip_deblock: bool,
    skip_sao: bool,
) -> kimg_heif::codecs::heif::hevc::picture::HevcPicture {
    let data = std::fs::read(corpus).expect("read corpus");
    let container = HeifContainer::parse(&data).expect("HeifContainer::parse");
    let config = container.tile_configs[tile_idx].clone();
    let items = &container.items;
    let item = &items.items[container.tiles[tile_idx]];
    let length = ItemContainer::get_item_length(item);
    let mut payload = vec![0u8; length as usize];
    items
        .resolve(item, &mut payload)
        .expect("resolve item payload");
    decode_item_flags(&config, &payload, skip_deblock, skip_sao).expect("decode_item_flags")
}

/// 逐平面比较图画与 dump 的 u16 平面；返回 (失配数, 首差索引, 首差 ours, 首差 ref, maxDelta)。
fn diff_planes(plane: &[u16], oracle: &[u16]) -> (usize, usize, u16, u16, u32) {
    let n = plane.len().min(oracle.len());
    let mut count = 0usize;
    let mut first = None;
    let mut max_delta = 0u32;
    for i in 0..n {
        let a = plane[i];
        let b = oracle[i];
        if a != b {
            count += 1;
            if first.is_none() {
                first = Some(i);
            }
            let d = if a > b { a - b } else { b - a };
            max_delta = max_delta.max(d as u32);
        }
    }
    let (fi, fa, fb) = match first {
        Some(i) => (i, plane[i], oracle[i]),
        None => (0, 0, 0),
    };
    (count, fi, fa, fb, max_delta)
}

/// 解析冒烟：testsrc2 解码返回 Ok 且尺寸正确（先跑通这条再看平面）。
#[test]
fn testsrc2_smoke_decodes_1280x720() {
    let corpus = PathBuf::from(CORPUS_DIR).join("synth-screen-testsrc2-1280x720-main8.heic");
    if !corpus.exists() {
        println!("SKIP: missing {corpus:?}");
        return;
    }
    let pic = decode_tile(&corpus, 0, false, false);
    assert_eq!(pic.width, 1280);
    assert_eq!(pic.height, 720);
    assert_eq!(pic.chroma_width, 640);
    assert_eq!(pic.chroma_height, 360);
    assert_eq!(pic.output_width(), 1280);
    assert_eq!(pic.output_height(), 720);
    println!("smoke: testsrc2 decoded {}x{} bd={}", pic.width, pic.height, pic.bit_depth_y);
}

/// 单图一块的 to 对照；`dump_suffix` = `""`（带滤波）或 `"_nofilter"`（消融）。
/// 手机 grid t0 一并走查（有语料/dump 才算数）。
fn check_t0_suite(skip_deblock: bool, skip_sao: bool, dump_suffix: &str) {
    let mut checked = 0usize;
    let mut skipped = 0usize;
    for &(md5, file) in SINGLE_TILES {
        let corpus = PathBuf::from(CORPUS_DIR).join(file);
        let dump = PathBuf::from(ORACLE_DIR).join(format!("{md5}_t0{dump_suffix}.yuv"));
        if !corpus.exists() {
            println!("SKIP: missing corpus {corpus:?}");
            skipped += 1;
            continue;
        }
        if !dump.exists() {
            println!("SKIP: missing oracle {dump:?}");
            skipped += 1;
            continue;
        }
        let pic = decode_tile(&corpus, 0, skip_deblock, skip_sao);

        let data = std::fs::read(&dump).unwrap();
        assert!(data.len() >= 32, "{file}: dump 过短");
        let y_w = i32_at(&data, 0);
        let y_h = i32_at(&data, 4);
        let c_w = i32_at(&data, 8);
        let c_h = i32_at(&data, 12);
        let crop = [
            i32_at(&data, 16),
            i32_at(&data, 20),
            i32_at(&data, 24),
            i32_at(&data, 28),
        ];
        assert_eq!((y_w, y_h), (pic.width, pic.height), "{file}: Y 尺寸");
        assert_eq!((c_w, c_h), (pic.chroma_width, pic.chroma_height), "{file}: 色度尺寸");
        assert_eq!(crop, [pic.crop_left, pic.crop_right, pic.crop_top, pic.crop_bottom], "{file}: crop");

        let y_n = (y_w * y_h) as usize;
        let c_n = (c_w * c_h) as usize;
        let expect = 32 + (y_n + 2 * c_n) * 2;
        assert_eq!(data.len(), expect, "{file}: dump 载荷长度");

        let oracle_y = read_u16s(&data[32..32 + y_n * 2]);
        let oracle_cb = read_u16s(&data[32 + y_n * 2..32 + (y_n + c_n) * 2]);
        let oracle_cr = read_u16s(&data[32 + (y_n + c_n) * 2..32 + (y_n + 2 * c_n) * 2]);

        let mut fail = false;
        for (name, plane, oracle) in [
            ("Y", &pic.planes[0], &oracle_y),
            ("Cb", &pic.planes[1], &oracle_cb),
            ("Cr", &pic.planes[2], &oracle_cr),
        ] {
            let (count, fi, fa, fb, maxd) = diff_planes(plane, oracle);
            if count != 0 {
                let cw = if name == "Y" { y_w } else { c_w };
                println!(
                    "  {name}: {count}/{} 失配；首差样本 {fi}（x={}, y={}）, ours={fa} oracle={fb}, maxDelta={maxd}",
                    plane.len(),
                    fi % cw as usize,
                    fi / cw as usize,
                );
                fail = true;
            }
        }
        if fail {
            panic!("{file}: 与 C# dump {dump_suffix:?} 失配");
        }
        checked += 1;
        println!("PASS {file}: {}x{} (sps {}x{}, crop {:?}) mismatches=0", pic.width, pic.height, pic.width, pic.height, crop);
    }

    // 手机 grid t0：512×512 小 tile。
    {
        let (md5, file) = PHONE_TILE_T0;
        let corpus = PathBuf::from(CORPUS_DIR).join(file);
        let dump = PathBuf::from(ORACLE_DIR).join(format!("{md5}_t0{dump_suffix}.yuv"));
        if corpus.exists() && dump.exists() {
            let pic = decode_tile(&corpus, 0, skip_deblock, skip_sao);
            let data = std::fs::read(&dump).unwrap();
            let y_w = i32_at(&data, 0);
            let y_h = i32_at(&data, 4);
            assert_eq!((y_w, y_h), (pic.width, pic.height), "{file} t0: Y 尺寸");
            let y_n = (y_w * y_h) as usize;
            let c_w = i32_at(&data, 8);
            let c_h = i32_at(&data, 12);
            let c_n = (c_w * c_h) as usize;
            assert_eq!((c_w, c_h), (pic.chroma_width, pic.chroma_height), "{file} t0: 色度尺寸");
            let oracle_y = read_u16s(&data[32..32 + y_n * 2]);
            let oracle_cb = read_u16s(&data[32 + y_n * 2..32 + (y_n + c_n) * 2]);
            let oracle_cr = read_u16s(&data[32 + (y_n + c_n) * 2..32 + (y_n + 2 * c_n) * 2]);
            for (name, plane, oracle) in [
                ("Y", &pic.planes[0], &oracle_y),
                ("Cb", &pic.planes[1], &oracle_cb),
                ("Cr", &pic.planes[2], &oracle_cr),
            ] {
                let (count, fi, fa, fb, maxd) = diff_planes(plane, oracle);
                assert_eq!(count, 0, "{file} t0 {name}: {count} 失配 首差@{fi} ours={fa} oracle={fb} maxDelta={maxd}");
            }
            checked += 1;
            println!("PASS {file} t0: {}x{} mismatches=0", pic.width, pic.height);
        } else {
            println!("SKIP: missing {corpus:?} 或 {dump:?}");
            skipped += 1;
        }
    }

    println!("oracle_decode{dump_suffix:?}: checked={checked} skipped={skipped}");
}

/// 全量：单 tile main8 图片 + 手机 grid t0，与 C# 带滤波 dump（`_t0.yuv`）逐字节对照。
/// 目标：main8 全图 Oracle 全绿（mismatches=0）。
#[test]
fn oracle_t0_matches_csharp() {
    check_t0_suite(false, false, "");
}

/// 全量：与 C# nofilter dump（`_t0_nofilter.yuv` = `-deblock -sao` 消融）逐字节对照。
/// 保留：确认 in-loop 滤波接入没拆坏帧内重建。
#[test]
fn oracle_t0_nofilter_matches_csharp() {
    check_t0_suite(true, true, "_nofilter");
}
