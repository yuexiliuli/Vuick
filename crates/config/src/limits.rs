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
/// [`OPEN_CHUNK_TARGET_BYTES`]。取 256KB：首块越小首屏越早，但过小会让**首个消费批**
/// 收满通道容量（4×后续 1MB 块）再挂载、反而晚（实测 96KB→首挂 296ms vs 256KB→170ms）。
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
/// 「打开最近」每类（文件夹 / 文件）最近记录的**上限条数**：超过最旧的被挤出；
/// 面板四区域模式只展示前 10 条，完整列表模式展示全部（≤此上限）。
pub const RECENT_MAX: usize = 50;
/// 音乐播放器音乐目录的**默认值**（`music_dir` 未设置时读取的兜底路径；反斜杠
/// 是 Windows 路径字面量，与 `D:\music\music` 目录一致）。
pub const DEFAULT_MUSIC_DIR: &str = "D:\\music\\music";
/// 自动折叠候选（gutter 默认指示器）的**全量扫描行数上限**：≤该行数的文档打开/编辑
/// 时整篇扫一遍缩进折叠候选（O(行数)，几十万行毫秒级），gutter 默认显示可折叠指示器；
/// 超过则由「手动 Ctrl+Shift+[/点击」按需折叠（候选为空，避免超大文档每键 O(N) 重扫）。
pub const FOLD_CANDIDATE_FULL_MAX_LINES: usize = 200_000;
/// 括号标尺竖线的**视口回看行数**：配对已按 `open_line` 升序，标尺只迭代
/// `open_line ∈ [视口顶 − 回看, 视口底]` 的配对（二分窗口）——避免每帧对全文配对逐条
/// 算相交判定（大文档 1k+ 对 × 每帧 = 每键几 ms 白烧，实际视口内可见竖线个位数）。
/// 回看 = 覆盖「开在可视区上方、竖线仍伸进视口」的常规跨屏块（含数百~两千行的大
/// 函数/注释块）；**仅当配对开行在视口顶上方超过此值**（即已滚进超长块内部深处）才
/// 跳过——此时外层竖线离当前上下文过远、本就不具可见意义，视作省略。窗口按「pair
/// 密度 × (回看+视口高)」封顶每帧迭代量（病理文档 ~2k 行窗口内也仅数百对，~µs 级），
/// 回看再放宽不明显增成本、只决定「多长跨度的竖线还画」；任意有限值都无法覆盖
/// 整篇单块类病理输入（span > 回看的配对），属显式的省略取舍。
pub const BRACKET_GUIDE_WINDOW_LOOKBACK_LINES: usize = 2048;
/// 侧边栏搜索结果条数上限。
pub const SEARCH_RESULT_CAP: usize = 200;
/// 文件夹级内容搜索「总命中条数」上限：达到即截断并提示（不无限累积渲染分组）。
pub const FOLDER_SEARCH_RESULT_CAP: usize = 500;
/// 侧边栏「当前文件」搜索**文件名命中行**的**渲染**条数上限（Pattern 5.5 低成本护栏）：
/// 扫描层 [`SEARCH_RESULT_CAP`]（200，文件名 + 行内容共用一个配额）已把命中数 ≤200 封顶，
/// 本上限在**渲染物化侧**再兜一层——即使未来扫描上限放宽/删除，渲染行数也恒 ≤ 该值；
/// 超限只渲染前 `cap` 行 + 一行「已显示前 N 条…」指示（不截断底层命中缓存）。
pub const SEARCH_FILE_RESULTS_CAP: usize = 200;
/// 侧边栏「当前文件」搜索**行内容命中行**的**渲染**条数上限（同上护栏，配额独立于文件行）。
pub const SEARCH_LINE_RESULTS_CAP: usize = 200;
/// 侧边栏 **PDF 全文检索命中行**的**渲染**条数上限：PDF 检索结果（`search_snippets`）
/// 目前没有任何扫描上限，命中数可无界增长；渲染物化封顶到该值并追加一行「…」指示。
pub const SEARCH_PDF_HIT_CAP: usize = 200;
/// WYSIWYG 行整形缓存容量（按**块下标**计）：滚动/带重建复用已整形的块（文本/代码/
/// 表格），避免每档重建把可视带整批重 shape + 代码块重跑 syntect（对齐 `view` 的
/// 行虚拟化复用与 `editor` 「只重排变更区」）。超出后按最近最少使用淘汰。
pub const SHAPED_ROW_CACHE_CAP: usize = 16_384;
/// WYSIWYG 数学排版槽缓存容量（按**公式源码键**计）：每唯一 LaTeX 公式一张位图
/// （DPR2 下 ~10-100KB CPU 字节 + 1 个 GPU 纹理）。旧实现无上限 → 同一标签页持续
/// 打开不同公式文档时位图与纹理只涨不降；上限 + LRU 后驻留数与「可视带公式数 × 数倍
/// 余量」同阶（带内公式数 = 视口行数量级 ≤ 数十；1000 张对命中率与内存折中：1000×
/// ~100KB ≈ 百 MB 级峰值，远超任何单篇文档可并存的公式量）。
pub const MATH_SLOTS_CACHE_CAP: usize = 1000;
/// WYSIWYG 代码块高亮解析缓存容量（按 **(语言, 代码全文)** 键计）：整块 syntect 高亮
/// runs 缓存（纯 CPU 数据）。512 块覆盖常见文档交互面 + 长滚动窗口（大文档几十个
/// 代码块已足够往复滚动复用），超出按最近最少使用淘汰（命中率与内存折中）。
pub const CODE_PARSE_CACHE_CAP: usize = 512;
/// WYSIWYG 数学卡高估算缓存（按**公式源码键**计）的**条目上限**：每个互异公式源一条
/// `源→显示行数估算`（`math_line_height_h` 的确定性结果）。估算值可确定性重算，所以
/// 超限**不丢正确性、只丢缓存**（超限源退回每次 fold_plan 重建都重算 LaTeX→Unicode 转换
/// 的性能路径）。上限防长期编辑会话里「互异公式源只增不减、不随编辑回收」把缓存撑成
/// 内存无底洞。撞线即打一次 warn（见 content_sync::math_height_estimate）。
pub const MATH_LINE_MAP_CAP: usize = 10_000;

// ===== EPUB 解析安全上限（审查 epub#1 zip 炸弹 / 图片数 / 总量 DoS 面）=====

/// EPUB `read_zip_entry_bytes` 单条目**解压输出**上限（含容器/OPF/章节/图片等全部 zip 条目）；
/// 作用：压缩炸弹（zip bomb）从「解压无限内存」钳到 ~16MiB/条目。
pub const EPUB_MAX_CHAPTER_BYTES: usize = 16 * 1024 * 1024;

/// 单张图片**解压字节**上限（对齐 pdf COVER_SIZE_BUDGET 口径）：防单图超大分配。
pub const EPUB_MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;

/// 全书记住的最大图片**张数**上限（对齐 image 解码 MAX_GIF_FRAMES 同量级，512 ≈ 2×）。
pub const EPUB_MAX_IMAGE_COUNT: usize = 512;

/// 全部图片**常驻内存总字节**上限：图片张数×单张上限仍可到 16GiB，需总量再闸一道。
pub const EPUB_MAX_IMAGE_TOTAL_BYTES: usize = 256 * 1024 * 1024;

/// 整书拼接后的 Markdown 文本**总字节**上限：spine 章节再小，章数×内容也能撑爆内存。
pub const EPUB_MAX_MARKDOWN_BYTES: usize = 256 * 1024 * 1024;

/// spine **章节数**上限：防「数千章节各读 16MiB」的分散型总量攻击。
pub const EPUB_MAX_CHAPTER_COUNT: usize = 10_000;
