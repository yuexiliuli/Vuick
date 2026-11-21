    use super::*;
    use crate::pdf::test_util;
    use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// 页操作集成测试的窗口宿主：整窗渲染一个 PdfViewer（同 view.rs 的 GridHost 姿势）。
    struct PageOpHost {
        pdf: Entity<PdfViewer>,
    }
    impl gpui::Render for PageOpHost {
        fn render(
            &mut self,
            _w: &mut gpui::Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            gpui::div().size_full().child(self.pdf.clone())
        }
    }

    /// 等待 PDF 加载完成（`is_loaded`；pdfium 串行锁守卫约定由调用方先取）。宿主与实体
    /// 已由调用方在 `add_window_view` 内就位，本函数只泵帧到加载完成。
    fn wait_loaded(cx: &mut VisualTestContext, holder: &Rc<RefCell<Option<Entity<PdfViewer>>>>) {
        cx.simulate_resize(gpui::size(px(1500.), px(900.)));
        for _ in 0..3 {
            cx.run_until_parked();
        }
        let mut loaded = false;
        for _ in 0..1200 {
            cx.run_until_parked();
            loaded = cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).is_loaded());
            if loaded {
                break;
            }
        }
        assert!(loaded, "PDF 应在 1200 帧内加载完成");
    }

    /// 「内存级页面操作 + 内存级重载」异步链路的达成谓词：`is_loaded` 在重载期间短暂
    /// 变 false（`open_from_bytes` 先关旧档再异步重开），泵帧直到重新 true 且 `cond` 成立。
    fn wait_until(
        cx: &mut VisualTestContext,
        holder: &Rc<RefCell<Option<Entity<PdfViewer>>>>,
        cond: impl Fn(&PdfViewer) -> bool,
    ) {
        for _ in 0..1200 {
            cx.run_until_parked();
            let ok = cx.update(|_w, cx| {
                let v = holder.borrow().as_ref().unwrap().read(cx);
                v.is_loaded() && cond(v)
            });
            if ok {
                return;
            }
        }
        panic!("等待条件超时（1200 帧）");
    }

    /// 逐页尺寸序列（页面模型看待存工作副本当前页序的客观透镜）。
    fn dims_of(
        cx: &mut VisualTestContext,
        holder: &Rc<RefCell<Option<Entity<PdfViewer>>>>,
    ) -> Vec<(f32, f32)> {
        cx.update(|_w, cx| holder.borrow().as_ref().unwrap().read(cx).dims.clone())
    }

    /// wave8 评审 R6：撤销栈**整档快照条目预算** —— 连续 >[`UNDO_MAX_MEM_ENTRIES`] 个
    /// mem 操作后栈内 `RestoreMemory` 条目数封顶 = 预算、**最旧被淘汰**、最近一条仍可撤
    /// （`pop_back` 取到最新镜像）、非 RestoreMemory 类型条目不被淘汰。直接测预算裁剪纯
    /// 函数（5 个 `RestoreMemory` 入栈点唯一入口 `enforce_undo_mem_budget` 的底层，不依赖
    /// PDF/worker；真实操作链路由既有 mem op/undo 集成测试覆盖）。
    #[test]
    fn undo_mem_budget_caps_restore_memory_entries() {
        let mut stack: VecDeque<UndoEntry> = VecDeque::new();
        // 栈底（最旧）先放一条注释逆操作（非 mem 类型，应被预算保留、不被淘汰）。
        stack.push_back(UndoEntry::RemoveAnnot { id: 99 });
        // 连续 N+2 条 RestoreMemory（bytes 以 i 递增区分新旧）→ 总量 > 预算 → 应淘汰最旧 2 条。
        for i in 0..(UNDO_MAX_MEM_ENTRIES + 2) {
            stack.push_back(UndoEntry::RestoreMemory {
                bytes: vec![i as u8],
                annots_before: Vec::new(),
            });
        }
        enforce_undo_mem_budget(&mut stack);
        // 断言 1：RestoreMemory 条目数 == 预算 N。
        let mem_count = stack
            .iter()
            .filter(|e| matches!(e, UndoEntry::RestoreMemory { .. }))
            .count();
        assert_eq!(
            mem_count, UNDO_MAX_MEM_ENTRIES,
            "RestoreMemory 条目数应封顶 = 预算 {UNDO_MAX_MEM_ENTRIES}"
        );
        // 断言 2：最旧 mem（bytes=0/1）被淘汰 → 栈内最旧留存 mem 的 bytes[0] == 2。
        let oldest_mem = stack.iter().find_map(|e| match e {
            UndoEntry::RestoreMemory { bytes, .. } => Some(bytes[0]),
            _ => None,
        });
        assert_eq!(
            oldest_mem,
            Some(2),
            "最旧 2 条 mem（bytes=0/1）应被淘汰，最旧留存应=2: {oldest_mem:?}"
        );
        // 断言 3：非 RestoreMemory 类型不被预算淘汰（RemoveAnnot 仍在栈内）。
        assert!(
            stack
                .iter()
                .any(|e| matches!(e, UndoEntry::RemoveAnnot { id: 99 })),
            "注释型条目不应被预算淘汰"
        );
        // 断言 4：最近一条仍可撤 —— pop_back 取到最后推的 mem 镜像（bytes = N+1）。
        match stack.pop_back() {
            Some(UndoEntry::RestoreMemory { bytes, .. }) => {
                assert_eq!(
                    bytes[0],
                    (UNDO_MAX_MEM_ENTRIES + 1) as u8,
                    "栈顶应为最新 mem 条目"
                )
            }
            other => panic!("栈顶应是最新 mem 条目，实际 {other:?}"),
        }
    }

    /// wave7a：`reorder_pages`（整体反转）后页面模型反映新页序（页数不变、尺寸序列 = 原逆序）。
    #[gpui::test]
    async fn reorder_pages_reverses_page_model(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let pdf_path = test_util::test_pdf_paths()[1].clone(); // pdf测试2.pdf：长文多页
        let (_root, mut vcx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageOpHost { pdf }
        });
        wait_loaded(&mut vcx, &holder);
        let dims = dims_of(&mut vcx, &holder);
        let n = dims.len();
        assert!(n >= 4, "夹具页数需 ≥4，实际 {n}");
        // 期望 = 原尺寸序列逆序（位置 i 处为原 n-1-i）。
        let expect: Vec<(f32, f32)> = dims.iter().rev().cloned().collect();
        let mut order: Vec<usize> = (0..n).collect();
        order.reverse();
        vcx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.reorder_pages(order, cx);
            });
        });
        wait_until(&mut vcx, &holder, |v| v.dims.len() == n);
        let dims2 = dims_of(&mut vcx, &holder);
        assert_eq!(dims2.len(), n, "重排后页数不变");
        assert_eq!(dims2, expect, "重排后页面模型页尺寸序列应为原逆序");
        assert!(
            vcx.update(|_w, cx| holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .last_modify_error()
                .is_none()),
            "重排成功不应有错误"
        );
    }

    /// wave7a：`batch_delete_pages` 删 [0,1] 后页面模型页数 n-2；`undo_last` 撤销后页数/
    /// 尺寸序列还原（整批一次可撤）。
    #[gpui::test]
    async fn batch_delete_updates_model_and_undo_restores(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let pdf_path = test_util::test_pdf_paths()[1].clone();
        let (_root, mut vcx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageOpHost { pdf }
        });
        wait_loaded(&mut vcx, &holder);
        let dims = dims_of(&mut vcx, &holder);
        let n = dims.len();
        assert!(n >= 4, "夹具页数需 ≥4，实际 {n}");
        // 删最前 2 页 → 期望新序列 = 原 [2..n]。
        let expect_after: Vec<(f32, f32)> = dims[2..].to_vec();
        vcx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.batch_delete_pages(vec![0, 1], cx);
            });
        });
        wait_until(&mut vcx, &holder, |v| v.page_count() == n - 2);
        let dims2 = dims_of(&mut vcx, &holder);
        assert_eq!(dims2, expect_after, "批删 [0,1] 后页面模型 = 原 [2..]");
        // 撤销（Ctrl+Z 语义的字典链路）→ 页数/尺寸序列还原。
        vcx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.undo_last(cx);
            });
        });
        wait_until(&mut vcx, &holder, |v| v.page_count() == n);
        let dims3 = dims_of(&mut vcx, &holder);
        assert_eq!(dims3, dims, "批删撤销后页面模型应还原为删前");
        assert_eq!(dims3.len(), n, "撤销后页数还原");
    }

    /// wave7a：`batch_delete_pages` 越界页 → `last_modify_error` 置位、页面模型不变。
    #[gpui::test]
    async fn batch_delete_rejects_out_of_range_in_view(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let pdf_path = test_util::test_pdf_paths()[1].clone();
        let (_root, mut vcx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageOpHost { pdf }
        });
        wait_loaded(&mut vcx, &holder);
        let n = dims_of(&mut vcx, &holder).len();
        vcx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.batch_delete_pages(vec![0, n], cx); // n 越界
            });
        });
        for _ in 0..5 {
            vcx.run_until_parked();
        }
        // 页面模型不变（越界被预先拒，不起异步加载）。
        assert_eq!(dims_of(&mut vcx, &holder).len(), n, "越界批删后页数不变");
        let err = vcx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .last_modify_error()
                .map(String::from)
        });
        assert!(err.is_some(), "越界批删应置 last_modify_error");
    }

    /// wave7a：`reorder_pages` 长度不匹配 → `last_modify_error` 置位、页面模型不变。
    #[gpui::test]
    async fn reorder_rejects_length_mismatch_in_view(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let pdf_path = test_util::test_pdf_paths()[1].clone();
        let (_root, mut vcx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageOpHost { pdf }
        });
        wait_loaded(&mut vcx, &holder);
        let n = dims_of(&mut vcx, &holder).len();
        let short: Vec<usize> = (0..n.saturating_sub(1)).collect();
        vcx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.reorder_pages(short, cx);
            });
        });
        for _ in 0..5 {
            vcx.run_until_parked();
        }
        assert_eq!(
            dims_of(&mut vcx, &holder).len(),
            n,
            "长度不匹配重排后页数不变"
        );
        let has_err = vcx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .last_modify_error()
                .is_some()
        });
        assert!(has_err, "长度不匹配重排应置 last_modify_error");
    }

    // ---- 页面网格视图 wave7b：提取多页（副本）/ 拆分区间 / 合并另一 PDF ----
    //
    // 夹具实测：pdf测试.pdf = 18 页（横向 792×612，本批改写源）、pdf测试2.pdf = 259 页
    // （纵向，本批当前文档）。期望序列均为手工推导（K177/K143）。

    /// wave7b 临时输出路径（pid + 纳秒戳唯一，落系统 temp，勿污染仓库）。
    fn wave7b_out(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "pdf-view-wave7b-{name}-{}-{nanos}.pdf",
            std::process::id()
        ))
    }

    /// wave7b：`export_pages_to`（副本语义）成功后当前模型页数/尺寸不变，且成功信号
    /// （`last_extract_success`）置位供壳层 toast。
    #[gpui::test]
    async fn export_pages_keeps_model_and_reports_success(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let pdf_path = test_util::test_pdf_paths()[1].clone();
        let (_root, mut vcx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageOpHost { pdf }
        });
        wait_loaded(&mut vcx, &holder);
        let dims = dims_of(&mut vcx, &holder);
        let n = dims.len();
        assert!(n >= 3, "夹具页数需 ≥3，实际 {n}");
        let out = wave7b_out("export");
        vcx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.export_pages_to(vec![0, 2], out.clone(), cx);
            });
        });
        wait_until(&mut vcx, &holder, |v| v.last_extract_success.is_some());
        // 副本语义：模型页数/尺寸不变、无错误。
        assert_eq!(
            dims_of(&mut vcx, &holder),
            dims,
            "导出后当前模型不变（副本语义）"
        );
        assert!(
            vcx.update(|_w, cx| holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .last_modify_error()
                .is_none()),
            "导出成功不应有错误"
        );
        // 成功信号可被 `take` 一次消费。
        let msg = vcx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .update(cx, |v, _| v.take_last_extract_success())
        });
        assert!(
            msg.is_some() && msg.unwrap().contains("已导出"),
            "应弹成功 toast 文案"
        );
    }

    /// wave7b：`split_pages_to`（1,2）移除区间后模型页数 n-2、尺寸 = 源删 {1,2} 幸存序；
    /// `undo_last` 撤销后回 N 页（区间移除部分回位）。
    #[gpui::test]
    async fn split_pages_updates_model_and_undo_restores(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let pdf_path = test_util::test_pdf_paths()[1].clone();
        let (_root, mut vcx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageOpHost { pdf }
        });
        wait_loaded(&mut vcx, &holder);
        let dims = dims_of(&mut vcx, &holder);
        let n = dims.len();
        assert!(n >= 4, "夹具页数需 ≥4，实际 {n}");
        // 期望（算式）：拆分区间 (1,2) 长 2 → 幸存序 = 原 [0, 3..]。
        let expect_after: Vec<(f32, f32)> = dims
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 1 && *i != 2)
            .map(|(_, d)| *d)
            .collect();
        let out = wave7b_out("split");
        vcx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.split_pages_to((1, 2), out.clone(), cx);
            });
        });
        wait_until(&mut vcx, &holder, |v| v.page_count() == n - 2);
        let dims2 = dims_of(&mut vcx, &holder);
        assert_eq!(
            dims2, expect_after,
            "拆分 (1,2) 后模型 = 原删 {{1,2}} 幸存序"
        );
        // 拆分写文件 → 成功 toast 信号置位。
        assert!(
            vcx.update(|_w, cx| holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .last_extract_success
                .is_some()),
            "拆分成功应置成功信号"
        );
        // 撤销（Ctrl+Z 语义）→ 回 N 页、尺寸还原。
        vcx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.undo_last(cx);
            });
        });
        wait_until(&mut vcx, &holder, |v| v.page_count() == n);
        let dims3 = dims_of(&mut vcx, &holder);
        assert_eq!(dims3, dims, "拆分撤销后模型应还原");
    }

    /// wave7b：`merge_pdf_file` 追加后模型页数 n+m、撤销后回 N 页（整合并一次可撤）。
    #[gpui::test]
    async fn merge_pdf_file_updates_model_and_undo_restores(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let pdf_path = test_util::test_pdf_paths()[1].clone(); // 当前 = pdf测试2.pdf (259 页)
        let src_path = test_util::test_pdf_paths()[0].clone(); // 合并源 = pdf测试.pdf (18 页)
        let (_root, mut vcx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageOpHost { pdf }
        });
        wait_loaded(&mut vcx, &holder);
        let dims = dims_of(&mut vcx, &holder);
        let n = dims.len();
        assert!(n >= 1, "当前文档页数需 ≥1，实际 {n}");
        // 合并源页数（打开一次复用计数，pdfium 串行锁已持）。
        let m = {
            let src_bytes = std::fs::read(&src_path).expect("读取合并源应成功");
            let (_, _, src_dims) = PdfWorker::open_and_dims(src_bytes).expect("合并源应可打开");
            let m = src_dims.len();
            assert!(m >= 1, "合并源页数需 ≥1，实际 {m}");
            m
        };
        vcx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.merge_pdf_file(src_path, cx);
            });
        });
        // 期望（算式）：合并后总页数 = n + m。
        wait_until(&mut vcx, &holder, |v| v.page_count() == n + m);
        assert!(
            vcx.update(|_w, cx| holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .last_modify_error()
                .is_none()),
            "合并成功不应有错误"
        );
        // 撤销 → 回 N 页（尺寸序列还原）。
        vcx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.undo_last(cx);
            });
        });
        wait_until(&mut vcx, &holder, |v| v.page_count() == n);
        let dims2 = dims_of(&mut vcx, &holder);
        assert_eq!(dims2, dims, "合并撤销后模型应还原");
    }

    /// wave7b：`split_pages_to` 越界区间（终点 >= 页数）→ `last_modify_error` 置位、页面模型不变。
    #[gpui::test]
    async fn split_pages_rejects_out_of_range_in_view(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let pdf_path = test_util::test_pdf_paths()[1].clone();
        let (_root, mut vcx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageOpHost { pdf }
        });
        wait_loaded(&mut vcx, &holder);
        let n = dims_of(&mut vcx, &holder).len();
        let out = wave7b_out("split-invalid");
        vcx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.split_pages_to((0, n), out.clone(), cx); // 终点 n 越界
            });
        });
        for _ in 0..5 {
            vcx.run_until_parked();
        }
        assert_eq!(dims_of(&mut vcx, &holder).len(), n, "越界拆分后页数不变");
        let err = vcx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .last_modify_error()
                .map(String::from)
        });
        assert!(err.is_some(), "越界拆分应置 last_modify_error");
    }

    /// wave7b：`merge_pdf_file` src 不存在 → `last_modify_error` 置位、页面模型不变。
    #[gpui::test]
    async fn merge_pdf_file_rejects_bad_source_in_view(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let pdf_path = test_util::test_pdf_paths()[1].clone();
        let (_root, mut vcx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageOpHost { pdf }
        });
        wait_loaded(&mut vcx, &holder);
        let n = dims_of(&mut vcx, &holder).len();
        let missing = wave7b_out("merge-missing");
        vcx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.merge_pdf_file(missing, cx);
            });
        });
        wait_until(&mut vcx, &holder, |v| v.last_modify_error().is_some());
        assert_eq!(
            dims_of(&mut vcx, &holder).len(),
            n,
            "src 不存在合并后页数不变"
        );
    }

    /// wave8 评审 R7：`merge_pdf_file` 读端**真边界** —— 合并源超过
    /// [`PDF_MERGE_MAX_SRC_BYTES`]（稀疏文件，只 `set_len` 拉长不占实际磁盘）→ 读端元数据
    /// 预检快拒 → `last_modify_error` 报「合并源过大」、「当前文档页数不变」（绝不把超限
    /// 字节送进 pdfium worker）。校验的是读端有界拦截的真实执行，非桩。
    #[gpui::test]
    async fn merge_pdf_file_rejects_oversized_source(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let pdf_path = test_util::test_pdf_paths()[1].clone();
        let (_root, mut vcx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageOpHost { pdf }
        });
        wait_loaded(&mut vcx, &holder);
        let n = dims_of(&mut vcx, &holder).len();
        // 稀疏超限源：文件长度 = 上限 + 1（NTFS 惰性分配，不占实际磁盘、秒建）；读端预检
        // 按元数据长度快拒，不会真正读 512MB。
        let dir = std::env::temp_dir().join("vuick-pdf-tests");
        let _ = std::fs::create_dir_all(&dir);
        let oversize = dir.join(format!("merge-oversize-{}.pdf", std::process::id()));
        let _ = std::fs::remove_file(&oversize);
        {
            let f = std::fs::File::create(&oversize).expect("创建超限源应成功");
            f.set_len(PDF_MERGE_MAX_SRC_BYTES + 1)
                .expect("拉长超限源应成功");
        }
        let ov2 = oversize.clone();
        vcx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.merge_pdf_file(ov2, cx);
            });
        });
        wait_until(&mut vcx, &holder, |v| v.last_modify_error().is_some());
        let msg = vcx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .last_modify_error()
                .map(String::from)
        });
        assert!(
            msg.as_deref().is_some_and(|m| m.contains("合并源过大")),
            "超限源应报「合并源过大」：{msg:?}"
        );
        assert_eq!(dims_of(&mut vcx, &holder).len(), n, "超限合并后页数不变");
        let _ = std::fs::remove_file(&oversize);
    }

    /// wave8 评审 P1：导出到**当前打开的文档路径** → 预检**同步**置错（Err）且盘上源
    /// 文件**未被覆盖**（字节不变）。滥用场景：导出/拆分把盘上源文件原子替换成子集，
    /// 而内存镜像未变、dirty 不置 → 原文件丢失且无撤销保护；故预检在菜单决策点拦截。
    /// 用可写临时副本跑（源夹具可能带 ReadOnly），断言「Err 置位 + 盘上字节原样」真实
    /// 执行（非 SKIP、非假绿）。
    #[gpui::test]
    async fn export_to_current_doc_path_rejected_and_disk_untouched(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let dir = std::env::temp_dir().join("vuick-pdf-tests");
        let _ = std::fs::create_dir_all(&dir);
        let cur = dir.join(format!("p1-self-{}.pdf", std::process::id()));
        let _ = std::fs::remove_file(&cur);
        // 可写重建（fs::copy 会继承源只读属性 → 用 read+write，K47）。
        test_util::copy_fixture_writable(&test_util::test_pdf_paths()[0], &cur)
            .expect("工作副本应可写");
        let disk0 = std::fs::read(&cur).expect("读副本应成功");
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let cur2 = cur.clone();
        let (_root, mut vcx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(cur2, cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageOpHost { pdf }
        });
        wait_loaded(&mut vcx, &holder);
        let n = dims_of(&mut vcx, &holder).len();
        // out == 当前文档路径 → 预检同步置错；不起异步、不重编码。
        vcx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.export_pages_to(vec![0, 1], cur.clone(), cx);
            });
        });
        for _ in 0..5 {
            vcx.run_until_parked();
        }
        let err = vcx.update(|_w, cx| {
            holder
                .borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .last_modify_error()
                .map(String::from)
        });
        assert!(err.is_some(), "导出到当前文档路径应被预检拒绝");
        let msg = err.unwrap();
        assert!(
            msg.contains("不能覆盖当前打开的文档"),
            "错误文案应命中 P1 防护：{msg}"
        );
        assert_eq!(
            dims_of(&mut vcx, &holder).len(),
            n,
            "预检拦截不改变模型页数"
        );
        // 盘上源文件未被覆盖（字节原样）——被拒绝的导出绝不能原子替换成子集。
        let disk1 = std::fs::read(&cur).expect("重读副本应成功");
        assert_eq!(disk1, disk0, "导出被拒后盘上源文件不得被覆盖");
        let _ = std::fs::remove_file(&cur);
    }

    /// wave8 评审 P2-A2：`reorder_pages` 传恒等序（0..n）→ 早退 no-op——不提交 worker
    /// （不整档重编码）、不置脏（pending_page_ops 不变）、不入撤销快照（undo_stack 空）、
    /// 页面序不变、不置错。
    #[gpui::test]
    async fn reorder_identity_is_noop_without_dirty(cx: &mut TestAppContext) {
        if test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _pdf_guard = test_util::lock().lock().unwrap();
        let holder: Rc<RefCell<Option<Entity<PdfViewer>>>> = Rc::default();
        let pdf_path = test_util::test_pdf_paths()[1].clone();
        let (_root, mut vcx) = cx.add_window_view(|_window, cx| {
            let pdf = cx.new(|cx| PdfViewer::new(pdf_path.clone(), cx));
            *holder.borrow_mut() = Some(pdf.clone());
            PageOpHost { pdf }
        });
        wait_loaded(&mut vcx, &holder);
        let dims = dims_of(&mut vcx, &holder);
        let n = dims.len();
        let (ops0, undo0) = vcx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (v.pending_page_ops, v.undo_stack.len())
        });
        let identity: Vec<usize> = (0..n).collect();
        vcx.update(|_w, cx| {
            holder.borrow().as_ref().unwrap().update(cx, |v, cx| {
                v.reorder_pages(identity, cx);
            });
        });
        // identity 早退是同步 no-op（不 spawn/不 reload）→ 泵若干帧后状态应原样。
        for _ in 0..10 {
            vcx.run_until_parked();
        }
        let (ops1, undo1, dims1, err) = vcx.update(|_w, cx| {
            let v = holder.borrow().as_ref().unwrap().read(cx);
            (
                v.pending_page_ops,
                v.undo_stack.len(),
                v.dims.clone(),
                v.last_modify_error().is_some(),
            )
        });
        assert_eq!(ops1, ops0, "identity 重排不置脏（pending_page_ops 不变）");
        assert_eq!(undo1, undo0, "identity 重排不入撤销快照");
        assert_eq!(dims1, dims, "identity 重排页面序不变");
        assert!(!err, "identity 重排不应置错");
    }
