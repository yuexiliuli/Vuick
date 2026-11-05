// 连续多解确定性（同线程池复用）：对同一测试线程连续解码多帧，「planes / above_b / above_nz /
// row_filters / EmitScratch 每线程池复用」路径下各帧字节恒等于其首解（次序无关：A→B→A 中两次
// A 逐字节一致）。覆盖「同尺寸池复用」与「跨尺寸（grow 后容量内复用/收缩）」。属池化硬闸之上
// 的针对性对拍（池复用不得改变确定性输出；缓冲内容确定性证明见 `subbuffers.rs`）。
use super::*;
use super::super::container;

struct LossySample {
    name: String,
    bitstream: Vec<u8>,
    width: u32,
    height: u32,
}

/// 收集外部语料中的静态有损 / lossy-alpha fixture（缺则空表 → SKIP 约定，见 §5）。
/// `container::parse` 出容器后再取原始 VP8 帧载荷（`bitstream`）供有损解码。
fn load_lossy_samples() -> Vec<LossySample> {
    let mut names = webp_fixtures_support::webp_fixture_names();
    names.sort();
    let mut out = Vec::new();
    for name in names {
        let Some(p) = webp_fixtures_support::webp_fixture_path(&name) else {
            continue;
        };
        let Ok(data) = std::fs::read(&p) else {
            continue;
        };
        // scope 隔离 `container::parse` 对 `data` 的借用（c 持有 &[u8] 引用），
        // 取出尺寸 + 克隆位流后即可移入样本。
        let (width, height, bitstream) = {
            let Ok(c) = container::parse(&data) else {
                continue;
            };
            if c.family != container::Family::StaticLossy
                && c.family != container::Family::LossyAlpha
            {
                continue;
            }
            let Some(frame) = c.bitstream else {
                continue;
            };
            (c.width, c.height, frame.to_vec())
        };
        out.push(LossySample {
            name,
            bitstream,
            width,
            height,
        });
    }
    out
}

/// tight 解（y/u/v 紧密平面 + rgba 直出均填满）→ 全平面逐字节可比。
fn decode_lossy(s: &LossySample) -> (Vp8Yuv, Vec<u8>) {
    let mut yuv = decode_vp8_tight(&s.bitstream, s.width, s.height).expect("lossy 解码");
    let rgba = yuv_to_rgba_fancy(&mut yuv);
    (yuv, rgba)
}

fn assert_yuv_eq(a: &Vp8Yuv, b: &Vp8Yuv, name: &str, tag: &str) {
    assert_eq!(a.y, b.y, "{name}: {tag} y 紧密平面逐字节一致");
    assert_eq!(a.u, b.u, "{name}: {tag} u 平面逐字节一致");
    assert_eq!(a.v, b.v, "{name}: {tag} v 平面逐字节一致");
}

#[test]
fn multi_decode_pool_reuse_byte_stable() {
    let samples = load_lossy_samples();
    if samples.is_empty() {
        eprintln!("SKIP: missing lossy webp fixtures（外部语料，见 fixtures/webp/mod.rs）");
        return;
    }
    // 同尺寸池复用：同一输入连续解两次 → 逐字节一致（第 2 解完整走池 take/return）。
    let a = &samples[0];
    let (yuv1, rgba1) = decode_lossy(a);
    let (yuv2, rgba2) = decode_lossy(a);
    assert_eq!(rgba1, rgba2, "{}: 池复用第 2 解 RGBA 逐字节一致", a.name);
    assert_yuv_eq(&yuv1, &yuv2, &a.name, "池复用第 2 解");
    // 跨尺寸复用：中间插一个尺寸不同的样本 b（强制 planes grow 或容量内复用），再解 a
    // → 仍与首解一致（次序无关）；b 自身重复解也一致。
    if let Some(b) = samples.iter().find(|s| (s.width, s.height) != (a.width, a.height)) {
        let (by1, br1) = decode_lossy(b);
        let (by2, br2) = decode_lossy(b);
        assert_eq!(br1, br2, "{}: 池复用第 2 解 RGBA 逐字节一致", b.name);
        assert_yuv_eq(&by1, &by2, &b.name, "池复用第 2 解");
        // A→B→A：B（不同尺寸）在中间后 A 仍恒定。
        let (yuv3, rgba3) = decode_lossy(a);
        assert_eq!(rgba1, rgba3, "{}: A→B→A 次序无关（跨尺寸后 A 仍恒定）", a.name);
        assert_yuv_eq(&yuv1, &yuv3, &a.name, "A→B→A 次序无关");
        eprintln!("WEBP_POOL_REUSE_OK\t{}x{}\t+\t{}x{}", a.width, a.height, b.width, b.height);
    } else {
        eprintln!("WEBP_POOL_REUSE_OK\tsame-size\t{}x{}", a.width, a.height);
    }
}
