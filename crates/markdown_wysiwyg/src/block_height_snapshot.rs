//! 块高恒等快照（防几何漂移护栏，替代已删的 `layout_tests`）。
//!
//! shard 整形并行面（`shape_docrow_shard` 及 `shape_*_shards` 系）已随预览引擎删除
//! 而真删，`shape_docrow` 是唯一生产几何实现。本测试把**代表性合成文档**（标题/
//! 段落/折行长行/列表/引注/代码/表格，与旧 parity 测试同源语料）在当前主题/宽度下
//! 各块的真实 `frame.block_height(k)` 钉成数值快照——任何几何改动（行高系数/折行
//! 宽/块公式/滚动条预留/图片占位高）都会在此显式红，值变更需**肉眼确认**后同步更新。
//! 严禁把已删 shard 面的符号引进本文件。

use gpui::{App, AppContext, TestAppContext, Window, px};

use crate::test_support::bind_local_keymaps;
use crate::wysiwyg::{WyEditor, build_frame};

/// 编辑宿主（复用 wysiwyg 测试惯例：封装 `WyEditor` 为根元素）。
struct WyHost {
    ed: gpui::Entity<WyEditor>,
}
impl gpui::Render for WyHost {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        self.ed.clone()
    }
}

/// 代表性合成文档：标题 / 空行 / 段落 / 折行长行 / 列表 / 引注 / 代码 / 表格 全覆盖。
fn representative_doc() -> &'static str {
    concat!(
        "# 标题\n",
        "\n",
        "普通短段落。\n",
        "\n",
        "the quick brown fox jumps over the lazy dog, and keeps jumping and ",
        "jumping over the fence again and again to make this line wrap into ",
        "several visual lines for sure.\n",
        "\n",
        "- 列表项\n",
        "\n",
        "> 引述行\n",
        "\n",
        "```rust\nfn main() {}\nlet x = 1;\n```\n",
        "\n",
        "| a | b |\n",
        "|---|---|\n",
        "| 1 | 2 |\n",
    )
}

/// 块高恒等快照：沿真实渲染管线（`build_frame` 全带宽整形）逐块钉死 `block_height(k)`。
#[gpui::test]
fn block_height_snapshot_representative_doc(cx: &mut TestAppContext) {
    cx.update(bind_local_keymaps);
    let src = representative_doc();
    let width = 400f32;
    let (host, vcx) = cx.add_window_view(|window, cx| {
        let ed = cx.new(|cx| WyEditor::new(window, cx).with_text(src));
        WyHost { ed }
    });
    let vcx: &mut gpui::VisualTestContext = vcx;
    vcx.update(|window, cx: &mut App| {
        let ed = host.read(cx).ed.clone();
        let frame = build_frame(ed.read(cx), window, px(width), (0., 1_000_000.));
        // 块高恒等快照：防几何漂移；值变更需肉眼确认后同步更新。
        // 顺序 = 标题 / 空行 / 短段 / 空行 / 折行长行(5 视觉行) / 空行 / 列表 / 空行 /
        //        引注 / 空行 / 代码(3 行) / 空行 / 表格 / 尾空行，共 14 块。
        let rounded: Vec<f32> = (0..frame.block_count())
            .map(|k| (f32::from(frame.block_height(k)) * 100.).round() / 100.)
            .collect();
        assert_eq!(
            rounded,
            vec![
                43.66, 23.2, 23.2, 23.2, 116.0, 23.2, 23.2, 23.2, 23.2, 23.2, 83.2, 23.2, 89.4,
                23.2,
            ],
            "块高恒等快照漂移：几何改动（行高/折行/块公式）需肉眼确认是否可接受后再同步更新"
        );
    });
}
