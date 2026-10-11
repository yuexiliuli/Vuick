//! 性能 / 解析 / 缓存相关的阈值上限常量。

/// 编辑器自动软换行阈值：超过该行数的文档不换行（保持滚动/打开流畅）。
pub const EDITOR_WRAP_MAX_LINES: usize = 30_000;
/// 超过该字节数的**文本类文件**（元数据判断，无需读盘）走「后台打开」流水线：
/// 读盘→归一化→Rope→行宽统计→(md)scan_blocks 全部在后台线程（统计与 scan_blocks
/// 还并行跑），UI 线程只做 O(1) 装配（`Buffer::from_parts`）。低于此值保持同步打开
/// （小而快，且保留既有测试/行为）。取 128KB：几 KB~几十 KB 的常见源码/笔记仍即时
/// 打开，再大（含中等 md）不再随体积线性冻住主线程。
pub const OPEN_ASYNC_MIN_BYTES: u64 = 128 * 1024;
/// 大文档「分块读盘渐进装配」：后台**每读满这么多归一化字节**（整行），就把这一块
/// 推进给 UI 线程追加进编辑器 —— 内容随读随显（+ 滚动条横向量程随块补齐），不再等
/// 整篇读完才挂载。取 1MB：主线程每次只做 O(1MB) 的 rope 追加 ≈ 毫秒级，不卡帧。
pub const OPEN_CHUNK_TARGET_BYTES: usize = 1024 * 1024;
/// 首个分块的下限：仅首块用它（更小，几十 KB 即出首屏），之后统一用
/// [`OPEN_CHUNK_TARGET_BYTES`]。
pub const OPEN_FIRST_CHUNK_BYTES: usize = 256 * 1024;
/// 大文档分块读盘的原始字节缓存大小（每次 `File::read` 读这么多；与发行块大小无关）。
pub const READ_BUF_BYTES: usize = 256 * 1024;
/// 编辑器 ListState **固定窗口虚拟化**的窗口条数：列表恒持这么多条目（O(1)，与文档行数无关），
/// 滚动位置由 `window_base`（窗口首行绝对行号）+ 窗口内相对滚动合成，
/// 目标滚出窗口即 `reset` 重定位（O(窗口)）。512 行 × 22px ≈ 11k px 可视高度，远超任何单屏。
pub const EDITOR_LIST_PROVISIONAL_ROWS: usize = 512;
/// 纵向最大滚动量底部的**额外**行数（在横向滚动条厚度之上再预留的余量）：
/// 滚到底时最后一行停在「横向滚动条之上 + 再多 8 行」之上，保证不被叠加的横向滚动条遮住。
pub const EDITOR_VSCROLL_BOTTOM_EXTRA_LINES: usize = 8;
/// 自动折叠候选（gutter 默认指示器）的**全量扫描行数上限**：≤该行数的文档打开/编辑
/// 时整篇扫一遍缩进折叠候选（O(行数)，几十万行毫秒级），gutter 默认显示可折叠指示器；
/// 超过则由「手动 Ctrl+Shift+[/点击」按需折叠（候选为空，避免超大文档每键 O(N) 重扫）。
pub const FOLD_CANDIDATE_FULL_MAX_LINES: usize = 200_000;
/// 侧边栏搜索结果条数上限。
pub const SEARCH_RESULT_CAP: usize = 200;
/// WYSIWYG 行整形缓存容量（按**块下标**计）：滚动/带重建复用已整形的块（文本/代码/
/// 表格），避免每档重建把可视带整批重 shape + 代码块重跑 syntect（对齐 `view` 的
/// 行虚拟化复用与 `editor` 「只重排变更区」）。超出后按最近最少使用淘汰。
pub const SHAPED_ROW_CACHE_CAP: usize = 16_384;
