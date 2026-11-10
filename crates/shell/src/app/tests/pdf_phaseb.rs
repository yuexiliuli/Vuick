// Phase B：PDF 交互 UI 集成测试（B1 选中弹卡 / B2 右键菜单 / B3 书签持久化+跳转）。
// 全部走真实 PdfViewer + 真 PDF 夹具（缺夹具 SKIP），经 `pdf::test_util::lock()` 串行防
// pdfium 原生并发。每个用例用**唯一的临时副本**（`temp_pdf_copy`）——标注写回 / 页操作
// 都改源文件，且书签按规范化路径键，副本路径唯一避免跨用例的书签存储污染（K8）。

use pdf::AnnotKind;

fn pdf_fixture() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/pdf测试.pdf")
}

fn missing_pdf() -> bool {
    pdf::test_util::missing_pdf_fixture()
}

/// 夹具副本（每用例唯一 tag）：测试隔离，不污染共享 `target/pdf测试.pdf`。
fn temp_pdf_copy(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("gpui-editor-pdf-tests");
    let _ = std::fs::create_dir_all(&dir);
    let p = dir.join(format!("{tag}-{}.pdf", std::process::id()));
    let _ = std::fs::remove_file(&p);
    let _ = pdf::test_util::copy_fixture_writable(&pdf_fixture(), &p);
    p
}

/// 打开 PDF 选项卡并泵到 PdfViewer 加载完成。
fn open_pdf(app: &Entity<EditorApp>, cx: &mut VisualTestContext, path: &std::path::Path) {
    cx.update(|window, cx| {
        app.update(cx, |app, cx| app.open_file(path.to_str().unwrap(), window, cx));
    });
    let ok = wait_pdf_loaded(app, cx);
    assert!(ok, "PdfViewer 应加载完成：{}", path.display());
}

fn wait_pdf_loaded(app: &Entity<EditorApp>, cx: &mut VisualTestContext) -> bool {
    for _ in 0..400 {
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let loaded = cx.update(|_w, cx| {
            tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab])
                .map(|pv| pv.read(cx).is_loaded())
                .unwrap_or(false)
        });
        if loaded {
            return true;
        }
    }
    false
}

/// 等一个谓词为真（有界）：注释写回 / 页操作重载等异步完成用。
fn pump_until(cx: &mut VisualTestContext, mut pred: impl FnMut(&mut VisualTestContext) -> bool) -> bool {
    for _ in 0..400 {
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(2));
        if pred(cx) {
            return true;
        }
    }
    false
}

/// 活动 PDF 窗口内前若干页里第一个可用字符段：返回 `((page, k), (p1, p2))`（第 `page`
/// 页、字符 k..k+3 的两个窗口坐标）。先同步把前若干页文本读入缓存（`load_text_for_test`
/// 阻塞等 worker，摆脱「可见页预取」时序），再逐页扫描字符盒（封面页可能无文本层）。
#[allow(clippy::type_complexity)]
fn first_char_range_points(
    app: &Entity<EditorApp>,
    cx: &mut VisualTestContext,
) -> ((usize, usize), (gpui::Point<gpui::Pixels>, gpui::Point<gpui::Pixels>)) {
    let mut found: Option<((usize, usize), (gpui::Point<gpui::Pixels>, gpui::Point<gpui::Pixels>))> = None;
    cx.update(|window, cx| {
        let pv = tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab])
            .expect("活动选项卡应挂 PDF")
            .clone();
        for page in 0..12usize {
            pv.update(cx, |v, _| {
                v.load_text_for_test(page);
            });
            let mut start: Option<(usize, gpui::Point<gpui::Pixels>)> = None;
            for k in 0..800usize {
                if let (Some(p), Some(q)) = (
                    pv.read(cx).char_window_pos(window, page, k),
                    pv.read(cx).char_window_pos(window, page, k + 3),
                ) {
                    start = Some((k, p));
                    found = Some(((page, k), (p, q)));
                    break;
                }
            }
            if start.is_some() {
                break;
            }
        }
    });
    found.expect("前 12 页应存在可用字符盒（文本层未缓存？）")
}

/// 真实拖选（按下→移动到终点→松开）。
fn drag_select(cx: &mut VisualTestContext, from: gpui::Point<gpui::Pixels>, to: gpui::Point<gpui::Pixels>) {
    let mods = gpui::Modifiers::default();
    cx.simulate_mouse_move(from, gpui::MouseButton::Left, mods);
    cx.simulate_mouse_down(from, gpui::MouseButton::Left, mods);
    cx.simulate_mouse_move(to, gpui::MouseButton::Left, mods);
    cx.simulate_mouse_up(to, gpui::MouseButton::Left, mods);
    settle_frames(cx, 3);
}

/// 测试探针直接弹选中卡（跳过真实拖选几何）：选区 `(page, lo)..(page, hi)`。
fn open_sel_card(app: &Entity<EditorApp>, cx: &mut VisualTestContext, page: usize, lo: usize, hi: usize) {
    cx.update(|window, cx| {
        let pv = tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]).unwrap().clone();
        pv.update(cx, |v, cx| v.open_selection_card_for_test((page, lo), (page, hi), window, cx));
    });
    settle_frames(cx, 3);
}

/// 右键 PDF 正文区（窗口中部）→ 弹 PDF 菜单（返回活动 PDF 实体）。
fn pdf_right_click(app: &Entity<EditorApp>, cx: &mut VisualTestContext) -> gpui::Entity<pdf::PdfViewer> {
    let pos = gpui::Point::new(px(600.), px(400.));
    cx.simulate_mouse_down(pos, gpui::MouseButton::Right, gpui::Modifiers::default());
    cx.simulate_mouse_up(pos, gpui::MouseButton::Right, gpui::Modifiers::default());
    settle_frames(cx, 3);
    let pdf = cx.update(|_w, cx| {
        tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab])
            .expect("PDF 菜单需活动 PDF")
            .clone()
    });
    assert!(cx.debug_bounds("pdf-menu-bookmark").is_some(), "右键应弹「添加书签」");
    pdf
}

fn menu_item_disabled(app: &Entity<EditorApp>, cx: &mut VisualTestContext, ix: usize) -> bool {
    cx.update(|_w, cx| {
        app.read(cx)
            .pdf_menu_entity
            .as_ref()
            .map(|m| m.read(cx).is_item_disabled(ix))
            .unwrap_or(true)
    })
}

fn clipboard_text(cx: &mut VisualTestContext) -> Option<String> {
    cx.update(|_w, cx| {
        cx.read_from_clipboard()
            .map(|i| i.text().unwrap_or_default())
    })
}

/// 展开侧边栏「目录」面板（PDF 打开后默认仅活动栏）。
fn open_toc_panel(app: &Entity<EditorApp>, cx: &mut VisualTestContext) {
    cx.update(|_w, cx| {
        app.update(cx, |app, _| app.sidebar.toggle_view(sidebar::SidebarView::Toc));
    });
    settle_frames(cx, 3);
}

// ---------------------------------------------------------------------------
// B1：选中弹卡（复制 / 高亮 / 删除线 / 下划线）
// ---------------------------------------------------------------------------

/// B1 端到端：真实拖选 → 弹卡（4 项在场）；点「复制」→ 剪贴板含选中文本、关卡（选区
/// 保留可反复复制）；重新选中并点「高亮」→ 注释**只进内存待存**（pending≥1、置脏、磁盘
/// 不变），随后 `save()` → 落盘清脏 → 重开读回 Highlight。
#[gpui::test]
async fn b1_drag_selection_card_copy_and_highlight(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("b1card");
    let disk0 = std::fs::read(&p).expect("读初始磁盘");
    open_pdf(&app, cx, &p);

    // 真实拖选 → 弹卡
    let ((page, k), (a, b)) = first_char_range_points(&app, cx);
    drag_select(cx, a, b);
    assert!(cx.debug_bounds("pdf-sel-card").is_some(), "拖选结束应弹卡");
    for sel in ["pdf-sel-card-copy", "pdf-sel-card-highlight", "pdf-sel-card-strike", "pdf-sel-card-underline"] {
        assert!(cx.debug_bounds(sel).is_some(), "弹卡应含「{sel}」");
    }
    // 布局为「菜单形式」：一行一项（同列对齐、y 单调递增），防回退为横向按钮条（K2）。
    let items = [
        "pdf-sel-card-copy",
        "pdf-sel-card-highlight",
        "pdf-sel-card-strike",
        "pdf-sel-card-underline",
    ]
    .map(|sel| cx.debug_bounds(sel).expect("弹卡项").center());
    let xs: Vec<f32> = items.iter().map(|p| p.x.as_f32()).collect();
    let ys: Vec<f32> = items.iter().map(|p| p.y.as_f32()).collect();
    assert!(
        xs.iter().all(|x| (x - xs[0]).abs() < 1.0),
        "菜单应一行一项（同行对齐）：xs={xs:?}"
    );
    assert!(
        ys.windows(2).all(|w| w[0] < w[1]),
        "菜单应竖向排列（y 递增）：ys={ys:?}"
    );
    // 定位回归断言：menu 出现在「松开鼠标的位置」（同右键菜单定位口径）。水平锚点==松手点
    // （旧行为=选区几何中心，会落在选区中点而偏左）；垂直受 `snap_to_window_with_margin`
    // 近窗缘钳制（与右键菜单共享该夹回），故断言松手点落在卡片纵向范围内即满足两态。
    let card = cx.debug_bounds("pdf-sel-card").expect("弹卡应已定位");
    let card_origin = card.origin;
    let card_bottom = card_origin.y.as_f32() + card.size.height.as_f32();
    assert!(
        (card_origin.x.as_f32() - b.x.as_f32()).abs() < 8.,
        "菜单水平应锚在松手点：card x={}，松手点 x={}",
        card_origin.x.as_f32(),
        b.x.as_f32()
    );
    assert!(
        card_origin.y.as_f32() <= b.y.as_f32() + 0.5 && card_bottom >= b.y.as_f32() - 0.5,
        "菜单纵向应覆盖松手点：card top={} bottom={card_bottom}，松手点 y={}",
        card_origin.y.as_f32(),
        b.y.as_f32()
    );

    // 点「复制」→ 剪贴板含选中文本；卡片关闭、选区保留
    let copy = cx.debug_bounds("pdf-sel-card-copy").expect("复制项").center();
    cx.simulate_click(copy, gpui::Modifiers::default());
    settle_frames(cx, 3);
    let clip = clipboard_text(cx);
    assert!(
        clip.as_deref().is_some_and(|s| !s.trim().is_empty()),
        "复制应把选中文本写入剪贴板：{clip:?}"
    );
    assert!(cx.debug_bounds("pdf-sel-card").is_none(), "复制后关卡");

    // 重新弹卡（同页同段）→ 点「高亮」→ 只进内存待存（pending≥1、dirty、磁盘不变）
    let wk = cx.update(|window, cx| {
        let pv = tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]).unwrap().clone();
        pv.update(cx, |v, cx| {
            v.open_selection_card_for_test((page, k), (page, k + 3), window, cx)
        });
        pv
    });
    settle_frames(cx, 3);
    assert!(cx.debug_bounds("pdf-sel-card").is_some(), "重新弹卡");
    let hl = cx.debug_bounds("pdf-sel-card-highlight").expect("高亮项").center();
    cx.simulate_click(hl, gpui::Modifiers::default());
    settle_frames(cx, 3);
    let pending = pump_until(cx, |cx| {
        cx.update(|_w, cx| {
            wk.read(cx).pending_annot_count() >= 1
        })
    });
    assert!(pending, "点「高亮」后应有一条待存注释");
    assert!(
        cx.update(|_w, cx| wk.read(cx).is_dirty()),
        "加注释后应置脏（待保存）"
    );
    assert_eq!(
        std::fs::read(&p).expect("重读磁盘"),
        disk0,
        "注释只进内存：磁盘 bytes 必须不变（待保存模型）"
    );
    assert!(cx.debug_bounds("pdf-sel-card").is_none(), "写卡后关卡+清选区");

    // save() → 落盘清脏 → 重开文件读回 Highlight（效果断言；待保存模型下 live 渲染 doc
    // 不重载——注释以叠加层渲染，落盘内容须重开文件验证，与写回断言口径一致）。
    cx.update(|_w, cx| wk.update(cx, |v, cx| {
        v.save(cx);
    }));
    let saved = pump_until(cx, |cx| {
        cx.update(|_w, cx| !wk.read(cx).is_dirty() && wk.read(cx).pending_annot_count() == 0)
    });
    assert!(saved, "save 后应清 pending 与 dirty");
    let saved_bytes = std::fs::read(&p).expect("save 后重读");
    let (w2, d2, _) = pdf::PdfWorker::open_and_dims(saved_bytes).expect("重开保存文件");
    let anns = w2.annotations_for_page(d2, page).expect("读回注释");
    assert!(
        anns.iter().any(|i| i.kind == AnnotKind::Highlight),
        "save 后重开应读回 Highlight（写盘效果断言）；err={:?}",
        cx.update(|_w, cx| wk.read(cx).last_modify_error().map(|s| s.to_string()))
    );
    w2.close(d2);
    let _ = std::fs::remove_file(&p);
}

/// B1 关闭路径：点卡片外 / Esc / 滚动均关闭弹卡。
#[gpui::test]
async fn b1_selection_card_closes_on_outside_escape_scroll(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("b1close");
    open_pdf(&app, cx, &p);
    let pdf = cx.update(|_w, cx| tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]).unwrap().clone());
    let ((page, k), _) = first_char_range_points(&app, cx);

    // 点卡片外（PDF 正文空白处）→ 关闭
    open_sel_card(&app, cx, page, k, k + 3);
    cx.simulate_click(gpui::Point::new(px(60.), px(700.)), gpui::Modifiers::default());
    settle_frames(cx, 3);
    assert!(cx.debug_bounds("pdf-sel-card").is_none(), "点卡片外应关闭");

    // Esc → 关闭
    open_sel_card(&app, cx, page, k, k + 3);
    cx.simulate_keystrokes("escape");
    settle_frames(cx, 3);
    assert!(cx.debug_bounds("pdf-sel-card").is_none(), "Esc 应关闭弹卡");

    // 滚动（跳页）→ 关闭
    open_sel_card(&app, cx, page, k, k + 3);
    cx.update(|_w, cx| {
        pdf.update(cx, |v, cx| {
            v.scroll_to_page(1);
            cx.notify();
        });
    });
    settle_frames(cx, 4);
    assert!(cx.debug_bounds("pdf-sel-card").is_none(), "滚动后应关闭弹卡");
    let _ = std::fs::remove_file(&p);
}

// ---------------------------------------------------------------------------
// B2：右键菜单（添加书签 / 插入页面 / 删除页面 / 提取页面）
// ---------------------------------------------------------------------------

/// B2+B3 端到端：右键 PDF → 四项菜单在场（多页文档删除项**启用**）；点「添加书签」→
/// 书签入内存 + 原子落盘（磁盘 load 仍在）；TOC「书签」区段出两条 → 点第 2 页书签
/// 跳页（current_page=2）；点删除 × 逐条移除 → 空 → 区段消失 + 磁盘清空。
#[gpui::test]
async fn b2_pdf_menu_bookmark_persists_and_toc_jumps(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("b2bm");
    open_pdf(&app, cx, &p);
    let pdf = pdf_right_click(&app, cx);

    for item in ["pdf-menu-bookmark", "pdf-menu-insert", "pdf-menu-delete", "pdf-menu-extract"] {
        assert!(cx.debug_bounds(item).is_some(), "右键菜单应含「{item}」");
    }
    assert!(!menu_item_disabled(&app, cx, 3), "多页文档「删除页面」应启用");

    // 点「添加书签」（当前页 = 第 1 页）
    let bm_item = cx.debug_bounds("pdf-menu-bookmark").expect("添加书签").center();
    cx.simulate_click(bm_item, gpui::Modifiers::default());
    settle_frames(cx, 4);
    let path = pdf.read_with(cx, |v, _| v.path().to_path_buf());
    let bms = cx.update(|_w, cx| app.read(cx).pdf_bookmarks_for(&path).map(|v| v.to_vec()));
    assert_eq!(bms.as_ref().map(|v| v.len()), Some(1), "应存 1 条书签");
    assert_eq!(bms.as_ref().and_then(|v| v.first()).map(|b| b.page_index), Some(0), "书签应为第 1 页");

    // 磁盘持久化：load() 重读仍在
    let disk = crate::app::pdf_bookmarks::load()
        .remove(&crate::app::pdf_bookmarks::normalize_key(&path))
        .unwrap_or_default();
    assert_eq!(disk.len(), 1, "书签应原子落盘（重开仍在）");

    // 再直接加一条第 2 页书签（TOC 跳转用）
    cx.update(|_w, cx| {
        app.update(cx, |app, cx| app.add_pdf_bookmark(&path, 1, "第 2 页".into(), cx));
    });
    // 回到第 1 页（确定性基线）
    cx.update(|_w, cx| {
        pdf.update(cx, |v, cx| {
            v.scroll_to_page(0);
            cx.notify();
        });
    });
    settle_frames(cx, 3);

    // TOC「书签」区段
    open_toc_panel(&app, cx);
    assert!(cx.debug_bounds("sidebar-toc-bookmarks").is_some(), "应有「书签」区段");
    assert!(cx.debug_bounds("sidebar-toc-bookmark-0").is_some());
    assert!(cx.debug_bounds("sidebar-toc-bookmark-1").is_some());

    // 点第 2 页书签 → scroll_to_page(1) → current_page 首见页 = 2
    let row1 = cx.debug_bounds("sidebar-toc-bookmark-1").expect("第 2 页书签").center();
    cx.simulate_click(row1, gpui::Modifiers::default());
    settle_frames(cx, 4);
    let cur = cx.update(|_w, cx| pdf.read(cx).current_page().map(|(a, _)| a));
    assert_eq!(cur, Some(2), "点书签应跳转到第 2 页，实际 {cur:?}");

    // 删除 × → 逐条移除 → 空区段消失 + 磁盘同步
    let del1 = cx.debug_bounds("sidebar-toc-bookmark-del-1").expect("删除按钮").center();
    cx.simulate_click(del1, gpui::Modifiers::default());
    settle_frames(cx, 3);
    assert!(cx.debug_bounds("sidebar-toc-bookmark-1").is_none(), "删除后第 2 页书签行应消失");
    let del0 = cx.debug_bounds("sidebar-toc-bookmark-del-0").expect("删除按钮").center();
    cx.simulate_click(del0, gpui::Modifiers::default());
    settle_frames(cx, 3);
    assert!(cx.debug_bounds("sidebar-toc-bookmarks").is_none(), "无书签时区段应隐藏");
    let disk_after = crate::app::pdf_bookmarks::load()
        .remove(&crate::app::pdf_bookmarks::normalize_key(&path))
        .unwrap_or_default();
    assert!(disk_after.is_empty(), "全删后磁盘不应再有该路径书签");
    let _ = std::fs::remove_file(&p);
}

/// B2：插入页面（确认框「插入」）→ page_count+1；删除页面（确认框「删除」）→ 复原。
#[gpui::test]
async fn b2_insert_and_delete_page_with_confirm(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("b2indel");
    open_pdf(&app, cx, &p);
    let pdf = pdf_right_click(&app, cx);
    let count0 = cx.update(|_w, cx| pdf.read(cx).page_count());
    assert!(count0 >= 2, "夹具应足量页：{count0}");

    // 插入页面：确认框 → 「插入」
    let ins = cx.debug_bounds("pdf-menu-insert").expect("插入页面").center();
    cx.simulate_click(ins, gpui::Modifiers::default());
    settle_frames(cx, 3);
    assert!(cx.has_pending_prompt(), "插入页面前应弹确认框");
    let (msg, _) = cx.pending_prompt().expect("确认框");
    assert_eq!(msg, "插入页面", "确认框标题一致");
    cx.simulate_prompt_answer("插入");
    let grown = pump_until(cx, |cx| {
        cx.update(|_w, cx| pdf.read(cx).page_count() == count0 + 1)
    });
    assert!(
        grown,
        "插入后页数 +1：{count0} → {}",
        cx.update(|_w, cx| pdf.read(cx).page_count())
    );

    // 删除页面：确认框 → 「删除」→ 复原
    pdf_right_click(&app, cx);
    let del = cx.debug_bounds("pdf-menu-delete").expect("删除页面").center();
    cx.simulate_click(del, gpui::Modifiers::default());
    settle_frames(cx, 3);
    assert!(cx.has_pending_prompt(), "删除页面前应弹确认框");
    cx.simulate_prompt_answer("删除");
    let shrunk = pump_until(cx, |cx| {
        cx.update(|_w, cx| pdf.read(cx).page_count() == count0)
    });
    assert!(
        shrunk,
        "删除后页数复原为 {count0}，实际 {}",
        cx.update(|_w, cx| pdf.read(cx).page_count())
    );
    let _ = std::fs::remove_file(&p);
}

/// B2：提取页面产出独立 PDF（第 1 页）→ 重开该产物页数 == 1；且该单页文档上
/// 「删除页面」菜单项**禁用**（唯一页护栏）。
#[gpui::test]
async fn b2_extract_creates_single_page_and_delete_disabled(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("b2extract");
    open_pdf(&app, cx, &p);
    let pdf = cx.update(|_w, cx| tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]).unwrap().clone());

    // 提取第 1 页到独立文件（生产路径经 cfg(not(test)) 走另存为对话框；测试直接驱动 API）
    let out = std::env::temp_dir()
        .join("gpui-editor-pdf-tests")
        .join(format!("b2out-{}.pdf", std::process::id()));
    let _ = std::fs::remove_file(&out);
    cx.update(|_w, cx| {
        let o = out.clone();
        pdf.update(cx, |v, cx| v.extract_page_to(0, o, cx));
    });
    let produced = pump_until(cx, |cx| {
        let _ = cx;
        out.exists() && std::fs::metadata(&out).map(|m| m.len() > 3).unwrap_or(false)
    });
    assert!(produced, "提取应产出独立 PDF 文件");
    std::thread::sleep(std::time::Duration::from_millis(50));

    // 重开产物：页数 == 1，右键「删除页面」禁用、「插入页面」启用
    open_pdf(&app, cx, &out);
    let single_pdf = pdf_right_click(&app, cx);
    let pc = cx.update(|_w, cx| single_pdf.read(cx).page_count());
    assert_eq!(pc, 1, "提取产物应为 1 页，实际 {pc}");
    assert!(menu_item_disabled(&app, cx, 3), "唯一页「删除页面」应禁用");
    assert!(!menu_item_disabled(&app, cx, 2), "唯一页「插入页面」仍可用");
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(&out);
}

// ---------------------------------------------------------------------------
// C：待保存模型（P1）——Ctrl+Z 撤销 / 灰框与删除注释 / tab dirty 圆点 / 关闭保存提示
// ---------------------------------------------------------------------------

/// C1 端到端（待保存）：点「高亮」→ 注释只进内存（磁盘不变）+ tab dirty 圆点置真
/// （`_pdf_sub` observe 同步）；Ctrl+Z 撤销 → 该条移除、dirty 回落、磁盘仍不变。
#[gpui::test]
async fn c1_pending_annot_ctrlz_undo_and_tab_dirty(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("c1undo");
    let disk0 = std::fs::read(&p).expect("读初始磁盘");
    open_pdf(&app, cx, &p);
    let wk = cx.update(|_w, cx| tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]).unwrap().clone());
    let ((page, k), (a, _)) = first_char_range_points(&app, cx);

    // 加一条高亮（待存）
    open_sel_card(&app, cx, page, k, k + 3);
    let hl = cx.debug_bounds("pdf-sel-card-highlight").expect("高亮项").center();
    cx.simulate_click(hl, gpui::Modifiers::default());
    settle_frames(cx, 3);
    assert!(
        pump_until(cx, |cx| cx.update(|_w, cx| wk.read(cx).pending_annot_count() == 1)),
        "点「高亮」后应有一条待存注释"
    );
    assert_eq!(std::fs::read(&p).expect("重读磁盘"), disk0, "注释只进内存：磁盘不变");
    // tab.dirty 经 observe 同步置真（工具栏圆点）
    assert!(
        pump_until(cx, |cx| cx.update(|_w, cx| app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab].dirty)),
        "加注释后 tab.dirty 应置真（observe 同步）"
    );

    // 点击被注释文字（聚焦 PDF + 灰框焦），再 Ctrl+Z 撤销该条注释
    let mods = gpui::Modifiers::default();
    cx.simulate_mouse_down(a, gpui::MouseButton::Left, mods);
    cx.simulate_mouse_up(a, gpui::MouseButton::Left, mods);
    settle_frames(cx, 3);
    assert!(
        cx.update(|_w, cx| wk.read(cx).focused_annot_id()).is_some(),
        "点击被注释文字应产生灰框焦"
    );
    cx.simulate_keystrokes("ctrl-z");
    settle_frames(cx, 3);
    assert!(
        pump_until(cx, |cx| {
            cx.update(|_w, cx| wk.read(cx).pending_annot_count() == 0 && !wk.read(cx).is_dirty())
        }),
        "Ctrl+Z 应撤销该条待存注释并清脏"
    );
    assert!(
        pump_until(cx, |cx| cx.update(|_w, cx| !app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab].dirty)),
        "撤销后 tab.dirty 应回落"
    );
    assert_eq!(std::fs::read(&p).expect("重读磁盘"), disk0, "撤销全程磁盘不变（未保存）");
    let _ = std::fs::remove_file(&p);
}

/// C2 端到端（待保存）：点击被注释文字出现**灰框**（`focused_annot_id` 置位）；右键该处
/// → 菜单含 [删除注释]（其余四项仍在）；点 [删除注释] → 该条待存注释消失、dirty 回落、
/// 灰框清空、磁盘仍原样。
#[gpui::test]
async fn c2_greybox_and_annot_delete_menu(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("c2grey");
    let disk0 = std::fs::read(&p).expect("读初始磁盘");
    open_pdf(&app, cx, &p);
    let wk = cx.update(|_w, cx| tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]).unwrap().clone());
    let ((page, k), (a, _)) = first_char_range_points(&app, cx);

    // 加一条高亮（待存）→ 点被注释文字 → 灰框焦
    open_sel_card(&app, cx, page, k, k + 3);
    let hl = cx.debug_bounds("pdf-sel-card-highlight").expect("高亮项").center();
    cx.simulate_click(hl, gpui::Modifiers::default());
    settle_frames(cx, 3);
    assert!(
        pump_until(cx, |cx| cx.update(|_w, cx| wk.read(cx).pending_annot_count() == 1)),
        "应有一条待存注释"
    );
    let mods = gpui::Modifiers::default();
    cx.simulate_mouse_down(a, gpui::MouseButton::Left, mods);
    cx.simulate_mouse_up(a, gpui::MouseButton::Left, mods);
    settle_frames(cx, 3);
    let focused = cx.update(|_w, cx| wk.read(cx).focused_annot_id());
    assert!(focused.is_some(), "点击被注释文字应出现灰框焦");

    // 右键同点 → 菜单含 [删除注释] + 原四项
    cx.simulate_mouse_down(a, gpui::MouseButton::Right, mods);
    cx.simulate_mouse_up(a, gpui::MouseButton::Right, mods);
    settle_frames(cx, 3);
    assert!(cx.debug_bounds("pdf-menu-del-annot").is_some(), "灰框上右键应含「删除注释」");
    for item in ["pdf-menu-bookmark", "pdf-menu-insert", "pdf-menu-delete", "pdf-menu-extract"] {
        assert!(cx.debug_bounds(item).is_some(), "右键菜单其余项应保留「{item}」");
    }

    // 点 [删除注释] → 该条待存注释消失、dirty 回落、灰框清、磁盘不变
    let del = cx.debug_bounds("pdf-menu-del-annot").expect("删除注释").center();
    cx.simulate_click(del, gpui::Modifiers::default());
    settle_frames(cx, 3);
    assert!(
        pump_until(cx, |cx| {
            cx.update(|_w, cx| wk.read(cx).pending_annot_count() == 0 && !wk.read(cx).is_dirty())
        }),
        "删除注释后 pending=0 且 dirty 回落"
    );
    assert!(
        cx.update(|_w, cx| wk.read(cx).focused_annot_id()).is_none(),
        "删除后灰框应清除"
    );
    assert_eq!(std::fs::read(&p).expect("重读磁盘"), disk0, "删除注释不落盘：磁盘不变");
    let _ = std::fs::remove_file(&p);
}

/// C3 端到端（待保存）：关闭 dirty PDF 选项卡 → 保存提示三态——「取消」留在当前、`
/// 「保存」落盘后关闭（磁盘变化 / 页数保持）、重新打开加注释后「不保存」直接关（磁盘原样
/// = 不保存即还原）。
#[gpui::test]
async fn c3_pdf_dirty_close_prompt_save_and_discard(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("c3close");
    let disk0 = std::fs::read(&p).expect("读初始磁盘");
    open_pdf(&app, cx, &p);
    let ((page, k), _) = first_char_range_points(&app, cx);
    let add_annot = |app: &Entity<EditorApp>, cx: &mut VisualTestContext| {
        // 重新预热该页文本层：渲染的 LRU 缓存可能把 early page 文本淘汰 → 弹卡锚点算不出
        cx.update(|_w, cx| {
            let pv = {
                let model = app.read(cx).model.borrow();
                let active = model.active_tab;
                tab_pdf(&model.tabs[active]).cloned()
            };
            if let Some(pv) = pv {
                pv.update(cx, |v, _| v.load_text_for_test(page));
            }
        });
        open_sel_card(app, cx, page, k, k + 3);
        let hl = cx.debug_bounds("pdf-sel-card-highlight").expect("高亮项").center();
        cx.simulate_click(hl, gpui::Modifiers::default());
        settle_frames(cx, 3);
        assert!(
            pump_until(cx, |cx| {
                cx.update(|_w, cx| {
                    tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab])
                        .map(|v| v.read(cx).pending_annot_count() == 1)
                        .unwrap_or(false)
                })
            }),
            "应有一条待存注释"
        );
    };
    add_annot(&app, cx);

    // PDF 选项卡下标（open_pdf 会新建 tab，初始空 tab 仍在 → 定位到 Pdf reader 那个）
    let pdf_idx = |app: &Entity<EditorApp>, cx: &mut VisualTestContext| -> usize {
        cx.update(|_w, cx| {
            app.read(cx)
                .model.borrow().tabs
                .iter()
                .position(|t| matches!(&t.reader, MediaReader::Pdf(_)))
                .expect("应有 PDF 选项卡")
        })
    };

    // Part A：关闭 → 弹提示 → 「取消」→ 不关、dirty 保留
    let idx = pdf_idx(&app, cx);
    cx.update(|window, app_ctx| {
                app.update(app_ctx, |a, cx| a.close_tab(idx, window, cx));
            });
    settle_frames(cx, 3);
    assert!(cx.has_pending_prompt(), "dirty PDF 关闭应弹保存提示");
    cx.simulate_prompt_answer("Cancel");
    settle_frames(cx, 3);
    assert!(!cx.has_pending_prompt(), "确认框已应答");
    let (n_tabs, pdf_dirty) = cx.update(|_w, cx| {
        let app_ref = app.read(cx);
        let idx = app_ref
            .model.borrow().tabs
            .iter()
            .position(|t| matches!(&t.reader, MediaReader::Pdf(_)))
            .expect("应有 PDF 选项卡");
        (app_ref.model.borrow().tabs.len(), app_ref.model.borrow().tabs[idx].dirty)
    });
    assert!(n_tabs == 2 && pdf_dirty, "取消后选项卡应保留且 dirty 仍在");

    // Part B：再关 → 「保存」→ 落盘后关闭；磁盘变化（相对原始基线）
    let idx = pdf_idx(&app, cx);
    cx.update(|window, app_ctx| {
                app.update(app_ctx, |a, cx| a.close_tab(idx, window, cx));
            });
    settle_frames(cx, 3);
    assert!(cx.has_pending_prompt(), "再次关闭应弹保存提示");
    cx.simulate_prompt_answer("保存");
    // 关闭「保存」流内部用 timer 轮询（dirty 清）→ 须推进虚拟时钟（advance_clock）让
    // 轮询醒来，同时真实 sleep 给 save 的 worker 落盘时间。
    let mut closed = false;
    for _ in 0..200 {
        cx.executor().advance_clock(std::time::Duration::from_millis(20));
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(8));
        closed = cx.update(|_w, cx| {
            !app.read(cx).model.borrow().tabs.iter().any(|t| matches!(&t.reader, MediaReader::Pdf(_)))
        });
        if closed {
            break;
        }
    }
    assert!(closed, "保存并关闭后 PDF 选项卡应移除");
    assert_ne!(
        std::fs::read(&p).expect("保存后重读"),
        disk0,
        "关闭「保存」应把待存注释落盘"
    );

    // Part C：重开（已含注释）→ 再加一条注释 → 「不保存」直接关 → 磁盘相对 part B 原样
    open_pdf(&app, cx, &p);
    let disk_b = std::fs::read(&p).expect("part C 基线");
    add_annot(&app, cx);
    let idx = pdf_idx(&app, cx);
    cx.update(|window, app_ctx| {
                app.update(app_ctx, |a, cx| a.close_tab(idx, window, cx));
            });
    settle_frames(cx, 3);
    assert!(cx.has_pending_prompt(), "不保存路径也应弹提示");
    cx.simulate_prompt_answer("不保存");
    let closed2 = pump_until(cx, |cx| {
        cx.update(|_w, cx| {
            !app.read(cx).model.borrow().tabs.iter().any(|t| matches!(&t.reader, MediaReader::Pdf(_)))
        })
    });
    assert!(closed2, "「不保存」应直接关闭");
    assert_eq!(
        std::fs::read(&p).expect("重读磁盘"),
        disk_b,
        "「不保存」丢弃待存：磁盘保持原样（不保存即还原）"
    );
    let _ = std::fs::remove_file(&p);
}

/// D1 端到端（评审修复）：另存为（`save_as`）成功后，承载该 PDF 的选项卡 `file_path`
/// 应随 `viewer.path` 重指新路径（tab 栏标题 / 书签不再显示旧文件名），新文件含注释、
/// 原文件保持原样，dirty/pending 清空。
#[gpui::test]
async fn d1_save_as_updates_tab_path_and_writes_annots(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("d1saveas");
    let out = p.with_extension(format!("{}_另存.pdf", p.file_stem().unwrap().to_string_lossy()));
    let _ = std::fs::remove_file(&out);
    open_pdf(&app, cx, &p);

    let wk = cx
        .update(|_w, cx| tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab]).unwrap().clone());
    let ((page, k), _) = first_char_range_points(&app, cx);
    open_sel_card(&app, cx, page, k, k + 3);
    let hl = cx.debug_bounds("pdf-sel-card-highlight").expect("高亮项").center();
    cx.simulate_click(hl, gpui::Modifiers::default());
    settle_frames(cx, 3);
    assert!(
        pump_until(cx, |cx| cx.update(|_w, cx| wk.read(cx).pending_annot_count() == 1)),
        "应有一条待存注释"
    );

    // 另存到新路径 → save_as 异步落盘 + viewer.path 重指向 + observe 同步 tab.file_path
    cx.update(|_w, cx| wk.update(cx, |v, cx| v.save_as(out.clone(), cx)));
    assert!(
        pump_until(cx, |cx| {
            cx.update(|_w, cx| {
                let idx = app
                    .read(cx)
                    .model.borrow().tabs
                    .iter()
                    .position(|t| matches!(&t.reader, MediaReader::Pdf(_)))
                    .unwrap();
                app.read(cx).model.borrow().tabs[idx].file_path.as_deref() == Some(out.as_path())
            })
        }),
        "另存为后选项卡 file_path 应同步为新路径（tab 标题不再显示旧名）"
    );
    assert!(
        pump_until(cx, |cx| {
            cx.update(|_w, cx| {
                !wk.read(cx).is_dirty() && wk.read(cx).pending_annot_count() == 0
            })
        }),
        "另存为后应清 pending/dirty"
    );

    // 新文件已含注释；原文件保持原样（另存不改源）
    let buf = std::fs::read(&out).expect("另存文件应存在");
    let (w2, d2, _) = pdf::PdfWorker::open_and_dims(buf.clone()).expect("重开另存文件");
    let anns = w2.annotations_for_page(d2, page).expect("读回注释");
    assert!(
        anns.iter().any(|a| a.kind == AnnotKind::Highlight),
        "另存文件应含高亮注释"
    );
    w2.close(d2);
    let src = std::fs::read(&p).expect("重读原文件");
    let (w3, d3, _) = pdf::PdfWorker::open_and_dims(src.clone()).expect("重开原文件");
    let src_anns = w3.annotations_for_page(d3, page).expect("读回原文件注释");
    assert!(
        !src_anns.iter().any(|a| a.kind == AnnotKind::Highlight),
        "另存不改写原文件：源文件不应含注释"
    );
    w3.close(d3);
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(&out);
}

// ---------------------------------------------------------------------------
// 图片交互（B2 扩展）：右键命中页内图片 → 「复制图片」；左键选中态由 pdf crate 交互
// 测试覆盖（`image_click_selects_and_clears`）。夹具无图 → SKIP（文本型夹具常态）。
// ---------------------------------------------------------------------------

/// 找第一张**可定位大图**的 `(页, obj, 命中点窗口坐标)`：同步读入前 20 页图片盒，
/// 过滤紧凑噪声盒（≥10pt），滚到该页后取中心窗口坐标（图不在视口会收不到派发）。
/// 夹具无图 → `None`（调用方 SKIP）。
#[allow(clippy::type_complexity)]
fn first_image_hit_point(
    app: &Entity<EditorApp>,
    cx: &mut VisualTestContext,
) -> Option<(usize, usize, gpui::Point<gpui::Pixels>)> {
    let mut target: Option<(usize, usize)> = None;
    cx.update(|_w, cx| {
        let pv = tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab])
            .expect("活动选项卡应挂 PDF")
            .clone();
        for page in 0..20usize {
            pv.update(cx, |v, _| {
                v.load_images_for_test(page);
            });
            let hit = pv.read(cx).image_boxes(page).and_then(|boxes| {
                boxes
                    .iter()
                    .find(|b| b.x1 - b.x0 > 10.0 && b.y1 - b.y0 > 10.0)
                    .map(|b| (page, b.obj_index))
            });
            if hit.is_some() {
                target = hit;
                break;
            }
        }
    });
    let (page, obj) = target?;
    cx.update(|_w, cx| {
        let pv = tab_pdf(&app.read(cx).model.borrow().tabs[app.read(cx).model.borrow().active_tab])
            .expect("活动选项卡应挂 PDF")
            .clone();
        pv.update(cx, |v, cx| {
            v.scroll_to_page(page);
            cx.notify();
        });
    });
    settle_frames(cx, 8);
    cx.update(|window, cx| {
        let model = app.read(cx).model.borrow();
        let active = model.active_tab;
        let pv = tab_pdf(&model.tabs[active])?;
        pv.read(cx).image_pos(window, page, obj)
    })
    .map(|pos| (page, obj, pos))
}

/// 右键**命中页内图片** → 菜单除基四项外追加「复制图片」（`pdf-menu-copy-image`）；
/// 右键**空白/页边距** → 无「复制图片」项、菜单仍含书签项。选中描框态由 pdf crate
/// 交互测试覆盖。夹具无图 → SKIP。
#[gpui::test]
async fn image_right_click_shows_and_hides_copy_image_menu(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("imgmenu");
    open_pdf(&app, cx, &p);
    let Some((_page, _obj, pos)) = first_image_hit_point(&app, cx) else {
        eprintln!("SKIP: pdf测试.pdf 前 20 页无可定位大图（文本型夹具无图）");
        return;
    };
    let mods = gpui::Modifiers::default();

    // 右键在图片上 → 菜单含「复制图片」+ 基项仍在
    cx.simulate_mouse_down(pos, gpui::MouseButton::Right, mods);
    cx.simulate_mouse_up(pos, gpui::MouseButton::Right, mods);
    settle_frames(cx, 3);
    assert!(
        cx.debug_bounds("pdf-menu-copy-image").is_some(),
        "右键命中图片应含「复制图片」"
    );
    assert!(
        cx.debug_bounds("pdf-menu-bookmark").is_some(),
        "命中图片时基项「添加书签」仍在"
    );

    // 右键空白页边距（左上角沟槽）→ 无「复制图片」、书签项仍在
    let blank = gpui::Point::new(px(30.), px(30.));
    cx.simulate_mouse_down(blank, gpui::MouseButton::Right, mods);
    cx.simulate_mouse_up(blank, gpui::MouseButton::Right, mods);
    settle_frames(cx, 3);
    assert!(
        cx.debug_bounds("pdf-menu-copy-image").is_none(),
        "右键非图片不应含「复制图片」"
    );
    assert!(
        cx.debug_bounds("pdf-menu-bookmark").is_some(),
        "右键空白处基项「添加书签」仍在"
    );
}

/// 右键图片 → 点「复制图片」→ 剪贴板 CF_DIB 回读像素与该图 worker 直接取数**逐字节**
/// 一致（端到端：菜单 → pdf 异步取数 → tray 写剪贴板）。剪贴板不可用 → 优雅 SKIP。
/// 夹具无图 → SKIP。
#[cfg(windows)]
#[gpui::test]
async fn image_copy_image_writes_clipboard_dib(cx: &mut TestAppContext) {
    if missing_pdf() {
        eprintln!("SKIP: missing target/pdf测试*.pdf");
        return;
    }
    let _guard = pdf::test_util::lock().lock().unwrap();
    init_test(cx);
    let (app, cx) = add_app(cx);
    cx.simulate_resize(gpui::size(px(1200.), px(800.)));
    for _ in 0..3 {
        cx.run_until_parked();
    }
    let p = temp_pdf_copy("imgcopy");
    open_pdf(&app, cx, &p);
    let Some((page, obj, pos)) = first_image_hit_point(&app, cx) else {
        eprintln!("SKIP: pdf测试.pdf 前 20 页无可定位大图（文本型夹具无图）");
        return;
    };
    let mods = gpui::Modifiers::default();

    // 期望像素：与菜单动作同源（view request_image_pixels → worker ImagePixels），
    // 测试线程同步取（阻塞等结果，真实对比源）。
    let expected = cx.update(|_w, cx| {
        let model = app.read(cx).model.borrow();
        let active = model.active_tab;
        let pv = tab_pdf(&model.tabs[active])?;
        pv.read(cx).request_image_pixels(page, obj)
    });
    let Some(rx) = expected else {
        eprintln!("SKIP: 无法向 worker 请求图片像素（对象失效）");
        return;
    };
    let expect_pix = rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("worker 应返回像素")
        .expect("按自然分辨率取像应成功");
    assert!(
        expect_pix.width > 0 && expect_pix.height > 0,
        "期望像素应正尺寸"
    );

    // 右键图片 → 点「复制图片」
    cx.simulate_mouse_down(pos, gpui::MouseButton::Right, mods);
    cx.simulate_mouse_up(pos, gpui::MouseButton::Right, mods);
    settle_frames(cx, 3);
    let item = cx
        .debug_bounds("pdf-menu-copy-image")
        .expect("右键命中图片应出现「复制图片」")
        .center();
    cx.simulate_mouse_down(item, gpui::MouseButton::Left, mods);
    cx.simulate_mouse_up(item, gpui::MouseButton::Left, mods);

    // 泵到剪贴板 CF_DIB 就绪（异步菜单动作 + worker + tray 写）
    let ok = pump_until(cx, |cx| {
        cx.update(|_w, _| tray::read_clipboard_image_bgra().is_some())
    });
    if !ok {
        eprintln!("SKIP: 剪贴板不可用（无交互会话/被占用），未读到 CF_DIB");
        return;
    }
    let (back, bw, bh) = cx
        .update(|_w, _| tray::read_clipboard_image_bgra())
        .expect("已读到 CF_DIB");
    assert_eq!(
        (bw, bh),
        (expect_pix.width, expect_pix.height),
        "剪贴板 DIB 尺寸应与原图自然分辨率一致"
    );
    assert_eq!(
        back, expect_pix.bgra,
        "剪贴板 CF_DIB 像素应与 worker 直接取数逐字节一致（原图嵌入分辨率）"
    );
}

