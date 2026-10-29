//! 页面显示层**纸色重着色**（paper-only shading）：只改渲染位图像素中的「纸面」色，
//! 内容（文字 / 图片 / 图形）像素保持原色，**绝不写回 PDF 文件、不改 pdfium 解析**。
//!
//! 语义：把近白（纸面）像素按亮度渐变着成纸色；`ink` 为 `Some` 时（深色档），把
//! 近黑墨迹像素按亮度渐变提亮为墨色（浅色）——浅纸上的黑字在深色档若只改纸面会
//! 变成「黑字黑底 + 浅灰描边」不可读（用户复报）。中间调（[`INK_L_HI`]..[`PAPER_L_LO`]
//! 之间：深/中调图形、图片内容）一律原样不动。文本层（选区/搜索）由 view 独立
//! 叠加层绘制、不依赖位图像素，因此本重着色不破坏文本层机制。
//!
//! 执行位置：**worker 渲染线程内**对 bgra 缓冲就地改（每瓦片数 ms 级一次性开销），
//! 主线程 / 文档 / 文件零接触；参数为 `None` 时完全直通（零像素循环、零成本）。
//!
//! 提速：墨/纸两带权重按 256 级整数感知亮度预计算成 `const` LUT（编译期求值），
//! 每像素只做一次整数亮度索引 + 查表，去掉逐像素的 f32 除/round 与 clamp。

use std::sync::{
    LazyLock,
    atomic::{AtomicBool, Ordering},
};

/// T4 GPU 重着色门控（**默认 off**）：环境变量 `GPUI_PDF_GPU_RECOLOR` 非空且非 `"0"`
/// 时开启。开启后 PDF 显示路径的 raw→着色改在 **GPU 绘制时**由片元着色器完成
/// （`img().recolor(...)` → `RecolorSprite` 片元），worker **不再对显示路径做 CPU 逐像素
/// apply_recolor**——CPU 税归零、切档天然 0 重算 0 重栅（只改图片元素参数）。不开启时
/// 一切行为与 dbd3236 **逐字节一致**（CPU 路径照常）。
///
/// GPU 像素输出无法 headless 断言（K23②/③）→ 默认 off，真机由用户复验后翻转默认；
/// headless 只证「机制 + CPU 税消失 + 门控」（T4 验收口径）。
pub(crate) static GPU_RECOLOR_GATE: LazyLock<AtomicBool> = LazyLock::new(|| {
    let on = std::env::var_os("GPUI_PDF_GPU_RECOLOR").is_some_and(|v| !v.is_empty() && v != "0");
    AtomicBool::new(on)
});

/// 当前 GPU 重着色门控（进程级：单 worker 单进程，与 worker 单例对齐；生产读一次后
/// 进程内恒定，不随 env 变化动态翻转）。
pub(crate) fn gpu_recolor_gate() -> bool {
    let on = GPU_RECOLOR_GATE.load(Ordering::Relaxed);
    if on {
        // 真机诊断 1/4：门控激活（进程级首次读为真时打一次；门控 off 完全静默）。
        diag_once(&DIAG_GATE_ON, || {
            eprintln!("[pdf] gpu recolor enabled (GPUI_PDF_GPU_RECOLOR=on)");
        });
    }
    on
}

/// T4 GPU 重着色**真机诊断**基础设施：门控开启时每类只打印一次到 stderr（进程级 latch，
/// 避免逐瓦片/逐帧刷屏）；正常使用（门控 off）零输出。选 stderr 便于 `2>&1 | grep` 过滤。
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
pub(crate) fn diag_view_recolor(bg: [u8; 3], ink: Option<[u8; 3]>) {
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
#[cfg(test)]
pub(crate) fn set_gpu_recolor_gate_for_test(v: bool) {
    GPU_RECOLOR_GATE.store(v, Ordering::Relaxed);
}

/// 把本 crate 的重着色参数映射为 gpui 图片元素级参数（T4 GPU 通道）：字段同构
/// （`bg` = 纸色、`ink` = 墨色可选），由 view 在门控开启时挂到 `img().recolor(...)`。
pub(crate) fn to_image_recolor(p: RecolorParams) -> gpui::ImageRecolor {
    gpui::ImageRecolor {
        bg: p.bg,
        ink: p.ink,
    }
}

/// 纸重着色参数：`bg` = 纸色（仅近白纸面像素映射至此）；`ink` = 墨色（`Some` 时把
/// 近黑内容/文字像素映射至此，深色档用）。RGB 通道（0..=255）；alpha 一贯保持 255。
/// `ink=None` → 内容像素完全直通（既有「只改纸面」行为）。
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
/// 【P1 升级开关】此常量即为切换开关：若真机自验仍糊，把 `INK_L_HI` 升到 ~0.9 即切换
/// 为「全带反色式映射」（墨带几乎覆盖全部亮度，纸带仅剩最白高光微染），只改此常量，
/// 曲线代码与 LUT 自动随常量重生成，无需其它改动。
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

/// 纸墨重着色（纯函数，就地改 `bgra` 像素）：每像素感知亮度取**整数近似**
/// `li = (299R + 587G + 114B) / 1000 ∈ [0,255]` 作 LUT 索引（纯灰像素 `li` 恰等于
/// 灰度级本身，与浮点公式逐位一致；彩色像素以亮度灰级近似，允许 ±1 通道舍入差），
/// 各通道 `out = orig + (bg - orig) × PAPER_WEIGHT_LUT[li]`——近白纸面趋向纸色、
/// 亮度低于 [`PAPER_L_LO`] 的内容不动；若设了 `ink`，近黑墨迹再叠
/// `out = orig + (ink - orig) × INK_WEIGHT_LUT[li]`——纯黑墨迹趋向墨色、亮度高于
/// [`INK_L_HI`] 的中间调内容不动。两段作用域（≤0.5 与 ≥0.9）在亮度上不相交，
/// 顺序无关。A 恒 255。
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
/// 的近黑影区」，这类近黑像素同样会被微染向墨色——深/中调内容（L∈0.5..0.9）则
/// 绝不触碰。
pub fn apply_recolor(bgra: &mut [u8], params: RecolorParams) {
    let [bg_r, bg_g, bg_b] = params.bg.map(f32::from);
    let ink = params.ink.map(|[r, g, b]| (f32::from(r), f32::from(g), f32::from(b)));
    let n = bgra.len() / 4;
    // 小缓冲（瓦片级）：单线程扫描，输出与既有版本逐字节一致（golden）。
    if n < PARALLEL_MIN_PX {
        shade_pixels(bgra, bg_r, bg_g, bg_b, ink);
        return;
    }
    // 取机器可用并行度并封顶；1 核时连 scope 都省，直接单线程。
    let workers = std::thread::available_parallelism()
        .map(|p| p.get().min(PARALLEL_MAX_THREADS))
        .unwrap_or(1)
        .max(1);
    if workers <= 1 {
        shade_pixels(bgra, bg_r, bg_g, bg_b, ink);
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
            s.spawn(move || shade_pixels(head, bg_r, bg_g, bg_b, ink));
            start = end;
        }
    });
}

/// 对 `bgra` 整段（从下标 0 扫描到 `len/4` 个像素）就地执行同一套双带 LUT 映射
/// （与旧版单线程逐像素循环逐字节一致）。并行各块切片与单线程路径共用本函数；
/// 块与块互不重叠故可安全并跑。`#[inline]` 便于每线程循环充分内联。
#[inline]
fn shade_pixels(bgra: &mut [u8], bg_r: f32, bg_g: f32, bg_b: f32, ink: Option<(f32, f32, f32)>) {
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

    /// 深色档用户回归断言（夜间语义：纸面→深色、墨迹→浅色）：纯黑文字墨迹必须提亮
    /// 为墨色（浅色），纯白纸面映射为深色纸色，中灰内容原样——「黑字黑底 + 白边」
    /// 不再出现（对比旧版：黑字内部保持黑色在深底上不可读）。
    #[test]
    fn apply_recolor_night_lightens_ink() {
        let params = RecolorParams { bg: [0x20, 0x20, 0x20], ink: Some([0xD8, 0xD8, 0xD8]) };
        let mut buf = vec![
            0, 0, 0, 255, // 纯黑（文字墨迹）L=0 → a_ink=1 → 墨色
            255, 255, 255, 255, // 纯白（纸面）L=1 → a_paper=1 → 纸色
            128, 128, 128, 255, // 中灰（图形/图片内容）L≈0.502 > 0.5 → 两段都不动
            32, 32, 32, 255, // 低亮（L≈0.1255 < 0.5）→ 提亮向墨色
        ];
        apply_recolor(&mut buf, params);
        // 墨色 0xD8=216 → 缓冲 B=216,G=216,R=216；纸色 0x20=32 同理。
        assert_eq!(&buf[0..4], &[216, 216, 216, 255], "纯黑墨迹应提亮为浅色（不再黑底黑字）");
        assert_eq!(&buf[4..8], &[32, 32, 32, 255], "纯白纸面应映射为深色纸色");
        assert_eq!(&buf[8..12], &[128, 128, 128, 255], "中灰内容应原样不动");
        // 32：L=32/255≈0.1255，a_ink=1-(32/255)/0.5=1-0.251≈0.749
        //   → 32+(216-32)*0.749≈169.8 → 170（旧 0.25 阈值下 a≈0.498→124，墨带加宽后提亮更多）。
        assert_eq!(&buf[12..16], &[170, 170, 170, 255], "近黑墨迹应连续提亮向墨色");
    }

    /// 墨带加宽边界（决策留痕：墨带 0.25 → 0.5）+ 图片中调保护 + `ink=None` 直通：
    /// 新阈值 0.5 下 96(L≈0.376) 已进入墨带被提亮；带界之上 128(L≈0.502) 与中带样例
    /// 192(L≈0.753) 原样不动（图片中调保护）；`ink=None` 时黑字逐字节直通；A 恒 255。
    #[test]
    fn apply_recolor_ink_band_mid_content_and_none_passthrough() {
        let with_ink = RecolorParams { bg: [0x20, 0x20, 0x20], ink: Some([0xD8, 0xD8, 0xD8]) };
        let mut buf = vec![
            96, 96, 96, 255, // L≈0.376 < 0.5 → 墨带内 → 提亮
            128, 128, 128, 255, // L≈0.502 > 0.5 → 中间调保护 → 原样
            192, 192, 192, 255, // L≈0.753 ∈ (0.5, 0.9) → 图片中调保护 → 原样
            0, 0, 0, 255, // 纯黑 → 墨色
        ];
        apply_recolor(&mut buf, with_ink);
        // 96：a_ink=1-(96/255)/0.5=0.24706 → 96+(216-96)*0.24706≈125.65→126
        //   （旧 0.25 阈值下 96 原样不动；墨带加宽后进入提亮带）。
        assert_eq!(&buf[0..4], &[126, 126, 126, 255], "墨带加宽后 96(L≈0.376) 应提亮向墨色");
        assert_eq!(&buf[4..8], &[128, 128, 128, 255], "L≈0.502 恰在带上界之上 → 中调保护原样");
        assert_eq!(&buf[8..12], &[192, 192, 192, 255], "中带样例(0.5,0.9) 图片中调保护 → 原样");
        assert_eq!(&buf[12..16], &[216, 216, 216, 255], "纯黑墨迹提亮为墨色");

        let no_ink = RecolorParams { bg: [0x20, 0x20, 0x20], ink: None };
        let mut buf2 = vec![0, 0, 0, 0, 255, 255, 255, 255];
        apply_recolor(&mut buf2, no_ink);
        // ink=None：黑字保持原样（纯黑直通），纸面照旧染深——既有行为不回归；A 恒 255。
        assert_eq!(&buf2[0..4], &[0, 0, 0, 255], "ink=None 黑字逐字节直通且 A 置 255");
        assert_eq!(&buf2[4..8], &[32, 32, 32, 255], "ink=None 纸面仍应染为纸色");
    }

    /// 夜间档合成 AA 渐变 0..=255 输出**无斑驳 / 无局部亮度反转**（需求 4a/4b）。
    /// 旧 0.25 墨带下，过渡像素（L∈0.25..0.90）被原样留在 64~230 亮灰、浮在深纸底
    /// #202020 上，输出在墨带上沿（≈64）处由「提亮」突变为「原样」，亮灰平台 + 纸带
    /// 压暗之间形成局部亮斑 / 锯齿（实测 216→…→69→64→90→130→… 非单调）。
    /// 本断言逐段写死新曲线特性，并点名它消掉旧 zigzag 的哪一段：
    ///   (a) 墨带陡降段 [0,104] 严格非增 —— 消掉旧「亮灰平台停升」处 64↑90 的回折反转；
    ///   (b) 墨带尾段 [105,127] 相邻上浮 ≤2（纯舍入级）—— 杜绝深谷再回抬的 U 型反转；
    ///   (c) 带界 127↔128 两侧输出差 ≤2 —— 无接缝（旧 0.25 带界处 65↔64 亦无缝，此处
    ///       主要防新带界出现阶跃）；
    ///   (d) 中带 [128,229] 逐级 +1 原样 —— 图片中调保护，过渡区宽度被墨带压缩一半；
    ///   (e) 纸带 [230,254] 单调压暗向纸色 —— 纸面侧无反转；
    ///   (f) 全局 [0,229] 相邻差有界 ≤4 —— 粗粒度无尖刺。
    #[test]
    fn apply_recolor_night_aa_gradient_no_moire() {
        let params = RecolorParams { bg: [0x20, 0x20, 0x20], ink: Some([0xD8, 0xD8, 0xD8]) };
        let mut buf: Vec<u8> = Vec::with_capacity(256 * 4);
        for g in 0..=255u8 {
            buf.extend_from_slice(&[g, g, g, 255]);
        }
        apply_recolor(&mut buf, params);
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
    /// 只负责把并行路径钉死回单线程基线。
    #[test]
    fn apply_recolor_parallel_equals_single_thread() {
        const N: usize = 4 * 1024 * 1024; // 4Mpx
        let with_ink = RecolorParams { bg: [0x20, 0x20, 0x20], ink: Some([0xD8, 0xD8, 0xD8]) };
        let (bg_r, bg_g, bg_b) = (f32::from(0x20u8), f32::from(0x20u8), f32::from(0x20u8));
        let ink = Some((f32::from(0xD8u8), f32::from(0xD8u8), f32::from(0xD8u8)));

        let mut par = big_bgra(N);
        let mut single = big_bgra(N);
        // 并行路径（N ≥ PARALLEL_MIN_PX → 进入 scope 分块）。
        apply_recolor(&mut par, with_ink);
        // 单线程 golden：同一映射整段逐像素扫描。
        shade_pixels(&mut single, bg_r, bg_g, bg_b, ink);
        if par != single {
            let first = par.iter().zip(&single).position(|(a, b)| a != b);
            panic!("并行与单线程输出不一致（首个差异字节 @ {first:?}）");
        }

        // ink=None：仅纸带作用，两路径逐字节也应一致。
        apply_recolor(&mut par, RecolorParams { bg: [0x20, 0x20, 0x20], ink: None });
        shade_pixels(&mut single, bg_r, bg_g, bg_b, None);
        if par != single {
            let first = par.iter().zip(&single).position(|(a, b)| a != b);
            panic!("并行与单线程输出不一致（ink=None @ {first:?}）");
        }
    }

    /// 墙钟基准（需求 4；默认 `#[ignore]` 不进常规测试）。人工跑：
    /// `cargo test -p pdf apply_recolor_bench_wall_clock -- --ignored --nocapture`。
    /// 1Mpx / 8Mpx 缓冲：`apply_recolor`（大缓冲并行）vs `shade_pixels`（单线程 golden），
    /// 每轮同输入（copy_from_slice 复位），多轮取中位数；报告加速比与线程数（debug 墙钟）。
    #[test]
    #[ignore]
    fn apply_recolor_bench_wall_clock() {
        use std::time::Instant;

        const ROUNDS: usize = 11;
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
            shade_pixels(&mut single_buf, bg_r, bg_g, bg_b, ink);
            apply_recolor(&mut par_buf, with_ink);

            let mut single_t: Vec<f64> = Vec::with_capacity(ROUNDS);
            let mut par_t: Vec<f64> = Vec::with_capacity(ROUNDS);
            for _ in 0..ROUNDS {
                single_buf.copy_from_slice(&pristine);
                let t0 = Instant::now();
                shade_pixels(&mut single_buf, bg_r, bg_g, bg_b, ink);
                single_t.push(t0.elapsed().as_secs_f64() * 1000.0);

                par_buf.copy_from_slice(&pristine);
                let t1 = Instant::now();
                apply_recolor(&mut par_buf, with_ink);
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
