//! media-ffmpeg 静态链接桥（B2，opt-in）
//!
//! 默认（无 `static-link` feature）：零动作，完全走 B1 shared 链路
//! （ffmpeg-sys 的 FFMPEG_DIR 分支动态链接 avcodec/avformat/avutil/
//! swresample/swscale 五件 dll）。
//!
//! 当打开 `static-link`（且目标为 MSVC）时：
//!   - ffmpeg-sys 以 `static=` 把 FFMPEG_DIR/lib 下的静态 COFF 归档
//!     （avcodec.lib 等，由 scripts/build-slim-ffmpeg-static-msvc.sh
//!     --toolchain=msvc --enable-static 产出）直接链进本 cdylib；
//!   - ffmpeg-sys 在 Windows 上**无条件**发一批系统导入库（ffmpeg-sys-next
//!     9.0.0 build.rs 的 ole32/oleaut32/gdi32/user32/vfw32/strmiids/bcrypt/
//!     shlwapi/shell32，与 feature 分支无关），但 link.exe 对**符号未被引用**
//!     的导入库直接丢弃。本仓静态 FFmpeg 对象实际引用的系统库只有 3 个
//!     （B1 共享包导入表 + 静态预案实证）：
//!     bcrypt ← avutil（random_seed，直调 BCryptGenRandom）
//!     user32 ← avutil
//!     ole32  ← avcodec
//!   - 此处仍显式补这三个：防 ffmpeg-sys 上游裁剪/修改后静态链缺系统库
//!     （防御性冗余；与 ffmpeg-sys 重复发同一导入库无害）。
//!   - d3d11/dxgi/dxva2 **不需要链**：avutil hwcontext_d3d11va/dxva2
//!     走运行时 dlopen("d3d11.dll")+GetProcAddress 加载，静态链零依赖；
//!     网络相关（ws2_32/secur32）因 --disable-network 白名单无需。
//!
//! 本 build.rs 负责在 MSVC+static 时补这三个系统导入库，并顺带对
//! FFMPEG_DIR 指向做个常识校验（静态包 avcodec.lib 应 >512KB；若偏小
//! 怀疑是 B1 shared 的 128KB 导入库，发警告提醒改指静态包）。
use std::env;
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");

    // 仅 static-link feature 且 MSVC 目标下生效；其余一概零动作。
    if env::var("CARGO_FEATURE_STATIC_LINK").is_err() {
        return;
    }
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }

    // 静态 FFmpeg 归档依赖的系统库（顺序无强制要求，MSVC link 对未决
    // 引用会继续向后搜索）。
    for lib in ["bcrypt", "user32", "ole32"] {
        println!("cargo:rustc-link-lib={lib}");
    }

    if let Ok(dir) = env::var("FFMPEG_DIR") {
        let p = Path::new(&dir).join("lib").join("avcodec.lib");
        if let Ok(md) = std::fs::metadata(&p)
            && md.len() < 512 * 1024
        {
            println!(
                "cargo:warning=static-link 生效但 FFMPEG_DIR/lib/avcodec.lib 偏小（{}B），疑似 shared 导入库，请指到 --toolchain=msvc 静态包（E:\\Rust\\temp\\ffmpeg-slim-static）",
                md.len()
            );
        }
    }
}
