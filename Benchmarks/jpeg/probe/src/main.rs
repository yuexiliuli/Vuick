//! kimg_jpeg 自研引擎（OURS） vs libjpeg-turbo 3.2.0（turbojpeg.dll，黑盒）同窗交错中位探针。
//! 第三路 C# 参照（Sdcb.SimdCodecs.Jpeg）在相邻 csharp_runner，由 compare_refs.py 合并成三路表。
//!
//! 两种 turbo 口径（--mode / env `KIMG_JPEG_TURBO_MODE`）：
//! - `tj2-rgb`（默认，既有 acceptance 口径）：`tjDecompress2` 经典 TurboJPEG 2 API，
//!   彩色→TJPF_RGB(3B/px)、灰度→TJPF_GRAY(1B/px)，与自研 RGB/L8 同布局；计时含每轮 header 解析
//!   （与历史 C#/turbo≈1.076 基线同语义，勿改）。
//! - `tj3-rgba`（Sdcb 官方基准对齐口径）：`tj3Decompress8` TurboJPEG 3 API，
//!   一律 TJPF_RGBA(4B/px、灰度亦 4B，等同 Sdcb `DecodeTo(..., Rgba32, Full)`)、默认参数
//!   （slow IDCT + fancy upsampling）、reused buffer；计时直接调 tj3Decompress8（不额外 header 解析）。
//! 自研侧两种口径下均照旧：`read_image` 到 RGB/L8 布局计时。
//! 宽高一律取 tj2 `tjDecompressHeader`（同一 DLL 同一 SOF 解析；tj3 无独立 header API）。
//!
//! 运行（release，长命令前台阻塞到结束，禁后台化）：
//! ```text
//! cargo run --release --manifest-path Benchmarks/jpeg/probe/Cargo.toml -- --mode tj3-rgba
//! ```
//! env / 参数：
//! - `--corpus <dir>` 优先；否则 env `KIMG_JPEG_CORPUS`；缺省 `Benchmarks/corpus/JPEG`
//! - `--mode <tj2-rgb|tj3-rgba>` 优先；否则 env `KIMG_JPEG_TURBO_MODE`；缺省 `tj2-rgb`
//! - env `KIMG_JPEG_TURBO_DLL`：turbojpeg.dll 路径，缺省 `Benchmarks/jpeg/bin/turbojpeg.dll`
//! - env `KIMG_JPEG_TURBO_ROUNDS`：计时轮数，缺省 9（另各带 1 轮暖机）
//!
//! 输出契约（compare_refs.py / run_acceptance.ps1 解析，勿改前缀）：
//! ```text
//! KIMG_JPEG_VS_TURBO profile=release rounds=9 mode=tj2-rgb corpus=<绝对路径> files=<n>
//! KIMG_JPEG_VS_TURBO perfile <文件名> <w>x<h> gray=<0|1> ours=<ms> turbo_tj2_rgb=<ms> ratio_ours_turbo=<r>
//! KIMG_JPEG_VS_TURBO AGG files=<n> mode=tj2-rgb geomean_ours=<ms> geomean_turbo=<ms> ratio_ours_turbo=<r>
//! KIMG_JPEG_VS_TURBO done.
//! ```
//! perfile 的 turbo 字段名随 mode：`turbo_tj2_rgb=` / `turbo_tj3_rgba=`，compare_refs.py 自动识别。
//! `turbojpeg.dll` 用 kernel32 LoadLibrary 运行期加载（零新依赖），libjpeg-turbo 只当黑盒；
//! tj3 API 符号（tj3Init/tj3Decompress8/tj3Destroy）先 kernel32 GetProcAddress 实证存在再调用。

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
// kernel32 动态加载 turbojpeg.dll（经典 TurboJPEG 2 API + TurboJPEG 3 API，只调 API）
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
const TJPF_RGBA: c_int = 7;
// TurboJPEG 3 `tj3Init` 的 init type：TJINIT_DECOMPRESS = 1（见 libjpeg-turbo 3.x turbojpeg.h）。
const TJINIT_DECOMPRESS: c_int = 1;

/// 复用 GetProcAddress 的符号装载宏（两种 API 共用，置模块级；lib 经参数显式传入保卫生）。
macro_rules! sym {
    ($lib:expr, $name:literal, $t:ty) => {
        unsafe {
            let p = GetProcAddress($lib, concat!($name, "\0").as_ptr() as *const c_uchar);
            if p.is_null() {
                return Err(format!("GetProcAddress {}=0", $name));
            }
            std::mem::transmute::<*mut c_void, $t>(p)
        }
    };
}

// ---------------------------------------------------------------------------
// tj2：经典 TurboJPEG 2 API（header + tjDecompress2），既有路径原样保留
// ---------------------------------------------------------------------------

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
        let init: unsafe extern "C" fn() -> TjHandle = sym!(lib, "tjInitDecompress", unsafe extern "C" fn() -> TjHandle);
        let header: unsafe extern "C" fn(TjHandle, *const c_uchar, c_ulong, *mut c_int, *mut c_int) -> c_int =
            sym!(lib, "tjDecompressHeader", unsafe extern "C" fn(TjHandle, *const c_uchar, c_ulong, *mut c_int, *mut c_int) -> c_int);
        let dec2: unsafe extern "C" fn(TjHandle, *const c_uchar, c_ulong, *mut c_uchar, c_int, c_int, c_int, c_int, c_int) -> c_int =
            sym!(lib, "tjDecompress2", unsafe extern "C" fn(TjHandle, *const c_uchar, c_ulong, *mut c_uchar, c_int, c_int, c_int, c_int, c_int) -> c_int);
        let destroy: unsafe extern "C" fn(TjHandle) -> c_int =
            sym!(lib, "tjDestroy", unsafe extern "C" fn(TjHandle) -> c_int);
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

    /// tj2 解码到 `out`（须已按 w*h*?(3|1) 分配好，不足则越界写）。计时含每轮 header 解析（既有口径）。
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
// tj3：TurboJPEG 3 API（tj3Init/tj3Decompress8/tj3Destroy），Sdcb 对齐口径
// ---------------------------------------------------------------------------

struct TurboJpeg3 {
    lib: *mut c_void,
    handle: TjHandle,
    dec8: unsafe extern "C" fn(TjHandle, *const c_uchar, usize, *mut c_uchar, c_int, c_int) -> c_int,
    destroy: unsafe extern "C" fn(TjHandle),
}

impl TurboJpeg3 {
    fn load() -> Result<TurboJpeg3, String> {
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
        let init: unsafe extern "C" fn(c_int) -> TjHandle =
            sym!(lib, "tj3Init", unsafe extern "C" fn(c_int) -> TjHandle);
        let dec8: unsafe extern "C" fn(TjHandle, *const c_uchar, usize, *mut c_uchar, c_int, c_int) -> c_int =
            sym!(lib, "tj3Decompress8", unsafe extern "C" fn(TjHandle, *const c_uchar, usize, *mut c_uchar, c_int, c_int) -> c_int);
        let destroy: unsafe extern "C" fn(TjHandle) =
            sym!(lib, "tj3Destroy", unsafe extern "C" fn(TjHandle));
        let handle = unsafe { (init)(TJINIT_DECOMPRESS) };
        if handle.is_null() {
            return Err("tj3Init(TJINIT_DECOMPRESS) 失败".into());
        }
        Ok(TurboJpeg3 { lib, handle, dec8, destroy })
    }

    /// tj3 解码到 `out`（须已按 w*h*4 分配，不足则越界写）；pitch=w*4 紧密 RGBA，默认参数。
    /// 计时直接调 tj3Decompress8，无额外 header 调用（对齐 Sdcb `DecodeTo(Rgba32)` 姿态）。
    fn decode_rgba(&self, bytes: &[u8], out: &mut [u8], w: usize, h: usize) -> Result<(), String> {
        debug_assert_eq!(out.len(), w * h * 4, "tj3-rgba 输出缓冲尺寸不匹配");
        let rc = unsafe {
            (self.dec8)(
                self.handle,
                bytes.as_ptr(),
                bytes.len(),
                out.as_mut_ptr(),
                (w * 4) as c_int,
                TJPF_RGBA,
            )
        };
        if rc != 0 {
            return Err(format!("tj3Decompress8 rc={rc}"));
        }
        Ok(())
    }
}

impl Drop for TurboJpeg3 {
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
// 口径选择
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum TurboMode {
    /// 旧对齐口径：彩色 TJPF_RGB(3B)、灰度 TJPF_GRAY(1B)；计时含 tj2 header。
    Tj2Rgb,
    /// Sdcb 对齐口径：一律 TJPF_RGBA(4B)；计时直接 tj3Decompress8。
    Tj3Rgba,
}

impl TurboMode {
    fn key(self) -> &'static str {
        match self {
            TurboMode::Tj2Rgb => "tj2-rgb",
            TurboMode::Tj3Rgba => "tj3-rgba",
        }
    }

    /// perfile 行 turbo 字段名（compare_refs.py 自动识别）。
    fn turbo_field(self) -> &'static str {
        match self {
            TurboMode::Tj2Rgb => "turbo_tj2_rgb",
            TurboMode::Tj3Rgba => "turbo_tj3_rgba",
        }
    }

    /// turbo 每像素字节数（tj2-rgb：彩色3/灰度1；tj3-rgba：一律4 = Sdcb Rgba32）。
    fn bpp(self, gray: bool) -> usize {
        match self {
            TurboMode::Tj2Rgb => {
                if gray {
                    1
                } else {
                    3
                }
            }
            TurboMode::Tj3Rgba => 4,
        }
    }
}

fn parse_mode() -> Option<TurboMode> {
    // 优先 CLI --mode，其次 env KIMG_JPEG_TURBO_MODE，缺省 tj2-rgb。
    let mut cli_mode: Option<String> = None;
    {
        let args: Vec<String> = std::env::args().collect();
        let mut i = 1;
        while i + 1 < args.len() {
            if args[i] == "--mode" {
                cli_mode = Some(args[i + 1].clone());
                break;
            }
            i += 1;
        }
    }
    let m = cli_mode
        .or_else(|| std::env::var("KIMG_JPEG_TURBO_MODE").ok())
        .unwrap_or_else(|| "tj2-rgb".to_string());
    match m.as_str() {
        "tj2-rgb" => Some(TurboMode::Tj2Rgb),
        "tj3-rgba" => Some(TurboMode::Tj3Rgba),
        other => {
            eprintln!("未知 turbo mode: {other}（支持 tj2-rgb / tj3-rgba）");
            None
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
    let Some(mode) = parse_mode() else {
        return;
    };
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
    // tj3 仅 tj3-rgba 口径需要（本 DLL 已实证含 tj3* 符号；缺符号则由 load 报错）。
    let tj3 = match mode {
        TurboMode::Tj3Rgba => match TurboJpeg3::load() {
            Ok(t) => Some(t),
            Err(e) => {
                println!("KIMG_JPEG_VS_TURBO load turbojpeg.dll (tj3) 失败：{e}");
                return;
            }
        },
        TurboMode::Tj2Rgb => None,
    };

    let files = collect_jpegs(&dir);
    if files.is_empty() {
        println!("KIMG_JPEG_VS_TURBO corpus empty: {}", dir.display());
        return;
    }
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    println!(
        "KIMG_JPEG_VS_TURBO profile={profile} rounds={rounds} mode={} corpus={corpus_display} files={}",
        mode.key(),
        files.len()
    );

    // 冒烟：turbo 解码头拿 w/h；两种口径各自 turbo 预解 + 自研 new+read_image 预解；任一失败 → SKIP。
    struct Case {
        name: String,
        bytes: Vec<u8>,
        gray: bool,
        w: usize,
        h: usize,
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
        let n_ours = w * h * if gray { 1 } else { 3 };
        let mut ours_buf = vec![0u8; n_ours];
        let ok_t = match mode {
            TurboMode::Tj2Rgb => tj
                .decode(&bytes, &mut vec![0u8; w * h * mode.bpp(gray)], !gray)
                .is_ok(),
            TurboMode::Tj3Rgba => tj3
                .as_ref()
                .expect("tj3-rgba 已加载")
                .decode_rgba(&bytes, &mut vec![0u8; w * h * 4], w, h)
                .is_ok(),
        };
        let ok_o = ours_decode(&bytes, &mut ours_buf).is_ok();
        if !ok_t || !ok_o {
            skips.push(format!("{name}: turbo={ok_t} ours={ok_o} 未全过"));
            continue;
        }
        cases.push(Case { name, bytes, gray, w, h, ours_buf });
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
    let tfield = mode.turbo_field();
    for c in cases.iter_mut() {
        let o = median_ms(
            || {
                let _ = ours_decode(&c.bytes, &mut c.ours_buf).expect("冒烟已保证");
            },
            rounds,
        );
        let n_turbo = c.w * c.h * mode.bpp(c.gray);
        let mut turbo_buf = vec![0u8; n_turbo];
        let t = match mode {
            TurboMode::Tj2Rgb => median_ms(
                || {
                    let _ = tj
                        .decode(&c.bytes, &mut turbo_buf, !c.gray)
                        .expect("冒烟已保证");
                },
                rounds,
            ),
            TurboMode::Tj3Rgba => median_ms(
                || {
                    let _ = tj3
                        .as_ref()
                        .expect("tj3-rgba 已加载")
                        .decode_rgba(&c.bytes, &mut turbo_buf, c.w, c.h)
                        .expect("冒烟已保证");
                },
                rounds,
            ),
        };
        agg_ours.push(o);
        agg_turbo.push(t);
        println!(
            "KIMG_JPEG_VS_TURBO perfile {} {}x{} gray={} ours={o:.3} {tfield}={t:.3} ratio_ours_turbo={:.3}",
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
        "KIMG_JPEG_VS_TURBO AGG files={} mode={} geomean_ours={g_ours:.3} geomean_turbo={g_turbo:.3} ratio_ours_turbo={:.3}",
        cases.len(),
        mode.key(),
        g_ours / g_turbo
    );
    println!("KIMG_JPEG_VS_TURBO done.");
}
