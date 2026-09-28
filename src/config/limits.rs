//! 性能 / 解析 / 缓存相关的阈值上限常量。

/// 超过该体积的文档不做语法高亮（工作集过大，无法支撑换代场景；正常文档远小于此）。
#[allow(dead_code)] // 语法引擎（暂未接线）使用
pub const MAX_TEXT_BYTES: usize = 24 * 1024 * 1024;
/// 可见窗口**向前**多解析的行数（语法状态前缀），使跨越行边界的注释/字符串/
/// 代码块在大多数情况下也能正确配色。
#[allow(dead_code)] // 语法引擎（暂未接线）使用
pub const LOOKBACK_LINES: usize = 48;
/// 计算时向可见窗口上下各扩展的行数：滚动在小余量内命中缓存不重算。
/// 调小 → 每次重算更快、颜色更早落屏。
#[allow(dead_code)] // 语法引擎（暂未接线）使用
pub const WINDOW_PAD: usize = 16;
/// 单次**同步**解析的行数上限：超过则本次不着色（避免超大视口卡帧）。
#[allow(dead_code)] // 语法引擎（暂未接线）使用
pub const SYNC_MAX_PARSE_LINES: usize = 2000;
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
/// 渲染行数超过该阈值启用「窗口级懒解析」（大字文档不整篇物化）。
pub const WINDOWED_MIN_BLOCKS: usize = 4_000;
/// 窗口懒解析时在可见范围上下各多物化的块数（过扫余量，减少反复重解析）。
pub const WINDOWED_OVERDRAW_BLOCKS: usize = 24;
/// **自适应过扫**：实际余量 = max(基准 [`WINDOWED_OVERDRAW_BLOCKS`], 可见块数 × 系数)。
/// 高视口 / 快速滚动时缓冲随之变大，窗口重建次数变少（配合代码块高亮 LRU 缓存，
/// 滚动手感更跟手）；系数取 0.5 → 余量约半屏。上限见 [`WINDOWED_OVERDRAW_MAX`]。
pub const WINDOWED_OVERDRAW_FACTOR: f32 = 0.5;
/// 自适应过扫的块数上限（防超高视口把窗口拉得过大、每次重建过慢）。
pub const WINDOWED_OVERDRAW_MAX: usize = 120;
/// 窗口模式代码块语法高亮的小型 LRU 容量（按块数）：滚动时复用相邻窗口已解析的
/// 代码块（syntect 整块解析是滚动手感主成本），容量有限防滚遍整篇把结果全部驻留。
pub const CODE_HL_LRU_CAP: usize = 96;
/// 大跳转（一次新进窗口的代码块**块数**超过该值，或文本累计超过
/// [`CODE_HL_DEFER_BYTES`]）触发**延后分片解析**：交 `pending_code_hl` 队列，由后续帧
/// 按 [`CODE_HL_PARSE_BYTES_PER_FRAME`] × [`CODE_HL_PARSE_BLOCKS_PER_FRAME`] 预算逐步
/// 补色（首帧这些块先纯文本/继承上色，帧不卡）；未超阈值的同步解析（现状）。
/// 与编辑器「滚动延后上色 + 停稳补齐」同一廉价帧思路，换成**分片**而非跨线程。
pub const CODE_HL_DEFER_BLOCKS: usize = 2;
pub const CODE_HL_DEFER_BYTES: usize = 4 * 1024;
/// 延后队列每帧最多解析的代码块文本字节预算：debug 下 syntect ~1ms/8KB，8KB/帧把
/// 单帧摊薄在 ~1-2ms（release 进一步缩放）；剩余下帧续。
pub const CODE_HL_PARSE_BYTES_PER_FRAME: usize = 8 * 1024;
/// 延后队列每帧最多解析的代码块**块数**：大批小块（每块固定解析开销几十 µs~ms，
/// 与字节无关）也按块数封顶，避免一眼几十个小块把单帧重新打满。
pub const CODE_HL_PARSE_BLOCKS_PER_FRAME: usize = 8;
/// 侧边栏搜索结果条数上限。
pub const SEARCH_RESULT_CAP: usize = 200;
/// WYSIWYG 行整形缓存容量（按**块下标**计）：滚动/带重建复用已整形的块（文本/代码/
/// 表格），避免每档重建把可视带整批重 shape + 代码块重跑 syntect（对齐 `view` 的
/// 行虚拟化复用与 `editor` 「只重排变更区」）。超出后按最近最少使用淘汰。
pub const SHAPED_ROW_CACHE_CAP: usize = 16_384;
