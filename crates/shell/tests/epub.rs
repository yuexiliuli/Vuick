//! EPUB 域（epub_wy 单源 target，§8.3）：EPUB 迁 WyEditor 只读宿主 + 章节翻转 + 注记/引文
//! 统一宿主 + 远跳自驱动。断言内部与 lib 单测 `src/app/tests/epub_wy.rs` 逐字一致
//! （P1 测试域化）；`epub_flip_chapter` 经 §8.6 外提 pub 化访问；`common::init_test` 注入
//! `VUICK_TEST`。

mod common;

use common::*;

// P4-1：EPUB 迁 WyEditor 只读宿主的 app 层回归（include! 进 `app::tests`）。
// 依赖 `target/测试epub.epub`（不入库、手动装配到 target/）。回退开关断言用测试专用
// 原子开关（免 env 竞态）。

/// P4-1：打开 测试epub.epub → 自动建 WyEditor 只读宿主（`md_wy`），预览路由判定走新宿主；
/// 只读、内容含书名、有多个章目标题；MarkdownPreview 仍喂（注记/引文/页码回退源）。
#[gpui::test]
async fn epub_opens_in_wy_readonly_host(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let epub = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/测试epub.epub");
    assert!(epub.exists(), "测试 EPUB 应存在");
    cx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.open_file(epub.to_str().unwrap(), window, cx)
        });
    });
    for _ in 0..8 {
        cx.run_until_parked();
    }
    let (md_wy, editable, has_text, toc_len, routed) = cx.update(|_w, cx| {
        let a = app.read(cx);
        let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
        (
            t.payload.md_wy.clone(),
            t.payload.md_wy
                .as_ref()
                .map(|w| w.read(cx).is_editable())
                .unwrap_or(true),
            t.payload.md_wy
                .as_ref()
                .map(|w| w.read(cx).text().contains("共产党宣言"))
                .unwrap_or(false),
            t.payload.md_wy
                .as_ref()
                .map(|w| w.read(cx).toc_entries().len())
                .unwrap_or(0),
            a.md_wy_preview().is_some(),
        )
    });
    assert!(md_wy.is_some(), "EPUB 应建 WyEditor 只读宿主");
    assert!(!editable, "EPUB 宿主应只读（禁编辑）");
    assert!(has_text, "宿主内容应含书名");
    assert!(toc_len >= 2, "应有多个章目标题（实际={toc_len}）");
    assert!(routed, "EPUB 预览应路由到 WyEditor 宿主");

    // 真实渲染进 WYSIWYG 宿主：build_frame 读 Doc（EPUB 源）+ 内存图（epubimg://）+ EPUB
    // 语义（%%ALIGN%%/标题居中）→ 应产出内存图行与对齐行（P4-1 需求 1/2 的渲染回归）。
    let (img_rows, aligned_rows, mem_images) = cx.update(|window, cx| {
        let a = app.read(cx);
        let wy = a.model.borrow().tabs[a.model.borrow().active_tab].payload.md_wy.clone().unwrap();
        let width = window.viewport_size().width.max(px(1.));
        let frame = markdown_wysiwyg::build_frame(
            wy.read(cx),
            window,
            width,
            (0., 999_999.),
        );
        let images = frame.rows.iter().filter(|r| r.image.is_some()).count();
        let aligned = frame.rows.iter().filter(|r| !r.align_offsets.is_empty()).count();
        let mem = frame
            .rows
            .iter()
            .filter(|r| r.image.as_ref().map(|i| i.src.as_ref().starts_with("epubimg://")).unwrap_or(false))
            .count();
        (images, aligned, mem)
    });
    assert!(img_rows >= 1, "WYSIWYG EPUB 宿主应含图片行（实际={img_rows}）");
    assert!(mem_images >= 1, "WYSIWYG EPUB 宿主图片行 src 应为 epubimg:// 内存图（实际={mem_images}）");
    assert!(aligned_rows >= 1, "WYSIWYG EPUB 宿主应含对齐行（%%ALIGN%%/标题居中，实际={aligned_rows}）");
}

/// P4-1：章节翻转（上一章/下一章）调用 WyEditor 的「章节间滚动」接口：
/// 下一章 → 滚动目标设为相邻章标题源码行；上一章 → 回到首章。
#[gpui::test]
async fn epub_flip_chapter_moves_between_headings(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let epub = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/测试epub.epub");
    cx.update(|window, cx| {
        app.update(cx, |app, cx| {
            app.open_file(epub.to_str().unwrap(), window, cx)
        });
    });
    for _ in 0..8 {
        cx.run_until_parked();
    }
    // 下一章：应把滚动目标设到「当前章之后的章标题」源码行。
    let (moved, target_is_heading) = cx.update(|_w, cx| {
        let moved = app.update(cx, |a, cx| a.epub_flip_chapter(1, cx));
        let idx = app.read(cx).model.borrow().active_tab;
        let wy = app.read(cx).model.borrow().tabs[idx].payload.md_wy.clone();
        let is_heading = wy
            .as_ref()
            .map(|w| {
                let tgt = w.read(cx).scroll_target_line();
                let entries = w.read(cx).toc_entries();
                tgt.map(|l| entries.iter().any(|&(e, _, _)| e == l))
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        (moved, is_heading)
    });
    assert!(moved, "多章且未到末章应能翻下一章");
    assert!(target_is_heading, "滚动目标应为某个章标题源码行");
    // 让重钉落位（prepaint 逐帧把目标块精确顶写回宿主）：有界等待「顶行已离开首块」。
    let idx0 = cx.update(|_w, cx| app.read(cx).model.borrow().active_tab);
    let initial_top = cx.update(|_w, cx| {
        app.read(cx).model.borrow().tabs[idx0]
            .payload
            .md_wy
            .as_ref()
            .and_then(|w| w.read(cx).top_source_line())
            .unwrap_or(0)
    });
    let mut landed = false;
    for _ in 0..60 {
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        cx.run_until_parked();
        let top = cx.update(|_w, cx| {
            app.read(cx).model.borrow().tabs[idx0]
                .payload
                .md_wy
                .as_ref()
                .and_then(|w| w.read(cx).top_source_line())
                .unwrap_or(0)
        });
        if top != initial_top {
            landed = true;
            break;
        }
    }
    assert!(landed, "翻章应驱动 WyEditor 滚动落位（顶行应离开首块）");
    // 上一章回到首章（已落位 → 当前在第二章 → 可翻回）。
    let moved_back = cx.update(|_w, cx| app.update(cx, |a, cx| a.epub_flip_chapter(-1, cx)));
    assert!(moved_back, "上一章应可翻回");
}

// P4-2：注记悬浮/引文跳转在 EPUB 全流程（app 层）回归见 `app/tests/pdf_epub.rs`
// 与 `markdown/wysiwyg/element/tests/epub.rs`（`wy_note_*`）。回退开关测试已随
// `MarkdownPreview` 后端删除（M5-B5 P4-2 终态）。

/// P4-2：注记/引文在**统一正确宿主**（只读 WyEditor，MD 与 EPUB 同一管线）的 app 层
/// 回归——打开含脚注 `[^1]`/`[!1]` 的 md，走真实 `open_file → ensure_md_wy` 喂源后：
/// 引文索引 refs/defs/details 以**源码行**记（与 EPUB 宿主同机制）；`navigate_marker`
/// 双向跳转设置对端滚动目标 + 加亮。换源后索引随 content_gen 失效。
#[gpui::test]
async fn md_wy_notes_citation_index_and_navigation_app_level(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = std::env::temp_dir().join("gpui-wy-notes-app.md");
    std::fs::write(&p, "正文引用[^1]\n\n[!1] 注记正文\n").unwrap();
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.open_file(p.to_str().unwrap(), window, cx));
    });
    for _ in 0..8 {
        cx.run_until_parked();
    }
    let (cit_ref_line, cit_def_line, note_txt, routed) = cx.update(|_w, cx| {
        let a = app.read(cx);
        let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
        let wy = t.payload.md_wy.clone().expect("MD 打开应建统一 WyEditor 预览");
        let cit = wy.read(cx).citation_index();
        (
            cit.refs.get(&1).map(|(l, _)| *l),
            cit.defs.get(&1).map(|(l, _)| *l),
            wy.read(cx).note_text_for(1),
            a.md_wy_preview().is_some(),
        )
    });
    assert!(routed, "MD 预览应路由到 WyEditor（统一宿主）");
    assert_eq!(cit_ref_line, Some(0), "文内 `[^1]` 应收录（行 0）");
    assert_eq!(cit_def_line, Some(2), "定义 `[!1]` 应收录（行 2）");
    assert_eq!(note_txt.as_deref(), Some("注记正文"), "缺 EPUB 注记表时正文回退 details");
    // 引用 → 定义 双向跳转（同一 update 内读，scroll_target 下一帧被 prepaint 消费）。
    let (stl, mhl) = cx.update(|_w, cx| {
        let a = app.read(cx);
        let wy = a.model.borrow().tabs[a.model.borrow().active_tab].payload.md_wy.clone().unwrap();
        let hit = markdown_wysiwyg::NoteMarkerHit {
            src_line: 0,
            n: 1,
            is_def: false,
            bounds: gpui::Bounds::new(gpui::point(px(0.), px(0.)), gpui::size(px(10.), px(10.))),
        };
        wy.update(cx, |w, cx| {
            w.navigate_marker(&hit, cx);
            (w.scroll_target_line(), w.marker_hl_test())
        })
    });
    assert_eq!(stl, Some(2), "引用 → 定义：滚动目标应为定义行");
    assert_eq!(mhl, Some((2, 1, true)), "定义标记应加亮 (2, n=1, is_def)");
    let _ = std::fs::remove_file(&p);
}

/// P4-2 回归：真实 EPUB（`target/测试epub.epub`，注记定义在尾部注释章）上「文内引用 →
/// 定义」**超远距跳转**必须命中「重钉收敛」路径、落位快且有界——直接锁死「点击 [n] →
/// 数秒白屏后才跳到注释」这一类回归：
/// 1. `advance_pending_scroll` 复用帧的收敛判据按**同号**比较（`cur` 正内容偏移 vs
///    `target` 负句柄偏移 → `diff = cur + target`）：旧实现 `cur - target` 同向加倍，
///    远距跳转恒被误判成「用户接管」提前弃钉（首帧写偏移被宿主复位/量程化时从此白屏）。
/// 2. 首帧重建出的远带帧**先不装回显示帧**（本帧 paint 仍用旧偏移，装回去会白一帧），
///    落位帧由 prepaint 按远带重建显示——跳转过程**无 `visible=0` 空白帧**。
/// 断言口径：真实渲染路径（走 `#md-wy-scroll` 宿主），跳动驱动帧数（≤ 上限）内
/// `top_source_line()` 到达定义块起点、`scroll_target` 已消费。
#[gpui::test]
async fn wy_note_jump_far_lands_in_bounded_frames_app_level(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let epub = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/测试epub.epub");
    assert!(epub.exists(), "target/测试epub.epub 应存在");
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.open_file(epub.to_str().unwrap(), window, cx));
    });
    // 等待 WY 宿主就绪 + 首帧渲染。
    let mut ready = false;
    for _ in 0..120 {
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        cx.run_until_parked();
        ready = cx.update(|_, cx| {
            app.read(cx).model.borrow().tabs.last().map(|t| {
                t.payload.md_wy.is_some()
                    && t.payload.md_wy
                        .as_ref()
                        .map(|w| w.read(cx).text().len() > 100)
                        .unwrap_or(false)
            })
            .unwrap_or(false)
        });
        if ready {
            break;
        }
    }
    assert!(ready, "WY 宿主应就绪");
    // 选一条文内 ref（靠前）→ 其 def 在尾部注释章。
    let (n, ref_line, def_line) = cx.update(|_, cx| {
        let a = app.read(cx);
        let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
        let wy = t.payload.md_wy.clone().unwrap();
        let cit = wy.read(cx).citation_index();
        let best = cit.refs.iter().find_map(|(&n, &(rl, _))| {
            cit.defs.get(&n).map(|&(dl, _)| (n, rl, dl))
        });
        best.expect("应有 ref→def 对")
    });
    // 触发注记跳转（同真实点击路径：navigate_marker → set_scroll_target）。
    let hit = markdown_wysiwyg::NoteMarkerHit {
        src_line: ref_line,
        n,
        is_def: false,
        bounds: gpui::Bounds::new(
            gpui::point(gpui::px(0.), gpui::px(0.)),
            gpui::size(gpui::px(10.), gpui::px(10.)),
        ),
    };
    cx.update(|_, cx| {
        let a = app.read(cx);
        let wy = a.model.borrow().tabs[a.model.borrow().active_tab].payload.md_wy.clone().unwrap();
        wy.update(cx, |w, cx| w.navigate_marker(&hit, cx));
    });
    // 驱动真实重排帧：落在 = scroll_target 已消费且顶行到达定义块起点。
    let mut landed_at: Option<u32> = None;
    for frame in 0..12u32 {
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        cx.run_until_parked();
        let (target_active, top) = cx.update(|_, cx| {
            let a = app.read(cx);
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
            let wy = t.payload.md_wy.clone().unwrap();
            (
                wy.read(cx).scroll_target_line().is_some(),
                wy.read(cx).top_source_line(),
            )
        });
        if !target_active && top == Some(def_line) {
            landed_at = Some(frame);
            break;
        }
    }
    assert!(
        landed_at.is_some(),
        "注记远距跳转应在上限帧内落位到定义块（def_line={def_line}）：引用 {ref_line} → 定义；\
         若触发 SCROLL_TARGET_MAX_APPLIES 兜底说明复用收敛未生效（回查 advance_pending_scroll 判据）"
    );
    let frame_n = landed_at.unwrap_or(99);
    assert!(
        frame_n <= 4,
        "注记远距跳转落位帧数应 ≤ 4（实测 {frame_n}）：远带帧重建在首帧、落位帧显示，\
         不应出现逐帧重钉/重排"
    );
    // 落位后宿主偏移停稳在远端（负偏移，内容坐标 ≈ def 块顶），且不再有滚动目标待处理。
    let (off_y, target_none) = cx.update(|_, cx| {
        let a = app.read(cx);
        let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
        let wy = t.payload.md_wy.clone().unwrap();
        (wy.read(cx).host_scroll().map(|(h, _)| h.offset().y.as_f32()), wy.read(cx).scroll_target_line().is_none())
    });
    assert!(target_none, "落位后不应残留滚动目标");
    assert!(
        off_y.is_some_and(|y| y < -10_000.),
        "注记定义在尾部注释章，宿主偏移应落在远端（实测 {off_y:?}）"
    );
}

/// P4-2 回归（本轮 bug）：注记远距跳转必须**无输入事件自驱落位**——点击确认后，app
/// 自身持续请求帧直到「远带帧成为显示帧」，绝不依赖鼠标/键盘事件触发重绘。
///
/// 根因（gpui 源码证据）：`advance_pending_scroll` 在 WyEditor **元素 prepaint** 里调
/// `window.refresh()`，而 gpui `Window::refresh` 只在 `draw_phase == None`（draw 之外）
/// 才 set_dirty 调度下一帧（deps gpui src/window.rs）——在 prepaint 相位里调用是 no-op
///（只置一个从不被读取的 `refreshing` 标志）。于是首帧写完宿主偏移后，若再没有输入事件，
/// 就再没有下一帧：滚动容器不按新偏移重排 → 远带页永不成为显示帧（**thumb 已到目标、
/// 内容冻结，鼠标一动突然跳**，用户实测）。
///
/// 本测试在跳转后**不 refresh、不发任何输入**，只投递 **app 自己请求的帧**：
/// `Window::simulate_next_frame`（gpui 提供给测试的「模拟平台按 on_next_frame 需求投递
/// 下一帧」入口）。修复前 app 从不请求帧 → 一帧都投递不出 → 顶行停在原块（冻结被复现）；
/// 修复后 ≤ 帧数内自驱落位。既有测试 `wy_note_jump_far_lands_in_bounded_frames_app_level`
/// 每步 `cx.cx.refresh()` 强制喂帧，掩盖了「无事件 → 无刷新 → 冻结」，故抓不到本 bug。
#[gpui::test]
async fn wy_note_jump_far_self_drives_without_input_events(cx: &mut TestAppContext) {
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(980.), px(720.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let epub = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/测试epub.epub");
    assert!(epub.exists(), "target/测试epub.epub 应存在");
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.open_file(epub.to_str().unwrap(), window, cx));
    });
    // 等待 WY 宿主就绪 + 首帧渲染（就绪阶段允许驱动帧；**跳转之后才严禁任何手动 refresh**）。
    let mut ready = false;
    for _ in 0..120 {
        cx.run_until_parked();
        let _ = cx.cx.refresh();
        cx.run_until_parked();
        ready = cx.update(|_, cx| {
            app.read(cx).model.borrow().tabs.last().map(|t| {
                t.payload.md_wy.is_some()
                    && t.payload.md_wy
                        .as_ref()
                        .map(|w| w.read(cx).text().len() > 100)
                        .unwrap_or(false)
            })
            .unwrap_or(false)
        });
        if ready {
            break;
        }
    }
    assert!(ready, "WY 宿主应就绪");
    // 排空就绪阶段（首帧/暖机/滚动条淡入等）遗留的帧请求——真实平台每帧都投递、不会
    // 累积，这里手动排净，确保下面的 `delivered` 只反映「跳转后 app 自己新请求的帧」。
    for _ in 0..64 {
        cx.run_until_parked();
        let drained = cx.update(|window, cx| window.simulate_next_frame(cx));
        cx.run_until_parked();
        if drained == 0 {
            break;
        }
    }
    // 选一条文内 ref（靠前）→ 其 def 在尾部注释章。
    let (n, ref_line, def_line) = cx.update(|_, cx| {
        let a = app.read(cx);
        let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
        let wy = t.payload.md_wy.clone().unwrap();
        let cit = wy.read(cx).citation_index();
        let best = cit.refs.iter().find_map(|(&n, &(rl, _))| {
            cit.defs.get(&n).map(|&(dl, _)| (n, rl, dl))
        });
        best.expect("应有 ref→def 对")
    });
    // 触发注记跳转（同真实点击路径：navigate_marker → set_scroll_target）。
    let hit = markdown_wysiwyg::NoteMarkerHit {
        src_line: ref_line,
        n,
        is_def: false,
        bounds: gpui::Bounds::new(
            gpui::point(gpui::px(0.), gpui::px(0.)),
            gpui::size(gpui::px(10.), gpui::px(10.)),
        ),
    };
    cx.update(|_, cx| {
        let a = app.read(cx);
        let wy = a.model.borrow().tabs[a.model.borrow().active_tab].payload.md_wy.clone().unwrap();
        wy.update(cx, |w, cx| w.navigate_marker(&hit, cx));
    });
    // —— 关键：跳转后不 refresh、不发输入；只投递 app 主动请求的下一帧 ——
    // `window.simulate_next_frame(cx)` = 模拟平台把 app 经 on_next_frame 登记的帧投递出
    // 去；没登记就投递 0 帧 = 什么都不发生 = 冻结（修复前的落态）。
    let mut landed_at: Option<u32> = None;
    for frame in 0..8u32 {
        cx.run_until_parked();
        let delivered = cx.update(|window, cx| window.simulate_next_frame(cx));
        cx.run_until_parked();
        let (target_active, top) = cx.update(|_, cx| {
            let a = app.read(cx);
            let t = &a.model.borrow().tabs[a.model.borrow().active_tab];
            let wy = t.payload.md_wy.clone().unwrap();
            (
                wy.read(cx).scroll_target_line().is_some(),
                wy.read(cx).top_source_line(),
            )
        });
        if !target_active && top == Some(def_line) {
            landed_at = Some(frame);
            break;
        }
        if frame >= 1 && delivered == 0 && !target_active {
            break; // 无帧请求也无待办目标：再等也不会变化（冻结）。
        }
    }
    assert!(
        landed_at.is_some(),
        "注记跳转【无输入事件】应在自驱帧内落位到定义块（def_line={def_line}，引用 {ref_line}）：\
         app 必须持续请求帧直到远带成为显示帧——若失败说明 prepaint 里写了宿主偏移却无帧调度\
         （gpui 帧内 refresh 是 no-op），界面冻结、要等鼠标事件才跳"
    );
    let frame_n = landed_at.unwrap_or(99);
    assert!(
        frame_n <= 3,
        "自驱落位帧数应 ≤ 3（实测 {frame_n}）：首帧写偏移、次帧容器按新偏移重排显示远带，\
         不应出现逐帧重钉/重排"
    );
}
