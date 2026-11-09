//! 页面显示层**纸墨重着色**：只改渲染位图像素（纸面 / 墨迹），内容像素按策略决定
//! 是否原样保留，**绝不写回 PDF 文件、不改 pdfium 解析**。
//!
//! 语义：
//! - 纸带（感知亮度 `L ≥ PAPER_L_LO`，近白纸面）：按亮度渐变成纸色 `bg`——两策略共有；
//! - 墨带（`L ≤ INK_L_HI`，近黑墨迹）：按亮度渐变成墨色 `ink`（深色档）——两策略共有；
//! - 中间带（`INK_L_HI < L < PAPER_L_LO`）：**策略分界**——
//!   - [`RecolorCurve::ContentProtect`]：原样保留（深/中调图形、图片内容），既有行为；
//!   - [`RecolorCurve::FullBand`]（默认）：整段亮度单调映射，纯白→纸色、纯黑→墨色、
//!     中间灰平滑插值，**无“原样保留”平台**——根治小字缩放下中灰杂散镶边（见下）。
//!
//! 【用户问题】夜间背景色下，小字默认缩放的文字边缘“细锯齿 / 杂乱 / 不干净”：双带曲线
//! 把中间带 (0.5, 0.9) 原样保留成中灰镶边浮在深纸上（与深纸、浅墨双色混杂）；小字笔画
//! 仅 1~1.5 设备像素时该镶边主宰观感。方案 = 「全带单调映射」（FullBand）+ A/B 开关
//! （`GPUI_PDF_RECOLOR_CURVE`，默认全带；真机无效即回 ContentProtect）。
//!
//! 执行位置：**worker 渲染线程内**对 bgra 缓冲就地改（每瓦片数 ms 级一次性开销），
//! 主线程 / 文档 / 文件零接触；参数为 `None` 时完全直通（零像素循环、零成本）。
//!
//! 提速：各策略权重按 256 级整数感知亮度预计算成 `const` LUT（编译期求值），
//! 每像素只做一次整数亮度索引 + 查表，去掉逐像素的 f32 除/round 与 clamp。
//! CPU LUT 与 GPU 片元 `recolor_sprite`（`vendor/zed/.../shaders.hlsl`）**同一策略**
//! （`RecolorSprite.switches.y` 携带曲线、shader 分支）——灰度像素逐位一致；彩色像素的
//! CPU↔GPU 通道差随策略分档：**FullBand 整段单调（斜率≈1）≤1 LSB**；**ContentProtect
//! 纸带（斜率 10）彩色近白理论最大 ~10 LSB**（同策不同量化，视觉不可辨，见
//! [`apply_recolor_with_curve`]）。

use std::sync::{
    LazyLock,
    atomic::{AtomicBool, AtomicU8, Ordering},
};

/// GPU 重着色门控（**生产默认开**）：环境变量 `GPUI_PDF_GPU_RECOLOR` **缺省 = GPU 开**；
/// 显式设 `"0"` / 空串 = 强制回 CPU（既有 dbd3236 逐字节一致路径）。开启后 PDF 显示路径
/// 的 raw→着色改在 **GPU 绘制时**由片元着色器完成（`img().recolor(...)` → `RecolorSprite`
/// 片元），worker **不再对显示路径做 CPU 逐像素 apply_recolor**——CPU 税归零、切档天然
/// 0 重算 0 重栅（只改图片元素参数）。
///
/// `#[cfg(test)]` 下默认**关**（避免测试依赖 GPU 像素输出）：测试显式设 env 或经
/// [`set_gpu_recolor_gate_for_test`] 才走 GPU。headless 只证「机制 + CPU 税消失 + 门控」，
/// GPU 像素输出由用户真机复验（K23②/③）。
pub(crate) static GPU_RECOLOR_GATE: LazyLock<AtomicBool> = LazyLock::new(|| {
    AtomicBool::new(gpu_gate_env_default())
});

/// 门控 env 解析（纯函数，可测）：`Some(true)` = 显式开、`Some(false)` = 显式关（`"0"` /
/// 空串）、`None` = 未设（交回调用方决定默认值）。生产默认开、测试默认关见
/// [`.gpu_gate_env_default`]。
fn gpu_gate_env(v: Option<&std::ffi::OsStr>) -> Option<bool> {
    match v {
        None => None,
        Some(s) if s.is_empty() || s == "0" => Some(false),
        Some(_) => Some(true),
    }
}

/// 进程启动时按构建形态取门控默认：生产 = 开（`GPUI_PDF_GPU_RECOLOR=0` 可强制回 CPU）；
/// 测试 = 关（仅 env 显式非空非 `"0"` 才开，与旧语义一致）。
fn gpu_gate_env_default() -> bool {
    #[cfg(test)]
    {
        // 测试默认关：显式设成非空且非 "0" 才开。
        gpu_gate_env(std::env::var_os("GPUI_PDF_GPU_RECOLOR").as_deref())
            .or(Some(false))
            .unwrap()
    }
    #[cfg(not(test))]
    {
        // 生产默认开：缺省=开；显式 "0"/空串 = 强制回 CPU。
        gpu_gate_env(std::env::var_os("GPUI_PDF_GPU_RECOLOR").as_deref())
            .unwrap_or(true)
    }
}

/// 当前 GPU 重着色门控（进程级：单 worker 单进程，与 worker 单例对齐；生产读一次后
/// 进程内恒定，不随 env 变化动态翻转）。
pub fn gpu_recolor_gate() -> bool {
    let on = GPU_RECOLOR_GATE.load(Ordering::Relaxed);
    if on {
        // 真机诊断 1/4：门控激活（进程级首次读为真时打一次；门控 off 完全静默）。
        diag_once(&DIAG_GATE_ON, || {
            // 门控为真时 env 只可能是「显式非 0 非空」或「缺省」。
            let how = match gpu_gate_env(std::env::var_os("GPUI_PDF_GPU_RECOLOR").as_deref()) {
                Some(_) => "GPUI_PDF_GPU_RECOLOR=on",
                None => "default on",
            };
            eprintln!("[pdf] gpu recolor enabled ({how})");
        });
    }
    on
}

/// 曲线策略：纸墨重着色如何把亮度映射到纸/墨双色。两策略都为纯函数、可在 CPU LUT 与
/// GPU shader 同策执行（`RecolorSprite.switches.y` 携带、shader 分支）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecolorCurve {
    /// 内容保护（**双带**，既有行为）：纸带 `L≥0.9→bg`、墨带 `L≤0.5→ink`、
    /// 中间带 (0.5, 0.9) 原样保留。图片/图形中调不触碰，但小字 AA 过渡像素会
    /// 落在被保留的中灰带上 → 深纸上中灰杂散镶边（用户报告的“细锯齿/脏”）。
    ContentProtect,
    /// 全带单调（**默认**）：整段亮度单调映射 `out = bg + (ink - bg) × (1 - L)`——
    /// 纯白→纸色、纯黑→墨色、中间灰平滑插值，**无“原样保留”平台**（消灭中灰杂带）。
    /// `ink=None` 时降级为仅纸带映射、内容不动（与 ContentProtect 一致：没有墨色目标
    /// 就没有“全带重映射”可言，内容/墨迹需保持自身颜色以免不可读）。
    FullBand,
}

// 曲线 latch 编码（0 = ContentProtect、1 = FullBand），与 `#[repr]` 无关、纯存储。
const CURVE_CONTENT_PROTECT: u8 = 0;
const CURVE_FULLBAND: u8 = 1;

/// 曲线 A/B 开关（**进程级 latch，默认全带**）：`GPUI_PDF_RECOLOR_CURVE` 精确为 `"0"`
/// → ContentProtect；其余（缺省/空/`"1"` 等）→ FullBand。与 `GPU_RECOLOR_GATE` 同姿势：
/// 进程启动时一次读定，运行期恒定（改 env 需重启进程）。
pub(crate) static RECOLOR_CURVE_LATCH: LazyLock<AtomicU8> = LazyLock::new(|| {
    let cp = std::env::var_os("GPUI_PDF_RECOLOR_CURVE").is_some_and(|v| v == "0");
    AtomicU8::new(if cp { CURVE_CONTENT_PROTECT } else { CURVE_FULLBAND })
});

/// 当前曲线策略（进程级）。默认 [`RecolorCurve::FullBand`]；非默认曲线首次读到打一次
/// stderr 诊断（A/B 切换可追溯；FullBand 默认路径本诊断零输出——注意「进程零输出」不再
/// 成立：GPU 门控生产默认开，`gpu_recolor_gate` 每进程首读打一行 `[pdf] gpu recolor
/// enabled`，N1 口径 = **每进程至多一行 stderr**，一次性非刷屏）。
pub fn recolor_curve() -> RecolorCurve {
    let c = match RECOLOR_CURVE_LATCH.load(Ordering::Relaxed) {
        CURVE_CONTENT_PROTECT => RecolorCurve::ContentProtect,
        _ => RecolorCurve::FullBand,
    };
    if c == RecolorCurve::ContentProtect {
        // A/B 诊断：ContentProtect 为非默认（回退档），打一次便于真机对表。
        diag_once(&DIAG_CURVE_CONTENT_PROTECT, || {
            eprintln!("[pdf] recolor curve=contentprotect (GPUI_PDF_RECOLOR_CURVE=0)");
        });
    }
    c
}

/// 测试覆盖：显式钉死曲线（须持 pdfium-test 全局串行锁时调用；测试结束必须还原）。
/// 跨 crate 可见性：主 crate `pdf`（视图层）的测试模块同用此钩子，故取消 `#[cfg(test)]`
///（latch 本身恒编译，非测试构建下此钩子惰性、不会被生产路径调用）。
#[doc(hidden)]
pub fn set_recolor_curve_for_test(c: RecolorCurve) {
    let v = match c {
        RecolorCurve::ContentProtect => CURVE_CONTENT_PROTECT,
        RecolorCurve::FullBand => CURVE_FULLBAND,
    };
    RECOLOR_CURVE_LATCH.store(v, Ordering::Relaxed);
}

/// T4 GPU 重着色**真机诊断**基础设施：门控开启时每类只打印一次到 stderr（进程级 latch，
/// 避免逐瓦片/逐帧刷屏）；每个诊断**每进程至多一行**（N1：GPU 门控生产默认开 → 每进程
/// 启动/首次开 PDF 各至多打印一行 `[gpui] recolor sprite pipeline ok`（vendor）与
/// `[pdf] gpu recolor enabled`，非「默认零输出」）。选 stderr 便于 `2>&1 | grep` 过滤。
/// 每条诊断都是「该环节确实发生」的证据，覆盖全链路 4 点：
///   1) 门控激活（本模块）；2) worker 原图直传封面/瓦片（parser 调用）；3) view 首个
///   img().recolor 挂参（render 调用）；4) gpui 管线就绪（vendor/directx_renderer）。
/// headless 测试里打到 stderr 属正常，不改变任何断言语义。
pub(crate) fn diag_once(fired: &AtomicBool, msg: impl FnOnce()) {
    if !fired.swap(true, Ordering::Relaxed) {
        msg();
    }
}

pub(crate) static DIAG_GATE_ON: AtomicBool = AtomicBool::new(false);
pub(crate) static DIAG_RAW_COVER: AtomicBool = AtomicBool::new(false);
pub(crate) static DIAG_RAW_TILE: AtomicBool = AtomicBool::new(false);
pub(crate) static DIAG_VIEW_RECOLOR: AtomicBool = AtomicBool::new(false);
/// A/B 曲线诊断（非默认 ContentProtect 首次读到打一次；FullBand 默认**本诊断**零输出——
/// 但不等于进程零输出，GPU 门控另有每进程一行，见 [`gpu_recolor_gate`] / N1 口径）。
pub(crate) static DIAG_CURVE_CONTENT_PROTECT: AtomicBool = AtomicBool::new(false);

/// 真机诊断 2/4：worker 跳过 CPU apply_recolor 直传原图——首个封面/首个瓦片各打一次。
/// `finalize_render`（整页/瓦片渲染）与 `recolored_entries`（切档刷新）共用同一组 latch：
/// 进程内每类只报一次，最早触发者先手。
pub(crate) fn diag_raw_passthrough(kind: crate::parser::RawKind) {
    match kind {
        crate::parser::RawKind::Cover => diag_once(&DIAG_RAW_COVER, || {
            eprintln!("[pdf] gpu: raw passthrough cover");
        }),
        crate::parser::RawKind::Tile => diag_once(&DIAG_RAW_TILE, || {
            eprintln!("[pdf] gpu: raw passthrough tile");
        }),
        crate::parser::RawKind::Mid | crate::parser::RawKind::Thumb => {}
    }
}

/// 真机诊断 3/4：view 首个 `img().recolor(Some)` 挂参（打印实际纸/墨参数，便于与 CPU
/// 路径对表/核对夜间档是否走对参数）。
pub fn diag_view_recolor(bg: [u8; 3], ink: Option<[u8; 3]>) {
    diag_once(&DIAG_VIEW_RECOLOR, || {
        let ink_s = ink
            .map(|[r, g, b]| format!("0x{r:02X}{g:02X}{b:02X}"))
            .unwrap_or_else(|| "none".to_string());
        eprintln!(
            "[pdf] gpu: img.recolor attached bg=0x{:02X}{:02X}{:02X} ink={}",
            bg[0], bg[1], bg[2], ink_s
        );
    });
}

/// 测试覆盖：仅可在持 pdfium-test 全局串行锁（`test_util::lock`）时调用——所有触碰
/// worker/view 的 pdf 测试都持同一把锁，覆盖后进程内瞬时一致；测试结束必须还原。
/// 跨 crate 可见性：同 [`set_recolor_curve_for_test`]（主 crate 测试模块同用，恒编译）。
#[doc(hidden)]
pub fn set_gpu_recolor_gate_for_test(v: bool) {
    GPU_RECOLOR_GATE.store(v, Ordering::Relaxed);
}

/// **R8 单通道（luma）位图门控**：GPU 重着色 + 曲线 [`RecolorCurve::FullBand`] + 墨色启用
/// （`ink=Some`）时，瓦片/封面/中清/缩略显示位图只存**感知亮度**（1 字节/px，数学无损
/// -4×）——GPU shader 的 FullBand 分支 `out = bg + (ink-bg)·(1−L)` 只依赖亮度、不读原色，
/// 位图存 luma 再读回数学无损（≤1 LSB 量化差）。其余档（`ink=None` / ContentProtect
/// 需要原色 / light 浅色）必须保留原色 → 维持 BGRA。
/// 本判定在 **worker 编码侧**（`finalize_render`）与 **view 侧**（位图包装 / 字节账本 /
/// 切档重栅）同源调用——两侧取同一 `(params, gate, curve)`，保证编码与消费一致。
pub fn luma_gate_active(recolor: Option<RecolorParams>, gpu: bool) -> bool {
    gpu && recolor_curve() == RecolorCurve::FullBand && recolor.is_some_and(|p| p.ink.is_some())
}

/// **瓦片字节/像素**：R8 门控激活 = 1BPP（单通道亮度，-4×）；否则 4BPP（BGRA）。动态预算
/// （`nominal_per_tile`）与字节账本（`TileSlot::bytes`）按此收敛——避免「字节已除 4 而预算
/// 仍按 4BPP 估」导致预算语义错 4×。与 [`.luma_gate_active`] 同源（同一判定）。
pub fn tile_bytes_per_pixel(recolor: Option<RecolorParams>, gpu: bool) -> u64 {
    if luma_gate_active(recolor, gpu) {
        1
    } else {
        4
    }
}

/// BGRA 紧凑缓冲 → **R8 单通道感知亮度**（`w*h*1` 字节）。公式与 GPU shader
/// `recolor_sprite_fragment` **逐位同策**：`luma = dot(rgb/255, (0.299,0.587,0.114))`（f32），
/// 存 `u8 = round(0.299R + 0.587G + 0.114B)`（R 在 BGRA 字节 2）——shader 读回
/// `L/255`，与 BGRA 路径连续亮度差 ≤1/255，FullBand 输出差 ≤1 LSB（接受口径）。
/// 纯函数（不依赖任何状态），可单测。
pub fn encode_bgra_to_luma(bgra: &[u8]) -> Vec<u8> {
    let n = bgra.len() / 4;
    let mut out = Vec::with_capacity(n);
    let mut i = 0usize;
    for _ in 0..n {
        // BGRA：i=B, i+1=G, i+2=R。f32 与 shader 浮点公式同源（逐位一致目标）。
        let luma = 0.299f32 * (bgra[i + 2] as f32)
            + 0.587f32 * (bgra[i + 1] as f32)
            + 0.114f32 * (bgra[i] as f32);
        out.push(luma.round() as u8);
        i += 4;
    }
    out
}

/// 把本 crate 的重着色参数映射为 gpui 图片元素级参数（T4 GPU 通道）：字段同构
/// （`bg` = 纸色、`ink` = 墨色可选），并携带进程级曲线策略（[`recolor_curve`]），
/// 由 view 在门控开启时挂到 `img().recolor(...)` → `RecolorSprite.switches.y` → shader 分支。
pub fn to_image_recolor(p: RecolorParams) -> gpui::ImageRecolor {
    gpui::ImageRecolor {
        bg: p.bg,
        ink: p.ink,
        curve: match recolor_curve() {
            RecolorCurve::ContentProtect => 0,
            RecolorCurve::FullBand => 1,
        },
    }
}

/// 纸重着色参数：`bg` = 纸色（近白纸面像素映射至此）；`ink` = 墨色（`Some` 时把
/// 近黑内容/文字像素映射至此，深色档用）。RGB 通道（0..=255）；alpha 一贯保持 255。
/// `ink=None` → 内容像素完全直通（任何策略都只染纸面）。曲线策略是**进程级 latch**
/// （[`recolor_curve`] / `GPUI_PDF_RECOLOR_CURVE`），不随参数逐次携带——CPU 与 GPU 路径
/// 同读同一 latch 保持一致。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecolorParams {
    /// 纸色（R/G/B，0..=255）。
    pub bg: [u8; 3],
    /// 墨色（R/G/B，0..=255）；`Some` 时把近黑墨迹像素提亮至此色。
    pub ink: Option<[u8; 3]>,
}

/// 纸面亮度下限（感知亮度 / 255）：`L ≥` 该值视为「纸面」，按亮度渐变成纸色；
/// `L <` 该值视为「内容」，原样不动。0.90 为**内容保护优先**阈值：纯白(1.0)→纸色、
/// 纸边抗锯齿浅灰→微染、大多数内容（文字 / 深/中调图形 / 图片）→ 不变。
pub const PAPER_L_LO: f32 = 0.90;

/// 墨迹亮度上限（感知亮度 / 255）：`L ≤` 该值视为「墨迹」，按亮度渐变成 `ink` 色；
/// `L >` 该值视为中间调内容（图形 / 图片阴影）原样不动。
///
/// 【决策留痕：墨带 0.25 → 0.5】旧阈值 0.25 下，小字号抗锯齿过渡像素（L∈0.25..0.90）
/// 被原样留在 64~230 亮灰、浮在深纸底 #202020 上 → 局部亮斑 / 锯齿 zigzag（小字发虚、
/// 斑驳）。加宽至 0.5 后，大部分 AA 过渡像素纳入平滑提亮带，笔画整体统一浅色，
/// 仅近白纸面侧（L≥0.9）保留渐染；代价与纸面侧对称的已知局限：L∈0.25..0.5 的近黑
/// 影区（内容里的深色图形/图片暗部）也会被轻度提亮（用户已接受此微染）。
///
/// 【P1 升级开关 → 已由 FullBand 取代】旧思路「把 INK_L_HI 升到 ~0.9 追上纸带」只会在
/// 中间带留下亮度回峰（墨带把 L≈0.9 提亮、纸带又把更高亮压暗 → 非单调），并非去镶边
/// 的正解；正确方案是 [`RecolorCurve::FullBand`] 整段单调映射。本常量现仅服务
/// ContentProtect（双带）曲线，不再承担「全带开关」职责。
pub const INK_L_HI: f32 = 0.5;

/// 纸带权重 LUT：`idx` = 整数感知亮度（= 灰阶级，0..=255），`L = idx/255` 处的
/// 纸色权重 `a = (L - PAPER_L_LO) / (1 - PAPER_L_LO)` 截断到 [0,1]（`L < PAPER_L_LO`
/// 恒 0 = 内容不动）。编译期预计算成常量表，运行时零成本。
const PAPER_WEIGHT_LUT: [f32; 256] = {
    let mut t = [0.0f32; 256];
    let mut i = 0usize;
    while i < 256 {
        let lum = i as f32 / 255.0;
        let w = (lum - PAPER_L_LO) / (1.0 - PAPER_L_LO);
        t[i] = if w < 0.0 { 0.0 } else if w > 1.0 { 1.0 } else { w };
        i += 1;
    }
    t
};

/// 墨带权重 LUT：指标同为整数感知亮度；`u = L / INK_L_HI`，
/// 墨色权重 `a = 1 - u` 线性 1→0（`u ≥ 1` 恒 0 = 中间调内容不动）。
/// 一阶连续：w(0)=1、w(INK_L_HI)=0，在带上界与「原样保留」无缝衔接——带上界处
/// 输出收敛回原始值，不产生亮度阶跃。曲线选线性而非二次/余弦等平滑尾：后者会在
/// 带尾产生 ~20 级的深谷再回抬（局部亮度反转 U），违背「无斑驳」目标；线性带内
/// 近似严格单调、仅余舍入级 ±1 波动。
const INK_WEIGHT_LUT: [f32; 256] = {
    let mut t = [0.0f32; 256];
    let mut i = 0usize;
    while i < 256 {
        let lum = i as f32 / 255.0;
        let u = lum / INK_L_HI;
        t[i] = if u >= 1.0 { 0.0 } else { 1.0 - u };
        i += 1;
    }
    t
};

/// 全带权重 LUT（[`RecolorCurve::FullBand`]，默认策略）：`a[i] = 1 - i/255 = 1 - L`（线性），
/// 输出 `out = bg + (ink - bg) × a[i]`——纯黑(i=0)→1→墨色、纯白(i=255)→0→纸色、
/// 中间灰平滑插值。**严格单调**（每级降 (ink-bg)/255，整数舍入后不增不减），无“原样
/// 保留”平台——根治小字 AA 过渡像素的中灰杂带。与 GPU shader 同公式同策略
/// （`recolor_sprite_fragment` 的 FullBand 分支：`1 - saturate(luma)`）。
const FULLBAND_WEIGHT_LUT: [f32; 256] = {
    let mut t = [0.0f32; 256];
    let mut i = 0usize;
    while i < 256 {
        t[i] = 1.0 - i as f32 / 255.0;
        i += 1;
    }
    t
};

/// 并行阈值（像素数）：低于此值走单线程路径，避免线程创建开销。
/// 依据：本机 debug 实测单线程重着色吞吐 ~2-11ns/px（266k px 瓦片 0.5-3ms）；
/// Windows 下 `std::thread::scope` 每额外线程创建 + join 约 50-150µs（debug 更高）。
/// 阈值处（0.5Mpx）单线程约 1-6ms，若并行会被 spawn/join 数次开销（~0.2-0.6ms）
/// 吃掉大半毛收益；高于此（页/封面级）并行毛收益显著、开销占比 <10%。
/// 取 2^19 = 524288 px 作为「瓦片 vs 页/封面」的分界。
const PARALLEL_MIN_PX: usize = 1 << 19;

/// 并行度上限：本算法对同一缓冲属内存带宽型（扫描 + 就地写），>4 线程收益递减，
/// 且每多一线程多一次 spawn/join 固定开销。
const PARALLEL_MAX_THREADS: usize = 4;

/// 纸墨重着色（进程级默认曲线入口，就地改 `bgra` 像素）：曲线 = [`recolor_curve`]
/// （进程 latch，默认 [`RecolorCurve::FullBand`]）。具体映射语义见
/// [`apply_recolor_with_curve`]。**worker 路径调用本函数**（与 view/GPU 同读同一 latch）。
pub fn apply_recolor(bgra: &mut [u8], params: RecolorParams) {
    apply_recolor_with_curve(bgra, params, recolor_curve());
}

/// 纸墨重着色（纯函数，就地改 `bgra` 像素）：每像素感知亮度取**整数近似**
/// `li = (299R + 587G + 114B) / 1000 ∈ [0,255]` 作 LUT 索引（纯灰像素 `li` 恰等于
/// 灰度级本身，与浮点公式逐位一致；彩色像素以亮度灰级近似）。
/// 曲线由入参显式给定（CPU/GPU 同策的纯函数内核；GPU shader `recolor_sprite_fragment`
/// 用连续 `luma` 走同一策略）。**CPU↔GPU 彩色通道差分档**（W3）：灰度像素逐位一致；
/// FullBand 整段单调（每级 LUT 降 (ink-bg)/255、斜率≈1）→ 彩色 ≤1 LSB；ContentProtect
/// 纸带 `paper_w` 斜率 = 1/(1-0.90) = 10 → 彩色近白理论最大 ~10 LSB（同策不同量化，
/// 视觉不可辨）：
///
/// - [`RecolorCurve::ContentProtect`]（双带，既有语义）：纸带
///   `out = orig + (bg - orig) × PAPER_WEIGHT_LUT[li]`（近白纸面趋向纸色、低于
///   [`PAPER_L_LO`] 不动）；设了 `ink` 时近黑墨带再叠
///   `out = out + (ink - out) × INK_WEIGHT_LUT[li]`（≥[`INK_L_HI`] 的中间调不动）。
///   两带作用域（≤0.5 与 ≥0.9）不相交、顺序无关。
/// - [`RecolorCurve::FullBand`]（全带单调，默认）且 `ink=Some`：
///   `out = bg + (ink - bg) × FULLBAND_WEIGHT_LUT[li]`——整段亮度单调映射，纯黑→墨色、
///   纯白→纸色、中间灰平滑插值，**无“原样保留”平台**（根治小字 AA 中灰杂带）。
/// - [`RecolorCurve::FullBand`] 且 `ink=None`：降级为仅纸带映射、内容不动
///   （与 ContentProtect 纸带相同——没有墨色目标就无从“全带重映射”，避免内容不可读）。
///
/// A 恒 255。
///
/// 大缓冲（≥[`PARALLEL_MIN_PX`] 像素，整页/封面级）：用 `std::thread::scope` 按
/// [`PARALLEL_MAX_THREADS`]（实际取机器可用并行度封顶）把像素范围切成均匀连续块，
/// 每线程独立扫描自己那段（互不重叠、无共享可变量；同步等待全部块完成才返回）。
/// 输出与单线程逐字节一致（等值回归见 tests）。小缓冲（瓦片级）走单线程路径，
/// 零线程开销。
///
/// 输入为 BGRA 紧凑缓冲（`bgra[i]=B, [i+1]=G, [i+2]=R, [i+3]=A`）；`len` 按 4 对齐取整，
/// 尾部不足 4 字节忽略。空 / 无参数直通由调用方保证（`None` 不调用本函数）。
///
/// 已知局限：像素级无法区分「纸面」与「内容里的近白高光」（白底截图/高光区），
/// 这类近白像素同样会被当成纸面微染；对称地，`ink` 档无法区分「墨迹」与「内容里
/// 的近黑影区」，这类近黑像素同样会被微染向墨色。ContentProtect 对深/中调内容
/// （L∈0.5..0.9）绝不触碰；FullBand 则整段重映射（图片/图形被沿纸↔墨轴“淡化”——
/// 这正是 A/B 取舍：要无镶边、可接受内容淡化）。`ink=None` 时两策略均只触纸面。
pub fn apply_recolor_with_curve(bgra: &mut [u8], params: RecolorParams, curve: RecolorCurve) {
    let [bg_r, bg_g, bg_b] = params.bg.map(f32::from);
    let ink = params.ink.map(|[r, g, b]| (f32::from(r), f32::from(g), f32::from(b)));
    let n = bgra.len() / 4;
    // 小缓冲（瓦片级）：单线程扫描，输出与既有版本逐字节一致（golden）。
    if n < PARALLEL_MIN_PX {
        shade_pixels(bgra, bg_r, bg_g, bg_b, ink, curve);
        return;
    }
    // 取机器可用并行度并封顶；1 核时连 scope 都省，直接单线程。
    let workers = std::thread::available_parallelism()
        .map(|p| p.get().min(PARALLEL_MAX_THREADS))
        .unwrap_or(1)
        .max(1);
    if workers <= 1 {
        shade_pixels(bgra, bg_r, bg_g, bg_b, ink, curve);
        return;
    }
    // 均匀连续分块：每块 base 像素，余数分给前 rem 块各 +1，覆盖 [0,n) 不重叠、不留缝。
    let base = n / workers;
    let rem = n % workers;
    std::thread::scope(|s| {
        let mut start = 0usize;
        // 逐块按**相对字节差** split，取得像素对齐（4 字节）的独立切片交各线程独占；
        // `head` 即该块整体，shade_pixels 从 0 扫到末尾，不需要绝对像素下标。
        // scope 保证全部 join 后才返回。
        let mut bgra = bgra;
        for w in 0..workers {
            let end = start + base + usize::from(w < rem);
            let (head, tail) = bgra.split_at_mut((end - start) * 4);
            bgra = tail;
            s.spawn(move || shade_pixels(head, bg_r, bg_g, bg_b, ink, curve));
            start = end;
        }
    });
}

/// 对 `bgra` 整段（从下标 0 扫描到 `len/4` 个像素）就地执行某策略的 LUT 映射。并行各块
/// 切片与单线程路径共用本函数；块与块互不重叠故可安全并跑。`#[inline]` 便于充分内联；
/// 按策略一次性选中内核循环（避免每像素分支）；`curve == ContentProtect` 或
/// `FullBand + ink=None` 走 `shade_dual_band`（纸带/内容保护式），`FullBand + ink=Some`
/// 走 `shade_fullband`（全带单调）。
#[inline]
fn shade_pixels(
    bgra: &mut [u8],
    bg_r: f32,
    bg_g: f32,
    bg_b: f32,
    ink: Option<(f32, f32, f32)>,
    curve: RecolorCurve,
) {
    match curve {
        RecolorCurve::FullBand if ink.is_some() => {
            let (ink_r, ink_g, ink_b) = ink.unwrap();
            shade_fullband(bgra, bg_r, bg_g, bg_b, ink_r, ink_g, ink_b);
        }
        _ => shade_dual_band(bgra, bg_r, bg_g, bg_b, ink),
    }
}

/// FullBand 内核：整段亮度单调映射 `out = bg + (ink - bg) × FULLBAND_WEIGHT_LUT[li]`。
#[inline]
fn shade_fullband(
    bgra: &mut [u8],
    bg_r: f32,
    bg_g: f32,
    bg_b: f32,
    ink_r: f32,
    ink_g: f32,
    ink_b: f32,
) {
    let n = bgra.len() / 4;
    let mut i = 0usize;
    for _ in 0..n {
        // FullBand 输出只依赖整数亮度（不读原色）——整段亮度单调重映射到纸↔墨轴。
        let li = ((299 * (bgra[i + 2] as u32) + 587 * (bgra[i + 1] as u32) + 114 * (bgra[i] as u32))
            / 1000) as usize;
        let w = FULLBAND_WEIGHT_LUT[li];
        let b = bg_b + (ink_b - bg_b) * w;
        let g = bg_g + (ink_g - bg_g) * w;
        let r = bg_r + (ink_r - bg_r) * w;
        bgra[i] = b.round() as u8;
        bgra[i + 1] = g.round() as u8;
        bgra[i + 2] = r.round() as u8;
        bgra[i + 3] = 255;
        i += 4;
    }
}

/// 双带内核（ContentProtect 与 `ink=None` 的纸带）：纸带 `out = orig + (bg-orig)×pw`；
/// 设了 `ink` 时墨带再叠 `out = out + (ink-out)×iw`——与既有 dbd3236 逐字节一致。
#[inline]
fn shade_dual_band(
    bgra: &mut [u8],
    bg_r: f32,
    bg_g: f32,
    bg_b: f32,
    ink: Option<(f32, f32, f32)>,
) {
    let n = bgra.len() / 4;
    let mut i = 0usize;
    for _ in 0..n {
        let b0 = f32::from(bgra[i]);
        let g0 = f32::from(bgra[i + 1]);
        let r0 = f32::from(bgra[i + 2]);
        // 整数感知亮度 li = (299R+587G+114B)/1000 ∈ [0,255]，仅作 LUT 索引。
        let li = ((299 * (bgra[i + 2] as u32) + 587 * (bgra[i + 1] as u32) + 114 * (bgra[i] as u32))
            / 1000) as usize;
        let mut b = b0;
        let mut g = g0;
        let mut r = r0;
        let pw = PAPER_WEIGHT_LUT[li];
        if pw > 0.0 {
            b += (bg_b - b0) * pw;
            g += (bg_g - g0) * pw;
            r += (bg_r - r0) * pw;
        }
        if let Some((ink_r, ink_g, ink_b)) = ink {
            let iw = INK_WEIGHT_LUT[li];
            if iw > 0.0 {
                b += (ink_b - b) * iw;
                g += (ink_g - g) * iw;
                r += (ink_r - r) * iw;
            }
        }
        bgra[i] = b.round() as u8;
        bgra[i + 1] = g.round() as u8;
        bgra[i + 2] = r.round() as u8;
        bgra[i + 3] = 255;
        i += 4;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 纯函数断言：纯白 → 纸色；内容（黑 / 中灰）原样不动；近白按亮度渐染；A 恒 255。
    /// 参数 bp：bg=[10,20,30]（RGB）。（`ink=None`：内容像素直通 = 既有「只改纸面」。）
    #[test]
    fn apply_recolor_paper_only_and_alpha() {
        let params = RecolorParams { bg: [10, 20, 30], ink: None };
        // 白 / 黑 / 50% 灰 / 近白 250 / 内容灰 100（A=0 强制 255）。
        let mut buf: Vec<u8> = vec![
            255, 255, 255, 255, // 白：B,G,R,A
            0, 0, 0, 255, // 黑
            128, 128, 128, 255, // 50% 灰（内容，应不动）
            250, 250, 250, 255, // 近白 250（纸面，应渐染）
            100, 100, 100, 0, // 内容灰，A=0
        ];
        apply_recolor(&mut buf, params);

        // 白 L=1.0 → a=1 → 全纸色（RGB[10,20,30] → 缓冲 B=30,G=20,R=10）。
        assert_eq!(&buf[0..4], &[30, 20, 10, 255], "纯白纸面应映射为纸色");
        // 黑 L=0 → a=0 → 原样（内容不变）。
        assert_eq!(&buf[4..8], &[0, 0, 0, 255], "纯黑内容应原样不动");
        // 50% 灰 L≈0.50 < 0.90 → a=0 → 原样（内容不变）。
        assert_eq!(&buf[8..12], &[128, 128, 128, 255], "中灰内容应原样不动");
        // 近白 250：L=250/255≈0.9804，a=(0.9804-0.90)/0.10≈0.8039
        //   R:250+(10-250)*0.8039≈57.1→57 G:250+(20-250)*0.8039≈65.1→65
        //   B:250+(30-250)*0.8039≈73.1→73 → 缓冲 [73,65,57,255]。
        assert_eq!(&buf[12..16], &[73, 65, 57, 255], "近白纸面按亮度渐染为纸色");
        // 内容灰 100（L≈0.39，a=0）→ 原样；输入 A=0 被强制 255。
        assert_eq!(&buf[16..20], &[100, 100, 100, 255], "内容灰原样不动且 A 恒 255");
    }

    /// 亮度上界另一头：L 恰在阈值之下（0.89 < 0.90）→ a=0 完全不动。
    #[test]
    fn apply_recolor_below_threshold_is_unchanged() {
        let params = RecolorParams { bg: [0x20, 0x20, 0x20], ink: None };
        // 0.89*255 ≈ 226.95 → 227；L=227/255≈0.8902 略高于 0.90？——取 225（L≈0.882）
        // 与 240（L≈0.941，a≈0.41，应部分染向纸色）。
        let mut buf = vec![225, 225, 225, 255, 240, 240, 240, 255];
        apply_recolor(&mut buf, params);
        // 225：L≈0.8824 < 0.90 → a=0 → 原样。
        assert_eq!(&buf[0..4], &[225, 225, 225, 255], "阈值下（0.882）内容不动");
        // 240：L≈0.9412，a=(0.9412-0.90)/0.10≈0.4118
        //   R:240+(0x20-240)*0.4118 = 240-85.65=154.35→154（B/G 同）。
        assert_eq!(&buf[4..8], &[154, 154, 154, 255], "阈值上近白渐染到纸色方向");
    }

    /// 空缓冲（len==0）不 panic；len 非 4 对齐时尾部不足像素忽略、不越界。
    #[test]
    fn apply_recolor_empty_and_unaligned() {
        let params = RecolorParams { bg: [10, 20, 30], ink: None };
        let mut empty: Vec<u8> = Vec::new();
        apply_recolor(&mut empty, params);
        assert!(empty.is_empty());
        // 5 字节（= 1 整像素 + 1 冗余字节）：只处理前 4 字节，尾字节原样。
        let mut buf = vec![255, 255, 255, 255, 7];
        apply_recolor(&mut buf, params);
        assert_eq!(buf[0..4], [30, 20, 10, 255], "整像素按纸色处理");
        assert_eq!(buf[4], 7, "尾部分字节不应被处理");
    }

    /// 深色档用户回归断言（**ContentProtect 语义**，夜间：纸面→深色、墨迹→浅色）：纯黑
    /// 文字墨迹必须提亮为墨色（浅色），纯白纸面映射为深色纸色，中灰内容原样——
    /// 「黑字黑底 + 白边」不再出现（对比旧版：黑字内部保持黑色在深底上不可读）。
    /// 显式指定 ContentProtect 锁定「中间调原样」语义（FullBand 下中灰会被重映射，见
    /// `apply_recolor_fullband_*`）。
    #[test]
    fn apply_recolor_night_lightens_ink() {
        let params = RecolorParams { bg: [0x20, 0x20, 0x20], ink: Some([0xD8, 0xD8, 0xD8]) };
        let mut buf = vec![
            0, 0, 0, 255, // 纯黑（文字墨迹）L=0 → a_ink=1 → 墨色
            255, 255, 255, 255, // 纯白（纸面）L=1 → a_paper=1 → 纸色
            128, 128, 128, 255, // 中灰（图形/图片内容）L≈0.502 > 0.5 → 两段都不动
            32, 32, 32, 255, // 低亮（L≈0.1255 < 0.5）→ 提亮向墨色
        ];
        apply_recolor_with_curve(&mut buf, params, RecolorCurve::ContentProtect);
        // 墨色 0xD8=216 → 缓冲 B=216,G=216,R=216；纸色 0x20=32 同理。
        assert_eq!(&buf[0..4], &[216, 216, 216, 255], "纯黑墨迹应提亮为浅色（不再黑底黑字）");
        assert_eq!(&buf[4..8], &[32, 32, 32, 255], "纯白纸面应映射为深色纸色");
        assert_eq!(&buf[8..12], &[128, 128, 128, 255], "中灰内容应原样不动");
        // 32：L=32/255≈0.1255，a_ink=1-(32/255)/0.5=1-0.251≈0.749
        //   → 32+(216-32)*0.749≈169.8 → 170（旧 0.25 阈值下 a≈0.498→124，墨带加宽后提亮更多）。
        assert_eq!(&buf[12..16], &[170, 170, 170, 255], "近黑墨迹应连续提亮向墨色");
    }

    /// 墨带加宽边界（决策留痕：墨带 0.25 → 0.5，**ContentProtect 语义**）+ 图片中调保护 +
    /// `ink=None` 直通：新阈值 0.5 下 96(L≈0.376) 已进入墨带被提亮；带界之上 128(L≈0.502)
    /// 与中带样例 192(L≈0.753) 原样不动（图片中调保护）；`ink=None` 时黑字逐字节直通；
    /// A 恒 255。（FullBand 下 128/192 会被重映射，见 `apply_recolor_fullband_*`。）
    #[test]
    fn apply_recolor_ink_band_mid_content_and_none_passthrough() {
        let with_ink = RecolorParams { bg: [0x20, 0x20, 0x20], ink: Some([0xD8, 0xD8, 0xD8]) };
        let mut buf = vec![
            96, 96, 96, 255, // L≈0.376 < 0.5 → 墨带内 → 提亮
            128, 128, 128, 255, // L≈0.502 > 0.5 → 中间调保护 → 原样
            192, 192, 192, 255, // L≈0.753 ∈ (0.5, 0.9) → 图片中调保护 → 原样
            0, 0, 0, 255, // 纯黑 → 墨色
        ];
        apply_recolor_with_curve(&mut buf, with_ink, RecolorCurve::ContentProtect);
        // 96：a_ink=1-(96/255)/0.5=0.24706 → 96+(216-96)*0.24706≈125.65→126
        //   （旧 0.25 阈值下 96 原样不动；墨带加宽后进入提亮带）。
        assert_eq!(&buf[0..4], &[126, 126, 126, 255], "墨带加宽后 96(L≈0.376) 应提亮向墨色");
        assert_eq!(&buf[4..8], &[128, 128, 128, 255], "L≈0.502 恰在带上界之上 → 中调保护原样");
        assert_eq!(&buf[8..12], &[192, 192, 192, 255], "中带样例(0.5,0.9) 图片中调保护 → 原样");
        assert_eq!(&buf[12..16], &[216, 216, 216, 255], "纯黑墨迹提亮为墨色");

        let no_ink = RecolorParams { bg: [0x20, 0x20, 0x20], ink: None };
        let mut buf2 = vec![0, 0, 0, 0, 255, 255, 255, 255];
        // ink=None：两策略行为一致（仅纸带），走进程默认曲线即可；显式 ContentProtect 亦同。
        apply_recolor_with_curve(&mut buf2, no_ink, RecolorCurve::ContentProtect);
        // ink=None：黑字保持原样（纯黑直通），纸面照旧染深——既有行为不回归；A 恒 255。
        assert_eq!(&buf2[0..4], &[0, 0, 0, 255], "ink=None 黑字逐字节直通且 A 置 255");
        assert_eq!(&buf2[4..8], &[32, 32, 32, 255], "ink=None 纸面仍应染为纸色");
    }

    /// 夜间档合成 AA 渐变 0..=255 输出**无斑驳 / 无局部亮度反转**（**ContentProtect 语义**，
    /// 显式指定内置该策略的既有规范）。旧 0.25 墨带下，过渡像素（L∈0.25..0.90）被原样
    /// 留在 64~230 亮灰、浮在深纸底 #202020 上，输出在墨带上沿（≈64）处由「提亮」突变
    /// 为「原样」，亮灰平台 + 纸带压暗之间形成局部亮斑 / 锯齿（实测 216→…→69→64→90→
    /// 130→… 非单调）。本断言逐段写死 ContentProtect 曲线特性，并点名它消掉旧 zigzag
    /// 的哪一段：
    ///   (a) 墨带陡降段 [0,104] 严格非增 —— 消掉旧「亮灰平台停升」处 64↑90 的回折反转；
    ///   (b) 墨带尾段 [105,127] 相邻上浮 ≤2（纯舍入级）—— 杜绝深谷再回抬的 U 型反转；
    ///   (c) 带界 127↔128 两侧输出差 ≤2 —— 无接缝（旧 0.25 带界处 65↔64 亦无缝，此处
    ///       主要防新带界出现阶跃）；
    ///   (d) 中带 [128,229] 逐级 +1 原样 —— 图片中调保护，过渡区宽度被墨带压缩一半；
    ///   (e) 纸带 [230,254] 单调压暗向纸色 —— 纸面侧无反转；
    ///   (f) 全局 [0,229] 相邻差有界 ≤4 —— 粗粒度无尖刺。
    ///
    /// ⚠️ 关键差异：ContentProtect 在中带 [128,229] 是「逐级 +1 原样」——这正是小字缩放
    /// 下中灰杂散镶边的来源（该段被原样保留成亮灰平台浮在深纸上）。FullBand 对应断言见
    /// `apply_recolor_fullband_*`（该策略下中带被整段单调重映射、无平台）。
    #[test]
    fn apply_recolor_night_aa_gradient_no_moire() {
        let params = RecolorParams { bg: [0x20, 0x20, 0x20], ink: Some([0xD8, 0xD8, 0xD8]) };
        let mut buf: Vec<u8> = Vec::with_capacity(256 * 4);
        for g in 0..=255u8 {
            buf.extend_from_slice(&[g, g, g, 255]);
        }
        apply_recolor_with_curve(&mut buf, params, RecolorCurve::ContentProtect);
        // 灰渐变三通道一致，取 B 通道即输出亮度。
        let out = |g: usize| buf[g * 4];

        // (a) 墨带陡降段非增。
        for g in 0..104 {
            assert!(out(g + 1) <= out(g), "墨带 {g}→{} 出现亮度回折（旧 zigzag 段未消除）", g + 1);
        }
        // (b) 墨带尾段只允许舍入级上浮，无深谷再抬升。
        for g in 105..127 {
            assert!(
                out(g + 1) as i32 - out(g) as i32 <= 2,
                "墨带尾 {g}→{} 回折超过舍入级",
                g + 1
            );
        }
        // (c) 带界无缝：L≈0.5 两侧（127↔128）输出差 ≤2。
        assert!((out(127) as i32 - out(128) as i32).abs() <= 2, "墨带/中带边界出现接缝");
        // (d) 中带原样（图片中调保护）。
        for g in 128..229 {
            assert_eq!(out(g + 1), out(g) + 1, "中带 {g}→{} 应逐级 +1 原样", g + 1);
        }
        // (e) 纸带单调压暗向纸色。
        for g in 230..254 {
            assert!(out(g + 1) <= out(g), "纸带 {g}→{} 出现回折", g + 1);
        }
        // (f) 墨带+中带全局相邻差有界（纸带除外，其大斜率压暗属设计行为且已由 (e) 覆盖单调）。
        for g in 0..229 {
            assert!(
                (out(g + 1) as i32 - out(g) as i32).abs() <= 4,
                "墨带/中带 {g}→{} 相邻差超界",
                g + 1
            );
        }
        // 两端锚点：纯黑 → 墨色 216；纯白 → 纸色 32。
        assert_eq!(out(0), 216, "渐变首端纯黑应提亮为墨色");
        assert_eq!(out(255), 32, "渐变末端纯白应映射为纸色");
    }

    /// 【核心回归：去镶边 headless 断言】小字缩放抗锯齿剖面（1 像素宽笔画的亮度过渡
    /// `[0,64,128,192,229,255]`，横跨墨带→中带→纸带）：
    /// - **FullBand** 输出沿剖面**严格单调递减**（亮度一路从墨色滑到纸色，无局部反转、
    ///   无“原样保留”平台）——消灭用户报告的中灰杂散镶边；
    /// - **ContentProtect** 输出在同一剖面上**存在亮度回折**（中带 128→192→229 被原样
    ///   保留、亮度反而抬升，随后突降到纸色）——锁定两策略差异（中灰镶边根因只在
    ///   该策略存在，A/B 取舍据此对表）。
    /// 两端锚点两策略一致：纯黑→墨色、纯白→纸色。
    #[test]
    fn apply_recolor_fullband_aa_profile_monotonic() {
        let params = RecolorParams { bg: [0x20, 0x20, 0x20], ink: Some([0xD8, 0xD8, 0xD8]) };
        let profile = [0u8, 64, 128, 192, 229, 255];
        let shade = |curve: RecolorCurve| -> Vec<u8> {
            profile
                .iter()
                .map(|&g| {
                    let mut px = [g, g, g, 255];
                    apply_recolor_with_curve(&mut px, params, curve);
                    px[0] // 灰像素 B 通道 == 输出亮度。
                })
                .collect()
        };
        let fb = shade(RecolorCurve::FullBand);
        let cp = shade(RecolorCurve::ContentProtect);

        // FullBand：严格单调递减（相邻差 ≥1）。
        for w in fb.windows(2) {
            assert!(
                w[1] < w[0],
                "FullBand 剖面应严格单调递减（无平台/无局部亮度反转）：{fb:?}",
            );
        }
        // ContentProtect：中带原样保留 → 出现亮度回折（中灰镶边根因），锁定差异。
        let has_rise = cp.windows(2).any(|w| w[1] > w[0]);
        assert!(has_rise, "ContentProtect 剖面应在中带出现亮度回折（中灰镶边根因）: {cp:?}");

        // 锚点一致（两策略共同）：纯黑 → 墨色 216、纯白 → 纸色 32。
        assert_eq!(fb[0], 216, "FullBand 纯黑→墨色");
        assert_eq!(fb[5], 32, "FullBand 纯白→纸色");
        assert_eq!(cp[0], 216, "ContentProtect 纯黑→墨色");
        assert_eq!(cp[5], 32, "ContentProtect 纯白→纸色");
        // 剖面输出（供人工对表，`--nocapture` 可见）。
        eprintln!("[profile] ContentProtect: {cp:?}");
        eprintln!("[profile] FullBand:       {fb:?}");
    }

    /// FullBand 全渐变 0..=255：整段单调（非增，允许纯舍入级相等——斜率 -184/255≈-0.72/级
    /// 大于舍入量，不可能回折）；中带 **不再原样保留**（ContentProtect 下 128/192 原样
    /// 不动，FullBand 必须被重映射）；全带跨幅大（墨端 216 → 纸端 32）。
    #[test]
    fn apply_recolor_fullband_gradient_monotonic_no_platform() {
        let params = RecolorParams { bg: [0x20, 0x20, 0x20], ink: Some([0xD8, 0xD8, 0xD8]) };
        let mut buf: Vec<u8> = Vec::with_capacity(256 * 4);
        for g in 0..=255u8 {
            buf.extend_from_slice(&[g, g, g, 255]);
        }
        apply_recolor_with_curve(&mut buf, params, RecolorCurve::FullBand);
        let out = |g: usize| buf[g * 4];
        // 整段单调（非增）。
        for g in 0..255 {
            assert!(out(g + 1) <= out(g), "FullBand 渐变 {g}→{} 出现亮度反转", g + 1);
        }
        // 中带被重映射（无“原样保留”平台）。
        assert_ne!(out(128), 128, "FullBand 下 128 不应原样保留（中灰平台应消除）");
        assert_ne!(out(192), 192, "FullBand 下 192 不应原样保留");
        // 全带跨幅 + 锚点。
        assert_eq!(out(0), 216, "纯黑 → 墨色");
        assert_eq!(out(255), 32, "纯白 → 纸色");
        assert!(
            (out(0) as i32 - out(255) as i32) >= 180,
            "全带应跨满墨→纸大幅映射（当前 {}-{}）",
            out(0),
            out(255)
        );
    }

    /// FullBand 且 `ink=None`：降级为仅纸带映射、内容不动（与 ContentProtect 逐字节一致）——
    /// 没有墨色目标就无从“全带重映射”（否则黑字会染成纸色不可读）。语义抽查黑字直通。
    #[test]
    fn apply_recolor_fullband_ink_none_matches_contentprotect() {
        for g in [0u8, 64, 128, 192, 229, 255] {
            let mut a = [g, g, g, 255];
            let mut b = [g, g, g, 255];
            apply_recolor_with_curve(
                &mut a,
                RecolorParams { bg: [0x20, 0x20, 0x20], ink: None },
                RecolorCurve::FullBand,
            );
            apply_recolor_with_curve(
                &mut b,
                RecolorParams { bg: [0x20, 0x20, 0x20], ink: None },
                RecolorCurve::ContentProtect,
            );
            assert_eq!(a, b, "ink=None 时两策略应逐字节一致（仅纸带）@ g={g}");
        }
        let mut px = [0u8, 0, 0, 255];
        apply_recolor_with_curve(
            &mut px,
            RecolorParams { bg: [0x20, 0x20, 0x20], ink: None },
            RecolorCurve::FullBand,
        );
        assert_eq!(px, [0, 0, 0, 255], "FullBand+ink=None 黑字必须直通（不可读风险）");
    }

    /// CPU/GPU 一致性（headless 侧，需求 3）：CPU 整数 LUT（`li` = 灰度级）输出 vs 模拟
    /// GPU 片元公式（归一化连续 `luma`、`1-saturate(luma)` / 双带、再 ×255 取整——与
    /// `vendor/.../shaders.hlsl` 的 `recolor_sprite_fragment` 同公式）——对 0..=255 灰渐变
    /// 逐通道比 **≤1 LSB**。证明「同一策略」下 CPU LUT 与 GPU 连续亮度同策（本测试为
    /// **纯灰渐变**：灰度像素逐位一致或 ±1；彩色像素 FullBand ≤1 LSB、ContentProtect
    /// 纸带斜率 10 理论最大 ~10 LSB——W3 已注释分档，彩色不在本灰渐变测试覆盖内）。
    #[test]
    fn apply_recolor_curve_cpu_gpu_consistency() {
        let params = RecolorParams { bg: [0x20, 0x20, 0x20], ink: Some([0xD8, 0xD8, 0xD8]) };
        let gpu_ideal = |g: u8, curve: RecolorCurve| -> [u8; 4] {
            let orig_n = g as f32 / 255.0; // 纯灰：感知亮度 == 灰度级。
            let luma = orig_n;
            let bg_n = [(0x20u8 as f32) / 255.0; 3];
            let ink_n = [(0xD8u8 as f32) / 255.0; 3];
            let mut out = [0u8; 4];
            for c in 0..3 {
                let v = match curve {
                    RecolorCurve::FullBand => {
                        let fb = 1.0 - luma;
                        (bg_n[c] + (ink_n[c] - bg_n[c]) * fb) * 255.0
                    }
                    RecolorCurve::ContentProtect => {
                        let paper_w = ((luma - 0.90) / 0.10).clamp(0.0, 1.0);
                        let mut sh = orig_n + (bg_n[c] - orig_n) * paper_w;
                        let ink_w = 1.0 - (luma / 0.50).clamp(0.0, 1.0);
                        sh = sh + (ink_n[c] - sh) * ink_w;
                        sh * 255.0
                    }
                };
                out[c] = v.round() as u8;
            }
            out[3] = 255;
            out
        };
        for curve in [RecolorCurve::ContentProtect, RecolorCurve::FullBand] {
            for g in 0..=255u8 {
                let mut px = [g, g, g, 255];
                apply_recolor_with_curve(&mut px, params, curve);
                let ideal = gpu_ideal(g, curve);
                for c in 0..3 {
                    let d = (px[c] as i32 - ideal[c] as i32).abs();
                    assert!(
                        d <= 1,
                        "{curve:?} g={g} ch{c}: CPU={} 模拟GPU={} 差 {d} > 1 LSB",
                        px[c], ideal[c]
                    );
                }
            }
        }
    }

    /// **A-P2-1（一致性）**：R8 单通道位图 vs BGRA 位图在同一 FullBand 片元公式下的输出
    /// ≤1 LSB。模拟 GPU shader `recolor_sprite_fragment` 两条路径（BGRA 连续亮度 vs
    /// R8 量化亮度）：BGRA 路 `luma = dot(rgb/255, (0.299,0.587,0.114))`；R8 路先经
    /// [`encode_bgra_to_luma`] 存 `u8=round(0.299R+0.587G+0.114B)`，shader 读回 `L/255`，
    /// 再做 `out = bg + (ink−bg)·(1−luma)`（×255 取整）——对灰渐变全 0..=255 × 彩色抽样的
    /// 每通道差 ≤1 LSB。同时验证编码公式与 shader `0.299f/0.587f/0.114f` 逐位同策。
    #[test]
    fn ap2_r8_vs_bgra_shader_consistency_fullband() {
        let bg = [0x20u8, 0x20, 0x20];
        let ink = [0xD8u8, 0xD8, 0xD8];
        let bg_n = bg.map(|v| v as f32 / 255.0);
        let ink_n = ink.map(|v| v as f32 / 255.0);
        // FullBand 片元：`orig → luma → fb → out = bg + (ink−bg)·fb`，逐通道 round 到 u8。
        let fullband_bytes = |luma: f32, c: usize| -> u8 {
            let fb = 1.0 - luma;
            ((bg_n[c] + (ink_n[c] - bg_n[c]) * fb) * 255.0).round() as u8
        };
        // BGRA 路径：直接对 f32 连续亮度（shader 读 RGBA 后 dot）。
        let bg_path = |b: u8, g: u8, r: u8| -> [u8; 3] {
            let luma = 0.299f32 * (r as f32 / 255.0)
                + 0.587f32 * (g as f32 / 255.0)
                + 0.114f32 * (b as f32 / 255.0);
            [fullband_bytes(luma, 0), fullband_bytes(luma, 1), fullband_bytes(luma, 2)]
        };
        // R8 路径：编码端存 `round(0.299R+0.587G+0.114B)`，shader 读回 L/255。
        let r8_path = |b: u8, g: u8, r: u8| -> [u8; 3] {
            let px = [b, g, r, 255];
            let l = encode_bgra_to_luma(&px)[0] as f32 / 255.0;
            [fullband_bytes(l, 0), fullband_bytes(l, 1), fullband_bytes(l, 2)]
        };
        // 灰渐变全刻度（0..=255）。
        for g in 0..=255u8 {
            let a = bg_path(g, g, g);
            let b = r8_path(g, g, g);
            for c in 0..3 {
                let d = (a[c] as i32 - b[c] as i32).abs();
                assert!(d <= 1, "灰渐变 R8 vs BGRA 差 {d} > 1 LSB @ g={g} ch{c}");
            }
        }
        // 彩色抽样（非灰，覆盖 li 整数近似/浮点舍入边界）。
        for (bn, g, r) in [(0u8, 0, 255), (255, 0, 0), (0, 255, 0), (200, 30, 10), (10, 200, 30), (30, 10, 200), (128, 90, 200), (1, 254, 1), (254, 1, 254)] {
            let a = bg_path(bn, g, r);
            let b = r8_path(bn, g, r);
            for c in 0..3 {
                let d = (a[c] as i32 - b[c] as i32).abs();
                assert!(d <= 1, "彩色 R8 vs BGRA 差 {d} > 1 LSB @ ({bn},{g},{r}) ch{c}");
            }
        }
    }

    /// **A-P2-2（记账 bpp）**：`tile_bytes_per_pixel` 按 R8 门控给出 1/4 BPP——动态预算
    ///（`nominal_per_tile`）与字节账本（`TileSlot::bytes`）共用同一判定，避免预算语义错 4×。
    /// 确定性分支（不依赖进程 latch）直接断言；FullBand+ink 分支受曲线 latch 影响（env
    /// `GPUI_PDF_RECOLOR_CURVE=0` 时跳过）。
    #[test]
    fn ap2_tile_bytes_per_pixel_gate() {
        let night = RecolorParams { bg: [0x20, 0x20, 0x20], ink: Some([0xD8, 0xD8, 0xD8]) };
        assert_eq!(tile_bytes_per_pixel(None, true), 4, "recolor=None → BGRA 4BPP");
        assert_eq!(tile_bytes_per_pixel(None, false), 4, "recolor=None（CPU）→ BGRA 4BPP");
        assert_eq!(tile_bytes_per_pixel(Some(night), false), 4, "GPU 关闭 → BGRA 4BPP");
        let env_cp = std::env::var_os("GPUI_PDF_RECOLOR_CURVE").is_some_and(|v| v == "0");
        if !env_cp {
            assert_eq!(
                tile_bytes_per_pixel(Some(night), true),
                1,
                "GPU + FullBand + ink → R8 1BPP（-4×）"
            );
        }
    }

    /// 曲线 latch 默认（进程启动、无 `GPUI_PDF_RECOLOR_CURVE=0`）：应为
    /// [`RecolorCurve::FullBand`]；经 `apply_recolor`（latch 入口）验证中灰 128 被重映射
    /// 为 124（FullBand 语义，而非 ContentProtect 的原样 128）。env 显式 `=0` 时跳过
    ///（A/B 人工场景）。只读 latch、不翻转（避免与并行测试竞态）。
    #[test]
    fn apply_recolor_curve_default_is_fullband() {
        let env_cp = std::env::var_os("GPUI_PDF_RECOLOR_CURVE").is_some_and(|v| v == "0");
        if !env_cp {
            assert_eq!(recolor_curve(), RecolorCurve::FullBand, "默认曲线应为 FullBand");
            let mut px = [128u8, 128, 128, 255];
            apply_recolor(
                &mut px,
                RecolorParams { bg: [0x20, 0x20, 0x20], ink: Some([0xD8, 0xD8, 0xD8]) },
            );
            assert_eq!(px[0], 124, "latch 默认 FullBand：中灰 128 应重映射为 124（非原样）");
        }
    }

    /// GPU 门控默认翻转（需求 2）语义锁定：`gpu_gate_env` 解析 + 生产/测试缺省。
    /// 生产缺省 = **开**（`GPUI_PDF_GPU_RECOLOR=0` / 空串可强制回 CPU）；测试构型缺省 =
    /// **关**（只有显式 env 非空非 `"0"` 才开——避免测试依赖 GPU 像素输出）。
    #[test]
    fn gpu_gate_env_parse_and_defaults() {
        use std::ffi::OsStr;
        let env = std::env::var_os("GPUI_PDF_GPU_RECOLOR");
        // 纯解析。
        assert_eq!(gpu_gate_env(None), None, "未设 env：交回默认值判定");
        assert_eq!(gpu_gate_env(Some(OsStr::new("0"))), Some(false), "0 = 强制回 CPU");
        assert_eq!(gpu_gate_env(Some(OsStr::new(""))), Some(false), "空串 = 强制回 CPU");
        assert_eq!(gpu_gate_env(Some(OsStr::new("1"))), Some(true), "1 = 开");
        assert_eq!(gpu_gate_env(Some(OsStr::new("on"))), Some(true), "非 0 非空 = 开");
        // 生产缺省：未设 env → 开（`unwrap_or(true)` 即生产分支）。
        assert_eq!(gpu_gate_env(None).unwrap_or(true), true, "生产缺省 = GPU 开");
        assert_eq!(gpu_gate_env(Some(OsStr::new("0"))).unwrap_or(true), false, "显式 0 → 强制回 CPU");
        // 测试构型（本测试二进制）：只有显式 env 非空非 "0" 才开；否则关（含缺省）。
        let expect_on = gpu_gate_env(env.as_deref()).unwrap_or(false);
        assert_eq!(
            gpu_gate_env_default(),
            expect_on,
            "test 构型门控缺省应关（除非显式 env 开）"
        );
    }

    /// 确定性大缓冲填充：pixel i 的 B/G/R/A 用无依赖乘性伪随机——全亮度覆盖
    /// （墨/中/纸三带都命中）+ 非灰彩色（覆盖 `li` 整数近似路径）+ A 可变（映射强制 255）。
    fn big_bgra(n: usize) -> Vec<u8> {
        let mut buf = Vec::with_capacity(n * 4);
        for i in 0..n {
            let x = (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            buf.extend_from_slice(&[
                (x & 0xff) as u8,
                ((x >> 8) & 0xff) as u8,
                ((x >> 16) & 0xff) as u8,
                (i & 0xff) as u8,
            ]);
        }
        buf
    }

    /// 等值回归（需求 3）：同一大缓冲（4Mpx ≫ 并行阈值）下「并行 `apply_recolor` 结果
    /// == 单线程 `shade_pixels` 结果」逐字节一致；`ink=Some` 与 `ink=None` 各验一遍。
    /// 小缓冲单线程路径已由其 6 个既有字节级断言锁定 golden（本次未改它们）；本测试
    /// 只负责把**两个策略**的并行路径都钉死回单线程基线（显式传 `apply_recolor_with_curve`
    /// 与 `shade_pixels` 同策略，与进程 latch 默认解耦）。
    #[test]
    fn apply_recolor_parallel_equals_single_thread() {
        const N: usize = 4 * 1024 * 1024; // 4Mpx
        let with_ink = RecolorParams { bg: [0x20, 0x20, 0x20], ink: Some([0xD8, 0xD8, 0xD8]) };
        let (bg_r, bg_g, bg_b) = (f32::from(0x20u8), f32::from(0x20u8), f32::from(0x20u8));
        let ink = Some((f32::from(0xD8u8), f32::from(0xD8u8), f32::from(0xD8u8)));

        for curve in [RecolorCurve::ContentProtect, RecolorCurve::FullBand] {
            let mut par = big_bgra(N);
            let mut single = big_bgra(N);
            // 并行路径（N ≥ PARALLEL_MIN_PX → 进入 scope 分块）。
            apply_recolor_with_curve(&mut par, with_ink, curve);
            // 单线程 golden：同一映射整段逐像素扫描。
            shade_pixels(&mut single, bg_r, bg_g, bg_b, ink, curve);
            if par != single {
                let first = par.iter().zip(&single).position(|(a, b)| a != b);
                panic!("并行与单线程输出不一致（{curve:?} @ {first:?}）");
            }

            // ink=None：仅纸带作用（两策略同），两路径逐字节也应一致。
            apply_recolor_with_curve(
                &mut par,
                RecolorParams { bg: [0x20, 0x20, 0x20], ink: None },
                curve,
            );
            shade_pixels(&mut single, bg_r, bg_g, bg_b, None, curve);
            if par != single {
                let first = par.iter().zip(&single).position(|(a, b)| a != b);
                panic!("并行与单线程输出不一致（ink=None {curve:?} @ {first:?}）");
            }
        }
    }

    /// 墙钟基准（需求 4；默认 `#[ignore]` 不进常规测试）。人工跑：
    /// `cargo test -p pdf apply_recolor_bench_wall_clock -- --ignored --nocapture`。
    /// 1Mpx / 8Mpx 缓冲：`apply_recolor`（大缓冲并行）vs `shade_pixels`（单线程 golden），
    /// 每轮同输入（copy_from_slice 复位），多轮取中位数；报告加速比与线程数（debug 墙钟）。
    /// 固定用 ContentProtect 曲线（新 FullBand 内核更简单，性能只会不差于双带）。
    #[test]
    #[ignore]
    fn apply_recolor_bench_wall_clock() {
        use std::time::Instant;

        const ROUNDS: usize = 11;
        let curve = RecolorCurve::ContentProtect;
        let with_ink = RecolorParams { bg: [0x20, 0x20, 0x20], ink: Some([0xD8, 0xD8, 0xD8]) };
        let (bg_r, bg_g, bg_b) = (f32::from(0x20u8), f32::from(0x20u8), f32::from(0x20u8));
        let ink = Some((f32::from(0xD8u8), f32::from(0xD8u8), f32::from(0xD8u8)));
        let workers = std::thread::available_parallelism()
            .map(|p| p.get().min(PARALLEL_MAX_THREADS))
            .unwrap_or(1)
            .max(1);
        eprintln!("[bench] 并行线程数(封顶4): {workers}");

        for &n in &[1024 * 1024, 8 * 1024 * 1024] {
            let pristine = big_bgra(n);
            let mut single_buf = pristine.clone();
            let mut par_buf = pristine.clone();
            // 预热（线程/缓存）。
            shade_pixels(&mut single_buf, bg_r, bg_g, bg_b, ink, curve);
            apply_recolor_with_curve(&mut par_buf, with_ink, curve);

            let mut single_t: Vec<f64> = Vec::with_capacity(ROUNDS);
            let mut par_t: Vec<f64> = Vec::with_capacity(ROUNDS);
            for _ in 0..ROUNDS {
                single_buf.copy_from_slice(&pristine);
                let t0 = Instant::now();
                shade_pixels(&mut single_buf, bg_r, bg_g, bg_b, ink, curve);
                single_t.push(t0.elapsed().as_secs_f64() * 1000.0);

                par_buf.copy_from_slice(&pristine);
                let t1 = Instant::now();
                apply_recolor_with_curve(&mut par_buf, with_ink, curve);
                par_t.push(t1.elapsed().as_secs_f64() * 1000.0);
            }
            single_t.sort_by(|a, b| a.partial_cmp(b).unwrap());
            par_t.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let s_med = single_t[ROUNDS / 2];
            let p_med = par_t[ROUNDS / 2];
            eprintln!(
                "[bench] {n}px  单线中位 {s_med:.2}ms → 并行中位 {p_med:.2}ms  加速 {:.2}x",
                s_med / p_med
            );
        }
    }
}
