//! kimg webp（OURS 后端） vs libwebp 1.6.0 SIMD DLL vs image-webp 0.2.4 直连。
//! 同进程、同语料、同计时口径（暖机 1 轮 + 9 轮取中位）对拍。
//! 语料 = 套件快照：Benchmarks/corpus/webp（bootstrap.ps1 从
//! 仓库 target/imgbench-corpus/WEBP(20) + 外部代码库 WEBP 目录(52，env/参数可覆指定) 快照去重得 72 份）。
//!
//! 运行：
//!   $env:KIMG_WEBP_THREADS=0            # 0=强制单线程；不设 = 默认 MT（auto）
//!   $env:LIBWEBP_DLL=...                # 可覆盖；默认 = 探针 crate 相对 ../bin/libwebp.dll
//!   cargo run --release --manifest-path ...\Benchmarks\webp\compare\Cargo.toml

use std::ffi::c_void;
use std::io::Cursor;
use std::os::raw::c_int;
use std::os::windows::ffi::OsStrExt;
use std::time::Instant;

use kimg::AnimationDecoder as _;
use kimg::ImageDecoder as _;

const ROUNDS: usize = 9;

// ---------------------------------------------------------------------------
// 场景与语料（与 imgbench benches/webp_release_bench.rs 同逻辑）
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Scenario {
    StaticLossy,
    StaticLossless,
    AlphaLossless,
    Animated,
}

impl Scenario {
    fn name(self) -> &'static str {
        match self {
            Scenario::StaticLossy => "static-lossy",
            Scenario::StaticLossless => "static-lossless",
            Scenario::AlphaLossless => "alpha-lossless",
            Scenario::Animated => "animated",
        }
    }
    fn animated(self) -> bool {
        matches!(self, Scenario::Animated)
    }
}

static SCENARIOS: [Scenario; 4] = [
    Scenario::StaticLossy,
    Scenario::StaticLossless,
    Scenario::AlphaLossless,
    Scenario::Animated,
];

fn classify(bytes: &[u8]) -> Option<Scenario> {
    if bytes.len() < 30 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return None;
    }
    match &bytes[12..16] {
        b"VP8 " => Some(Scenario::StaticLossy),
        b"VP8L" => {
            if lossless_has_alpha(bytes) {
                Some(Scenario::AlphaLossless)
            } else {
                Some(Scenario::StaticLossless)
            }
        }
        b"VP8X" => {
            let flags = bytes[20];
            if flags & 0x02 != 0 {
                return Some(Scenario::Animated);
            }
            if bytes[12..].windows(4).any(|w| w == b"VP8L") {
                if lossless_has_alpha(bytes) {
                    Some(Scenario::AlphaLossless)
                } else {
                    Some(Scenario::StaticLossless)
                }
            } else {
                Some(Scenario::StaticLossy)
            }
        }
        _ => None,
    }
}

fn lossless_has_alpha(bytes: &[u8]) -> bool {
    let Ok(dec) = kimg::codecs::webp::WebPDecoder::new(Cursor::new(bytes)) else {
        return false;
    };
    let n = dec.total_bytes() as usize;
    if n == 0 {
        return false;
    }
    let mut buf = vec![0u8; n];
    if dec.read_image(&mut buf).is_err() {
        return false;
    }
    buf.chunks_exact(4).any(|px| px[3] != 0xFF)
}

fn collect_dir(dir: &std::path::Path, out: &mut Vec<(String, Vec<u8>)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<_> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "webp").unwrap_or(false))
        .collect();
    paths.sort();
    for p in paths {
        if let Ok(b) = std::fs::read(&p) {
            let name = p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.push((name, b));
        }
    }
}

// ---------------------------------------------------------------------------
// 两条解码通道（均输出 RGBA8 字节数；解不了返回 None）
// ---------------------------------------------------------------------------

/// kimg OURS 旧生产姿态：ImageReader→DynamicImage→to_rgba8（含 from_decoder 清零 +
/// to_rgba8 clone 两笔整帧临时分配）。动画走 into_frames。
fn kimg_decode_old(bytes: &[u8], anim: bool) -> Option<usize> {
    if anim {
        let dec = kimg::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).ok()?;
        let frames = dec.into_frames().collect_frames().ok()?;
        Some(frames.iter().map(|f| f.buffer().len()).sum())
    } else {
        let img = kimg::ImageReader::with_format(Cursor::new(bytes), kimg::ImageFormat::WebP)
            .decode()
            .ok()?;
        Some(img.to_rgba8().into_raw().len())
    }
}

/// kimg OURS 新直写路径（C# DecodeTo 镜像）：VP8L 静态（无损/无损-alpha）经
/// `decode_static_lossless_rgba8_into` 直写调用方复用缓冲（暖态零分配）；其它静态走
/// `WebPDecoder` 复用缓冲 `read_image`（省 from_decoder 清零 + to_rgba8 clone）。
/// `out` 为复用缓冲（长度须 ≥ w*h*4）。动画仍走 into_frames（开销与旧路径一致）。
fn kimg_decode_new(bytes: &[u8], anim: bool, out: &mut [u8]) -> Option<usize> {
    if anim {
        let dec = kimg::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).ok()?;
        let frames = dec.into_frames().collect_frames().ok()?;
        return Some(frames.iter().map(|f| f.buffer().len()).sum());
    }
    if let Ok(true) = kimg::codecs::webp::decode_static_lossless_rgba8_into(bytes, out) {
        return Some(out.len());
    }
    let dec = kimg::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).ok()?;
    let n = dec.total_bytes() as usize;
    if out.len() < n {
        return None;
    }
    dec.read_image(&mut out[..n]).ok()?;
    Some(n)
}

// ---------------------------------------------------------------------------
// libwebp 1.6.0 SIMD DLL（raw FFI）
// ---------------------------------------------------------------------------

struct WebpDll {
    _handle: *mut c_void,
    get_info: unsafe extern "C" fn(*const u8, usize, *mut c_int, *mut c_int) -> c_int,
    decode_into: unsafe extern "C" fn(*const u8, usize, *mut u8, usize, c_int) -> *mut u8,
}

// 进程存活期内 DLL 保持加载，句柄/函数指针 Send 安全（单一调用线程）。
unsafe impl Send for WebpDll {}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryW(name: *const u16) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
}

fn load_dll(path: &str) -> Result<WebpDll, String> {
    let wide: Vec<u16> = std::ffi::OsStr::new(path)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let handle = unsafe { LoadLibraryW(wide.as_ptr()) };
    if handle.is_null() {
        return Err(format!("LoadLibraryW 失败: {path}"));
    }
    unsafe fn sym(h: *mut c_void, name: &[u8]) -> Option<*mut c_void> {
        let p = unsafe { GetProcAddress(h, name.as_ptr()) };
        if p.is_null() {
            None
        } else {
            Some(p)
        }
    }
    let get_info = unsafe {
        std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*const u8, usize, *mut c_int, *mut c_int) -> c_int>(
            sym(handle, b"WebPGetInfo\0").ok_or("DLL 缺 WebPGetInfo")?,
        )
    };
    let decode_into = unsafe {
        std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*const u8, usize, *mut u8, usize, c_int) -> *mut u8>(
            sym(handle, b"WebPDecodeRGBAInto\0").ok_or("DLL 缺 WebPDecodeRGBAInto")?,
        )
    };
    Ok(WebpDll {
        _handle: handle,
        get_info,
        decode_into,
    })
}

/// 复用缓冲解码到 out（免 libwebp 每次 malloc 的参照失真），返回写入字节数 w*h*4；失败 None。
fn dll_decode_into(dll: &WebpDll, bytes: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut w: c_int = 0;
    let mut h: c_int = 0;
    if unsafe { (dll.get_info)(bytes.as_ptr(), bytes.len(), &mut w, &mut h) } == 0 {
        return None;
    }
    let n = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
    if out.len() < n {
        return None;
    }
    // WebPDecodeRGBAInto 的 stride 必须 >0（libwebp 要求）；传 width*4（C# 已验证同款调用）。
    let stride = (w as c_int).checked_mul(4)?;
    let p = unsafe { (dll.decode_into)(bytes.as_ptr(), bytes.len(), out.as_mut_ptr(), out.len(), stride) };
    if p.is_null() {
        return None;
    }
    Some(n)
}

/// 一次性解码（探测/分类用）：查尺寸、分配后解。
fn dll_decode_once(dll: &WebpDll, bytes: &[u8]) -> Option<usize> {
    let mut w: c_int = 0;
    let mut h: c_int = 0;
    if unsafe { (dll.get_info)(bytes.as_ptr(), bytes.len(), &mut w, &mut h) } == 0 {
        return None;
    }
    let n = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
    let mut buf = vec![0u8; n];
    dll_decode_into(dll, bytes, &mut buf)
}

// ---------------------------------------------------------------------------
// 计时与主流程
// ---------------------------------------------------------------------------

fn median_ms<F: FnMut()>(mut f: F, rounds: usize) -> f64 {
    f(); // 暖机 1 轮
    let mut acc: Vec<f64> = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let t = Instant::now();
        f();
        acc.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    acc.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mid = acc.len() / 2;
    if acc.len() % 2 == 0 {
        (acc[mid - 1] + acc[mid]) / 2.0
    } else {
        acc[mid]
    }
}

fn main() {
    // OURS 后端（M6 后默认即 ours，仍显式设以明确口径）。
    unsafe { std::env::set_var("KIMG_WEBP_BACKEND", "ours") };
    let dll_path = std::env::var("LIBWEBP_DLL").unwrap_or_else(|_| {
        format!("{}/../bin/libwebp.dll", env!("CARGO_MANIFEST_DIR"))
    });
    let dll = load_dll(&dll_path).expect("装载 libwebp DLL 失败");

    println!(
        "KIMG_WEBP_VS_DLL profile=release(json imgbench) rounds={ROUNDS} libwebp_dll={dll_path}"
    );

    // 语料：bootstrap 已把 `target/imgbench-corpus/WEBP`(20) + `SDCB_ROOT\测试图片\WEBP`(52)
    // 快照进套件 `corpus/webp`（文件名去重 72）。默认探针所在 crate 的相对路径；
    // `KIMG_WEBP_VS_DLL_CORPUS`（`;` 分隔多目录）可覆盖，保留多源语料口。
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let corpus_dirs: Vec<String> = std::env::var("KIMG_WEBP_VS_DLL_CORPUS")
        .map(|v| v.split(';').map(|s| s.to_string()).collect())
        .unwrap_or_else(|_| {
            vec![format!("{}/../../corpus/webp", env!("CARGO_MANIFEST_DIR"))]
        });
    for d in corpus_dirs {
        collect_dir(std::path::Path::new(&d), &mut files);
    }
    println!(
        "KIMG_WEBP_VS_DLL note corpus candidates={}",
        files.len()
    );

    // 分类 + 探测：任一启用通道解不了的直接 drop（与 bench 一致）。
    let mut corpus = Vec::new();
    for (name, bytes) in files {
        let Some(scen) = classify(&bytes) else {
            println!("KIMG_WEBP_VS_DLL note drop {name} (未知 WebP 头部)");
            continue;
        };
        let anim = scen.animated();
        let kimg_old_ok = kimg_decode_old(&bytes, anim).is_some();
        let dll_ok = anim || dll_decode_once(&dll, &bytes).is_some();
        // 新直写路径可解性（复用缓冲尺寸 = w*h*4；仅一次性探测，不计时）。
        let mut kimg_new_ok = false;
        if anim {
            kimg_new_ok = true;
        } else {
            let mut w: c_int = 0;
            let mut h: c_int = 0;
            let n = if unsafe { (dll.get_info)(bytes.as_ptr(), bytes.len(), &mut w, &mut h) } != 0 {
                (w as usize).checked_mul(h as usize).map(|n| n * 4)
            } else {
                None
            };
            if let Some(n) = n {
                kimg_new_ok = kimg_decode_new(&bytes, false, &mut vec![0u8; n]).is_some();
            }
        }
        if !(kimg_old_ok && dll_ok && kimg_new_ok) {
            println!(
                "KIMG_WEBP_VS_DLL note drop {name} (kimg_old={kimg_old_ok} kimg_new={kimg_new_ok} dll={dll_ok})"
            );
            continue;
        }
        corpus.push((scen, name, bytes));
    }

    let mut total = 0usize;
    for scen in SCENARIOS {
        let scen_files: Vec<&(Scenario, String, Vec<u8>)> =
            corpus.iter().filter(|(s, _, _)| *s == scen).collect();
        if scen_files.is_empty() {
            println!("KIMG_WEBP_VS_DLL {} SKIP (无该场景语料)", scen.name());
            continue;
        }
        total += scen_files.len();
        bench_scenario(scen, &scen_files, rounds(), &dll);
    }
    // 逐文件明细（对账用：同文件 kimg_new vs libwebp，暖机 1 + 9 轮中位，复用缓冲同姿态）。
    // KIMG_WEBP_THREADS 由外层 env 决定（不设=默认 MT / '0'=强制单线程），与场景聚合同姿态。
    // 说明：此段仅新增输出，不改动上方场景聚合逻辑（交接 §3.1 口径保持不变）。
    for (scen, name, bytes) in &corpus {
        if scen.animated() {
            continue;
        }
        let mut w: c_int = 0;
        let mut h: c_int = 0;
        if unsafe { (dll.get_info)(bytes.as_ptr(), bytes.len(), &mut w, &mut h) } == 0 {
            continue;
        }
        let n = (w as usize)
            .checked_mul(h as usize)
            .and_then(|v| v.checked_mul(4));
        let Some(n) = n else { continue };
        if n == 0 {
            continue;
        }
        let mut buf = vec![0u8; n];
        let kn = median_ms(
            || {
                let _ = kimg_decode_new(bytes, false, &mut buf).expect("探测已保证可解");
            },
            rounds(),
        );
        let lp = median_ms(
            || {
                let _ = dll_decode_into(&dll, bytes, &mut buf).expect("探测已保证可解");
            },
            rounds(),
        );
        // 宏块数 ≈ w*h/256（16×16 块）；>2304 才触发 kimg MT 有损重建∥滤波流水线。
        let mb = (w as u64 * h as u64) / 256;
        println!(
            "KIMG_WEBP_VS_DLL perfile {} {} {}x{} mb={} kimg_new={:.3} libwebp={:.3} ratio_knew_lib={:.3}",
            scen.name(),
            name,
            w,
            h,
            mb,
            kn,
            lp,
            kn / lp
        );
    }
    println!(
        "KIMG_WEBP_VS_DLL note corpus_size={} bench_files={total}",
        corpus.len()
    );
}

fn rounds() -> usize {
    std::env::var("KIMG_WEBP_VS_DLL_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(ROUNDS)
}

fn bench_scenario(
    scen: Scenario,
    files: &[&(Scenario, String, Vec<u8>)],
    rounds: usize,
    dll: &WebpDll,
) {
    let anim = scen.animated();
    let kimg_old_ms = median_ms(
        || {
            for (_, _, b) in files {
                let _ = kimg_decode_old(b, anim).expect("探测已保证可解");
            }
        },
        rounds,
    );
    if anim {
        println!(
            "KIMG_WEBP_VS_DLL {} files={} kimg_old={:.3}ms kimg_new={:.3}ms libwebp=N/A (动画需 WebPDemux*，本版仅 WebPDecodeRGBA)",
            scen.name(),
            files.len(),
            kimg_old_ms,
            kimg_old_ms
        );
        return;
    }
    // libwebp 复用缓冲（免每次 malloc 的参照失真）；kimg 新路径同批复用同一 `bufs`（同为
    // w*h*4，暖态零分配对拍，二者预分配各一次不计入计时）。
    let mut bufs: Vec<Vec<u8>> = files
        .iter()
        .map(|(_, _, b)| {
            let mut buf = Vec::new();
            let mut w: c_int = 0;
            let mut h: c_int = 0;
            if unsafe { (dll.get_info)(b.as_ptr(), b.len(), &mut w, &mut h) } != 0 {
                buf.resize((w as usize) * (h as usize) * 4, 0);
            }
            buf
        })
        .collect();
    let kimg_new_ms = median_ms(
        || {
            for (i, (_, _, b)) in files.iter().enumerate() {
                let _ = kimg_decode_new(b, false, &mut bufs[i]).expect("探测已保证可解");
            }
        },
        rounds,
    );
    let lp_ms = median_ms(
        || {
            for (i, (_, _, b)) in files.iter().enumerate() {
                let _ = dll_decode_into(dll, b, &mut bufs[i]).expect("探测已保证可解");
            }
        },
        rounds,
    );
    println!(
        "KIMG_WEBP_VS_DLL {} files={} kimg_old={:.3}ms kimg_new={:.3}ms libwebp={:.3}ms ratio_kimg_new_vs_libwebp={:.3} ratio_kimg_old_vs_new={:.3}",
        scen.name(),
        files.len(),
        kimg_old_ms,
        kimg_new_ms,
        lp_ms,
        kimg_new_ms / lp_ms,
        kimg_old_ms / kimg_new_ms
    );
}
