//! kimg_jpeg 自研引擎（OURS） vs libjpeg-turbo 3.2.0（turbojpeg.dll，黑盒）同窗交错中位探针。
//! 第三路 C# 参照（Sdcb.SimdCodecs.Jpeg）在相邻 csharp_runner，由 compare_refs.py 合并成三路表。
//!
//! 口径与 crates/kimg_jpeg/tests/jpeg_turbo_bench.rs 一致：单线程、复用调用方缓冲、直写
//! （turbo 用 `tjDecompress2` 经典 TurboJPEG 2 API，彩色→TJPF_RGB(3B/px)、灰度→TJPF_GRAY(1B/px)，
//! 与自研 RGB/L8 同布局）；灰度判定 = 自研 `color_type()==L8`。
//!
//! 运行（release，长命令前台阻塞到结束，禁后台化）：
//! ```text
//! cargo run --release --manifest-path Benchmarks/jpeg/probe/Cargo.toml
//! ```
//! env / 参数：
//! - `--corpus <dir>` 优先；否则 env `KIMG_JPEG_CORPUS`；缺省 `Benchmarks/corpus/JPEG`
//! - env `KIMG_JPEG_TURBO_DLL`：turbojpeg.dll 路径，缺省 `Benchmarks/jpeg/bin/turbojpeg.dll`
//! - env `KIMG_JPEG_TURBO_ROUNDS`：计时轮数，缺省 9（另各带 1 轮暖机）
//!
//! 输出契约（compare_refs.py / run_acceptance.ps1 解析，勿改前缀）：
//! ```text
//! KIMG_JPEG_VS_TURBO profile=release rounds=9 corpus=<绝对路径> files=<n>
//! KIMG_JPEG_VS_TURBO perfile <文件名> <w>x<h> gray=<0|1> ours=<ms> turbo=<ms> ratio_ours_turbo=<r>
//! KIMG_JPEG_VS_TURBO AGG files=<n> geomean_ours=<ms> geomean_turbo=<ms> ratio_ours_turbo=<r>
//! KIMG_JPEG_VS_TURBO done.
//! ```
//! `turbojpeg.dll` 用 kernel32 LoadLibrary 运行期加载（零新依赖），libjpeg-turbo 只当黑盒。

use std::ffi::{c_int, c_uchar, c_void, c_ulong, OsStr};
use std::io::Cursor;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::Instant;

use kimg_core::ColorType;
use kimg_core::ImageDecoder as _;
use kimg_jpeg::codecs::jpeg::JpegDecoder as OursJpeg;

const ROUNDS_DEFAULT: usize = 9;

// ---------------------------------------------------------------------------
// kernel32 动态加载 turbojpeg.dll（经典 TurboJPEG 2 API，只调 API）
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
            format!("{}/../bin/turbojpeg.dll", env!("CARGO_MANIFEST_DIR"))
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
// 解码通道与计时
// ---------------------------------------------------------------------------

fn ours_decode(bytes: &[u8], out: &mut [u8]) -> Result<(), String> {
    OursJpeg::new(Cursor::new(bytes))
        .map_err(|e| e.to_string())?
        .read_image(out)
        .map_err(|e| e.to_string())
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

/// 收集 .jpg（大小写不敏感扩展名），按名字排序保证确定性。
fn collect_jpegs(dir: &std::path::Path) -> Vec<PathBuf> {
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

fn main() {
    let rounds: usize = std::env::var("KIMG_JPEG_TURBO_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(ROUNDS_DEFAULT);

    // 语料：--corpus 参数 > env KIMG_JPEG_CORPUS > 缺省 Benchmarks/corpus/JPEG（相对探针 crate）。
    let mut cli_corpus: Option<String> = None;
    {
        let args: Vec<String> = std::env::args().collect();
        let mut i = 1;
        while i + 1 < args.len() {
            if args[i] == "--corpus" {
                cli_corpus = Some(args[i + 1].clone());
                i += 2;
            } else {
                i += 1;
            }
        }
    }
    let corpus_default = format!("{}/../../corpus/JPEG", env!("CARGO_MANIFEST_DIR"));
    let corpus_rel = cli_corpus
        .or_else(|| std::env::var("KIMG_JPEG_CORPUS").ok())
        .unwrap_or(corpus_default);
    let dir = std::path::PathBuf::from(&corpus_rel);
    if !dir.is_dir() {
        println!("KIMG_JPEG_VS_TURBO note corpus dir missing: {}", dir.display());
        return;
    }
    let canonical = std::fs::canonicalize(&dir).unwrap_or(dir.clone());
    // 去掉 Windows 扩展长路径前缀 `\\?\`（canonicalize 固有），保证 corpus= 打印干净绝对路径。
    let canonical_str = canonical.to_string_lossy();
    let corpus_display = canonical_str
        .strip_prefix(r"\\?\")
        .unwrap_or(&canonical_str);

    let tj = match TurboJpeg::load() {
        Ok(t) => t,
        Err(e) => {
            println!("KIMG_JPEG_VS_TURBO load turbojpeg.dll 失败：{e}");
            return;
        }
    };

    let files = collect_jpegs(&dir);
    if files.is_empty() {
        println!("KIMG_JPEG_VS_TURBO corpus empty: {}", dir.display());
        return;
    }
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    println!(
        "KIMG_JPEG_VS_TURBO profile={profile} rounds={rounds} corpus={corpus_display} files={}",
        files.len()
    );

    // 冒烟：turbo 解码头拿 w/h；自研 new+read_image 预解一次；任一失败 → SKIP 跳过。
    struct Case {
        name: String,
        bytes: Vec<u8>,
        gray: bool,
        w: usize,
        h: usize,
        n: usize,
        ours_buf: Vec<u8>,
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
        let (w, h) = match tj.header(&bytes) {
            Ok((w, h)) => (w as usize, h as usize),
            Err(e) => {
                skips.push(format!("{name}: turbo 头失败 {e}"));
                continue;
            }
        };
        // 灰度判定：自研 color_type()==L8 → 灰度（与 jpeg_turbo_bench 完全一致）。
        let gray = match OursJpeg::new(Cursor::new(&bytes)) {
            Ok(d) => d.color_type() == ColorType::L8,
            Err(e) => {
                skips.push(format!("{name}: 自研解码头失败 {e}"));
                continue;
            }
        };
        let n = w * h * if gray { 1 } else { 3 };
        let mut ours_buf = vec![0u8; n];
        let ok_t = tj.decode(&bytes, &mut vec![0u8; n], !gray).is_ok();
        let ok_o = ours_decode(&bytes, &mut ours_buf).is_ok();
        if !ok_t || !ok_o {
            skips.push(format!("{name}: turbo={ok_t} ours={ok_o} 未全过"));
            continue;
        }
        cases.push(Case { name, bytes, gray, w, h, n, ours_buf });
    }
    for s in &skips {
        println!("KIMG_JPEG_VS_TURBO SKIP {s}");
    }
    if cases.is_empty() {
        println!("KIMG_JPEG_VS_TURBO no benchable files");
        return;
    }

    // 逐文件同窗交错中位（各自复用缓冲，暖态零分配）。
    let mut agg_ours: Vec<f64> = Vec::new();
    let mut agg_turbo: Vec<f64> = Vec::new();
    for c in cases.iter_mut() {
        let o = median_ms(
            || {
                let _ = ours_decode(&c.bytes, &mut c.ours_buf).expect("冒烟已保证");
            },
            rounds,
        );
        let mut turbo_buf = vec![0u8; c.n];
        let t = median_ms(
            || {
                let _ = tj.decode(&c.bytes, &mut turbo_buf, !c.gray).expect("冒烟已保证");
            },
            rounds,
        );
        agg_ours.push(o);
        agg_turbo.push(t);
        println!(
            "KIMG_JPEG_VS_TURBO perfile {} {}x{} gray={} ours={o:.3} turbo={t:.3} ratio_ours_turbo={:.3}",
            c.name,
            c.w,
            c.h,
            if c.gray { 1 } else { 0 },
            o / t
        );
    }
    let g_ours = geomean(&agg_ours);
    let g_turbo = geomean(&agg_turbo);
    println!(
        "KIMG_JPEG_VS_TURBO AGG files={} geomean_ours={g_ours:.3} geomean_turbo={g_turbo:.3} ratio_ours_turbo={:.3}",
        cases.len(),
        g_ours / g_turbo
    );
    println!("KIMG_JPEG_VS_TURBO done.");
}
