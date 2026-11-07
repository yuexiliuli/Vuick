//! 真实文件容器解析冒烟：probe + hvcC SPS/PPS 真实解析（`synth-screen-testsrc2-1280x720-main8.heic`）。
//! 夹具缺失 → 打印 `SKIP: missing …` 后 return（SKIP 是常态不是失败）。
//! 运行：`cargo test -p kimg_heif --test parse_smoke -- --nocapture --test-threads=1`

use kimg_heif::codecs::heif::container::HeifContainer;
use kimg_heif::codecs::heif::hevc::nal::Nal;
use kimg_heif::codecs::heif::hevc::parameter_sets::{HevcPps, HevcSps};
use kimg_heif::codecs::heif::probe::heif_probe;

const CORPUS_FILE: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../Benchmarks/corpus/heif/synth-screen-testsrc2-1280x720-main8.heic");

#[test]
fn probe_and_parameter_sets_synth_main8() {
    let data = match std::fs::read(CORPUS_FILE) {
        Ok(d) => d,
        Err(e) => {
            println!("SKIP: missing {CORPUS_FILE} ({e})");
            return;
        }
    };

    let p = heif_probe(&data).expect("probe 应成功");
    println!(
        "probe: {}x{} bd={} tiles={} sps={}x{} bitY={} bitC={} chroma={} profile={} ctbLog2={} grid={} conf={:?}",
        p.width, p.height, p.bitdepth, p.tiles, p.sps_w, p.sps_h, p.bit_y, p.bit_c, p.chroma,
        p.profile, p.ctb_log2, p.grid, p.conf
    );
    assert_eq!((p.width, p.height), (1280, 720), "w/h 应等于 info.json 的 1280x720");
    assert_eq!(p.bitdepth, 8, "位深应 8");
    assert_eq!(p.tiles, 1, "单 tile");
    assert_eq!((p.sps_w, p.sps_h), (1280, 720), "SPS 编码尺寸");
    assert_eq!(p.chroma, 1, "4:2:0");
    assert_eq!(p.profile, 3, "Main Still Picture");
    assert_eq!(p.ctb_log2, 6, "CtbLog2SizeY=6");
    assert_eq!(p.grid, "1x1", "grid");
    assert_eq!(p.conf, [0, 0, 0, 0], "conformance window");

    // 真实解析 hvcC 里的 SPS 与 PPS（参数集 NAL 单元）。
    let container = HeifContainer::parse(&data).expect("容器解析应成功");
    let cfg = &container.tile_configs[0];
    let mut sps_ok = false;
    let mut pps_ok = false;
    for nal in &cfg.nal_units {
        match Nal::nal_type(nal).expect("NAL 头") {
            Nal::SpsNut => {
                let sps = HevcSps::parse(&Nal::unescape_to_array(nal)).expect("SPS 解析");
                assert_eq!(sps.width, 1280);
                assert_eq!(sps.height, 720);
                assert_eq!(sps.bit_depth_y, 8);
                assert_eq!(sps.chroma_format_idc, 1);
                assert_eq!(sps.ctb_log2_size_y, 6);
                sps_ok = true;
            }
            Nal::PpsNut => {
                let pps = HevcPps::parse(&Nal::unescape_to_array(nal)).expect("PPS 解析");
                assert_eq!(pps.sps_id, 0, "synth 主文件 PPS 引用 SPS 0");
                pps_ok = true;
            }
            _ => {}
        }
    }
    assert!(sps_ok, "hvcC 应含 SPS");
    assert!(pps_ok, "hvcC 应含 PPS");
}
