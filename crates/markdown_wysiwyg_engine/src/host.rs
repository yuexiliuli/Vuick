//! `WyRenderHost`：宿主 `WyEditor` 向引擎渲染管线暴露的**只读宿主态** trait。
//!
//! 迁移规则（seam）：引擎的 `shape`/`frame`/`paint`/`hit` 函数一律以
//! `rd: &RefCell<WyRenderModel>`（显式参数 + 内部可变）＋ `host: &dyn WyRenderHost`
//! 承接原 `&WyEditor`——**rd 与 host 字段互不同时可变借用**：凡「只被渲染管线消费 +
//! 失效由文档编辑触发」的状态走 `rd`；凡「宿主事件/编辑逻辑直接读写」的状态（doc /
//! caret / editable / epub / 图片缓存实体 / 内存图 / 悬停与选中 / 选区 / 搜索高亮 /
//! 数学与代码缓存等）经本 trait 只读访问。
//!
//! 硬约束：**trait 方法不得访问 `self.rd`**（防 re-entrant 借用：引擎函数持 `rd`
//! 借用时经 `host` 再借 `rd` 即运行时双借崩溃）。凡实现里需要读 `rd` 的宿主逻辑
//! （如 `reading_col_shift` 读帧宽）都不进本 trait，改由引擎侧以 `rd` 参数计算。
//! 若干方法（`request_code_parse`）虽经 `&self` 内部可变，但只碰宿主独有状态
//! （`code_parse`/`code_parse_need`），不触碰 `rd`。

use std::ops::Range;
use std::sync::Arc;

use gpui::{Bounds, Entity, Image, Pixels, RetainAllImageCache, Resource};

use crate::code_parse::CodeParseLookup;
use crate::model::InlineImageRef;

/// 渲染管线所需的宿主只读面（见模块头注释；方法不得碰 `self.rd`）。
pub trait WyRenderHost {
    /// 文档（Rope 单一数据源）。
    fn doc(&self) -> &markdown::doc::Doc;
    /// 当前光标源字节偏移。
    fn caret(&self) -> usize;
    /// 是否可编辑（只读预览门控）。
    fn is_editable(&self) -> bool;
    /// EPUB 阅读模式。
    fn is_epub(&self) -> bool;
    /// 元素在窗口中的 bounds（paint 写入；命中换算用）。
    fn last_bounds(&self) -> Option<Bounds<Pixels>>;
    /// 查内存图片表（EPUB 虚拟 src → 字节）。
    fn memory_image(&self, src: &str) -> Option<Arc<Image>>;
    /// 图片加载资源（解析后的路径/URL → gpui 加载器资源）。
    fn image_load_resource(&self, src: &str) -> Resource;
    /// 图片加载缓存实体（元素 paint `load` 用）。
    fn image_cache(&self) -> Entity<RetainAllImageCache>;
    /// 只读图片头部固有尺寸（EPUB 内存图字节 / 本地文件；`lazy_ensure_image_size`
    /// 的宿主只读半段），返回 `(w, h)`。
    fn image_intrinsic_size(&self, src: &str) -> Option<(f32, f32)>;
    /// 查数学排版槽（`key` → 槽的克隆；未命中 `None`）。纯查询，不移动 LRU 次序
    /// （对齐宿主 `MathSlotCache::get`；最近使用由宿主 `sync_math` 每帧 touch 维护）。
    fn math_slot(&self, key: &str) -> Option<markdown_wysiwyg_math::MathSlot>;
    /// 查代码块高亮解析缓存（命中即 touch 最近使用，对齐宿主 `CodeParseCache::lookup`）。
    fn code_parse_lookup(&self, lang: &str, code: &str) -> CodeParseLookup;
    /// shape 期登记一个需要后台解析的代码块（幂等）。**只碰宿主独有缓存态，不碰 rd**。
    fn request_code_parse(&self, lang: &str, code: &str, src_line: usize);
    /// 当前获得焦点的 mermaid 卡（`src_line`；滚轮缩放门控用）。
    fn focused_mermaid(&self) -> Option<usize>;
    /// 当前悬浮的图片行（编辑铅笔按钮亮显）。
    fn hover_image(&self) -> Option<usize>;
    /// 当前选中（点击）的图片行。
    fn selected_image(&self) -> Option<usize>;
    /// 当前悬浮的行内图片（编辑按钮亮显）。
    fn hover_inline_image(&self) -> Option<InlineImageRef>;
    /// 当前选中（点击）的行内图片。
    fn selected_inline_image(&self) -> Option<InlineImageRef>;
    /// 当前选区（归一化源字节区间，由 `sel_anchor`+`caret` 现算）；无选区/零宽 `None`。
    fn selection(&self) -> Option<Range<usize>>;
    /// 搜索跳转高亮的 (源码行, 小写查询词) 展开后的**绝对源区间**（`highlight_search`
    /// 现算）；无高亮 `None`。
    fn search_highlight_range(&self) -> Option<Range<usize>>;
}
