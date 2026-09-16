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
/// 编辑器 ListState「两段式装配」的行数阈值：超过该行数的文档，打开瞬间只建一个
/// **视口小列表**（首帧<1ms 显示内容），再在下一帧 defer 里补建全文 N 条统一高度
/// （O(N) 一次，移出“打开瞬间”的关键路径）。小于等于该阈值仍同步全量装配（便宜）。
pub const EDITOR_LIST_DEFER_MIN_LINES: usize = 4_096;
/// 两段式装配的「首帧视口小列表」条数：需 ≥ 最高分辨率的可见行 + 过扫余量。
/// 512 行 × 22px ≈ 11k px 可视高度，远超任何单屏；上半屏以上滚动由 defer 同位帧补齐。
pub const EDITOR_LIST_PROVISIONAL_ROWS: usize = 512;
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
/// 侧边栏搜索结果条数上限。
pub const SEARCH_RESULT_CAP: usize = 200;
