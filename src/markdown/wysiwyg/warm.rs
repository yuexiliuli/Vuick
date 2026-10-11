// ═══ M4 跳过归档：本仓 P1 冷启动 emoji 缺陷的**永久驻留防御**（勿删）═══
// P1 冷启动 emoji：Windows COLR 光栅失败被字模缓存钉住，上游 zed（含 93d7456）未修；
// M4 vendor gpui_windows+[patch] 已决定跳过 → 本组件为本仓永久驻留防御，勿删；若升级
// gpui 或引入 vendor 修复可整体移除并删除本注释。
// 本文件 = 进程级 COLR 光栅预热（一次/进程），与 cache.rs 的 `NotdefRetry` 状态机、
// frame/mod.rs Pass2 的 notdef/回退校验桥接同属一套防御体系（改前先读
// `WY_NOTDEF_DEBUG` / `WY_WARM_DEBUG` 实测取证，`docs/debug-envs.md`）。
use gpui::{point, px, Window};

/// 进程级 warm 状态（一次/进程）：`WARM_DONE` 置位后，各窗格 paint 顶首先查
/// [`warm_emoji_is_done`] 快速短路，**不再进入** [`warm_emoji_rasterizer`] 本体。
static WARM_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static WARM_ATTEMPTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static WARM_SIZES: std::sync::OnceLock<Vec<f32>> = std::sync::OnceLock::new();
const WARM_MAX_ATTEMPTS: usize = 60;
/// warm 是否已完成：各窗格 paint 前调用，完成后直接跳过 warm，零重复访问。
pub(crate) fn warm_emoji_is_done() -> bool {
    WARM_DONE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Windows 冷启动 color-emoji 光栅预热（一次/进程，**共享给所有文本窗格**：预览/
/// WYSIWYG/源码编辑区）。实测：DWrite 在冷进程里**首次**光栅化彩色 COLR 字形时
/// `rasterize_color` 失败，gpui 回退为单色（彩色 emoji 无单色轮廓 → 空白/方框），且
/// `sprite_atlas` 按 (font,glyph,size) 把坏结果钉住整次运行（字符在文本里、复制正常、
/// shape 正常——就是画不出）。各窗格在自己 `paint` 的**顶首**（画自己任何文字之前）
/// 都调本函数；**先画的窗格**先在此把 DWrite/D3D 彩字管线打热（冷僻字号 7px、首个
/// 可能失败、其后即成功），之后任何窗格的真实 emoji 首帧即正确，不再被某个先画窗格
/// 毒化。探针字体取窗口缺省文本样式即可——回退按字符独立，emoji 字符必路由到回退彩
/// 字字体。
pub(crate) fn warm_emoji_rasterizer(window: &mut Window, tag: &'static str) {
    // 冷启动 COLR 自愈：DWrite 彩色光栅在冷进程首次使用时失败/返回空白。平台层
    //（gpui_windows rasterize_glyph）已保证「空白不写 atlas 缓存」（返回 Err 即不入缓存），
    // 因此 paint_emoji 的 `.is_ok()` 从此“诚实”——失败 = 真的没彩出来。这里就借助它做
    // 帧驱动重试：**单帧内磨完全部字号**（WYSIWYG/编辑器/预览真实字号不同：分别取自
    // theme.font_size / font_size_px / window.text_style，默认 DEFAULT_FONT_SIZE=14），
    // 常规+粗体各自 shape 一遍合并 emoji 后统一屏幕外光栅；全部成功即 DONE，正常冷启动
    // 1 帧收工、不再被逐帧 window.refresh() 拖住启动；任一字号失败 → 强制下一帧整批重试
    //（失败不入缓存、自然自愈），直到全成功或帧数上限；给出总帧数上限，防 GPU/驱动
    // 彻底不行时无限自旋。
    if WARM_DONE.load(std::sync::atomic::Ordering::Relaxed) {
        // 高频重绘下每帧都打会刷屏 → 仅 WY_WARM_DEBUG=1 时打，且每进程**只打一次**
        //（「哪个窗格命中已完成的守卫」这一诊断目标达成后，默认路径不再喷日志）。
        static LOGGED_SKIP: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        if std::env::var_os("WY_WARM_DEBUG").is_some()
            && !LOGGED_SKIP.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            eprintln!("[WY-WARM] already done -> skip ({tag})");
        }
        return;
    }

    let sizes = WARM_SIZES.get_or_init(|| {
        let body_size = match window.text_style().font_size {
            gpui::AbsoluteLength::Pixels(p) => p.as_f32(),
            gpui::AbsoluteLength::Rems(r) => r.to_pixels(window.rem_size()).as_f32(),
        };
        let mut v: Vec<f32> = vec![body_size, 13.0, 14.0, 15.0, 16.0, 17.0, 18.0, 20.0, 24.0];
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v.dedup();
        // 仅保留合理范围，避免把冷僻/超大字号也磨进去。
        v.retain(|s| (10.0..=32.0).contains(s));
        v
    });

    let probe = "⚠✅❗❌❤️🔥  🚀✨★☆➡  😀😅🤔💡📌";
    let body = window.text_style().font();
    let mut body_bold = body.clone();
    body_bold.weight = gpui::FontWeight::BOLD;
    // 标题（## 等）形体 = 正文**粗体**（element.rs `text_style_for` 对 Heading 置 BOLD）：
    // emoji 回退落到的 FontId 可能与常规体不同 → atlas 键 (font,glyph,size) 不同 → 必须把
    // 粗体也一起磨热，否则「标题里的 ❗ 首次真实光栅」冷失败被钉成空白（图片后的标题恰是
    // 首见者——图片只是恰好排前面，非图片自身问题）。
    let bases: Vec<gpui::Font> = vec![body.clone(), body_bold];

    // —— 单帧内磨完全部字号；任一字号失败即整批下一帧重试（保留 COLR 冷自愈）——
    let started = std::time::Instant::now();
    let mut all_ok = true;
    let mut font_ids: Vec<u32> = Vec::new();
    // 先 shape 完再画：shape 拿不可变借用，画要走 window 可变借用，分开时段避免 E0502。
    // 常规 + 粗体分别 shape，合并 emoji（含各自字号）后再统一屏幕外 paint。
    let mut emojis: Vec<(gpui::FontId, gpui::GlyphId, f32)> = Vec::new();
    {
        let ts = window.text_system();
        for &fs in sizes.iter() {
            for font in &bases {
                let run = [gpui::TextRun {
                    len: probe.len(),
                    font: font.clone(),
                    ..Default::default()
                }];
                let line = ts.shape_line(probe.into(), px(fs), &run, None);
                emojis.extend(line.runs.iter().flat_map(|r| {
                    r.glyphs
                        .iter()
                        .filter(|g| g.is_emoji)
                        .map(move |g| (r.font_id, g.id, fs))
                }));
            }
        }
    }
    for (font_id, glyph_id, fs) in emojis {
        let fid = font_id.0 as u32;
        if !font_ids.contains(&fid) {
            font_ids.push(fid);
        }
        // 屏幕外绘制：仅触发 atlas 光栅化（图元被裁剪、无可见痕迹）。
        if window
            .paint_emoji(point(px(-1e7), px(-1e7)), font_id, glyph_id, px(fs))
            .is_err()
        {
            all_ok = false;
        }
    }
    if all_ok {
        WARM_DONE.store(true, std::sync::atomic::Ordering::Relaxed);
        eprintln!(
            "[WY-WARM] all {:?} sizes ok in {:.0}ms (base={:?}) done-by={tag}",
            sizes,
            started.elapsed().as_millis(),
            body.family
        );
    } else {
        // 仍有字号没磨热（COLR 冷 / 突发失败）：失败不入缓存 → 必须强制下一帧整批重试，
        // 否则该字号真实首光栅冷失败被原生 atlas 钉成豆腐块（旧逐帧版即靠这自愈）。
        let attempt = WARM_ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if attempt < WARM_MAX_ATTEMPTS {
            eprintln!(
                "[WY-WARM] partial fail(attempt={attempt}) sizes={sizes:?} font_ids={font_ids:?} from={tag} -> refresh",
            );
            window.refresh();
        } else {
            // 到上限仍不行 → 放弃自旋，等用户滚动/其它重绘触发（原生仍可能自愈）。
            WARM_DONE.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}
