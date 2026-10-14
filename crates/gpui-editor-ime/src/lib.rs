//! 共享 IME 状态机核心（M2.5：双份 `EntityInputHandler` 合一）。
//!
//! 原先 `src/editor/view/input.rs`（编辑器，按 byte 处理）与 `src/ui/input.rs`
//! （单行输入框，按 utf16 处理）各写了一套同名同构的组合逻辑。两套在 gpui 的
//! `EntityInputHandler` 界面（UTF-16 偏移）与各自文档内部（**都**是 UTF-8 字节）
//! 之间做的换算与落地编辑高度同构、只有少量断言不同。
//!
//! 设计：
//! - 状态机本体按 **byte** 单位计算（文档内部坐标本来就是字节）；
//! - UTF-16 ↔ byte 是 gpui 输入回路进/出的边界单位，换算委托给宿主
//!   [`ImeTextHost`]——编辑器用 rope 支撑的 O(log n) 换算、单行输入框直接
//!   用 `&str` 遍历（[`utf16_to_byte`]/[`byte_to_utf16`]），两侧语义一致：
//!   代理对内部偏移取字符起点、越界夹到边界；
//! - 两侧仅通过宿主 trait 的小面差异承载行为差异（是否维护 marked / 组合内
//!   光标、落地编辑的手段与 undo 意图、`selected_text_range.reversed`），
//!   **不改变任何一侧的对外行为**：编辑器的合成/确认/编辑/候选、committed 文本
//!   与 marked 区间语义、光标行为保持原样；单行输入框仍把 `replace_and_mark`
//!   当普通替换处理。

use std::ops::Range;

use gpui::{Context, UTF16Selection};

use gpui_editor_base::EditIntent;

/// 文本宿主（**byte 语义**）：IME 状态机通过它读写文档。偏移一律为 UTF-8 字节，
/// UTF-16 ↔ byte 换算与落地编辑由宿主按其存储结构提供。
pub trait ImeTextHost: Sized {
    /// 全文 UTF-16 code unit 数（gpui 的 `text_length_utf16`）。
    fn len_utf16(&self) -> usize;

    /// 字节偏移 → UTF-16 偏移（越界夹到边界）。
    fn byte_to_utf16(&self, byte: usize) -> usize;

    /// UTF-16 偏移 → 字节偏移（落在代理对中间时取该字符起点、越界夹到文本末尾）。
    fn utf16_to_byte(&self, offset_utf16: usize) -> usize;

    /// 字节区间文本；容错（越界/逆序不 panic），取不到返回 None。
    fn text_in_bytes(&self, range: Range<usize>) -> Option<String>;

    /// 当前选区（字节，已归一化为 start..end）。
    fn selection_bytes(&self) -> Range<usize>;

    /// 是否反选（`head < anchor`），驱动 `selected_text_range.reversed`。
    fn selection_reversed(&self) -> bool {
        false
    }

    /// 落定选区（字节）。编辑器据此重建 `Selection` 并重启光标闪烁。
    fn set_selection_bytes(&mut self, range: Range<usize>);

    /// 当前组合中（marked）区间（字节），无组合 = None。
    fn marked_bytes(&self) -> Option<Range<usize>>;

    /// 写/清组合中（marked）区间（字节）。
    fn set_marked_bytes(&mut self, range: Option<Range<usize>>);

    /// 是否维护 marked（组合下划线 + 组合内光标 `new_selected_range`）。
    /// 编辑器 = true；单行输入框 = false（不画下划线，`replace_and_mark` 按普通
    /// 替换处理、忽略组合内光标）——与两侧现状一致。
    fn supports_marked(&self) -> bool {
        true
    }

    /// 组合更新（`mark=true` 且新文本非空）后落定**组合内光标/选区**。
    ///
    /// `base_byte` = 组合起点字节；`new_text` = 组合当前文本；
    /// `new_selected_range` = 平台给的组合内区间（**相对组合文本起点**的 UTF-16
    /// 偏移；`None` = 光标落文本末尾）。
    ///
    /// 共享默认（编辑器/ui 语义）：把 `new_selected_range` 当 marked 文本内的
    /// 选区、光标落 head（区间末尾）。WYSIWYG 覆盖为「只取 `sel.start` 摆光标、
    /// 不回写 sel_anchor」——与其现状（`replace_and_mark` 仅按
    /// `offset_from_utf16(new_text, sel.start)` 摆 caret、不建选区）一致，避免把
    /// WYSIWYG 行为改成 editor 的。
    fn set_composition_caret(
        &mut self,
        base_byte: usize,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
    ) {
        let end = base_byte + new_text.len();
        let (s, e) = match new_selected_range {
            Some(sel) => (
                base_byte + utf16_to_byte(new_text, sel.start),
                base_byte + utf16_to_byte(new_text, sel.end),
            ),
            None => (end, end),
        };
        self.set_selection_bytes(s..e);
    }

    /// 落地一次替换（byte 区间 → 新文本）。约定：落地后光标停在新文本末尾；
    /// `intent` 描述 undo 事务粒度（无 undo 的宿主忽略）。
    fn apply_edit(
        &mut self,
        range: Range<usize>,
        text: &str,
        intent: EditIntent,
        cx: &mut Context<Self>,
    );
}

/// UTF-16 code unit 总数。
pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// 字节偏移 → 文本内 UTF-16 偏移（越界夹到边界）。
pub fn byte_to_utf16(s: &str, byte: usize) -> usize {
    s[..byte.min(s.len())].encode_utf16().count()
}

/// UTF-16 偏移 → 文本内字节偏移。落在代理对内部时取该字符起点（IMS 只给
/// code-unit 粒度，我们必须停留在 UTF-8 字符边界上）；越界夹到文本末尾。
pub fn utf16_to_byte(s: &str, offset_utf16: usize) -> usize {
    let mut bytes = 0usize;
    let mut units = 0usize;
    for ch in s.chars() {
        if units >= offset_utf16 {
            break;
        }
        let cu = ch.len_utf16();
        if units + cu > offset_utf16 {
            break;
        }
        units += cu;
        bytes += ch.len_utf8();
    }
    bytes
}

/// 把宿主接口的 UTF-16 区间（`None` = 替换当前组合/选区）解析为 byte 区间。
fn byte_range<H: ImeTextHost>(host: &H, range_utf16: Option<Range<usize>>) -> Range<usize> {
    match range_utf16 {
        Some(r) => host.utf16_to_byte(r.start)..host.utf16_to_byte(r.end),
        None => host
            .marked_bytes()
            .unwrap_or_else(|| host.selection_bytes()),
    }
}

/// `replace_text_in_range` / `replace_and_mark_text_in_range` 共用入口。
///
/// `mark = true` 表示组合更新/确认（编辑器侧走 Atomic、维护 marked 并按
/// `new_selected_range` 摆组合内光标）；`mark = false` 表示直输/提交（编辑器侧
/// 走 Typing、结束后清 marked，以免下一次组合把已提交文本当成旧 marked 覆盖）。
/// 单行输入框（`supports_marked = false`）两种都只落地替换，光标由 `apply_edit`
/// 放到新文本末尾——与现状一致。
pub fn replace<H: ImeTextHost>(
    host: &mut H,
    range_utf16: Option<Range<usize>>,
    new_text: &str,
    new_selected_range: Option<Range<usize>>,
    mark: bool,
    cx: &mut Context<H>,
) {
    let byte_range = byte_range(host, range_utf16);
    let intent = if mark {
        EditIntent::Atomic
    } else {
        EditIntent::Typing
    };
    host.apply_edit(byte_range.clone(), new_text, intent, cx);
    if !mark && host.supports_marked() {
        // 提交/直输会结束一次 IME 组合：Windows 提交候选只发
        // `replace_text_in_range(None, …)`（不调 unmark_text），不清掉 marked 的
        // 话下一次组合会把已提交文本当成旧 marked 区域再次替换。
        host.set_marked_bytes(None);
    } else if mark && host.supports_marked() {
        if new_text.is_empty() {
            // 组合取消：空替换已把光标落在区间起点（`apply_edit` 的约定），
            // 只需清掉 marked。
            host.set_marked_bytes(None);
        } else {
            host.set_marked_bytes(Some(byte_range.start..byte_range.start + new_text.len()));
            // `new_selected_range` 是**相对被标记文本（new_text）起点**的 UTF-16
            // 偏移（AppKit setMarkedText 的 selectedRange 语义），不是文档绝对
            // 偏移——当绝对偏移用会导致光标跳文档开头附近的旧崩溃（P2 回归）。
            // 落定组合内光标/选区的具体姿势由宿主定（编辑器=选区+head，
            // WYSIWYG=只用 sel.start 摆光标），见 [`ImeTextHost::set_composition_caret`]。
            host.set_composition_caret(byte_range.start, new_text, new_selected_range);
        }
    }
}

/// `unmark_text`：清掉组合中标记（无 undo/脏标副作用）。
pub fn unmark<H: ImeTextHost>(host: &mut H) {
    host.set_marked_bytes(None);
}

/// `marked_text_range`：把宿主的 byte marked 区间换算成 UTF-16 回路区间。
pub fn marked_text_range<H: ImeTextHost>(host: &H) -> Option<Range<usize>> {
    host.marked_bytes()
        .map(|r| host.byte_to_utf16(r.start)..host.byte_to_utf16(r.end))
}

/// `selected_text_range`：byte 选区 → UTF-16 回路的选区。
pub fn selected_text_range<H: ImeTextHost>(host: &H) -> UTF16Selection {
    let sel = host.selection_bytes();
    UTF16Selection {
        range: host.byte_to_utf16(sel.start)..host.byte_to_utf16(sel.end),
        reversed: host.selection_reversed(),
    }
}

/// `text_for_range`：UTF-16 区间 → 文档文本。
pub fn text_for_range<H: ImeTextHost>(host: &H, range_utf16: Range<usize>) -> Option<String> {
    let s = host.utf16_to_byte(range_utf16.start);
    let e = host.utf16_to_byte(range_utf16.end);
    host.text_in_bytes(s..e)
}

/// `set_selected_text_range` 的共享部分：UTF-16 区间 → 落定 byte 选区。
///
/// 注意：**不带 `notify`**——两侧宿主各自保留通知时机（编辑器当前不 notify、
/// 单行输入框要 notify），保持行为逐点不变。
pub fn set_selected_text_range<H: ImeTextHost>(host: &mut H, range_utf16: Range<usize>) {
    let s = host.utf16_to_byte(range_utf16.start);
    let e = host.utf16_to_byte(range_utf16.end);
    host.set_selection_bytes(s..e);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_helpers_round_trip_cjk_and_surrogate() {
        // 🎉 = U+1F389：2 个 UTF-16 code unit、4 字节 UTF-8；中 = 1 单元、3 字节。
        let s = "a🎉中b";
        assert_eq!(utf16_len(s), 5);
        assert_eq!(byte_to_utf16(s, 0), 0);
        assert_eq!(byte_to_utf16(s, 1), 1);
        assert_eq!(byte_to_utf16(s, 5), 3, "a🎉 = 1 + 2 单元");
        assert_eq!(byte_to_utf16(s, s.len()), 5);
        // 越界字节夹到末尾
        assert_eq!(byte_to_utf16(s, 999), 5);

        assert_eq!(utf16_to_byte(s, 0), 0);
        assert_eq!(utf16_to_byte(s, 3), 5, "a🎉 = 1 + 4 字节");
        assert_eq!(utf16_to_byte(s, 5), s.len());
        assert_eq!(utf16_to_byte(s, 999), s.len(), "越界夹到末尾");
    }

    #[test]
    fn utf16_to_byte_stays_on_char_boundary_mid_surrogate() {
        let s = "🎉X";
        // 落在代理对中间（offset 1）：取该字符起点 → 0
        assert_eq!(utf16_to_byte(s, 1), 0);
        // 越过整个代理对（offset 2）才前进 4 字节
        assert_eq!(utf16_to_byte(s, 2), 4);
        assert_eq!(utf16_to_byte(s, 3), 5);
    }

    #[test]
    fn byte_range_falls_back_to_marked_then_selection() {
        struct Fake {
            marked: Option<Range<usize>>,
            sel: Range<usize>,
        }
        impl ImeTextHost for Fake {
            fn len_utf16(&self) -> usize {
                unreachable!()
            }
            fn byte_to_utf16(&self, b: usize) -> usize {
                b
            }
            fn utf16_to_byte(&self, u: usize) -> usize {
                u
            }
            fn text_in_bytes(&self, _r: Range<usize>) -> Option<String> {
                unreachable!()
            }
            fn selection_bytes(&self) -> Range<usize> {
                self.sel.clone()
            }
            fn set_selection_bytes(&mut self, _r: Range<usize>) {}
            fn marked_bytes(&self) -> Option<Range<usize>> {
                self.marked.clone()
            }
            fn set_marked_bytes(&mut self, _r: Option<Range<usize>>) {}
            fn apply_edit(
                &mut self,
                _r: Range<usize>,
                _t: &str,
                _i: EditIntent,
                _cx: &mut Context<Self>,
            ) {
            }
        }
        // None 区间：有 marked 用 marked，无 marked 用选区
        let h = Fake {
            marked: Some(3..5),
            sel: 0..0,
        };
        assert_eq!(byte_range(&h, None), 3..5);
        let h = Fake {
            marked: None,
            sel: 7..9,
        };
        assert_eq!(byte_range(&h, None), 7..9);
        assert_eq!(byte_range(&h, Some(1..2)), 1..2);
    }
}
