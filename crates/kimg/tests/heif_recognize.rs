//! HEIF 识别 + 真解码冒烟：kimg 门面能按魔数识别 HEIC，`HeicDecoder` 能读尺寸，
//! `read_image` 能真解出 RGBA8，并与 `target/heif/oracle/25e82499….rgba`（testsrc2 主图）
//! 逐字节比对。语料缺失 → 打印 `SKIP: missing …` 后 return。

const SYNTH_FILE: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/heif/corpus/synth-screen-testsrc2-1280x720-main8.heic");
const TEST_SRC2_RGBA: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/heif/oracle/25e8249964d4f8b08e022919ff07d852.rgba");

/// 通过 `kimg::guess_format` 按魔数识别 + `HeicDecoder` 尺寸读取 + `read_image` 真解码对照 C# RGBA8。
#[test]
fn heif_format_recognized_and_decoded() {
    use kimg::ImageDecoder;

    let data = match std::fs::read(SYNTH_FILE) {
        Ok(d) => d,
        Err(e) => {
            println!("SKIP: missing {SYNTH_FILE} ({e})");
            return;
        }
    };

    let format = match kimg::guess_format(&data) {
        Ok(f) => f,
        Err(_) => {
            panic!("应能按魔数识别 HEIC");
        }
    };
    assert_eq!(format, kimg::ImageFormat::Heic, "魔数应识别为 Heic");

    let mut reader = kimg::ImageReader::new(std::io::Cursor::new(&data));
    reader.set_format(kimg::ImageFormat::Heic);
    let decoder = reader.into_decoder().expect("HeicDecoder 应可构造");
    assert_eq!(decoder.dimensions(), (1280, 720), "尺寸应从 hvcC SPS 读出");
    assert_eq!(decoder.color_type(), kimg::ColorType::Rgba8, "对齐 C# Decode → Rgba32");
    let mut buf = vec![0u8; (decoder.total_bytes()) as usize];
    decoder
        .read_image(&mut buf)
        .expect("read_image 应真解码出 RGBA8");

    // 对照 C# `.rgba`（w,h i32LE + RGBA8）：本文件无 transform，显示尺寸 == 画布。
    let want = match std::fs::read(TEST_SRC2_RGBA) {
        Ok(d) => d,
        Err(e) => {
            println!("SKIP: missing oracle {TEST_SRC2_RGBA} ({e})");
            return;
        }
    };
    assert_eq!(&buf[..], &want[8..], "testsrc2 RGBA 应逐字节等于 C# dump");
    assert!(
        buf[3] == 255 && buf[4 * 5 + 3] == 255,
        "RGBA8 的 alpha 通道应恒为 255"
    );
    println!("heif recognize: format=Heic dims=1280x720 RGBA8=decode OK (match C#)");
}
