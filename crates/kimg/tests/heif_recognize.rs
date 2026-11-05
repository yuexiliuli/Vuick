//! HEIF 格式识别可测性冒烟：kimg 门面能按魔数识别 HEIC、`HeicDecoder` 能读尺寸，
//! `read_image` 返回未实现错误（Phase-1 解码桩）。语料缺失 → 打印 `SKIP: missing …` 后 return。

const SYNTH_FILE: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/heif/corpus/synth-screen-testsrc2-1280x720-main8.heic");

/// 通过 `kimg::guess_format` 按魔数识别 + `HeicDecoder` 尺寸读取。
#[test]
fn heif_format_recognized_and_dimensions_read() {
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
    let mut decoder = reader.into_decoder().expect("HeicDecoder 应可构造");
    assert_eq!(decoder.dimensions(), (1280, 720), "尺寸应从 hvcC SPS 读出");
    let mut buf = vec![0u8; (decoder.total_bytes()) as usize];
    let err = decoder.read_image(&mut buf).err().expect("read_image 应返回未实现错误");
    assert!(
        err.to_string().contains("尚未实现"),
        "解码桩应返回„尚未实现“：{err}"
    );
    println!("heif recognize: format=Heic dims=1280x720 decode=NotImplemented");
}
