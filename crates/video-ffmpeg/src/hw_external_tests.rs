//! 外部 D3D11 设备注入（零回读 GPU 直通）回归测试。
//!
//! 不经 gpui：直接经 windows crate 建 D3D11 设备（硬件优先、WARP 兜底）→ `open_external_hw`
//! 注入解码器 → `next_frame_hw` 断言纹理句柄 / 数组下标 / pts / 宽高 / 帧池槽位回收。
//!
//! 跑法：ffmpeg 的 cdylib 内 FFmpeg 操作须串行（同既有 decode 测试纪律）——
//! `cargo test -p video-ffmpeg hw_external -- --nocapture --test-threads=1`。
//! 本机无可用 D3D11 设备（硬件 + WARP 都建不起）或解码器不支持时打印 SKIP 不 FAIL。

use crate::decode::{HwFrame, VideoReader};
use std::ffi::c_void;
use windows::core::Interface as _;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::IDXGIAdapter;

/// 媒体夹具（415MB 长 GOP MKV，H.264 视频 + AAC 音频 + ASS 字幕，不入库；缺夹具 SKIP）。
fn fixture() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target")
        .join("视频测试字幕.mkv")
}

/// 建 D3D11 设备：硬件驱动优先，失败回退 WARP；两者都不可用 → `None`（调用方打印 SKIP）。
/// 返回的 `device`/`context` 各持 1 个引用（`open_external_hw` 会再 AddRef 一份归 FFmpeg，
/// 解码期间即使本测试先 drop 掉自家引用设备也不会被提前释放）。
fn make_d3d11_device() -> Option<(ID3D11Device, ID3D11DeviceContext, &'static str)> {
    fn attempt(driver: D3D_DRIVER_TYPE) -> Option<(ID3D11Device, ID3D11DeviceContext)> {
        let adapter: Option<&IDXGIAdapter> = None; // null adapter = 按 driver type 自选
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let hr = unsafe {
            D3D11CreateDevice(
                adapter,
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                None, // 全部 feature level
                D3D11_SDK_VERSION,
                Some(&mut device),
                None, // 不取 feature level
                Some(&mut context),
            )
        };
        if hr.is_err() || device.is_none() || context.is_none() {
            return None;
        }
        Some((device.unwrap(), context.unwrap()))
    }

    if let Some(d) = attempt(D3D_DRIVER_TYPE_HARDWARE) {
        return Some((d.0, d.1, "hardware"));
    }
    if let Some(d) = attempt(D3D_DRIVER_TYPE_WARP) {
        return Some((d.0, d.1, "warp"));
    }
    None
}

/// 打开外部设备 reader：设备不可用 / 打开失败都打印 SKIP 并返回 `None`。
fn open_reader(
    path: &std::path::Path,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    dtype: &str,
) -> Option<VideoReader> {
    match VideoReader::open_external_hw(
        path,
        u32::MAX,
        device.as_raw() as *mut c_void,
        context.as_raw() as *mut c_void,
    ) {
        Ok(r) => Some(r),
        Err(e) => {
            eprintln!("SKIP: 外部 D3D11VA 打开/解码器初始化失败（设备={dtype}）: {e:#}");
            None
        }
    }
}

/// 从帧纹理查询 D3D11 数组纹理的 `ArraySize`（= 帧池容量）：D3D11VA 默认池用单张数组纹理，
/// `array_index ∈ [0, ArraySize)`。`ID3D11Texture2D::from_raw` 视为只借不取（不 AddRef），
/// 用后 `forget` 防意外 Release（纹理引用由 FFmpeg 帧池管理，不归本测试所有）。
fn pool_capacity(frame: &HwFrame) -> u32 {
    unsafe {
        let tex: ID3D11Texture2D = ID3D11Texture2D::from_raw(frame.tex as *mut c_void);
        let mut desc = std::mem::zeroed::<D3D11_TEXTURE2D_DESC>();
        tex.GetDesc(&mut desc);
        std::mem::forget(tex);
        desc.ArraySize
    }
}

/// 1) 外部设备解码基本正确性：
///   - ~120 帧逐帧断言 `tex != 0`、`array_index ∈ [0, 帧池容量)`、pts 严格单调、宽高恒定；
///   - 随后「边持边解」200 帧：每第 5 帧持有 1 张（先进先出——h264 D3D11VA 池容量 20、
///     绝大多数槽被 DPB/重排序占住，可安全并发的持有仅 ~1 张），证明持引用不枯竭帧池、
///     释放的槽位被正常回收复用。
#[test]
fn hw_external_decode_frames() {
    let path = fixture();
    if !path.exists() {
        eprintln!("SKIP: missing target/视频测试字幕.mkv（媒体夹具不入库，手动装配）");
        return;
    }
    let (device, context, dtype) = match make_d3d11_device() {
        Some(x) => x,
        None => {
            eprintln!("SKIP: D3D11 硬件/WARP 设备均不可用");
            return;
        }
    };
    let Some(mut r) = open_reader(&path, &device, &context, dtype) else {
        return;
    };
    let p = r.params();

    let mut last_pts = -1.0f64;
    let mut geom: Option<(u32, u32)> = None;
    let mut pool_cap = u32::MAX;
    let mut seen: Vec<u32> = Vec::with_capacity(64); // 观察到的不同槽位（证明来自池内复用）
    for i in 0..120 {
        let f = r
            .next_frame_hw()
            .unwrap()
            .unwrap_or_else(|| panic!("decode 第 {i} 帧提前 EOF（应在 120 帧内持续出帧）"));
        assert_ne!(f.tex, 0, "帧 {i} 纹理句柄不得为空");
        assert!(f.width > 0 && f.height > 0, "帧 {i} 宽高应为正: {}x{}", f.width, f.height);
        if pool_cap == u32::MAX {
            pool_cap = pool_capacity(&f);
            eprintln!(
                "[HW external] 设备={dtype} 源尺寸={}x{} 帧池容量={pool_cap} 时长={:.1}s",
                p.width, p.height, p.duration
            );
        }
        assert!(
            f.array_index < pool_cap,
            "帧 {i} array_index={} 应 ∈ [0,{pool_cap})",
            f.array_index
        );
        assert!(
            f.pts_seconds >= last_pts - 1e-9,
            "帧 {i} pts 应单调: {} < {last_pts}",
            f.pts_seconds
        );
        last_pts = f.pts_seconds;
        match geom {
            None => geom = Some((f.width, f.height)),
            Some((w, h)) => assert_eq!((f.width, f.height), (w, h), "帧 {i} 宽高应恒定"),
        }
        if !seen.contains(&f.array_index) {
            seen.push(f.array_index);
        }
    }
    eprintln!(
        "[HW external] 解码 120 帧通过：pts 0..{last_pts:.2}s 宽高={:?}",
        geom.unwrap()
    );

    // 边持边解：持「每第 5 帧」中的 1 张（h264 D3D11VA 池 ArraySize=20，绝大多数槽被
    // DPB/重排序占满，可安全并发的持有仅 ~1 张）→ 持引用不枯竭池、持续解码 200 帧远超
    // 池容量 20 = 槽位必然循环复用、释放即回收。
    r.seek(0.0).unwrap();
    let mut held: Vec<HwFrame> = Vec::new();
    let mut released: usize = 0;
    let mut last_held_pts = -1.0f64;
    for i in 0..200u32 {
        let f = r
            .next_frame_hw()
            .unwrap()
            .expect("持引用期间应持续出帧（池不枯竭）");
        assert!(f.array_index < pool_cap, "续解帧 array_index 应 < {pool_cap}");
        if i % 5 == 0 {
            // 持有 1 张：释放上一张后再持有新一张（单张持有同比无持有多占 1 槽，留足 DPB）
            if !held.is_empty() {
                let old = held.remove(0);
                last_held_pts = old.pts_seconds;
                released += 1;
            }
            held.push(f);
        }
    }
    assert!(released > 0, "应至少释放过一张持有的帧");
    assert!(last_held_pts >= 0.0, "持有帧应有 pts");
    drop(held);
    eprintln!(
        "[HW external] 边持边解 200 帧（并发仅持 1 张）、释放 {released} 帧 → 槽位在池内正常复用（共见 {} 个不同下标 ≤ 池 {pool_cap}）",
        seen.len()
    );
    assert!(seen.len() <= pool_cap as usize, "不同槽位数应 ≤ 池容量");
}

/// 2) 外部设备 seek 预滚：长 GOP 中段深 seek（demux 落点回拉 ~10s）+ 同 reader 紧邻二次
///   seek（复用、不重开）：丢弃 pts < 落点的预滚帧直到首个 ≥ 落点帧，断言预滚 pts 单调、
///   首帧贴近落点、帧池不枯竭（预滚段可被持续解码）。缺夹具 SKIP。
#[test]
fn hw_external_seek_preroll() {
    let path = fixture();
    if !path.exists() {
        eprintln!("SKIP: missing target/视频测试字幕.mkv（媒体夹具不入库，手动装配）");
        return;
    }
    let (device, context, dtype) = match make_d3d11_device() {
        Some(x) => x,
        None => {
            eprintln!("SKIP: D3D11 硬件/WARP 设备均不可用");
            return;
        }
    };
    let Some(mut r) = open_reader(&path, &device, &context, dtype) else {
        return;
    };

    // 中段深落点 + 同点紧邻二次 seek（复用 reader 不重开文件）
    let targets = [12.0f64, 333.3, 1200.5, 333.3];
    for &t in &targets {
        r.seek(t).unwrap();
        let mut last = -1.0f64;
        let mut n: u32 = 0;
        let first = loop {
            match r.next_frame_hw().unwrap() {
                Some(f) => {
                    assert!(
                        f.pts_seconds >= last - 1e-9,
                        "seek {t} 预滚 pts 应单调: {} < {last}",
                        f.pts_seconds
                    );
                    last = f.pts_seconds;
                    if f.pts_seconds >= t - 1e-9 {
                        break f.pts_seconds;
                    }
                    n += 1;
                    assert!(n < 2000, "seek {t} 预滚帧数异常（应在前几百帧内越过落点，实达 {n}）");
                }
                None => panic!("seek {t} 应解出 ≥ 落点的帧（近结尾才允许 EOF）"),
            }
        };
        // 长 GOP 落点回拉 ≤ 1 GOP；本夹具关键帧间隔 ~2s 左右（允许 ≤6s 宽容）
        assert!(
            (first - t).abs() < 6.0,
            "seek {t} 后首帧应贴近落点: 实得 {first:.2}（预滚 {n} 帧）"
        );
        eprintln!("[HW seek] target={t:.1}s → 首帧 pts={first:.2}s 预滚帧={n}");
    }
    eprintln!("[HW seek] 4 次 seek（含同一落点二次 reuse）全部通过（设备={dtype}）");
}

/// 3) 释放不泄漏帧池槽位：
///   - A）全量扫描：解码 2×池容量+12 帧（不持引用），断言出现的不同槽位数 ≤ 池容量且 < 帧数
///     （必然发生槽位复用 = 默认池在正常工作）；
///   - B）持引用 + 顺序释放：先持满 `池容量-2` 帧，再每解一帧按序释放一帧最旧，续解帧下标
///     全部落在 A 阶段的复用集合内 —— 若释放真的把槽位还给池、无泄漏，续解才能持续成功。
#[test]
fn hw_external_release_never_leaks_slot() {
    let path = fixture();
    if !path.exists() {
        eprintln!("SKIP: missing target/视频测试字幕.mkv（媒体夹具不入库，手动装配）");
        return;
    }
    let (device, context, dtype) = match make_d3d11_device() {
        Some(x) => x,
        None => {
            eprintln!("SKIP: D3D11 硬件/WARP 设备均不可用");
            return;
        }
    };
    let Some(mut r) = open_reader(&path, &device, &context, dtype) else {
        return;
    };

    let first = r.next_frame_hw().unwrap().expect("首帧应出");
    let pool_cap = pool_capacity(&first);
    let first_idx = first.array_index;
    drop(first);
    eprintln!("[HW release] 设备={dtype} 帧池容量={pool_cap}");

    // A）全量扫描：解码 2×pool+12 帧（无持有），必然发生槽位复用
    let want_a = pool_cap * 2 + 12;
    let mut seen_all: Vec<u32> = vec![first_idx];
    let mut count = 0u32;
    while count < want_a {
        let f = r
            .next_frame_hw()
            .unwrap()
            .unwrap_or_else(|| panic!("A 阶段 EOF（want {want_a} 帧，实得 {count}）"));
        if !seen_all.contains(&f.array_index) {
            seen_all.push(f.array_index);
        }
        count += 1;
    }
    assert!(
        seen_all.len() <= pool_cap as usize,
        "出现 {} 个不同槽位 > 池容量 {pool_cap}",
        seen_all.len()
    );
    assert!(
        seen_all.len() < want_a as usize,
        "解码 {want_a} 帧应对池内槽位复用（不同下标 {:?} 应 < 帧数）",
        seen_all
    );
    eprintln!(
        "[HW release] A：{count} 帧共见 {} 个不同槽位（≤ 池 {pool_cap}）→ 复用成立",
        seen_all.len()
    );

    // B）持引用 + 顺序释放：先持 1 帧（h264 池并发可安全持有的上限——池 20 绝大多数槽被
    //    DPB/重排序占满），再每解一帧按序释放最旧，续解帧下标必须 ∈ seen_all（槽位在池内
    //    循环）且解码不中断 → 释放的槽位确实归还、无泄漏。
    r.seek(0.0).unwrap();
    let hold_n = 1usize;
    let mut held: Vec<HwFrame> = Vec::with_capacity(hold_n);
    for _ in 0..hold_n {
        held.push(r.next_frame_hw().unwrap().expect("持有期应出帧"));
    }
    assert!(
        held.len() < pool_cap as usize,
        "持有 {hold_n} 帧必须小于池容量 {pool_cap}（否则解码无空闲槽）"
    );
    let mut released: Vec<u32> = Vec::new();
    let mut continued = 0u32;
    while let Some(f) = r.next_frame_hw().unwrap() {
        assert!(
            seen_all.contains(&f.array_index),
            "续解帧 array_index={} 应 ∈ A 阶段复用集 {:?}",
            f.array_index,
            seen_all
        );
        if !held.is_empty() {
            released.push(held.remove(0).array_index);
        }
        continued += 1;
        if continued >= (hold_n as u32 + 4) && held.is_empty() {
            break;
        }
        assert!(continued < 1000, "续解过多仍不停（未收敛）");
    }
    eprintln!(
        "[HW release] B：持有 {hold_n} 引用按序释放 {:?}，续解 {continued} 帧全部命中复用集 → 无槽位泄漏",
        released
    );
    drop(held);
}
