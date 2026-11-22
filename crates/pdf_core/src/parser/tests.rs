    use super::*;

    /// R1/R4（纯逻辑，不触 pdfium）：错误映射按 io kind 区分——PermissionDenied →
    /// 可操作中文 + 注明不破坏原文件；其余 io 错误保原文案、前缀「写回 PDF 失败：」。
    #[test]
    fn write_io_error_maps_busy_vs_other() {
        let busy = map_write_io_error(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert!(is_writeback_busy_error(&busy), "被占用/只读应命中 busy 文案: {busy}");
        assert!(
            busy.contains("PDF 正被其他程序打开或文件只读；请关闭占用它的程序后重试")
                && busy.contains("原文件未受影响"),
            "busy 文案应含指引与不破坏原文件说明: {busy}"
        );
        let other = map_write_io_error(std::io::Error::new(std::io::ErrorKind::Other, "磁盘空间不足"));
        assert!(!is_writeback_busy_error(&other), "非占用错误不应命中 busy: {other}");
        assert!(
            other.starts_with("写回 PDF 失败：") && other.contains("磁盘空间不足"),
            "其余 io 错误应保原文案加前缀: {other}"
        );
        // is_access_denied 的 Windows 裸码分支：ERROR_ACCESS_DENIED = 5
        assert!(is_access_denied(&std::io::Error::from_raw_os_error(5)));
        assert!(!is_access_denied(&std::io::Error::from_raw_os_error(112)));
    }

    /// R2/R4：rename 成功分支与「重试耗尽最终失败」分支（目标为已存在目录 → rename 必败）。
    #[test]
    fn rename_with_retry_succeeds_or_gives_up() {
        let dir = std::env::temp_dir().join(format!(
            "pdf-rename-retry-{}-{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录应成功");
        // 成功分支：tmp → target（同目录），rename 应移走 tmp 并落到 target
        let tmp_ok = dir.join("src.pdf");
        let tgt_ok = dir.join("dst.pdf");
        std::fs::write(&tmp_ok, b"hello").expect("写 tmp 应成功");
        rename_with_retry(&tmp_ok, &tgt_ok).expect("正常 rename 应成功");
        assert!(!tmp_ok.exists() && tgt_ok.is_file(), "成功应移走 tmp 并生成 target");
        // 最终失败分支：target 为已存在目录 → 任何平台 rename 都必败（重试耗尽后上抛）
        let tmp_fail = dir.join("src2.pdf");
        std::fs::write(&tmp_fail, b"hello").expect("写 tmp 应成功");
        let tgt_dir = dir.join("existing_dir");
        std::fs::create_dir_all(&tgt_dir).expect("建目录应成功");
        let r = rename_with_retry(&tmp_fail, &tgt_dir);
        assert!(r.is_err(), "覆盖已存在目录应失败");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R0a/R4：rename 失败时临时文件必须被清理（不留 `.pid.seq.tmp.pdf` 点文件），
    /// 且错误上抛为带语义的 anyhow（非裸 io Display「拒绝访问」）。
    #[test]
    fn commit_rename_failure_cleans_tmp() {
        let dir = std::env::temp_dir().join(format!(
            "pdf-commit-clean-{}-{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录应成功");
        let tmp = dir.join(".pid.seq.tmp.pdf");
        std::fs::write(&tmp, b"hello").expect("写 tmp 应成功");
        let tgt_dir = dir.join("existing_dir");
        std::fs::create_dir_all(&tgt_dir).expect("建目录应成功");
        let err = commit_rename(&tmp, &tgt_dir).expect_err("覆盖已存在目录应失败");
        assert!(!tmp.exists(), "rename 失败后临时文件应被清理: {}", tmp.display());
        let msg = format!("{err:#}");
        assert!(!msg.is_empty(), "错误文案不应为空");
        assert!(
            msg.contains("写回 PDF 失败：") || is_writeback_busy_error(&msg),
            "错误文案应带写回语义前缀或占用指引: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 磁盘候选路径顺序（纯函数，不触文件系统）：分发布局 `bin\pdf\` 先于 exe 旁置、
    /// 再 resources —— 与 `locate_dll` 优先级一致（env 之外无内嵌兜底，2026-11-22 去内嵌）。
    #[test]
    fn pdfium_disk_candidates_bin_pdf_then_exe_then_resources() {
        let dir = std::path::Path::new("C:/app/dist");
        let cands = disk_candidates(Some(dir), "C:/repo/crates/pdf_core");
        assert_eq!(
            cands[0],
            dir.join("bin").join("pdf").join(PDFIUM_VERSIONED_NAME),
            "dist 布局 bin\\pdf\\ 应排第一"
        );
        assert_eq!(cands[1], dir.join(PDFIUM_VERSIONED_NAME), "exe 旁置应排第二");
        assert!(
            cands[2]
                .parent()
                .map(|p| p.ends_with("resources/pdfium"))
                .unwrap_or(false),
            "resources 候选应最后：{}",
            cands[2].display()
        );
        // exe_dir 缺失（如测试进程无 current_exe）→ 只留 resources 候选
        let no_exe = disk_candidates(None, "C:/repo/crates/pdf_core");
        assert_eq!(no_exe.len(), 1, "无 exe_dir 时只剩 resources 候选");
    }

    fn target_pdf_paths() -> Vec<std::path::PathBuf> {
        // P4：本 crate 位于 crates/pdf，夹具在工作区根 target/（上溯两级）。
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target");
        // 本机 Rust 进程内 pdfium 无法解析 fixtures/ 下的 PDF（FormatError），只用 target/ 下
        // 生产路径可解析的文件：0=基础文本型、1=长文（≥50 页）。
        vec![
            manifest.join("pdf测试.pdf"),
            manifest.join("pdf测试2.pdf"),
        ]
    }

    /// 文本提取冒烟：真实 PDF 首页能取出非空文本层，且每个字符盒有限、落在页内。
    #[test]
    fn extracts_page_text_from_real_pdf() {
        if crate::pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let p = target_pdf_paths()[0].clone();
        assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, _dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let rx = worker.request_text(doc_id, 0);
        let text = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("应收到文本结果")
            .expect("文本提取应成功");
        assert!(
            text.chars.len() >= 10,
            "内容页应有足量文本字符，实际 {}",
            text.chars.len()
        );
        assert!(text.width > 0.0 && text.height > 0.0, "页面尺寸应为正");
        assert!(
            text.synthetic_count <= text.chars.len(),
            "degenerate 字符数不应超过总数"
        );
        for c in &text.chars {
            let (l, r, b, t) = (c.left, c.right, c.bottom, c.top);
            for v in [l, r, b, t] {
                assert!(v.is_finite(), "盒字段应有限: {c:?}");
            }
            assert!(
                l >= 0.0 - 1e-3 && r <= text.width + 1e-3,
                "X 应在页内 [0,{}]: {c:?}",
                text.width
            );
            assert!(
                b >= 0.0 - 1e-3 && t <= text.height + 1e-3,
                "Y 应在页内 [0,{}]: {c:?}",
                text.height
            );
            if !c.synthetic {
                assert!(r >= l && t >= b, "有效字符盒应非负尺寸: {c:?}");
            }
        }
        // 抽取应稳定：同一页重复抽取，字符序列一致
        let rx2 = worker.request_text(doc_id, 0);
        let text2 = rx2
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("应收到第二次结果")
            .expect("第二次抽取应成功");
        assert_eq!(text.chars, text2.chars, "同页重复抽取字符应一致");
    }

    /// 未打开的文档请求文本 → None（区别于“页面无文本”的 Some(空)）。
    #[test]
    fn text_request_for_unknown_doc_returns_none() {
        if crate::pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let p = target_pdf_paths()[0].clone();
        assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, _doc_id, _dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let rx = worker.request_text(u64::MAX, 0); // 从没打开过的 doc
        let res = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("应收到应答")
            ;
        assert!(res.is_none(), "未知文档的文本请求应返回 None");
    }

    /// 检索子串匹配（纯逻辑，不触 pdfium）：大小写折叠、跳过 NUL、非重叠。
    #[test]
    fn search_page_text_matches_casefold_skips_nul_non_overlap() {
        use super::{search_page_text, PageText, PdfTextChar};
        // 可见序列 = "ab" + NUL 跳过 + "ABc" → "abABc"
        let mut chars = Vec::new();
        for (i, ch) in ['a', 'b', '\0', 'A', 'B', 'c'].into_iter().enumerate() {
            chars.push(PdfTextChar {
                ch,
                left: i as f32,
                bottom: 0.0,
                right: i as f32 + 1.0,
                top: 1.0,
                synthetic: ch == '\0',
                origin_x: i as f32,
                origin_y: 0.0,
            });
        }
        let text = PageText {
            page_index: 0,
            width: 100.0,
            height: 100.0,
            chars,
            synthetic_count: 1,
        };
        let ndl = |s: &str| s.chars().collect::<Vec<char>>();
        // 可见序列 "abABc"：大小写折叠下 ab/AB/aB 都在 0..2 与 3..5 两处命中
        assert_eq!(
            search_page_text(&text, &ndl("ab")),
            vec![(0, 2), (3, 5)],
            "ab 折叠后命中可见序列两处"
        );
        assert_eq!(
            search_page_text(&text, &ndl("AB")),
            vec![(0, 2), (3, 5)],
            "AB 折叠后与 ab 同命中"
        );
        assert_eq!(
            search_page_text(&text, &ndl("aB")),
            vec![(0, 2), (3, 5)],
            "aB 混合大小写折叠命中"
        );
        assert_eq!(search_page_text(&text, &ndl("c")), vec![(5, 6)], "c 命中可见序列末");
        // NUL 被跳过 → 可见序列连续：'abc'(折叠) 命中可见 'ABc'(3..6)；'bA' 跨过 NUL 连续命中
        assert_eq!(
            search_page_text(&text, &ndl("abc")),
            vec![(3, 6)],
            "跳过 NUL 后可见序列连续，abc(折叠) 命中 ABc"
        );
        assert_eq!(
            search_page_text(&text, &ndl("bA")),
            vec![(1, 4)],
            "bA 跨过 NUL 仍为可见序列连续"
        );
        // 非重叠：连续重复只取不重叠段
        let rep = PageText {
            page_index: 0,
            width: 100.0,
            height: 100.0,
            chars: ['a', 'a', 'a', 'a']
                .into_iter()
                .enumerate()
                .map(|(i, ch)| PdfTextChar {
                    ch,
                    left: i as f32,
                    bottom: 0.0,
                    right: i as f32 + 1.0,
                    top: 1.0,
                    synthetic: false,
                    origin_x: i as f32,
                    origin_y: 0.0,
                })
                .collect(),
            synthetic_count: 0,
        };
        assert_eq!(search_page_text(&rep, &ndl("aa")), vec![(0, 2), (2, 4)], "非重叠命中");
        // 空串 / 长于文本 → 空
        assert_eq!(search_page_text(&text, &ndl("")), Vec::<(usize, usize)>::new());
        assert_eq!(search_page_text(&text, &ndl("abABcx")), Vec::<(usize, usize)>::new());
    }

    /// 纯逻辑（不触 pdfium）：字符盒回退策略 tight > loose > synthetic。
    #[test]
    fn resolve_char_box_prefers_tight_then_loose_then_synthetic() {
        use super::{CharBox, resolve_char_box};
        let tight = Some(CharBox {
            left: 10.0,
            bottom: 20.0,
            right: 12.0,
            top: 22.0,
        });
        let (b, syn) = resolve_char_box(tight, Some(CharBox::ZERO));
        assert!(!syn && b.left == 10.0 && b.top == 22.0, "tight 正常应直接采用");

        // tight 退化（0 宽 0 高）→ 回退到可用的 loose
        let (b2, syn2) = resolve_char_box(Some(CharBox::ZERO), Some(CharBox {
            left: 1.0,
            bottom: 2.0,
            right: 4.0,
            top: 5.0,
        }));
        assert!(!syn2 && b2.right - b2.left > 0.0, "退化的 tight 应回退 loose");

        // 全部退化 → 标记 synthetic（保留盒内容便于复制）
        let (b3, syn3) = resolve_char_box(Some(CharBox::ZERO), Some(CharBox::ZERO));
        assert!(syn3 && b3 == CharBox::ZERO, "全退化应标记 synthetic");

        // 全部缺失 → synthetic + ZERO
        let (b4, syn4) = resolve_char_box(None, None);
        assert!(syn4 && b4 == CharBox::ZERO, "双 None 应 synthetic + ZERO");

        // 负尺寸（引擎异常返回）→ 视为退化，不回退已坏的 tight
        let neg = Some(CharBox {
            left: 10.0,
            bottom: 20.0,
            right: 8.0,
            top: 22.0,
        });
        let (b5, syn5) = resolve_char_box(neg, None);
        assert!(syn5, "负宽 tight 应标记 synthetic");
        assert!(b5 == CharBox::ZERO, "无可用回退时应为 ZERO");
    }

    /// 文本提取单页端到端延迟摸底（Release 目标 ~8ms/页）。打印，不构成断言。
    #[test]
    #[ignore]
    fn bench_page_text_extraction() {
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let p = target_pdf_paths()[1].clone();
        assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let n = dims.len().min(8);
        let mut ms: Vec<u64> = Vec::new();
        for idx in 0..n {
            let t0 = std::time::Instant::now();
            let rx = worker.request_text(doc_id, idx);
            let res = rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("应收到文本结果");
            let m = t0.elapsed().as_millis() as u64;
            ms.push(m);
            if let Some(t) = res {
                eprintln!(
                    "page {idx}: {m}ms, {} chars (synthetic {})",
                    t.chars.len(),
                    t.synthetic_count
                );
            }
        }
        let avg = ms.iter().sum::<u64>() / n as u64;
        eprintln!("BENCH-TEXT: 单页文本提取均值 {avg}ms; 明细 {ms:?}");
    }

    /// 切档不重栅（worker 侧纯逻辑，零 pdfium）：`recolored_entries` 对已缓存**未着色
    /// 原图**按新参数现算显示副本——夜间档把纯黑墨迹提亮为墨色、纯白纸面染成纸色，
    /// 原图 `bgra` 保持未着色单真值不被触碰；`None`（默认档）直通 = 原图副本原样。
    #[test]
    fn recolor_refresh_recolors_cached_raw_and_keeps_raw() {
        // 一份「单瓦片」缓存原图：左像素纯黑（文字墨迹）、右像素纯白（纸面），A=255。
        let raw: Vec<u8> = vec![0, 0, 0, 255, 255, 255, 255, 255];
        let cache = vec![RawEntry {
            kind: RawKind::Tile,
            page: 0,
            row: 0,
            col: 0,
            x0: 0,
            y0: 0,
            w: 2,
            h: 1,
            full_w: 16,
            full_h: 16,
            scale: 1.0,
            bgra: raw.clone(),
        }];
        let night = RecolorParams {
            bg: [0x20, 0x20, 0x20],
            ink: Some([0xD8, 0xD8, 0xD8]),
        };
        let entries = recolored_entries(&[&cache[0]], Some(night), false);
        assert_eq!(entries.len(), 1, "每条缓存原图应回传一条重着色副本");
        let e = &entries[0];
        assert_eq!(e.kind, RawKind::Tile);
        assert_eq!(e.page, 0);
        assert_eq!((e.row, e.col), (0, 0));
        assert_eq!(e.bgra[0..4], [216, 216, 216, 255], "纯黑墨迹应提亮为墨色 0xD8");
        assert_eq!(e.bgra[4..8], [32, 32, 32, 255], "纯白纸面应染成纸色 0x20");
        // 原图（单真值）不被触碰：缓存 raw 仍是未着色原样。
        assert_eq!(cache[0].bgra, raw, "缓存原图应保持未着色单真值");

        // 默认档（None）直通：副本 = 原图原样（清回默认不重栅、不改像素）。
        let passthrough = recolored_entries(&[&cache[0]], None, false);
        assert_eq!(passthrough[0].bgra, raw, "None 直通应原样回传原图副本");
        assert_eq!(cache[0].bgra, raw, "直通路径同样不触碰缓存原图");
    }

    /// 切档不重栅（worker 侧纯逻辑）：`cache_raw` 同密度同尺寸复用既有原图（不重复占内存）、
    /// 异密度替换旧条目；`finalize_render` 的「已缓存同密度 → 直用本轮 raw 作出口、不重复
    /// 写入缓存」分支保持缓存条目数不变。预算超支时先入先出回收最旧（传极小预算直接验证）。
    #[test]
    fn raw_cache_reuses_same_density_and_budgets_fifo() {
        let mut cache: Vec<RawEntry> = Vec::new();
        let entry = |scale: f32, r: u8, row: u32, col: u32| RawEntry {
            kind: RawKind::Tile,
            page: 0,
            row,
            col,
            x0: 0,
            y0: 0,
            w: 1,
            h: 1,
            full_w: 16,
            full_h: 16,
            scale,
            bgra: vec![r, r, r, 255],
        };
        // 首次写入 → 真插入。
        assert!(cache_raw(&mut cache, entry(1.0, 9, 0, 0), 1 << 20));
        assert_eq!(cache.len(), 1);
        // 同密度同尺寸 → 复用既有、不重复插入、不替换（False 分支）。
        assert!(!cache_raw(&mut cache, entry(1.0, 1, 0, 0), 1 << 20));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache[0].bgra, vec![9, 9, 9, 255], "复用应保留既有原图");
        // 异密度 → 替换旧条目（缓存仍一格）。
        assert!(cache_raw(&mut cache, entry(2.0, 2, 0, 0), 1 << 20));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache[0].scale, 2.0);

        // finalize_render：已缓存同密度直用本轮 raw 作出口、不再写入缓存（条目数不变）。
        let out = finalize_render(
            &mut cache,
            RawKind::Tile,
            0,
            0,
            0,
            0,
            0,
            1,
            1,
            16,
            16,
             2.0,
            vec![7, 7, 7, 255],
            Some(RecolorParams { bg: [0x20, 0x20, 0x20], ink: None }),
            false,
            false,
        );
        assert_eq!(cache.len(), 1, "同密度复用不应新增条目");
        let _ = out;

        // 预算收敛（FIFO 先入先出）：总预算 = 12 字节（= 3 条 × 4 字节）→ 第 4 条写入即
        // 超支，回收最旧（队首 = 首个写入），缓存保留最近 3 条。
        let mut tiny: Vec<RawEntry> = Vec::new();
        let budget = 12u64;
        let keys = [(0u32, 1u32), (1u32, 1u32), (1u32, 2u32), (0u32, 2u32)];
        for (i, &(row, col)) in keys.iter().enumerate() {
            cache_raw(&mut tiny, entry(1.0, 10 + i as u8, row, col), budget);
        }
        assert_eq!(tiny.len(), 3, "超预算应变回 3 条（挤出最旧）");
        assert!(
            !tiny.iter().any(|e| e.row == 0 && e.col == 1 && e.bgra[0] == 10),
            "最旧（首个写入 10）应被 FIFO 挤出"
        );
        assert!(
            tiny.iter().any(|e| e.row == 0 && e.col == 2 && e.bgra[0] == 13),
            "最新（末个写入 13）应存活"
        );
    }

    /// 内存去冗余（worker 侧缓存条件化，纯逻辑零 pdfium）：GPU 门控（`gpu=true`）下
    /// `finalize_render` **不写 raw 缓存**且 `bgra` **move 直传**（返回 == 入参原图，逐字节
    /// 等值，无 clone 副本）；P2 R8（`luma=true`）出口编码成单通道亮度（1BPP，-4×）——仍
    /// 不写缓存、不 clone 原图。CPU 回退（`gpu=false`）照常写缓存 + 着色副本。条件只取决于
    /// GPU/CPU 门控、不取决于当前 recolor 是否 Some：CPU 模式即使 recolor=None 也应保持缓存。
    #[test]
    fn finalize_render_gpu_skips_cache_and_cpu_writes_it() {
        let raw = vec![7u8, 7, 7, 255];
        let arg = |cache: &mut Vec<RawEntry>,
                   bgra: Vec<u8>,
                   recolor: Option<RecolorParams>,
                   gpu: bool,
                   luma: bool| {
            finalize_render(
                cache,
                RawKind::Tile,
                0,
                0,
                0,
                0,
                0,
                1,
                1,
                16,
                16,
                1.0,
                bgra,
                recolor,
                gpu,
                luma,
            )
        };
        let night = RecolorParams { bg: [0x20, 0x20, 0x20], ink: Some([0xD8, 0xD8, 0xD8]) };

        // GPU 门控 + 非 R8（luma=false）：不写缓存、不克隆，出口 == 原图逐字节等值。
        let mut gpu_cache: Vec<RawEntry> = Vec::new();
        let out = arg(&mut gpu_cache, raw.clone(), Some(night), true, false);
        assert!(gpu_cache.is_empty(), "GPU 门控不应写 raw 缓存");
        assert_eq!(out, raw, "GPU 出口应 move 直传原图（逐字节等值，无克隆副本）");

        // P2 R8（luma=true）：出口编码成 1BPP 亮度（-4×），仍不写缓存、不克隆原图。
        let mut r8_cache: Vec<RawEntry> = Vec::new();
        let out_r8 = arg(&mut r8_cache, raw.clone(), Some(night), true, true);
        assert!(r8_cache.is_empty(), "GPU+R8 也不应写 raw 缓存");
        assert_eq!(
            out_r8,
            crate::recolor::encode_bgra_to_luma(&raw),
            "R8 出口应编码成单通道亮度（w*h*1）"
        );
        assert_eq!(out_r8.len(), 1, "R8 出口应为 1BPP（原 4BPP 的 1/4）");

        // CPU 门控：照常写缓存 + 着色副本；recolor=None 也应保持缓存（用户随时可切夜间，
        // 缓存供 SetRecolor/RecolorRefresh 消费）。
        let mut cpu_cache: Vec<RawEntry> = Vec::new();
        let out_none = arg(&mut cpu_cache, raw.clone(), None, false, false);
        assert_eq!(cpu_cache.len(), 1, "CPU 路径（recolor=None）也应写缓存");
        assert_eq!(out_none, raw, "None 直通出口 = 原图");
        let out_night = arg(&mut cpu_cache, raw.clone(), Some(night), false, false);
        assert_ne!(out_night, raw, "CPU + Some 应着色（与未着色原图不同）");
        assert_eq!(cpu_cache.len(), 1, "同密度同尺寸复用不新增条目");
    }

    /// F3/E3：worker 渲染队列对 Render（整页封面）与 RenderTile（瓦片）保持**同一 `render_q`
    /// 内的到达序 FIFO**——「先请求的在先出队」跨类成立。这正是 F3 评估结论的回归护栏：
    /// 停稳态「瓦片先派防封面堵小片」由 view 的**帧内**派发顺序（先瓦片后封面）保证；若按类
    /// 拆桶后「tile_q 恒先于 cover_q」会把**先入队的首屏封面**压到后到的瓦片之后（实测拖慢
    /// 首屏封面 ~1.8s、`tiles_fill_viewport` 变 flaky）——故保留混合 FIFO。快速态（F3）封面
    /// 不被瓦片阻塞则由 view 侧保证：中速带只派封面不派瓦片（见 `view::tiles::schedule_fast_covers`
    /// 与回归 `fast_band_dispatches_full_covers_without_tiles`）。
    #[test]
    fn enqueue_msg_keeps_cover_and_tile_in_one_fifo() {
        let mut render_q: VecDeque<(u64, u64, WorkerMsg)> = VecDeque::new();
        let mut mid_q: VecDeque<(u64, u64, WorkerMsg)> = VecDeque::new();
        let mut thumb_q: VecDeque<(u64, u64, WorkerMsg)> = VecDeque::new();
        let mut grid_q: VecDeque<(u64, u64, WorkerMsg)> = VecDeque::new();
        let mut rest_q: VecDeque<(u64, u64, WorkerMsg)> = VecDeque::new();
        let (tx, _rx) = mpsc::channel::<Option<RenderResult>>();
        let (ttx, _trx) = mpsc::channel::<Option<TileResult>>();
        // 先入队的封面 → 再入队的瓦片（跨帧真实次序：首屏封面早于后到的瓦片）。
        // L2.2 通道包装：发送时打戳 enqueue-n_gen（此处模拟 view 常规态 n_gen=0、z=0）。
        enqueue_msg(
            (0, 0, WorkerMsg::Render(7, RenderRequest { page: 7, scale: 2.0 }, tx)),
            &mut render_q,
            &mut mid_q,
            &mut thumb_q,
            &mut grid_q,
            &mut rest_q,
        );
        enqueue_msg(
            (0, 0, WorkerMsg::RenderTile(7, TileRequest { page: 7, scale: 2.0, tile_size: 512, row: 0, col: 1 }, ttx)),
            &mut render_q,
            &mut mid_q,
            &mut thumb_q,
            &mut grid_q,
            &mut rest_q,
        );
        assert_eq!(render_q.len(), 2, "Render 与 RenderTile 应同居 render_q");
        // 跨类 FIFO：先到的封面必须先于后到的瓦片出队（否则首屏封面被瓦片压住）。
        assert!(
            matches!(render_q.pop_front(), Some((_, _, WorkerMsg::Render(..)))),
            "先到的封面应先出队"
        );
        assert!(
            matches!(render_q.pop_front(), Some((_, _, WorkerMsg::RenderTile(..)))),
            "后到的瓦片随后出队"
        );
        assert_eq!(mid_q.len() + thumb_q.len() + rest_q.len(), 0, "其余桶应空");
    }

    /// 队列分桶断言：Grid 网格缩略图进**独立 grid_q**（背景信道，**最低优先级**）——
    /// 路由互不交混：render/mid/thumb/rest 各归其桶，grid 在上层全空后才被出队（run 循环
    /// 3.5 位于 rest 块之后）→ 网格洪流绝不阻塞/饿死阅读器既有请求（含 rest 类文本/检索）。
    #[test]
    fn enqueue_msg_keeps_grid_under_rest_in_its_own_background_bucket() {
        let mut render_q: VecDeque<(u64, u64, WorkerMsg)> = VecDeque::new();
        let mut mid_q: VecDeque<(u64, u64, WorkerMsg)> = VecDeque::new();
        let mut thumb_q: VecDeque<(u64, u64, WorkerMsg)> = VecDeque::new();
        let mut grid_q: VecDeque<(u64, u64, WorkerMsg)> = VecDeque::new();
        let mut rest_q: VecDeque<(u64, u64, WorkerMsg)> = VecDeque::new();
        let (tx, _rx) = mpsc::channel::<Option<RenderResult>>();
        let (ttx, _trx) = mpsc::channel::<Option<PageText>>();
        enqueue_msg(
            (0, 0, WorkerMsg::Grid(1, 3, 280.0, 1.0, tx.clone())),
            &mut render_q,
            &mut mid_q,
            &mut thumb_q,
            &mut grid_q,
            &mut rest_q,
        );
        enqueue_msg(
            (0, 0, WorkerMsg::Thumb(1, 4, tx.clone())),
            &mut render_q,
            &mut mid_q,
            &mut thumb_q,
            &mut grid_q,
            &mut rest_q,
        );
        enqueue_msg(
            (0, 0, WorkerMsg::Text(1, 5, ttx)),
            &mut render_q,
            &mut mid_q,
            &mut thumb_q,
            &mut grid_q,
            &mut rest_q,
        );
        assert_eq!(grid_q.len(), 1, "Grid 应入独立 grid_q");
        assert!(
            matches!(grid_q.front(), Some((_, _, WorkerMsg::Grid(..)))),
            "grid_q 应只装 Grid"
        );
        assert_eq!(rest_q.len(), 1, "Text(rest) 应入 rest_q 而非 grid_q");
        assert!(
            matches!(rest_q.front(), Some((_, _, WorkerMsg::Text(..)))),
            "rest_q 装 rest 类消息"
        );
        assert_eq!(render_q.len() + mid_q.len(), 0, "render/mid 桶应空");
        // 优先级序 render > mid > thumb > rest > grid：rest 与 grid 分居两桶，run 循环
        // 恒先出队 rest_q（3.5 网格块位于 rest 块之后）——Text/Search/Open/Close/写回
        // 永远先于网格卡执行；thumb 独立桶在前。
        assert!(
            matches!(thumb_q.pop_front(), Some((_, _, WorkerMsg::Thumb(..)))),
            "thumb 独立桶 FIFO"
        );
        assert!(
            matches!(rest_q.pop_front(), Some((_, _, WorkerMsg::Text(..)))),
            "rest 类（Text）应先于网格出队"
        );
        assert!(
            matches!(grid_q.pop_front(), Some((_, _, WorkerMsg::Grid(..)))),
            "grid 最后出队（最低优先级）"
        );
    }

    /// 分桶纯逻辑：相邻目标宽落同一桶（280/300 → 288 命中同项），跨桶边界分桶（310 → 320）。
    #[test]
    fn grid_bucket_width_quantizes_adjacent_to_same_bucket() {
        // GRID_BUCKET_PX=32：280/32=8.75→round=9→288；300/32=9.375→round=9→288 —— 同桶。
        assert_eq!(grid_bucket_width(280.0), 288);
        assert_eq!(grid_bucket_width(300.0), 288);
        // 310/32=9.6875→round=10→320 —— 跨桶边界。
        assert_eq!(grid_bucket_width(310.0), 320);
        // 极小目标宽兜底 ≥1 桶（不落 0）。
        assert_eq!(grid_bucket_width(1.0), 32);
        // DPI 折现：卡宽 260 × DPI1.5 = 390 → 384（390/32=12.1875→12→384）。
        assert_eq!(grid_bucket_width(260.0 * 1.5), 384);
    }

    /// 网格缓存纯逻辑（预算 LRU + key 复用）：同 key（页+分桶宽）写入更新不新增条目；
    /// 超预算后「最久未用」被逐出、最近用的存活（极小预算直接钉死逐出语义）。
    #[test]
    fn grid_cache_insert_budgets_lru_and_reuses_key() {
        let mut seq = 0u64;
        let mut cache: Vec<GridEntry> = Vec::new();
        let mut entry = |page: usize, bucket: u32, w: u32, byte: u8| {
            seq += 1;
            GridEntry {
                page,
                bucket_px: bucket,
                width: w,
                height: w, // 简化为方形（bytes = w*w*4）
                luma: false,
                bgra: vec![byte; (w * w * 4) as usize],
                last_used: seq,
            }
        };
        // 预算 = 3 条（每条约 4 字节：1×1×4）→ 第 4 条写入即超支，LRU 逐出最旧的 0 号。
        let budget = 12u64;
        assert!(grid_cache_insert(&mut cache, entry(0, 1, 1, 10), budget));
        assert!(grid_cache_insert(&mut cache, entry(1, 1, 1, 20), budget));
        assert!(grid_cache_insert(&mut cache, entry(2, 1, 1, 30), budget));
        assert!(grid_cache_insert(&mut cache, entry(3, 1, 1, 40), budget));
        assert_eq!(cache.len(), 3, "超预算应变回 3 条");
        assert!(
            !cache.iter().any(|e| e.page == 0),
            "最久未用（最先写入的 0 号）应被 LRU 逐出"
        );
        assert!(
            cache.iter().any(|e| e.page == 3),
            "最近写入（3 号）应存活"
        );
        // 同 key（页+桶）更新：不新增条目、bump last_used（content 更新）。
        let before = cache.len();
        assert!(!grid_cache_insert(&mut cache, entry(3, 1, 1, 99), budget));
        assert_eq!(cache.len(), before, "同 key 更新不应新增条目");
        assert!(cache.iter().all(|e| e.page != 0), "0 号已被逐出不复活");
    }

    /// Grid 档对已知页按目标宽 → 位图宽 == 量化后目标（像素宽由 scale=bucket/pw 推导，
    /// 位图宽恒等于 bucket_px，见 snapped_bitmap_len 稳定化）、高 == 按页纵横比（±1px 内，
    /// 期望值由目标页码真实 pt 尺寸推导，非拍脑袋，K177）。
    #[test]
    fn grid_render_matches_quantized_width_and_aspect() {
        if crate::pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let p = target_pdf_paths()[0].clone();
        assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let before = grid_render_count();
        // 页 0 真实 pt 尺寸（PDF 点），目标卡宽 280 逻辑 px、DPI 1.0 → 288 桶。
        let (pw, ph) = dims[0];
        assert!(pw > 0.0 && ph > 0.0, "页 0 尺寸应为正");
        let card_width = 280.0f32;
        let target_bucket = grid_bucket_width(card_width);
        let rx = worker.request_grid(doc_id, 0, card_width, 1.0, 0, 0);
        let res = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("应收到网格缩略图")
            .expect("网格渲染应成功");
        // 宽 == 量化桶（scale=bucket/pw 且 pw*scale==bucket → snapped=round=bucket，精确相等）。
        assert_eq!(res.width, target_bucket, "位图宽应等于量化后目标宽");
        // 高 == 按纵横比：ph/pw × bucket，±1px（page_bitmap_size 的 ceil 稳定化误差界）。
        let expect_h = (ph as f64 / pw as f64) * target_bucket as f64;
        let diff = (res.height as f64 - expect_h).abs();
        assert!(diff <= 1.0, "高度应按页纵横比（期望 ~{expect_h:.1}，实际 {}）", res.height);
        // 像素缓冲尺寸闭合（w*h*4 或 R8 w*h）。
        let expected_len = if res.luma {
            (res.width * res.height) as usize
        } else {
            (res.width * res.height * 4) as usize
        };
        assert_eq!(res.bgra.len(), expected_len, "像素缓冲应按 luma 口径闭合");
        assert_eq!(grid_render_count(), before + 1, "单次请求应恰好一次栅格");
    }

    /// 缓存命中：同页**同分桶宽**二次请求不重栅（渲染计数不再 +1，返回相同尺寸项）；
    /// 相邻目标宽落同桶 → 命中同项；换桶 → 重栅（计数再 +1，缓存 key 含分桶宽）。
    #[test]
    fn grid_cache_hits_same_page_bucket_and_rerenders_on_width_change() {
        if crate::pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let p = target_pdf_paths()[0].clone();
        assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, _dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let before = grid_render_count();
        // 第一次：280 → 渲染一次。
        let rx1 = worker.request_grid(doc_id, 0, 280.0, 1.0, 0, 0);
        let r1 = rx1.recv_timeout(std::time::Duration::from_secs(30)).expect("第一次应收到").expect("渲染应成功");
        assert_eq!(grid_render_count(), before + 1, "首次未命中应栅格一次");
        // 第二次同宽：命中缓存，不重栅。
        let rx2 = worker.request_grid(doc_id, 0, 280.0, 1.0, 0, 0);
        let r2 = rx2.recv_timeout(std::time::Duration::from_secs(30)).expect("第二次应收到").expect("渲染应成功");
        assert_eq!(grid_render_count(), before + 1, "同页同分桶宽二次请求不应重栅");
        assert_eq!((r2.width, r2.height), (r1.width, r1.height), "命中项尺寸与首渲一致");
        // 相邻宽落同桶（300 → 288 桶）：仍命中同项，不重栅。
        let rx3 = worker.request_grid(doc_id, 0, 300.0, 1.0, 0, 0);
        let r3 = rx3.recv_timeout(std::time::Duration::from_secs(30)).expect("相邻宽应收到").expect("渲染应成功");
        assert_eq!(grid_render_count(), before + 1, "相邻目标宽落同桶应命中不重栅");
        assert_eq!((r3.width, r3.height), (r1.width, r1.height), "同桶项尺寸一致");
        // 换桶（520 → 512 桶）：新桶未缓存 → 重栅一次；尺寸随新桶变。
        let rx4 = worker.request_grid(doc_id, 0, 520.0, 1.0, 0, 0);
        let r4 = rx4.recv_timeout(std::time::Duration::from_secs(30)).expect("换桶应收到").expect("渲染应成功");
        assert_eq!(grid_render_count(), before + 2, "换桶未命中应再栅格一次");
        assert_eq!(r4.width, grid_bucket_width(520.0), "换桶位图宽应等于新量化桶");
    }

    /// DIAG（诊断探针，保留不删、禁当回归）：拆「pdfium 栅格耗时 vs 每卡固定开销」+
    /// 量 `grid_cache_evict` 的 O(n) 字节求和成本，回答两个问题：
    /// ①「0.5× 低清快档为何只快 ~20%」——固定开销（doc 查表/recolor 取参/缓存扫描/通道
    ///    往返）与栅格面积无关；②「批量/并行是否值」——单发 vs 批量发送的每卡总耗时差。
    /// 只打印数字 + 宽松 sanity 断言（>0/有限/低清 < 全清），不做紧界回归（机器环境波动
    /// 不作数，K68 噪声机纪律）。
    #[test]
    fn grid_diag_fixed_overhead_and_evict_cost() {
        if crate::pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        // ---- ① grid_cache_evict O(n)：N=512 条真实尺寸合成条目（288 桶 A4 ≈458KB/条），
        // 拆两档成本：
        //   a) **字节求和**（预算内每次插入都付）：`o(n)` 单趟 `map(bytes).sum()`——预算足够
        //      时 over≤0、无逐出，只花这次求和；随缓存变大线性涨。
        //   b) **超支逐出**（只超预算才付）：额外 O(n) 找最旧 + O(n) remove 移位，到保底 1 条。
        let n_entries = 512usize;
        let card_bytes: usize = 288 * ((288.0f32 * 1.414).round() as usize) * 4;
        let mut cache: Vec<GridEntry> = Vec::with_capacity(n_entries);
        let mut seq = 0u64;
        for page in 0..n_entries {
            seq += 1;
            let h = ((288.0f32 * 1.414).round() as u32).max(1);
            cache.push(GridEntry {
                page,
                bucket_px: 288,
                width: 288,
                height: h,
                luma: false,
                bgra: vec![0u8; card_bytes],
                last_used: seq,
            });
        }
        let t_sum = std::time::Instant::now();
        let _over = cache.iter().map(GridEntry::bytes).sum::<u64>();
        let sum_us = t_sum.elapsed().as_secs_f64() * 1e6;
        let t0 = std::time::Instant::now();
        grid_cache_evict(&mut cache, 0);
        let evict_us = t0.elapsed().as_secs_f64() * 1e6;
        assert_eq!(cache.len(), 1, "budget=0 应逐出到保底 1 条");
        let total_mb = (n_entries * card_bytes) as f64 / (1024.0 * 1024.0);
        eprintln!(
            "[GRID-DIAG] grid_cache_evict O(n): n={n_entries} 卡({total_mb:.0}MB) → 字节求和(每次插入都付) {sum_us:.1}µs ≈ {:.1}ns/卡；超支全逐出(仅超预算才付) {evict_us:.1}µs ≈ {:.1}ns/卡",
            sum_us * 1e3 / n_entries as f64,
            evict_us * 1e3 / n_entries as f64
        );
        assert!(evict_us.is_finite() && evict_us > 0.0);

        // ---- ② 全清 vs 低清，worker 往返冷/热分账（批量：先全发再全收，杜绝预熟插空）。
        //   cold = 冷缓存（pdfium 栅格 + 固定开销）；hot = 同桶缓存命中（只固定开销：
        //   入队/出队/世代比对/doc 查表/recolor 取参/缓存扫描/通道回传）。
        //   pdfium 栅格 ≈ cold − hot。低清 0.5× 面积 ↓4× → 栅格 ≈/4；若固定开销占比大，
        //   整卡总耗时不会按 4× 降（直接回答「0.5× 为何只快 ~20%」）。
        let p = target_pdf_paths()[1].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let n_pages = dims.len().min(48);
        let pages: Vec<usize> = (0..n_pages).collect();
        let full_card = 280.0f32; // 288 桶（全清）
        let quick_card = 140.0f32; // 低清 0.5×（≥64 下限），落 160 桶 ≠ 288
        assert_ne!(
            grid_bucket_width(full_card),
            grid_bucket_width(quick_card),
            "DIAG 需要两档分桶不同"
        );

        fn batch_ms(worker: &PdfWorker, doc_id: DocId, pages: &[usize], card: f32) -> f64 {
            let t0 = std::time::Instant::now();
            let rxs: Vec<_> = pages
                .iter()
                .map(|&p| worker.request_grid(doc_id, p, card, 1.0, 0, 0))
                .collect();
            for (i, rx) in rxs.into_iter().enumerate() {
                let r = rx
                    .recv_timeout(std::time::Duration::from_secs(30))
                    .unwrap_or_else(|_| panic!("第 {i} 页未按时回传"));
                assert!(r.is_some(), "第 {i} 页渲染应成功");
            }
            t0.elapsed().as_secs_f64() * 1e3
        }

        let full_cold = batch_ms(&worker, doc_id, &pages, full_card) / n_pages as f64;
        let full_hot = batch_ms(&worker, doc_id, &pages, full_card) / n_pages as f64;
        let quick_cold = batch_ms(&worker, doc_id, &pages, quick_card) / n_pages as f64;
        let quick_hot = batch_ms(&worker, doc_id, &pages, quick_card) / n_pages as f64;
        let full_raster = (full_cold - full_hot).max(0.0);
        let quick_raster = (quick_cold - quick_hot).max(0.0);
        eprintln!(
            "[GRID-DIAG] 批量往返/卡（ms）：全清 cold={full_cold:.3} hot={full_hot:.3} raster≈{full_raster:.3} | 低清 cold={quick_cold:.3} hot={quick_hot:.3} raster≈{quick_raster:.3}"
        );
        eprintln!(
            "[GRID-DIAG] 固定开销占比：全清 {:.0}%、低清 {:.0}%；低清总耗时/全清总耗时 = {:.2}×（若 ≈1 说明固定开销主导）",
            100.0 * full_hot / full_cold.max(1e-9),
            100.0 * quick_hot / quick_cold.max(1e-9),
            quick_cold / full_cold.max(1e-9),
        );

        // ---- ③ 单发 vs 批量（消息处理/通道往返是否能被批量摊薄）——
        //   5 页单发逐条的总耗时 vs 同 5 页批量。批量断言用前 5 页（独立分桶、独立缓存）。
        let sub: Vec<usize> = (0..5).collect();
        let mut single_total = 0.0f64;
        for i in 0..5 {
            let t = std::time::Instant::now();
            let rx = worker.request_grid(doc_id, sub[i], full_card, 1.0, 0, 0);
            rx.recv_timeout(std::time::Duration::from_secs(30))
                .expect("单发应回传");
            single_total += t.elapsed().as_secs_f64() * 1e3;
        }
        let batch5 = batch_ms(&worker, doc_id, &sub, full_card);
        eprintln!(
            "[GRID-DIAG] 单发 vs 批量（5 页全清，缓存内）：单发 {:.3}ms/卡 vs 批量 {:.3}ms/卡 (= {:.2}×)",
            single_total / 5.0,
            batch5 / 5.0,
            single_total / batch5.max(1e-9),
        );
        assert!(single_total.is_finite() && batch5.is_finite());
        // 低清整卡总耗时应 <= 全清（面积 4× 差下的合理方向断言；噪声下不锁紧界）。
        assert!(
            quick_cold <= full_cold * 1.5 + 0.05,
            "低清冷渲不应比全清慢（方向 sanity）"
        );
        // 关档清理（含预熟游标），防残留 doc 占用 worker 空闲预熟。
        worker.close(doc_id);
    }

    /// DIAG（测绘遗留，保留不删、禁当回归）：reader 每号栅格真墙钟 + `cache_raw` O(n) 字节
    /// 求和/逐出开销。headless 无 pdfium → SKIP（夹具不入库）。
    #[test]
    fn reader_raster_cost_and_raw_evict_diag() {
        if crate::pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let p = target_pdf_paths()[1].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let n = dims.len();
        assert!(n >= 4, "夹具页数需 ≥4，实际 {n}");
        let px = 1usize; // 避开首页，取第 1 页（既有 bench 同口径）

        // 每单位光栅耗时（worker 串行 FIFO 往返 = pdfium 栅格 + 固定开销；固定开销 ~0.04ms
        // 级，见 grid_diag hot≈0.036ms，可忽略）。
        let ms = |label: &str, f: &mut dyn FnMut() -> (u32, u32)| {
            let t0 = std::time::Instant::now();
            let (w, h) = f();
            let m = t0.elapsed().as_secs_f64() * 1e3;
            assert!(m.is_finite() && m > 0.0, "{label} 应测到正耗时");
            eprintln!("[READER-DIAG] {label}: {m:.2}ms -> {w}x{h}px");
            m
        };
        // 整页封面：scale=1.0（≈596px 宽）与 scale=2.0（≈1192px 宽，headless sf=2 实际阅读密度）
        let cover10 = ms("cover@1.0", &mut || {
            let rx = worker.request_render(
                doc_id,
                RenderRequest {
                    page: px,
                    scale: 1.0,
                },
                0,
                0,
            );
            let r = rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .unwrap()
                .unwrap();
            (r.width, r.height)
        });
        let cover20 = ms("cover@2.0", &mut || {
            let rx = worker.request_render(
                doc_id,
                RenderRequest {
                    page: px,
                    scale: 2.0,
                },
                0,
                0,
            );
            let r = rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .unwrap()
                .unwrap();
            (r.width, r.height)
        });
        // 512px 瓦片：page@2.0 → 位图 ≈2w×2h，取中心格 (1,1)（非首格，避免边上空瓦片）
        let tile = ms("tile@512(sf2)", &mut || {
            let rx = worker.request_tile(
                doc_id,
                TileRequest {
                    page: px,
                    scale: 2.0,
                    tile_size: 512,
                    row: 1,
                    col: 1,
                },
                0,
                0,
            );
            let t = rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .unwrap()
                .unwrap();
            (t.w, t.h)
        });
        // L0 缩略（260px）与 L0.5 中清（520px，sf=1）
        let thumb = ms("thumb@260", &mut || {
            let rx = worker.request_thumb(doc_id, px, 0, 0);
            let r = rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .unwrap()
                .unwrap();
            (r.width, r.height)
        });
        let mid = ms("mid@520", &mut || {
            let rx = worker.request_mid(doc_id, px, 1.0, 0, 0);
            let r = rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .unwrap()
                .unwrap();
            (r.width, r.height)
        });

        // ---- cache_raw O(n) 字节求和（每次写入都付，超预算才逐出）----
        // 合成 n=512 条 ~1MB 原图（≈ 切档刷新可见集上界量级），拆求和与逐出两档。
        let n_raw = 256usize; // 256 条 ×1MB ≈256MB > 48MB 预算若干倍
        let raw_bytes = 512usize * (512usize * 4usize);
        let mut raw: Vec<RawEntry> = Vec::with_capacity(n_raw);
        for i in 0..n_raw {
            raw.push(RawEntry {
                kind: RawKind::Tile,
                page: i,
                row: 0,
                col: 0,
                x0: 0,
                y0: 0,
                w: 512,
                h: 512,
                full_w: 512,
                full_h: 512,
                scale: 1.0,
                bgra: vec![0u8; raw_bytes],
            });
        }
        let t_sum = std::time::Instant::now();
        let _over = raw.iter().map(RawEntry::bytes).sum::<u64>();
        let sum_us = t_sum.elapsed().as_secs_f64() * 1e6;
        let t0 = std::time::Instant::now();
        let mut work = raw; // move：从头逐出（budget=0 全逐、至少留 1）
        let first = work.remove(0);
        cache_raw(&mut work, first, 0); // budget=0 → 触发全逐出
        let evict_us = t0.elapsed().as_secs_f64() * 1e6;
        assert!(work.len() <= 1, "budget=0 应逐出到 ≤1 条");
        eprintln!(
            "[READER-DIAG] cache_raw O(n): n={n_raw} 条({}MB) → 字节求和 {sum_us:.1}µs ≈ {:.1}ns/条；超支全逐出 {evict_us:.1}µs（仅超预算才付）",
            (n_raw * raw_bytes) as f64 / (1024.0 * 1024.0),
            sum_us * 1e3 / n_raw as f64,
        );
        eprintln!(
            "[READER-DIAG] 汇总: cover@1.0={cover10:.1}ms cover@2.0={cover20:.1}ms tile={tile:.1}ms thumb={thumb:.1}ms mid={mid:.1}ms（同机同二进制交错，噪声 ±30% 只当方向）"
        );
        worker.close(doc_id);
    }

    /// T3 内存探针（DIAG，保留）：打开后 `stores` 未物化（None = 省一份源字节驻留）；
    /// 首写（内存删页）触发惰性物化后已物化（Some 字节长 > 0）。
    #[test]
    fn stores_lazy_materialize_memory_probe_diag() {
        if crate::pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let p = target_pdf_paths()[0].clone();
        assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let src_len = bytes.len();
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let n = dims.len();
        assert!(n >= 1, "夹具页数需 ≥1，实际 {n}");

        // ① 打开后 stores 尚未物化（None = 未镜像源字节 → 真省一份整本驻留）
        let before = worker.mem_state(doc_id);
        assert!(
            before.is_none(),
            "打开后 stores 应为空（未物化），实际 {before:?}"
        );

        // ② 首写操作（内存删除第 0 页）触发惰性物化 → 之后已物化（Some 字节长 > 0）
        let r = worker.mem_delete_page(doc_id, 0).expect("内存删除应成功");
        assert_eq!(r.count, n - 1, "删除后页数 -1");
        let after_len = worker
            .mem_state(doc_id)
            .expect("首写后 stores 应已物化（Some 字节）");
        assert!(after_len > 0, "物化字节长应 > 0，实际 {after_len}B");
        eprintln!(
            "[T3-DIAG] stores 延迟克隆：打开后 stores 字节 = 0（源字节 {src_len}B 不再镜像驻留，省一份整本）；首写惰性物化后 = {after_len}B"
        );
        worker.close(doc_id);
    }

    /// DLL 校验（防种植，纯函数）：sha256 对同一字节集恒等、异字节集必异，
    /// 且与公开向量一致（sha256("abc") = ba78...15ad）。
    #[test]
    fn sha256_hex_is_stable_and_distinguishes_content() {
        let a = b"pdfium binary bytes";
        let b = b"pdfium binary bytex";
        assert_eq!(sha256_hex(a), sha256_hex(a), "同字节集 hex 应恒等");
        assert_ne!(sha256_hex(a), sha256_hex(b), "异字节集 hex 必异");
        assert_eq!(sha256_hex(a).len(), 64, "sha256 hex 恒 64 字符");
        // 已知公开向量（FIPS 180-4）：sha256("abc")
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// DLL 复用裁决（纯函数）：「同长不同内容 → 判不复用」「同内容 → 复用」「长度不同 → 快拒」。
    #[test]
    fn pdfium_reuse_decision_gates_on_len_then_hash() {
        let good = sha256_hex(b"real pdfium");
        let planted = sha256_hex(b"planted evil dll");
        let len = 13u64; // b"real pdfium".len()
        // 同内容 → 复用。
        assert!(pdfium_reuse_decision(len, &good, len, &good));
        // 同长但内容不同（DLL 种植）→ 判不复用（走重解压覆盖）。
        assert!(!pdfium_reuse_decision(len, &planted, len, &good), "同长异内容应不复用");
        // 长度不同 → 快拒（即使 hash 意外相同也不复用；明显不同 size 免做全量 hash 读）。
        assert!(!pdfium_reuse_decision(len + 1, &good, len, &good), "长度不同应快拒");
        assert!(!pdfium_reuse_decision(1, &good, len, &good), "长度不同应快拒");
    }

    // ---- 页面网格视图 wave7a：重排 + 批量删除（内存级原语）----
    //
    // 客观 oracle（K177：期望值由真实测量推导，非拍脑袋）：
    // ①「页尺寸序列」：`open_and_dims` 的 dims 即 worker 经 `FPDF_GetPageSizeByIndexF`
    //    （页树 MediaBox，不加载页对象）量得的各页 (w, h)——重排/批删后新镜像重开再量，
    //    断言序列等于「原序列按操作映射」后的期望；
    // ②「逐页渲染像素哈希」：对去 stride 的紧凑 BGRA 缓冲算 FNV-1a，同一内容 → 同哈希、
    //    异内容 → 异哈希（不同页码内容各异，页数不变 ≠ 重排正确，哈希能抓错序）。
    // 期望映射均为手工推导（反转 → 位置 i 处为原 n-1-i；删 2/4 → 幸存序 [0,1,3,5,6,7]）。

    /// 渲染第 `page` 页（scale=1.0 全密度）并返回 (宽, 高, FNV-1a 哈希) —— 内容 oracle。
    fn render_page_probe(worker: &PdfWorker, doc_id: DocId, page: usize) -> (u32, u32, u64) {
        let rx = worker.request_render(doc_id, RenderRequest { page, scale: 1.0 }, 0, 0);
        let r = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("应收到渲染结果")
            .expect("渲染应成功");
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in &r.bgra {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        (r.width, r.height, h)
    }

    /// 逐页探针：`page..page+count`（不越界 n）的顺序哈希。
    fn render_page_probes(worker: &PdfWorker, doc_id: DocId, page: usize, n: usize) -> Vec<(u32, u32, u64)> {
        (0..n).map(|i| render_page_probe(worker, doc_id, page + i)).collect()
    }

    /// 重排：整体反转后（1）页尺寸序列 = 原逆序（2）最前/最后 2 页渲染哈希 = 原末/首 2 页
    /// 逆序——「页数不变 ≠ 重排正确」，用双 oracle 抓错序。
    #[test]
    fn mem_reorder_reverses_page_sequence_and_content() {
        if crate::pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let p = target_pdf_paths()[1].clone(); // pdf测试2.pdf：长文多页
        assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let n = dims.len();
        assert!(n >= 4, "夹具页数需 ≥4，实际 {n}");
        // 探明原图（FPDF_GetPageSizeByIndexF 量得的 dims 已在 open 回传）。
        let first2 = render_page_probes(&worker, doc_id, 0, 2);
        let last2 = render_page_probes(&worker, doc_id, n - 2, 2);
        // 整体反转排列（0 基全排列）。
        let mut order: Vec<usize> = (0..n).collect();
        order.reverse();
        // 期望：新序列位置 i = 原 n-1-i。
        let expect_dims: Vec<(f32, f32)> = dims.iter().rev().cloned().collect();
        let mem = worker.mem_reorder_pages(doc_id, order).expect("重排应成功");
        assert_eq!(mem.count, n, "重排后页数不变");
        // 新镜像重开 → 页尺寸序列 oracle。
        let (_, d2, dims2) = PdfWorker::open_and_dims(mem.after).expect("重排后镜像可打开");
        assert_eq!(dims2, expect_dims, "重排后页尺寸序列应为原逆序");
        // 逐页渲染哈希 oracle：新文档最前 2 页 == 原末 2 页逆序；最后 2 页 == 原首 2 页逆序。
        let new_first2 = render_page_probes(&worker, d2, 0, 2);
        assert_eq!(new_first2, last2.iter().rev().cloned().collect::<Vec<_>>());
        let new_last2 = render_page_probes(&worker, d2, n - 2, 2);
        assert_eq!(new_last2, first2.iter().rev().cloned().collect::<Vec<_>>());
    }

    /// 重排「整块搬移」（两段值连续的升序段）：后半块整体搬到最前——段式 import 追加的
    /// 可见断言（若 `FPDF_ImportPages` 的 index 用 0=prepend，结果会变回原序被此断言抓住）。
    #[test]
    fn mem_reorder_moves_contiguous_block() {
        if crate::pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let p = target_pdf_paths()[1].clone();
        assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let n = dims.len();
        assert!(n >= 6, "夹具页数需 ≥6，实际 {n}");
        let half = n / 2;
        // 新序 = [half..n] 块 + [0..half] 块：两段「值连续的升序段」，逐段 import 追加。
        let mut order: Vec<usize> = Vec::with_capacity(n);
        order.extend(half..n);
        order.extend(0..half);
        // 期望 dims = 原 dims 的后半块 + 前半块。
        let expect_dims: Vec<(f32, f32)> = dims[half..]
            .iter()
            .chain(dims[..half].iter())
            .cloned()
            .collect();
        let mem = worker.mem_reorder_pages(doc_id, order).expect("块搬移重排应成功");
        assert_eq!(mem.count, n, "重排后页数不变");
        let (_, d2, dims2) = PdfWorker::open_and_dims(mem.after).expect("重排后镜像可打开");
        assert_eq!(dims2, expect_dims, "块搬移后页尺寸序列 = 原后半块 + 原前半块");
        // 逐页渲染哈希：新文档最前 2 页 == 原后半块最前 2 页（= 原 half、half+1 页）。
        let new_first2 = render_page_probes(&worker, d2, 0, 2);
        let orig_half2 = render_page_probes(&worker, doc_id, half, 2);
        assert_eq!(new_first2, orig_half2, "块搬移后最前 2 页 = 原后半块最前 2 页");
    }

    /// 重排 round-trip：X → 反序 → 反序（即原序）后，页尺寸序列 + 逐页渲染哈希 == 原图
    /// （证重排可逆、无内容损——两次反转回到原序）。
    #[test]
    fn mem_reorder_round_trip_restores_content() {
        if crate::pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let p = target_pdf_paths()[1].clone();
        assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_a, dims_a) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let n = dims_a.len();
        assert!(n >= 3, "夹具页数需 ≥3，实际 {n}");
        let first3 = render_page_probes(&worker, doc_a, 0, 3);
        let mut rev: Vec<usize> = (0..n).collect();
        rev.reverse();
        // 第一次反转 → 新镜像重开。
        let m1 = worker.mem_reorder_pages(doc_a, rev.clone()).expect("第一次重排应成功");
        assert_eq!(m1.count, n);
        let (_, doc_b, dims_b) = PdfWorker::open_and_dims(m1.after).expect("第一次镜像可打开");
        assert_eq!(dims_b, dims_a.iter().rev().cloned().collect::<Vec<_>>(), "中间态 = 逆序");
        // 第二次反转（再反一次 = 回原序）。
        let m2 = worker.mem_reorder_pages(doc_b, rev.clone()).expect("第二次重排应成功");
        assert_eq!(m2.count, n);
        let (_, doc_c, dims_c) = PdfWorker::open_and_dims(m2.after).expect("第二次镜像可打开");
        assert_eq!(dims_c, dims_a, "两次反转后页尺寸序列应还原原序");
        // 逐页渲染哈希还原（最前 3 页）。
        let c_first3 = render_page_probes(&worker, doc_c, 0, 3);
        assert_eq!(c_first3, first3, "两次反转后最前 3 页渲染哈希应还原为原图");
    }

    /// 重排非法输入（缺页/超长/越界/重复）→ 立即 Err 且文档不变（stores 镜像原样、无副作用）。
    #[test]
    fn mem_reorder_rejects_invalid_permutations() {
        if crate::pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let p = target_pdf_paths()[1].clone();
        assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let n = dims.len();
        assert!(n >= 2, "夹具页数需 ≥2，实际 {n}");
        let before = worker.mem_bytes(doc_id).expect("应取回操作前镜像");
        // 缺页：长度 < n。
        let short: Vec<usize> = (0..n - 1).collect();
        assert!(worker.mem_reorder_pages(doc_id, short).is_err(), "缺页排列应 Err");
        // 超长：长度 > n。
        let long: Vec<usize> = (0..=n).collect();
        assert!(worker.mem_reorder_pages(doc_id, long).is_err(), "超长排列应 Err");
        // 越界：某值 >= n。
        let mut oob: Vec<usize> = (0..n).collect();
        oob[n - 1] = n;
        assert!(worker.mem_reorder_pages(doc_id, oob).is_err(), "越界页应 Err");
        // 重复：某值出现两次。
        let mut dup: Vec<usize> = (0..n).collect();
        dup[n - 1] = 0;
        assert!(worker.mem_reorder_pages(doc_id, dup).is_err(), "重复页应 Err");
        // 全部非法输入后文档不变（stores 镜像未被污染，页数/渲染随之不变）。
        let after = worker.mem_bytes(doc_id).expect("应取回操作后镜像");
        assert_eq!(after, before, "非法重排后镜像应原样（文档不变、无副作用）");
    }

    /// 批量删除 [2,4]（含去重：传 [2,4,2]）→ 页数 n-2、页尺寸序列与渲染哈希 = 原图去
    /// {2,4} 幸存序（[0,1,3,5,6,7]——降序删防下标位移错删的可见断言）。
    #[test]
    fn mem_batch_delete_removes_pages_and_content() {
        if crate::pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let p = target_pdf_paths()[1].clone();
        assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let n = dims.len();
        assert!(n >= 8, "夹具页数需 ≥8，实际 {n}");
        let orig_probes = render_page_probes(&worker, doc_id, 0, 8);
        // 手工推导期望：删除 0 基 {2,4}（重复 2 应去重只删一次），幸存序 = [0,1,3,5,6,7,...]。
        let mem = worker
            .mem_batch_delete_pages(doc_id, vec![2, 4, 2])
            .expect("批量删除应成功");
        assert_eq!(mem.count, n - 2, "删除 2 页后页数 = n-2（重复页去重）");
        let (_, d2, dims2) = PdfWorker::open_and_dims(mem.after).expect("批删后镜像可打开");
        let expect_dims: Vec<(f32, f32)> = dims
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 2 && *i != 4)
            .map(|(_, d)| *d)
            .collect();
        assert_eq!(dims2, expect_dims, "批删后页尺寸序列应去掉 {{2,4}}");
        // 渲染哈希：新文档前 6 页 == 原图幸存序 [0,1,3,5,6,7] 的探针。
        let new_first6 = render_page_probes(&worker, d2, 0, 6);
        let expect_probes: Vec<(u32, u32, u64)> =
            [0usize, 1, 3, 5, 6, 7].iter().map(|&i| orig_probes[i]).collect();
        assert_eq!(new_first6, expect_probes, "批删后前 6 页渲染哈希 = 原图去 {{2,4}} 幸存序");
    }

    /// 批量删除非法输入（越界 / 删光全部页）→ Err 且文档不变；撤销（`mem_restore_bytes`
    /// 操作前镜像）→ 页尺寸 + 渲染哈希还原（走既有 undo 链路 —— worker 侧＝快照恢复）。
    #[test]
    fn mem_batch_delete_rejects_invalid_and_undo_restores() {
        if crate::pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let p = target_pdf_paths()[1].clone();
        assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let n = dims.len();
        assert!(n >= 4, "夹具页数需 ≥4，实际 {n}");
        let first3 = render_page_probes(&worker, doc_id, 0, 3);
        let before = worker.mem_bytes(doc_id).expect("应取回操作前镜像");
        // 越界页 → Err。
        assert!(worker.mem_batch_delete_pages(doc_id, vec![n]).is_err(), "越界页应 Err");
        // 含越界的混合数组 → Err。
        assert!(worker.mem_batch_delete_pages(doc_id, vec![0, n, 2]).is_err(), "含越界应 Err");
        // 删光全部页（去重后 = 全部）→ Err（保留至少 1 页，防 0 页文档）。
        let all: Vec<usize> = (0..n).collect();
        assert!(worker.mem_batch_delete_pages(doc_id, all).is_err(), "删光全部页应 Err");
        assert_eq!(worker.mem_bytes(doc_id), Some(before.clone()), "非法批删后镜像应原样");
        // 合法批删（删最前 2 页）→ 撤销恢复镜像 → 页尺寸 + 渲染哈希还原。
        let mem = worker
            .mem_batch_delete_pages(doc_id, vec![0, 1])
            .expect("批删应成功");
        assert_eq!(mem.count, n - 2, "删 2 页后页数 = n-2");
        worker.mem_restore_bytes(doc_id, mem.before).expect("撤销恢复镜像应成功");
        let restored = worker.mem_bytes(doc_id).expect("应取回恢复后镜像");
        let (_, d2, dims2) = PdfWorker::open_and_dims(restored).expect("恢复后镜像可打开");
        assert_eq!(dims2, dims, "撤销后页尺寸序列应还原");
        let r_first3 = render_page_probes(&worker, d2, 0, 3);
        assert_eq!(r_first3, first3, "撤销后前 3 页渲染哈希应还原为原图");
    }

    // ---- 页面网格视图 wave7b：提取多页 / 拆分区间 / 合并另一 PDF ----
    //
    // 客观 oracle（同 wave7a）：「页尺寸序列」（`open_and_dims` 的 dims）+「逐页渲染像素
    // 哈希」（`render_page_probe`）。期望映射均为手工推导（K177/K143：先写算式再落断言）。
    // 夹具实测：pdf测试.pdf = 18 页（横向 792×612），pdf测试2.pdf = 259 页（纵向 595.3×841.9）。

    /// wave7b 临时输出路径（pid + 纳秒戳唯一，落系统 temp，勿污染仓库；对齐
    /// `view::open::tests::wave7b_out` 纳秒戳姿势，消除 pid 复用跨进程撞名的极小概率）。
    fn wave7b_output(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("pdf-wave7b-{name}-{}-{nanos}.pdf", std::process::id()))
    }

    /// 提取任意序 + 去重：[2,0,2] → 去重保留首次出现 = [2,0] → 新文件 2 页且尺寸/渲染
    /// 按 0 基新序对齐源页；原文档不变（副本语义）。
    #[test]
    fn export_pages_writes_arbitrary_order_and_dedup() {
        if crate::pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let p = target_pdf_paths()[0].clone();
        assert!(p.exists(), "测试 PDF 应存在: {}", p.display());
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let n = dims.len();
        assert!(n >= 3, "夹具页数需 ≥3，实际 {n}");
        // 原图 oracle：页 0、页 2 渲染哈希（提取结果逐页对齐源页）。
        let orig0 = render_page_probe(&worker, doc_id, 0);
        let orig2 = render_page_probe(&worker, doc_id, 2);
        let out = wave7b_output("export-order");
        // 期望（算式）：[2,0,2] 去重（保留首次出现序）= [2,0] →
        //   新文件页数 2、尺寸序列 [dims[2], dims[0]]、渲染 [orig2, orig0]。
        let written = worker
            .export_pages_to(doc_id, vec![2, 0, 2], out.clone())
            .expect("提取应成功");
        assert_eq!(written, 2, "去重后应写 2 页");
        let new_bytes = std::fs::read(&out).expect("新文件应存在且可读");
        let (_, d_new, dims_new) = PdfWorker::open_and_dims(new_bytes).expect("新文件应可打开");
        let expect_dims = vec![dims[2], dims[0]];
        assert_eq!(dims_new, expect_dims, "提取后尺寸序列 = 按 0 基新序对齐源页");
        let probes = render_page_probes(&worker, d_new, 0, 2);
        assert_eq!(probes, vec![orig2, orig0], "提取后逐页渲染哈希 = 源页按新序对齐");
        // 副本语义：原文档不变（首页渲染哈希仍一致）。
        let again0 = render_page_probe(&worker, doc_id, 0);
        assert_eq!(again0, orig0, "提取不改变当前文档（副本语义）");
    }

    /// 提取末页：[n-1] → 新文件 1 页且尺寸/渲染 = 源末页。
    #[test]
    fn export_pages_single_last_page_matches_source() {
        if crate::pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let p = target_pdf_paths()[0].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let n = dims.len();
        assert!(n >= 1, "夹具页数需 ≥1，实际 {n}");
        let last = render_page_probe(&worker, doc_id, n - 1);
        let out = wave7b_output("export-last");
        let written = worker
            .export_pages_to(doc_id, vec![n - 1], out.clone())
            .expect("提取末页应成功");
        assert_eq!(written, 1, "单页提取应写 1 页");
        let new_bytes = std::fs::read(&out).expect("新文件应存在且可读");
        let (_, d_new, dims_new) = PdfWorker::open_and_dims(new_bytes).expect("新文件应可打开");
        assert_eq!(dims_new, vec![dims[n - 1]], "末页提取尺寸 = 源末页");
        let probes = render_page_probes(&worker, d_new, 0, 1);
        assert_eq!(probes, vec![last], "末页提取渲染 = 源末页");
    }

    /// 提取非法输入（越界 / 含越界混合 / 空清单）→ Err 且零副作用（不产生文件、当前不变）。
    #[test]
    fn export_pages_rejects_invalid_without_side_effect() {
        if crate::pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let p = target_pdf_paths()[0].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let n = dims.len();
        assert!(n >= 1, "夹具页数需 ≥1，实际 {n}");
        let before = worker.mem_bytes(doc_id).expect("应取回操作前镜像");
        let out = wave7b_output("export-invalid");
        assert!(
            worker.export_pages_to(doc_id, vec![n], out.clone()).is_err(),
            "越界提取应 Err"
        );
        assert!(
            worker.export_pages_to(doc_id, vec![0, n], out.clone()).is_err(),
            "含越界的混合数组应 Err"
        );
        assert!(
            worker.export_pages_to(doc_id, vec![], out.clone()).is_err(),
            "空清单应 Err"
        );
        assert!(!out.exists(), "非法提取不应产生文件: {}", out.display());
        let after = worker.mem_bytes(doc_id).expect("应取回操作后镜像");
        assert_eq!(after, before, "非法提取后当前镜像原样（无副作用）");
    }

    /// 拆分 [1,3] 区间：新文件 = 源区间页原序（3 页）；当前 = 源删 {1,2,3} 幸存序（n-3 页）；
    /// 撤销恢复操作前镜像 → 回 N 页（新文件仍在）。
    #[test]
    fn split_to_file_extracts_range_and_trims_document() {
        if crate::pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let p = target_pdf_paths()[0].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let n = dims.len();
        assert!(n >= 4, "夹具页数需 ≥4，实际 {n}");
        // 原图 oracle：页 0..3（区间页 + 幸存首页）。
        let orig0 = render_page_probe(&worker, doc_id, 0);
        let orig1 = render_page_probe(&worker, doc_id, 1);
        let orig2 = render_page_probe(&worker, doc_id, 2);
        let orig3 = render_page_probe(&worker, doc_id, 3);
        let out = wave7b_output("split-range");
        let before = worker.mem_bytes(doc_id).expect("应取回操作前镜像");
        let res = worker.split_to_file(doc_id, (1, 3), out.clone()).expect("拆分应成功");
        // 算式：区间长 = 3 - 1 + 1 = 3 → 新文件 3 页；剩余 = n - 3 页。
        assert_eq!(res.0, 3, "新文件页数 = 区间长");
        assert_eq!(res.1.count, n - 3, "当前剩余页数 = n-3");
        // ① 新文件：区间页按原序（0 基 1..3），尺寸 + 渲染逐页对齐源。
        let new_bytes = std::fs::read(&out).expect("新文件应存在且可读");
        let (_, d_new, dims_new) = PdfWorker::open_and_dims(new_bytes).expect("新文件应可打开");
        let expect_dims_new: Vec<(f32, f32)> = dims[1..=3].to_vec();
        assert_eq!(dims_new, expect_dims_new, "新文件尺寸 = 源区间原序");
        let new_probes = render_page_probes(&worker, d_new, 0, 3);
        assert_eq!(new_probes, vec![orig1, orig2, orig3], "新文件逐页渲染 = 源区间页");
        // ② 当前文档：尺寸 = 源删 {1,2,3} 幸存序；首页渲染仍为源页 0。
        let (_, d_cur, dims_cur) = PdfWorker::open_and_dims(res.1.after.clone()).expect("当前镜像可打开");
        let expect_dims_cur: Vec<(f32, f32)> = dims
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 1 && *i != 2 && *i != 3)
            .map(|(_, d)| *d)
            .collect();
        assert_eq!(dims_cur, expect_dims_cur, "当前页尺寸 = 源删 {{1,2,3}} 幸存序");
        let cur0 = render_page_probe(&worker, d_cur, 0);
        assert_eq!(cur0, orig0, "当前首页渲染 = 源页 0");
        // 撤销拆分：恢复操作前镜像 → 当前回 N 页（区间移除部分回位），新文件仍在。
        worker.mem_restore_bytes(doc_id, res.1.before).expect("撤销恢复应成功");
        let restored = worker.mem_bytes(doc_id).expect("应取回恢复后镜像");
        let (_, _d_r, dims_r) =
            PdfWorker::open_and_dims(restored.clone()).expect("恢复后镜像可打开");
        assert_eq!(dims_r, dims, "撤销后页尺寸序列还原");
        assert_eq!(restored, before, "撤销后镜像 == 拆分前");
        assert!(out.exists(), "撤销后新文件仍存在: {}", out.display());
    }

    /// 拆分非法输入（start>end / 终点越界 / 覆盖全部页）→ Err 且零副作用（不产生文件、当前不变）。
    #[test]
    fn split_to_file_rejects_invalid_without_side_effect() {
        if crate::pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let p = target_pdf_paths()[0].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let n = dims.len();
        assert!(n >= 2, "夹具页数需 ≥2，实际 {n}");
        let before = worker.mem_bytes(doc_id).expect("应取回操作前镜像");
        let out = wave7b_output("split-invalid");
        assert!(
            worker.split_to_file(doc_id, (2, 1), out.clone()).is_err(),
            "start>end 应 Err"
        );
        assert!(
            worker.split_to_file(doc_id, (0, n), out.clone()).is_err(),
            "终点越界应 Err"
        );
        // 覆盖全部页（剩余 0 页）→ Err（防 0 页文档，对齐批删保留 ≥1 页不变式）。
        assert!(
            worker.split_to_file(doc_id, (0, n - 1), out.clone()).is_err(),
            "删光全部页应 Err"
        );
        assert!(!out.exists(), "非法拆分不应产生文件: {}", out.display());
        let after = worker.mem_bytes(doc_id).expect("应取回操作后镜像");
        assert_eq!(after, before, "非法拆分后当前镜像原样（无副作用）");
    }

    /// 合并：当前 N 页 + 合并源（pdf测试2.pdf，m 页）→ 总页 N+m、前 N 页尺寸 = 原、
    /// 合并字节（读端已走大小上界）追加：合并后总页数 = n+m、前 n 页尺寸 = 原、
    /// 尾部 m 页尺寸 = 合并源、尾部前 2 页渲染哈希 = 源2 各页；撤销 → 回 N 页且镜像还原。
    #[test]
    fn merge_from_bytes_appends_and_undo_restores() {
        if crate::pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let cur_p = target_pdf_paths()[0].clone(); // pdf测试.pdf：当前文档
        let src_p = target_pdf_paths()[1].clone(); // pdf测试2.pdf：合并源
        assert!(cur_p.exists() && src_p.exists(), "两夹具应存在");
        let bytes = std::fs::read(&cur_p).expect("读取当前 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let n = dims.len();
        assert!(n >= 1, "夹具页数需 ≥1，实际 {n}");
        let orig_first2 = render_page_probes(&worker, doc_id, 0, 2);
        let before = worker.mem_bytes(doc_id).expect("应取回操作前镜像");
        // 合并源：打开一次复用（d_src 供渲染基准、src_dims 供尺寸断言）。
        let (_, d_src, src_dims) = {
            let src_bytes = std::fs::read(&src_p).expect("读取合并源应成功");
            PdfWorker::open_and_dims(src_bytes).expect("合并源应可打开")
        };
        let m = src_dims.len();
        assert!(m >= 2, "合并源页数需 ≥2（尾部 2 页断言），实际 {m}");
        let src_first2 = render_page_probes(&worker, d_src, 0, 2);
        // 新消息形状：view 侧读字节 → `merge_from_bytes`（worker 不再读文件）。
        let src_bytes = std::fs::read(&src_p).expect("读取合并源字节应成功");
        let mem = worker
            .merge_from_bytes(doc_id, src_bytes)
            .expect("合并应成功");
        // 算式：合并后总页数 = n（当前）+ m（源全页）。
        assert_eq!(mem.count, n + m, "合并后总页数 = n+m");
        let (_, d_merged, all_dims) =
            PdfWorker::open_and_dims(mem.after.clone()).expect("合并后镜像可打开");
        assert_eq!(all_dims[..n].to_vec(), dims, "合并后前 n 页尺寸 = 原");
        assert_eq!(all_dims[n..].to_vec(), src_dims, "合并后尾部 m 页 = 源2 各页");
        let merged_tail2 = render_page_probes(&worker, d_merged, n, 2);
        assert_eq!(merged_tail2, src_first2, "合并后尾部 2 页渲染 = 源2 各页");
        // 撤销：恢复操作前镜像 → 回 n 页、前 2 页渲染还原、镜像 == 合并前。
        worker.mem_restore_bytes(doc_id, mem.before).expect("撤销应成功");
        let restored = worker.mem_bytes(doc_id).expect("应取回恢复后镜像");
        let (_, d_r, dims_r) =
            PdfWorker::open_and_dims(restored.clone()).expect("恢复后镜像可打开");
        assert_eq!(dims_r, dims, "撤销后页尺寸还原");
        let r_first2 = render_page_probes(&worker, d_r, 0, 2);
        assert_eq!(r_first2, orig_first2, "撤销后前 2 页渲染还原");
        assert_eq!(restored, before, "撤销后镜像 == 合并前");
    }

    /// 合并**坏字节**（损坏非 PDF / 空字节）→ Err 且当前文档不变（零副作用）。「合并源
    /// 不存在」的路径已在 view 侧（读端 `File::open` → `last_modify_error`）拦截，worker
    /// 只收「已走读端大小上界的字节」，故本侧不再测路径缺失。
    #[test]
    fn merge_from_bytes_rejects_bad_bytes_and_keeps_doc() {
        if crate::pdf::test_util::missing_pdf_fixture() {
            eprintln!("SKIP: missing target/pdf测试*.pdf（夹具不入库，本机无真 PDF）");
            return;
        }
        let _guard = crate::pdf::test_util::lock().lock().unwrap();
        let p = target_pdf_paths()[0].clone();
        let bytes = std::fs::read(&p).expect("读取 PDF 应成功");
        let (worker, doc_id, dims) = PdfWorker::open_and_dims(bytes).expect("打开应成功");
        let n = dims.len();
        assert!(n >= 1, "夹具页数需 ≥1，实际 {n}");
        let before = worker.mem_bytes(doc_id).expect("应取回操作前镜像");
        // 损坏字节（非 PDF，解析护栏快速 Err）→ Err。
        assert!(
            worker.merge_from_bytes(doc_id, b"this is not a pdf at all".to_vec()).is_err(),
            "损坏字节应 Err"
        );
        // 空字节（无页面）→ Err（0 页拒绝 / 解析失败）。
        assert!(
            worker.merge_from_bytes(doc_id, Vec::new()).is_err(),
            "空字节源应 Err"
        );
        // 当前文档不变（stores 镜像原样）。
        let after = worker.mem_bytes(doc_id).expect("应取回操作后镜像");
        assert_eq!(after, before, "非法合并后当前镜像原样（无副作用）");
    }
