use super::{
    WyEditor, build_frame, caret_geometry, cell_selection_display, display_to_source,
    layout_has_notdef, prewarm_band, quote_band_w, source_offset_at_local, source_to_display,
    table_block_height, table_cell_x0, table_row_top,
};
use gpui::{
    AppContext, Context, Entity, Render, TestAppContext, VisualTestContext, Window, point, px, size,
};

struct WyHost {
    ed: Entity<WyEditor>,
}
impl Render for WyHost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        self.ed.clone()
    }
}

/// 双编辑器宿主：同步 `set_source_text` 对照机 vs 后台 `install_open` 装配机。
struct DualWyHost {
    sync: Entity<WyEditor>,
    bg: Entity<WyEditor>,
}
impl Render for DualWyHost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        self.sync.clone()
    }
}

#[test]
fn byte_map_round_trip_is_identity_for_plain_text() {
    // 普通段落：渲染 == 源（map[k] = k，末位哨兵 = len）。
    let map: Vec<usize> = (0..=5).collect(); // "abcde" + 哨兵
    for j in 0..=5 {
        let src = display_to_source(&map, j);
        assert_eq!(src, j);
        assert_eq!(
            source_to_display(&map, src),
            j,
            "点渲染列 {j} → 源 {src} → 应回渲染列 {j}"
        );
    }
}

#[test]
fn hidden_markers_map_round_trip() {
    // `**bold**`：渲染文本 "bold"，map = [2,3,4,5,6]（哨兵 6 = 源段尾）。
    let map: Vec<usize> = vec![2, 3, 4, 5, 6];
    for j in 0..4 {
        let src = display_to_source(&map, j);
        assert_eq!(src, 2 + j);
        assert_eq!(
            source_to_display(&map, src),
            j,
            "点渲染列 {j} 与光标回渲染列应一致"
        );
    }
    // 光标在段尾（源 6）→ 渲染列 4（行尾）；点行尾（渲染列 4）→ 源段尾 6。
    assert_eq!(source_to_display(&map, 6), 4);
    assert_eq!(display_to_source(&map, 4), 6);
}

#[test]
fn cell_selection_display_intersects_with_cell_src() {
    // 单元格渲染 "abcd"，map = [8,9,10,11,12]（渲染字节→源，相对行首；哨兵 12）。
    let map = vec![8, 9, 10, 11, 12];
    // 选区完全落在格内 [9,11) → 渲染 [1,3)。
    assert_eq!(
        cell_selection_display(&map, 4, 8..12, 0, &(9..11)),
        Some((1, 3)),
        "格内选区应回渲染字节区间"
    );
    // 选区从格左侧跨入 → 钳到格起点，渲染 [0, …)。
    assert_eq!(
        cell_selection_display(&map, 4, 8..12, 0, &(5..11)),
        Some((0, 3)),
        "跨格起点选区应钳到格首"
    );
    // 选区跨出格终点 → 渲染 […, 4)（哨兵映射到格尾）。
    assert_eq!(
        cell_selection_display(&map, 4, 8..12, 0, &(10..20)),
        Some((2, 4)),
        "跨格终点选区应钳到格尾"
    );
    // 选区完全在格前 / 格后 / 空单元格 → 无交集。
    assert_eq!(cell_selection_display(&map, 4, 8..12, 0, &(0..8)), None);
    assert_eq!(cell_selection_display(&map, 4, 8..12, 0, &(12..15)), None);
    assert_eq!(cell_selection_display(&map, 4, 12..12, 0, &(0..20)), None);
    // 非零行首：绝对源区间 = 行首 + 格源区间。
    assert_eq!(
        cell_selection_display(&map, 4, 8..12, 100, &(109..111)),
        Some((1, 3)),
        "非零行首换算应正确"
    );
}

