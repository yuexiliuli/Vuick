//! kimg_png（OURS） vs 真机 libpng 1.6（DLL，简化 API→RGBA8）单核对拍探针（Benchmarks/png/libpng_probe）。
//!
//! libpng16.dll（1.6.59，MSYS2 mingw-w64-x86_64-libpng 官方打包，SHA256 8cf4e17e…cfc20）与其依赖
//! zlib1.dll 统一放 `Benchmarks/png/bin/`（gitignore，管理同 jpeg 的 `bin/turbojpeg.dll`）；
//! 通过 LOAD_WITH_ALTERED_SEARCH_PATH 使依赖从 DLL 所在目录解析，无需设 PATH。
//! 运行（release，opt-level=3，缺省即用 bin/ 下的 DLL）：
//! ```
//! cargo run --release --manifest-path Benchmarks\png\libpng_probe\Cargo.toml -- --corpus Benchmarks\corpus\PNG
//! ```
//! 可选：`--libpng <dllPath>` 指向其它 libpng16.dll。
//!
//! 口径：RGBA8 —— 对语料每张 PNG，ours 走 PngDecoder 自然 EXPAND 输出、libpng 走简化 API
//! `png_image_begin_read_from_memory`→`png_image_finish_read(PNG_FORMAT_RGBA)`→RGBA8。
//! 比较集 = layout=rgba8（ours 自然输出即 RGBA8，与 C# 对拍集同口径）；RGB8/gray8 等
//! layout=other 不计 geomean。
//! 单核口径：进程内强制 `KIMG_PNG_PIPELINE=0`（关双核管道，对齐 C#/libpng 单线程）。
//! libpng16.dll 用 kernel32 LoadLibrary 运行期加载（零新依赖），仅当黑盒（同 jpeg probe 姿势）。
//!
//! 输出契约：`KIMG_PNG_VS_LIBPNG` 前缀行（perfile + AGG），供脚本/手读解析。

use std::path::PathBuf;
use std::time::Instant;

use kimg_core::ColorType;
use kimg_core::ImageDecoder as _;
use kimg_png::codecs::png::PngDecoder as OursPng;

const ROUNDS_DEFAULT: usize = 9;
const PNG_FORMAT_RGBA: u32 = 0x03; // COLOR|ALPHA
const PNG_IMAGE_VERSION: u32 = 1;
const MESSAGE_LEN: usize = 64;

#[repr(C)]
struct PngImage {
    opaque: *mut core::ffi::c_void,
    version: u32,
    width: u32,
    height: u32,
    format: u32,
    flags: u32,
    colormap_entries: u32,
    warning_or_error: u32,
    message: [core::ffi::c_char; MESSAGE_LEN],
}

type BeginRead =
    unsafe extern "C" fn(*mut PngImage, *const core::ffi::c_void, usize) -> i32;
type FinishRead = unsafe extern "C" fn(
    *mut PngImage,
    *const core::ffi::c_void, // background (null)
    *mut core::ffi::c_void,  // buffer
    i32,                      // row_stride (0 = auto/tight)
    *const core::ffi::c_void, // colormap (null)
) -> i32;
type ImageFree = unsafe extern "C" fn(*mut PngImage);

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryExW(
        name: *const u16,
        h_file: *mut core::ffi::c_void,
        dw_flags: u32,
    ) -> *mut core::ffi::c_void;
    fn GetProcAddress(module: *mut core::ffi::c_void, name: *const u8) -> *mut core::ffi::c_void;
}

// LOAD_WITH_ALTERED_SEARCH_PATH：令 libpng16.dll 的依赖（zlib.dll）从 DLL 自身所在目录解析，
// 使探针零 PATH 自包含（DLL 与 zlib 同放 Benchmarks/png/bin/）。
const LOAD_WITH_ALTERED_SEARCH_PATH: u32 = 0x0000_0008;

struct LibpngLib {
    begin_read: BeginRead,
    finish_read: FinishRead,
    image_free: ImageFree,
}

fn load_libpng(path: &str) -> Result<LibpngLib, String> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let handle = unsafe {
        LoadLibraryExW(
            wide.as_ptr(),
            core::ptr::null_mut(),
            LOAD_WITH_ALTERED_SEARCH_PATH,
        )
    };
    if handle.is_null() {
        return Err(format!("LoadLibraryW 失败: {path}"));
    }
    let get = |name: &str| -> Option<*mut core::ffi::c_void> {
        let c = std::ffi::CString::new(name).ok()?;
        let p = unsafe { GetProcAddress(handle, c.as_ptr() as *const u8) };
        (!p.is_null()).then_some(p)
    };
    let begin_read = get("png_image_begin_read_from_memory")
        .ok_or_else(|| format!("GetProcAddress png_image_begin_read_from_memory=0"))?;
    let finish_read = get("png_image_finish_read")
        .ok_or_else(|| "GetProcAddress png_image_finish_read=0".to_string())?;
    let image_free = get("png_image_free")
        .ok_or_else(|| "GetProcAddress png_image_free=0".to_string())?;
    Ok(LibpngLib {
        begin_read: unsafe { core::mem::transmute::<_, BeginRead>(begin_read) },
        finish_read: unsafe { core::mem::transmute::<_, FinishRead>(finish_read) },
        image_free: unsafe { core::mem::transmute::<_, ImageFree>(image_free) },
    })
}

fn libpng_decode(lib: &LibpngLib, bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut image = PngImage {
        opaque: core::ptr::null_mut(),
        version: PNG_IMAGE_VERSION,
        width: 0,
        height: 0,
        format: 0,
        flags: 0,
        colormap_entries: 0,
        warning_or_error: 0,
        message: [0; MESSAGE_LEN],
    };
    let status = unsafe {
        (lib.begin_read)(
            &mut image,
            bytes.as_ptr() as *const core::ffi::c_void,
            bytes.len(),
        )
    };
    if status != 1 {
        unsafe { (lib.image_free)(&mut image) };
        return None;
    }
    image.format = PNG_FORMAT_RGBA;
    let (w, h) = (image.width, image.height);
    let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
    let status = unsafe {
        (lib.finish_read)(
            &mut image,
            core::ptr::null(),
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            0,
            core::ptr::null(),
        )
    };
    unsafe { (lib.image_free)(&mut image) };
    if status != 1 {
        return None;
    }
    Some((w, h, buf))
}

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

fn geomean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    (values.iter().map(|v| v.ln()).sum::<f64>() / values.len() as f64).exp()
}

fn collect_pngs(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension()
                .map(|x| x.eq_ignore_ascii_case("png"))
                .unwrap_or(false)
            {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn main() {
    unsafe { std::env::set_var("KIMG_PNG_PIPELINE", "0") };
    let rounds: usize = std::env::var("KIMG_PNG_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(ROUNDS_DEFAULT);

    let args: Vec<String> = std::env::args().collect();
    let mut corpus: Option<String> = None;
    let mut libpng_dll: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        if i + 1 < args.len() && args[i] == "--corpus" {
            corpus = Some(args[i + 1].clone());
            i += 2;
        } else if i + 1 < args.len() && args[i] == "--libpng" {
            libpng_dll = Some(args[i + 1].clone());
            i += 2;
        } else {
            i += 1;
        }
    }
    let corpus = corpus.ok_or_else(|| panic!("--corpus 必填")).unwrap();
    let libpng_dll = libpng_dll
        .unwrap_or_else(|| {
            // 仓库统一管理：Benchmarks/png/bin/libpng16.dll（与 jpeg/bin/turbojpeg.dll 同款）。
            // canonicalize 消除 `..` 段：LOAD_WITH_ALTERED_SEARCH_PATH 要求无 `..` 的绝对路径。
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("bin")
                .join("libpng16.dll")
                .canonicalize()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| {
                    concat!(env!("CARGO_MANIFEST_DIR"), "/../bin/libpng16.dll").to_string()
                })
        });
    let lib = load_libpng(&libpng_dll).expect("加载 libpng16.dll 失败");

    let dir = std::path::Path::new(&corpus);
    if !dir.is_dir() {
        println!("KIMG_PNG_VS_LIBPNG note corpus missing: {}", dir.display());
        return;
    }
    let files = collect_pngs(dir);
    if files.is_empty() {
        println!("KIMG_PNG_VS_LIBPNG corpus empty: {}", dir.display());
        return;
    }
    println!(
        "KIMG_PNG_VS_LIBPNG profile=release rounds={} corpus={} files={} libpng_dll={}",
        rounds,
        corpus,
        files.len(),
        libpng_dll
    );

    struct Case {
        name: String,
        bytes: Vec<u8>,
        w: usize,
        h: usize,
        color: ColorType,
        buf: Vec<u8>,
    }
    let mut cases: Vec<Case> = Vec::new();
    let mut skips: Vec<String> = Vec::new();
    for p in &files {
        let name = p
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let bytes = match std::fs::read(p) {
            Ok(b) => b,
            Err(e) => {
                skips.push(format!("{name}: 读文件失败 {e}"));
                continue;
            }
        };
        let dec = match OursPng::from_slice(&bytes) {
            Ok(d) => d,
            Err(e) => {
                skips.push(format!("{name}: 解码头失败 {e}"));
                continue;
            }
        };
        let (w, h) = dec.dimensions();
        let color = dec.color_type();
        let bpp = usize::from(color.bytes_per_pixel());
        let mut buf = vec![0u8; (w as usize) * (h as usize) * bpp];
        if dec.read_image(&mut buf).is_err() {
            skips.push(format!("{name}: ours 冒烟解码失败"));
            continue;
        }
        // libpng 冒烟 + 尺寸校验（不测速），提前把失败文件剔出计时集。
        match libpng_decode(&lib, &bytes) {
            Some((lw, lh, _)) => {
                if (lw as usize, lh as usize) != (w as usize, h as usize) {
                    skips.push(format!(
                        "{name}: 尺寸不一致 ours={w}x{h} libpng={lw}x{lh}（剔除）"
                    ));
                    continue;
                }
            }
            None => {
                skips.push(format!("{name}: libpng 冒烟解码失败"));
                continue;
            }
        }
        cases.push(Case {
            name,
            bytes,
            w: w as usize,
            h: h as usize,
            color,
            buf,
        });
    }
    for s in &skips {
        println!("KIMG_PNG_VS_LIBPNG SKIP {s}");
    }
    if cases.is_empty() {
        println!("KIMG_PNG_VS_LIBPNG no benchable files");
        return;
    }

    // 逐文件：字节恒等（layout=rgba8 时）→ 各通道计时 → 输出。
    let mut agg: Vec<f64> = Vec::new();
    let mut total_ours = 0.0;
    let mut total_libpng = 0.0;
    for c in cases.iter_mut() {
        let bpp = usize::from(c.color.bytes_per_pixel());
        let layout = if c.color == ColorType::Rgba8 && bpp == 4 {
            "rgba8"
        } else {
            "other"
        };
        let byteeq = if layout == "rgba8" {
            match libpng_decode(&lib, &c.bytes) {
                Some((_, _, ref buf)) if buf == &c.buf => "yes".to_string(),
                Some((_, _, ref buf)) => {
                    let n = buf
                        .iter()
                        .zip(c.buf.iter())
                        .filter(|(a, b)| a != b)
                        .count();
                    format!("diff{n}")
                }
                None => "libpng-fail".to_string(),
            }
        } else {
            "n/a".to_string()
        };
        let ours_ms = median_ms(
            || {
                let d = OursPng::from_slice(&c.bytes).expect("冒烟已保证");
                let _ = d.read_image(&mut c.buf).expect("冒烟已保证");
            },
            rounds,
        );
        let libpng_ms = median_ms(
            || {
                let _ = libpng_decode(&lib, &c.bytes).expect("冒烟已保证");
            },
            rounds,
        );
        if layout == "rgba8" {
            agg.push(ours_ms / libpng_ms);
            total_ours += ours_ms;
            total_libpng += libpng_ms;
        }
        println!(
            "KIMG_PNG_VS_LIBPNG perfile {} {}x{} color={:?} bpp={} layout={} byteeq={} ours={:.3} libpng={:.3} ours_libpng={:.3}",
            c.name,
            c.w,
            c.h,
            c.color,
            bpp,
            layout,
            byteeq,
            ours_ms,
            libpng_ms,
            ours_ms / libpng_ms
        );
    }
    let n = agg.len();
    let gm = geomean(&agg);
    println!(
        "KIMG_PNG_VS_LIBPNG AGG compare_rgba8={} geomean_ours_libpng={gm:.3} total_ours_ms={total_ours:.3} total_libpng_ms={total_libpng:.3}",
        n
    );
    println!("KIMG_PNG_VS_LIBPNG done.");
}
