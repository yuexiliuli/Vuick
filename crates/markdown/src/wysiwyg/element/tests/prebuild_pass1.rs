/// Step4-① Pass1 预建对拍：预建 fast path（`frame::build_prebuild_pass1` +
/// `frame::pass1_from_prebuild`）与既有串行首建 path（`frame::pass1_base_layout`
/// 无旧布局）在**同一 doc/主题/宽度**下逐块 `(高, src_line)` 完全一致——覆盖段落/
/// 中文/emoji/标题/列表/引述/分隔/空行/代码围栏/mermaid/图片/表格全分支。任何公式
/// 漂移（估算行数/卡高/图片占位/表格高）会在此现形：首帧物化高若与既有 pass1 不同
/// → 滚动带边界跳，回归锁死。
#[gpui::test]
fn prebuild_pass1_matches_first_build_pass1(cx: &mut TestAppContext) {
    use crate::markdown::wysiwyg::frame::{
        build_prebuild_pass1, pass1_base_layout, pass1_from_prebuild,
    };
    use crate::markdown::wysiwyg::height_epoch;
    use gpui::App;
    cx.update(crate::test_support::bind_local_keymaps);
    let text = "# 标题一\n\n段落 一二三 CJK中文字符.line.\n\n- 列表项\n> 引述\n\n---\n\n\
普通段落 paragraph with 中文 and emoji 🚀 done\n\n\
```rust\nfn main() {}\nlet x = 1;\n```\n\n\
```mermaid\ngraph LR\nA-->B\n```\n\n\
| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n\n\
![alt 图](images/x.png)\n\n尾行 paragraph\n";
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(text));
        WyHost { ed }
    });
    let vcx: &mut VisualTestContext = vcx;
    vcx.update(|_window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let width = px(900.);
        let avail =
            (f32::from(width) - 2. * f32::from(config::markdown::CONTENT_EDGE_PAD_X_PX))
                .max(1.);
        let n = ed.read(cx).doc().block_count();
        assert!(n >= 14, "样本应覆盖全部块类型，实际 n={n}");
        let wbits = f32::from(width).to_bits();
        let ref_out = ed.update(cx, |e, _| {
            let theme = e.theme.clone();
            // —— 既有串行首建 path（参考）——
            let (rebuilt, base, _) = e.ensure_block_layout(n, height_epoch(0, wbits, 0));
            assert!(rebuilt, "首建应重建");
            pass1_base_layout(e.doc(), e, n, &base, None, &theme, avail, e.theme.para_line());
            let h = (0..n).map(|i| base.borrow().height(i)).collect::<Vec<_>>();
            let s = (0..n).map(|i| base.borrow().src_line(i)).collect::<Vec<_>>();
            // —— 预建 fast path：同一 doc 重新注入（模拟 rope 冷开 install 后的首帧）——
            *e.block_layout.borrow_mut() = None;
            let pre = build_prebuild_pass1(e.doc());
            e.prebuild_slot.replace(Some(pre));
            let (rebuilt2, base2, _) = e.ensure_block_layout(n, height_epoch(0, wbits, 0));
            assert!(rebuilt2);
            assert!(
                pass1_from_prebuild(e, &base2, &theme, avail, e.theme.para_line()),
                "预建守卫应放行（同一 doc/未编辑/无缩放）"
            );
            assert!(e.prebuild_slot.borrow().is_none(), "预建槽应已清空");
            let h2 = (0..n).map(|i| base2.borrow().height(i)).collect::<Vec<_>>();
            let s2 = (0..n).map(|i| base2.borrow().src_line(i)).collect::<Vec<_>>();
            (h, s, h2, s2)
        });
        assert_eq!(ref_out.0.len(), ref_out.2.len(), "两路径块数应一致");
        for i in 0..n {
            assert!(
                (ref_out.0[i] - ref_out.2[i]).abs() < 1e-3,
                "块 {i}: 串行高 {} vs 预建高 {}（kind 漂移）",
                ref_out.0[i],
                ref_out.2[i]
            );
            assert_eq!(ref_out.1[i], ref_out.3[i], "块 {i} src_line 应一致");
        }
    });
}
