//! env 门控回退（独立进程）：`KIMG_JPEG_SCALED=0` → `read_image_scaled` 返回
//! Unsupported（默认开）。独立文件 = 独立进程，保证 OnceLock 进程内只读一次的口径
//! 不被其它用例先触发（jpeg_scaled.rs 同进程先解码会缓存开）。

mod fixtures;

use kimg_core::ImageDecoder as _;
use kimg_jpeg::codecs::jpeg::JpegDecoder;

#[test]
fn scaled_gate_env_off_returns_unsupported() {
    // 先设 env 再首次触发 OnceLock（本进程无其它 scaled 调用；2024 edition set_var 需 unsafe）
    unsafe { std::env::set_var("KIMG_JPEG_SCALED", "0") };
    let dir = fixtures::corpus_dir();
    let target = dir.join("420-q100-3mp__landscape-almsee.jpg");
    if !target.is_file() {
        println!("SKIP: missing {}", dir.display());
        return;
    }
    let bytes = std::fs::read(&target).unwrap();
    let dec = JpegDecoder::from_bytes(&bytes).unwrap();
    let (ct, total) = {
        let ct = dec.color_type();
        let (w, h) = dec.dimensions();
        (ct, w as usize * h as usize * usize::from(ct.bytes_per_pixel()))
    };
    let _ = total;
    let mut buf = vec![0u8; 16];
    let err = dec.read_image_scaled(2, &mut buf).unwrap_err().to_string();
    assert!(
        err.contains("support") || err.contains("Unsupported") || err.contains("关闭"),
        "env 关时应返回 Unsupported，实际：{err}"
    );
    println!("KIMG_JPEG_SCALED=0 → scaled 返回 Unsupported（回退全解兜底）✓ color_type={ct:?}");
}
