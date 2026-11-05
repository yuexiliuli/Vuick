//! libjpeg-turbo（turbojpeg.dll，官方 3.2.0 win-x64）vs 自研 jpeg vs zune 三路同窗口基准。
//!
//! 口径与 jpeg_bench 一致：单线程、复用调用方缓冲、直写（turbo 用 `tjDecompress2` 经典
//! TurboJPEG 2 API，彩色→TJPF_RGB(3B/px)、灰度→TJPF_GRAY(1B/px)，与自研 RGB/L8 同布局)，
//! 每文件三解码器逐轮交错取中位 → 同窗口 A/B 不受机器漂移。
//!
//! turbojpeg.dll 用 kernel32 LoadLibrary 运行期加载（零新依赖）。默认
//! `C:\Users\Administrator\Desktop\workspace\rust\sample\libjpeg-turbo\bin\turbojpeg.dll`，
//! env `KIMG_JPEG_TURBO_DLL` 覆盖。
//!
//! 运行（release）：
//! ```text
//! cargo test -p kimg_jpeg --release --test jpeg_turbo_bench -- --ignored --nocapture
//! ```
//! 输出 `JPEG_TURBO FILE <name> turbo=..ms ours=..ms zune=..ms` 与 `JPEG_TURBO AGG ...`
//! （ratio_turbo_ours = ours÷turbo，<1 即自研更快；ratio_ours_zune = ours÷zune）。

mod fixtures;

use std::ffi::{c_int, c_uchar, c_void, c_ulong, OsStr};
use std::io::Cursor;
use std::os::windows::ffi::OsStrExt;
use std::time::Instant;

use kimg_core::ImageDecoder as _;
use kimg_jpeg::codecs::jpeg::JpegDecoder as OursJpeg;

use fixtures::*;

// ---------------------------------------------------------------------------
// kernel32 动态加载 turbojpeg.dll（经典 TurboJPEG 2 API）
// ---------------------------------------------------------------------------

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryW(name: *const u16) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const c_uchar) -> *mut c_void;
    fn FreeLibrary(module: *mut c_void) -> c_int;
}

type TjHandle = *mut c_void;

const TJPF_RGB: c_int = 0;
const TJPF_GRAY: c_int = 6;

struct TurboJpeg {
    lib: *mut c_void,
    handle: TjHandle,
    header: unsafe extern "C" fn(TjHandle, *const c_uchar, c_ulong, *mut c_int, *mut c_int) -> c_int,
    dec2: unsafe extern "C" fn(
        TjHandle,
        *const c_uchar,
        c_ulong,
        *mut c_uchar,
        c_int,
        c_int,
        c_int,
        c_int,
        c_int,
    ) -> c_int,
    destroy: unsafe extern "C" fn(TjHandle) -> c_int,
}

impl TurboJpeg {
    fn load() -> Result<TurboJpeg, String> {
        let path = std::env::var("KIMG_JPEG_TURBO_DLL").unwrap_or_else(|_| {
            r"C:\Users\Administrator\Desktop\workspace\rust\sample\libjpeg-turbo\bin\turbojpeg.dll"
                .to_string()
        });
        let wide: Vec<u16> = OsStr::new(&path).encode_wide().chain(Some(0)).collect();
        let lib = unsafe { LoadLibraryW(wide.as_ptr()) };
        if lib.is_null() {
            return Err(format!(
                "LoadLibraryW 失败：{path}（错误 {}）",
                std::io::Error::last_os_error()
            ));
        }
        macro_rules! sym {
            ($name:literal, $t:ty) => {
                unsafe {
                    let p = GetProcAddress(lib, concat!($name, "\0").as_ptr() as *const c_uchar);
                    if p.is_null() {
                        return Err(format!("GetProcAddress {}=0", $name));
                    }
                    std::mem::transmute::<*mut c_void, $t>(p)
                }
            };
        }
        let init: unsafe extern "C" fn() -> TjHandle = sym!("tjInitDecompress", unsafe extern "C" fn() -> TjHandle);
        let header: unsafe extern "C" fn(TjHandle, *const c_uchar, c_ulong, *mut c_int, *mut c_int) -> c_int =
            sym!("tjDecompressHeader", unsafe extern "C" fn(TjHandle, *const c_uchar, c_ulong, *mut c_int, *mut c_int) -> c_int);
        let dec2: unsafe extern "C" fn(TjHandle, *const c_uchar, c_ulong, *mut c_uchar, c_int, c_int, c_int, c_int, c_int) -> c_int =
            sym!("tjDecompress2", unsafe extern "C" fn(TjHandle, *const c_uchar, c_ulong, *mut c_uchar, c_int, c_int, c_int, c_int, c_int) -> c_int);
        let destroy: unsafe extern "C" fn(TjHandle) -> c_int =
            sym!("tjDestroy", unsafe extern "C" fn(TjHandle) -> c_int);
        let handle = unsafe { (init)() };
        if handle.is_null() {
            return Err("tjInitDecompress 失败".into());
        }
        Ok(TurboJpeg { lib, handle, header, dec2, destroy })
    }

    fn header(&self, bytes: &[u8]) -> Result<(i32, i32), String> {
        let mut w: c_int = 0;
        let mut h: c_int = 0;
        let rc = unsafe {
            (self.header)(self.handle, bytes.as_ptr(), bytes.len() as c_ulong, &mut w, &mut h)
        };
        if rc != 0 {
            Err(format!("tjDecompressHeader rc={rc}"))
        } else if w <= 0 || h <= 0 {
            Err(format!("tjDecompressHeader 非法尺寸 {w}x{h}"))
        } else {
            Ok((w, h))
        }
    }

    /// 解码到 `out`（须已按 w*h*?(3|1) 分配好，不足则越界写）。
    fn decode(&self, bytes: &[u8], out: &mut [u8], rgb: bool) -> Result<(), String> {
        let (w, h) = self.header(bytes)?;
        let expect = (w as usize) * (h as usize) * if rgb { 3 } else { 1 };
        debug_assert_eq!(out.len(), expect, "turbo 输出缓冲尺寸不匹配");
        let fmt = if rgb { TJPF_RGB } else { TJPF_GRAY };
        let rc = unsafe {
            (self.dec2)(
                self.handle,
                bytes.as_ptr(),
                bytes.len() as c_ulong,
                out.as_mut_ptr(),
                w,
                0,
                h,
                fmt,
                0,
            )
        };
        if rc != 0 {
            return Err(format!("tjDecompress2 rc={rc}"));
        }
        Ok(())
    }
}

impl Drop for TurboJpeg {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { (self.destroy)(self.handle) };
        }
        if !self.lib.is_null() {
            unsafe { FreeLibrary(self.lib) };
        }
    }
}

// ---------------------------------------------------------------------------
// 解码器窗口（turbo / ours / zune）
// ---------------------------------------------------------------------------

fn ours_decode(bytes: &[u8], out: &mut [u8]) -> Result<(), String> {
    OursJpeg::new(Cursor::new(bytes))
        .map_err(|e| e.to_string())?
        .read_image(out)
        .map_err(|e| e.to_string())
}

fn zune_decode_into(bytes: &[u8], out: &mut [u8]) -> Result<(), String> {
    let (_, _, _, pix) = zune_decode(bytes)?;
    if pix.len() != out.len() {
        return Err("zune 输出长度不一致".into());
    }
    out.copy_from_slice(&pix);
    Ok(())
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

/// `#[ignore]` 基准入口。
#[test]
#[ignore]
fn turbo_bench_main() {
    let rounds: usize = std::env::var("KIMG_JPEG_TURBO_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);
    let tj = match TurboJpeg::load() {
        Ok(t) => t,
        Err(e) => {
            println!("JPEG_TURBO load turbojpeg.dll 失败：{e}");
            return;
        }
    };
    let dir = corpus_dir();
    if !dir.is_dir() {
        println!("JPEG_TURBO corpus dir missing: {}", dir.display());
        return;
    }
    let files = collect_jpegs(&dir);
    if files.is_empty() {
        println!("JPEG_TURBO corpus empty: {}", dir.display());
        return;
    }
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    println!("JPEG_TURBO profile={profile} rounds={rounds} files={}", files.len());

    // 冒烟：三路各解一次，记录每个文件的格式（RGB/Gray）与缓冲尺寸。
    struct Case {
        name: String,
        bytes: Vec<u8>,
        rgb: bool,
        n: usize,
        ours_buf: Vec<u8>,
        zune_buf: Vec<u8>,
    }
    let mut cases: Vec<Case> = Vec::new();
    for p in &files {
        let name = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let bytes = match std::fs::read(p) {
            Ok(b) => b,
            Err(_) => continue,
        };
        // turbo 解码头拿 w/h（越界安全：单独 header 调用，不写输出缓冲）
        let (w, h) = match tj.header(&bytes) {
            Ok((w, h)) => (w as usize, h as usize),
            Err(e) => {
                println!("JPEG_TURBO SMOKE {name}: turbo 失败 {e}，跳过");
                continue;
            }
        };
        // 用自研头判定灰度（与 jpeg_bench 口径一致：L8 → 灰度）
        let rgb = match OursJpeg::new(Cursor::new(&bytes)) {
            Ok(d) => match d.color_type() {
                kimg_core::ColorType::L8 => false,
                _ => true,
            },
            Err(e) => {
                println!("JPEG_TURBO SMOKE {name}: 自研解码头失败 {e}，跳过");
                continue;
            }
        };
        let n = w * h * if rgb { 3 } else { 1 };
        let mut ours_buf = vec![0u8; n];
        let mut zune_buf = vec![0u8; n];
        let mut turbo_buf = vec![0u8; n];
        let ok_t = tj.decode(&bytes, &mut turbo_buf, rgb).is_ok();
        let ok_o = ours_decode(&bytes, &mut ours_buf).is_ok();
        let ok_z = zune_decode_into(&bytes, &mut zune_buf).is_ok();
        if !ok_t || !ok_o || !ok_z {
            println!("JPEG_TURBO SMOKE {name}: turbo={ok_t} ours={ok_o} zune={ok_z} 未全过，跳过");
            continue;
        }
        println!("JPEG_TURBO SMOKE OK {name} {w}x{h} {} 字节={n}", if rgb { "Rgb24" } else { "Gray8" });
        cases.push(Case { name, bytes, rgb, n, ours_buf, zune_buf });
    }
    if cases.is_empty() {
        println!("JPEG_TURBO no benchable files");
        return;
    }

    // 逐文件三路交错取中位（同窗口）
    let mut agg_turbo: Vec<f64> = Vec::new();
    let mut agg_ours: Vec<f64> = Vec::new();
    let mut agg_zune: Vec<f64> = Vec::new();
    for c in cases.iter_mut() {
        let mut turbo_buf = vec![0u8; c.n];
        let t = median_ms(
            || {
                let _ = tj.decode(&c.bytes, &mut turbo_buf, c.rgb).expect("冒烟已保证");
            },
            rounds,
        );
        let o = median_ms(
            || {
                let _ = ours_decode(&c.bytes, &mut c.ours_buf).expect("冒烟已保证");
            },
            rounds,
        );
        let z = median_ms(
            || {
                let _ = zune_decode_into(&c.bytes, &mut c.zune_buf).expect("冒烟已保证");
            },
            rounds,
        );
        agg_turbo.push(t);
        agg_ours.push(o);
        agg_zune.push(z);
        println!(
            "JPEG_TURBO FILE {} turbo={t:.3}ms ours={o:.3}ms zune={z:.3}ms ours/turbo={:.3} ours/zune={:.3}",
            c.name,
            o / t,
            o / z
        );
    }
    let g_t = geomean(&agg_turbo);
    let g_o = geomean(&agg_ours);
    let g_z = geomean(&agg_zune);
    println!(
        "JPEG_TURBO AGG files={} turbo_geomean={g_t:.3}ms ours_geomean={g_o:.3}ms zune_geomean={g_z:.3}ms ours/turbo={:.3} ours/zune={:.3}",
        cases.len(),
        g_o / g_t,
        g_o / g_z
    );
    println!("JPEG_TURBO done.");
}
