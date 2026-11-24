//! EPUB 读取模块。
//!
//! 一个 EPUB 是 OCF 容器（zip）：
//! - `mimetype`：固定首文件，`application/epub+zip`
//! - `META-INF/container.xml`：指出 OPF 路径
//! - `content.opf`：书籍元数据 + 清单(manifest) + 阅读顺序(spine)
//! - 每个章节是一个 XHTML 文件（spine 决定阅读顺序）
//!
//! 本模块把 EPUB 解析成**平铺的 XHTML 文本流**后转成 Markdown，交给
//! 统一 WyEditor 只读宿主渲染（`markdown::wysiwyg::WyEditor`，M5-B5 P4-2
//! 终态；`MarkdownPreview` 预览引擎已删）。
//! 图片以**内存字节**形式保存在 [`EpubDocument::images`]（虚拟 src → 字节），
//! 渲染时交给预览直接从内存解码显示，全程不写硬盘。

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use gpui::{Image, ImageFormat};

use config::limits::{
    EPUB_MAX_CHAPTER_BYTES, EPUB_MAX_CHAPTER_COUNT, EPUB_MAX_IMAGE_BYTES, EPUB_MAX_IMAGE_COUNT,
    EPUB_MAX_IMAGE_TOTAL_BYTES, EPUB_MAX_MARKDOWN_BYTES,
};
use config::markdown::EPUB_IMG_SCHEME;
use markdown::RowAlign;

/// XML 解析的节点数硬上界（P0-B2）：roxmltree 默认 u32::MAX，恶意放大 XML
/// 可把 16MiB 章节放大成数百 MiB 节点树；1M 节点对正常 EPUB 章节绰绰有余。
const MAX_XML_NODES: u32 = 1_000_000;

/// zip **条目总数**硬上界（P0 三件套末条）：`ZipArchive::new` 会**全量物化整份
/// 中央目录**，恶意 EPUB 可塞海量条目撑大分配；正常书籍条目数（容器 + OPF +
/// HTML 章节 + 图片 + 杂项）通常数百内，65536 已是经典 zip 工具单档硬上限
/// （ZIP64 之前的 0xFFFF 条目 / 单卷），对齐该经典上限做安全余量。
const EPUB_MAX_ZIP_ENTRIES: usize = 65_536;

/// 递归遍历 XHTML 树的显式深度上限（P0-B2）：正文 `walk_body` 与文本提取
/// `node_text_epub`/`walk_text_lines` 两个遍历族各自独立封顶，防恶意深层嵌套
/// 撑爆调用栈 / 无界展开。正常排版嵌套 <50 层，256 绰绰有余；合法书籍输出
/// 在限内保持逐字节一致（超限子树截断、不产出）。
const MAX_TREE_DEPTH: usize = 256;

/// 带安全上限解析 XML（节点数硬上界 + 禁 DTD + 元素深度预扫描）：
/// 三处 `Document::parse` 统一入口。
///
/// P0-B2：`roxmltree::Document::parse` 对深层嵌套元素会**递归**（2 万层即栈溢出
/// abort），`nodes_limit` 只限**节点数**、不限**深度**——所以解析前先做一次
/// **元素嵌套深度预扫描**，超 [`MAX_TREE_DEPTH`] 即视为不可信深层结构直接拒绝，
/// 不让 roxmltree 解析（否则 16MiB 恶意章节可先炸掉调用栈）。
fn parse_xml(text: &str) -> anyhow::Result<roxmltree::Document<'_>> {
    if max_xml_element_depth(text) > MAX_TREE_DEPTH {
        bail!("XML 元素嵌套深度超上限（>{MAX_TREE_DEPTH} 层），拒绝解析");
    }
    roxmltree::Document::parse_with_options(
        text,
        roxmltree::ParsingOptions {
            nodes_limit: MAX_XML_NODES,
            allow_dtd: false,
            ..roxmltree::ParsingOptions::default()
        },
    )
    .map_err(Into::into)
}

/// 预扫描 XML 的**元素嵌套最大深度**（迭代、O(n)、不建树）。
///
/// 只统计真实元素开/闭标签，其余一律跳过（这些结构内部可能含会误导计数的
/// `<`/`>`）：注释 `<!--…-->`、`<![CDATA[…]]>`、`<?…?>`、`<!…>` 声明（含
/// DOCTYPE 内部子集）整体跳过；元素标签内部按引号跳过属性值。
///
/// 安全前提：对 **well-formed XML** 计深精确（开 `+1`/闭 `-1`/自闭合 `/>` 不变）；
/// 名字起始判定必须含非 ASCII 首字节（XML 1.0 `NameStartChar` 允许 Unicode 字母，
/// 见 [`is_xml_name_start`]）——只认 ASCII 会让 `<α>`（U+03B1 首字节 0xCE）这类
/// **合法名**落入裸 `<` 分支，深 Unicode 名树零计数放行、绕过预扫描直入 roxmltree
/// 递归（300+ 层即 0xC00000FD 栈溢出）。
/// 对非法裸 `<`（ASCII 非名起始）只多跳一个字节——这类输入 roxmltree 本就拒绝
/// （不会走到深树），故不存在「漏判后真去解析深树」的路径。
fn max_xml_element_depth(xml: &str) -> usize {
    let bytes = xml.as_bytes();
    let mut i = 0usize;
    let mut depth = 0usize;
    let mut max_depth = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        match *bytes.get(i + 1).unwrap_or(&0) {
            // `<!`：注释 / CDATA / DOCTYPE 等声明，整体跳过
            b'!' => {
                if bytes.get(i + 2) == Some(&b'-') && bytes.get(i + 3) == Some(&b'-') {
                    match find_bytes(bytes, i + 4, b"-->") {
                        Some(end) => i = end + 3,
                        None => return max_depth,
                    }
                } else if bytes.get(i + 2) == Some(&b'[') {
                    match find_bytes(bytes, i + 9, b"]]>") {
                        Some(end) => i = end + 3,
                        None => return max_depth,
                    }
                } else {
                    i = skip_declaration(bytes, i);
                }
            }
            // `<?`：处理指令
            b'?' => match find_bytes(bytes, i + 2, b"?>") {
                Some(end) => i = end + 2,
                None => return max_depth,
            },
            // `</`：结束标签
            b'/' => {
                depth = depth.saturating_sub(1);
                i = skip_to_tag_end(bytes, i).saturating_add(1);
            }
            c if is_xml_name_start(c) => {
                // 开始标签：判断是否自闭合（`/>`）再计深
                let end = skip_to_tag_end(bytes, i);
                let self_closing = end > i && bytes[end.saturating_sub(1)] == b'/';
                if !self_closing {
                    depth += 1;
                    max_depth = max_depth.max(depth);
                }
                i = end.saturating_add(1);
            }
            // 裸 `<`（非法 XML，roxmltree 会拒绝）：跳过
            _ => i += 1,
        }
    }
    max_depth
}

/// 在 `bytes[start..]` 里找第一个子串 `pat`，返回其起始索引（找不到返回 `None`）。
fn find_bytes(bytes: &[u8], start: usize, pat: &[u8]) -> Option<usize> {
    if start >= bytes.len() || pat.is_empty() {
        return None;
    }
    let mut i = start;
    while i + pat.len() <= bytes.len() {
        if &bytes[i..i + pat.len()] == pat {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// 跳过 `<!…>` 声明（DOCTYPE/ENTITY 等）：尊重引号与内部子集 `[…]`，
/// 返回收尾 `>` 之后的下标（找不到则返回 `bytes.len()`）。
fn skip_declaration(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    let mut bracket = 0i32;
    let mut quote = 0u8;
    while i < bytes.len() {
        let c = bytes[i];
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
        } else if c == b'"' || c == b'\'' {
            quote = c;
        } else if c == b'[' {
            bracket += 1;
        } else if c == b']' {
            bracket -= 1;
        } else if c == b'>' && bracket <= 0 {
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

/// 跳过标签体的收尾 `>`（元素开始/结束标签共用），尊重引号；返回 `>` 下标。
fn skip_to_tag_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    let mut quote = 0u8;
    while i < bytes.len() {
        let c = bytes[i];
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
        } else if c == b'"' || c == b'\'' {
            quote = c;
        } else if c == b'>' {
            return i;
        }
        i += 1;
    }
    bytes.len()
}

/// XML 名字起始字符（宽松判据）。well-formed 名字必以字母/`_`/`:` 开头；
/// 且 XML 1.0 `NameStartChar` 允许**非 ASCII** 起始（Unicode 字母如 `α`=U+03B1，
/// UTF-8 首字节 ≥ 0x80）——非 ASCII 首字节一律视为名字起始即可：多算只朝更安全偏
/// （把非名起始当成名字开标签计深只会多计、不会漏计；真正非法名 roxmltree 本会
/// 拒绝，不会走到深树）。
fn is_xml_name_start(c: u8) -> bool {
    c >= 0x80 || c.is_ascii_alphabetic() || c == b'_' || c == b':'
}

/// EPUB 解析的安全硬上限（依赖注入式，可测）：压缩炸弹 / 图片数 / 总量 DoS 护栏。
/// 生产用 [`EpubLimits::default`]（对齐 `config::limits` 常量）；测试注入小值，
/// 用 KB 级内存造微型恶意 epub 验证各闸（K124：构造期内所有分配都有独立硬上界）。
#[derive(Clone, Copy, Debug)]
pub struct EpubLimits {
    /// 单 zip 条目解压输出上限（容器/OPF/章节/图片共用）。
    pub max_entry_bytes: usize,
    /// 单张图片解压字节上限。
    pub max_image_bytes: usize,
    /// 全书记住的最大图片张数。
    pub max_image_count: usize,
    /// 全部图片常驻内存总字节上限。
    pub max_image_total_bytes: usize,
    /// 整书拼接 Markdown 总字节上限。
    pub max_markdown_bytes: usize,
    /// spine 章节数上限。
    pub max_chapter_count: usize,
}

impl Default for EpubLimits {
    fn default() -> Self {
        Self {
            max_entry_bytes: EPUB_MAX_CHAPTER_BYTES,
            max_image_bytes: EPUB_MAX_IMAGE_BYTES,
            max_image_count: EPUB_MAX_IMAGE_COUNT,
            max_image_total_bytes: EPUB_MAX_IMAGE_TOTAL_BYTES,
            max_markdown_bytes: EPUB_MAX_MARKDOWN_BYTES,
            max_chapter_count: EPUB_MAX_CHAPTER_COUNT,
        }
    }
}

/// 解析后的 EPUB 文档：元数据 + 按 spine 顺序的章节 Markdown 文本 + 内存图片。
#[derive(Debug, Clone, Default)]
pub struct EpubDocument {
    /// 书名（dc:title）
    pub title: String,
    /// 作者（dc:creator）
    pub author: String,
    /// 按阅读顺序拼接的整书 Markdown 文本（每章以章标题开始）
    pub markdown: String,
    /// 章节数
    pub chapter_count: usize,
    /// 内存图片表：markdown 里的虚拟 src（`epubimg://xxx`）→ 图片字节。
    /// 渲染时由预览查表直接解码，不落盘。
    pub images: HashMap<String, Arc<Image>>,
    /// 脚注表：注记号 → 注释正文（正文里 `[^n]` 悬浮卡片的数据源）。
    pub notes: std::collections::BTreeMap<usize, String>,
}

/// 从 `.epub` 文件路径解析文档（用 [`EpubLimits::default`] 安全上限）。
pub fn parse_epub_file(path: &Path) -> Result<EpubDocument> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("无法打开 EPUB 文件：{}", path.display()))?;
    parse_epub_reader(file)
}

/// 从任意 `Read + Seek` 读取器解析 EPUB（zip 需要 seek），用默认安全上限。
pub fn parse_epub_reader<R: Read + std::io::Seek>(reader: R) -> Result<EpubDocument> {
    parse_epub_reader_limited(reader, EpubLimits::default())
}

/// 带注入上限地解析（生产默认 [`EpubLimits::default`]；测试注入小值验证各闸）。
/// 全部 DoS 面在此收敛：单条目 take 限长、图片数、图片总量、整书 markdown、章节数。
fn parse_epub_reader_limited<R: Read + std::io::Seek>(
    reader: R,
    limits: EpubLimits,
) -> Result<EpubDocument> {
    let mut archive = zip::ZipArchive::new(reader).context("无效的 EPUB（zip 容器读取失败）")?;

    // zip 条目总数硬上界：`ZipArchive::new` 已全量物化中央目录，超限即整档拒绝
    // （结构条目三件套 + 海量垃圾条目都在 `len()` 内，一并计入）。
    if archive.len() > EPUB_MAX_ZIP_ENTRIES {
        bail!(
            "zip 条目总数超上限（{} > {EPUB_MAX_ZIP_ENTRIES}），拒绝解析",
            archive.len()
        );
    }

    // 1) 定位 OPF：读 META-INF/container.xml
    let container_xml = read_zip_entry(
        &mut archive,
        "META-INF/container.xml",
        limits.max_entry_bytes,
    )
    .context("EPUB 缺少 META-INF/container.xml")?;
    let opf_path = find_opf_path(&container_xml)?;

    // 2) 读取 OPF：解析 metadata + manifest + spine
    let opf_xml = read_zip_entry(&mut archive, &opf_path, limits.max_entry_bytes)
        .with_context(|| format!("读取 OPF 失败：{opf_path}"))?;
    let (title, author, spine_hrefs) = parse_opf(&opf_xml, &opf_path)?;
    if spine_hrefs.len() > limits.max_chapter_count {
        bail!(
            "EPUB 章节数超上限（{} > {}），拒绝解析",
            spine_hrefs.len(),
            limits.max_chapter_count
        );
    }

    // 3) 按 spine 顺序逐章读取 XHTML → 转 Markdown；图片保留在内存（不落盘）
    let mut markdown = String::new();
    let mut images: HashMap<String, Arc<Image>> = HashMap::new();
    // 全局脚注表（键经「跨章冲突重编号」后整书唯一）
    let mut notes: std::collections::BTreeMap<usize, String> = std::collections::BTreeMap::new();
    // 跨章冲突时顺延分配的「下一个空闲全局脚注号」（随原号保留的号自动越过）
    let mut note_next_free: usize = 1usize;
    let base_dir = parent_zip_dir(&opf_path);
    // 虚拟 src → 字段/条目的去重（同一 zip 内路径只解码一次进内存）
    let mut img_cache: HashMap<String, String> = HashMap::new();
    let mut img_counter = 0usize;
    let mut img_total_bytes = 0usize;
    for href in &spine_hrefs {
        let entry_path = join_zip_path(&base_dir, href);
        let chapter_html = read_zip_entry(&mut archive, &entry_path, limits.max_entry_bytes)
            .with_context(|| format!("读取章节失败：{entry_path}"))?;
        let chapter_dir = {
            let s = href.rsplit_once('/').map(|x| x.0).unwrap_or("").to_string();
            if s.is_empty() {
                base_dir.clone()
            } else {
                join_zip_path(&base_dir, &s)
            }
        };
        let mut on_img = |src: &str, _alt: &str| -> Option<String> {
            if src.is_empty() {
                return None;
            }
            // 去重键 = 解析后的 zip **完整路径**，而非原始 src：不同目录章节共用同名
            // `src`（如 `images/p.png`）会指向 zip 内**不同**文件，按 src 去重会让
            // 后章命中首章条目 → 渲染出首章字节（错图）。
            let full = join_zip_path(&chapter_dir, src);
            if let Some(vsrc) = img_cache.get(&full) {
                return Some(vsrc.clone());
            }
            if img_counter >= limits.max_image_count {
                return None; // 图片数超限：不再收进内存（渲染以 alt 占位兜底）
            }
            let file = archive.by_name(&full).ok()?;
            let mut bytes = Vec::new();
            // 单张图片解压限长（take 钳峰值，防压缩炸弹把单张解到几百 MiB 先分配）。
            file.take(limits.max_image_bytes as u64 + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.is_empty() {
                return None;
            }
            if bytes.len() > limits.max_image_bytes {
                return None; // 声明小实大（超 take 哨兵）：拒绝
            }
            if img_total_bytes.saturating_add(bytes.len()) > limits.max_image_total_bytes {
                return None; // 图片总字节超限：拒绝后续图片（已有图片保留，渲染部分可见）
            }
            img_counter += 1;
            img_total_bytes += bytes.len();
            // 以文件真实签名推断格式（扩展名可能误导：如 00001.jpeg 实为 PNG）
            let format = image_format_for_bytes(&bytes)?;
            let ext = format_ext(format);
            let vsrc = format!("{EPUB_IMG_SCHEME}{img_counter}.{ext}");
            // 内存持有字节：Image::from_bytes(format, bytes)
            images.insert(vsrc.clone(), Arc::new(Image::from_bytes(format, bytes)));
            img_cache.insert(full, vsrc.clone());
            Some(vsrc)
        };
        // 本章独立脚注表（裸号 → 文本）：`xhtml_to_markdown` 按本章原始裸号登记；
        // 跨章冲突重编号 + 并入全局表在下一步统一处理（不再 first-wins 丢后章定义）。
        let mut chapter_notes: std::collections::BTreeMap<usize, String> =
            std::collections::BTreeMap::new();
        let chapter_md = xhtml_to_markdown(&chapter_html, &mut on_img, &mut chapter_notes);
        // 跨章冲突重编号：只对「本章定义了脚注」的裸号做换算——
        // 与先前章节登记号无冲突的原样保留（全书连续编号的书型行为不变）；
        // 冲突的（每章独立重新编号的书型）顺延取全局空闲号。纯引用号（其定义在
        // 其它章节，如全局编号书型正文章引用末尾注释章）保持原号，跨章引用不受影响。
        // 本章定义与正文引用共用同一张映射，保证 `[^N]`/`[!N]` 与 notes 键一致。
        let chapter_md = {
            let mut map: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
            for &local in chapter_notes.keys() {
                let g = if !notes.contains_key(&local) {
                    local
                } else {
                    while notes.contains_key(&note_next_free) {
                        note_next_free += 1;
                    }
                    let g = note_next_free;
                    note_next_free += 1;
                    g
                };
                if note_next_free <= g {
                    note_next_free = g + 1;
                }
                map.insert(local, g);
            }
            for (local, text) in chapter_notes {
                let g = *map.get(&local).unwrap_or(&local);
                notes.entry(g).or_insert(text);
            }
            renumber_note_markers(&chapter_md, &|n| *map.get(&n).unwrap_or(&n))
        };
        if chapter_md.trim().is_empty() {
            continue;
        }
        if markdown.len().saturating_add(chapter_md.len()) > limits.max_markdown_bytes {
            bail!(
                "EPUB 整书 Markdown 超上限（>{max} 字节），拒绝继续拼接",
                max = limits.max_markdown_bytes
            );
        }
        markdown.push_str(&chapter_md);
        markdown.push('\n');
    }

    Ok(EpubDocument {
        title,
        author,
        markdown,
        chapter_count: spine_hrefs.len(),
        images,
        notes,
    })
}

/// 由图片字节的魔数（签名）推断 gpui 图片格式，不信任扩展名。
/// 常见 EPUB 会出现「扩展名 .jpeg 但实为 PNG」的脏数据。
fn image_format_for_bytes(data: &[u8]) -> Option<ImageFormat> {
    if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some(ImageFormat::Png)
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(ImageFormat::Jpeg)
    } else if data.starts_with(b"GIF8") {
        Some(ImageFormat::Gif)
    } else if data.starts_with(b"RIFF")
        && data.get(8..12) == Some(b"WEBP")
    {
        Some(ImageFormat::Webp)
    } else if data.starts_with(b"BM") {
        Some(ImageFormat::Bmp)
    } else if data.starts_with(b"<svg")
        || data.starts_with(b"<?xml")
        || data
            .get(..128)
            .is_some_and(|s| String::from_utf8_lossy(s).contains("<svg"))
    {
        Some(ImageFormat::Svg)
    } else if data.starts_with(b"II*\x00") || data.starts_with(b"MM\x00*") {
        Some(ImageFormat::Tiff)
    } else if data.starts_with(&[0x00, 0x00, 0x01, 0x00]) {
        Some(ImageFormat::Ico)
    } else {
        None
    }
}

/// ImageFormat → 小写扩展名（用于虚拟 src 命名）。
fn format_ext(f: ImageFormat) -> String {
    match f {
        ImageFormat::Png => "png".into(),
        ImageFormat::Jpeg => "jpeg".into(),
        ImageFormat::Webp => "webp".into(),
        ImageFormat::Gif => "gif".into(),
        ImageFormat::Svg => "svg".into(),
        ImageFormat::Bmp => "bmp".into(),
        ImageFormat::Tiff => "tiff".into(),
        ImageFormat::Ico => "ico".into(),
        ImageFormat::Pnm => "pnm".into(),
    }
}

/// 从 zip 读取单个条目为 UTF-8 字符串（带解压上限，防 zip 炸弹）。
///
/// 三防（K124：构造期内分配必须与文件界有独立硬上界）：
/// 1. **读前声明预检**：`ZipFile::size()`（未压缩大小）超限 → 零分配快速 Err；
/// 2. **读中 `take(max+1)` 限长**：把解压峰值钳到 ~max+1，即使声明小实大（压缩炸弹）
///    也最多物化 max+1 字节即被打断——只读后复核会先全量物化几百 MiB 才 Err；
/// 3. **读后实际长度复核**：超过 max 即 Err（超 take 哨兵说明声明与实解压不符）。
fn read_zip_entry<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
    max_bytes: usize,
) -> Result<String> {
    let file = archive
        .by_name(name)
        .with_context(|| format!("zip 中不存在 {name}"))?;
    if file.size() > max_bytes as u64 {
        bail!(
            "zip 条目 {name} 声明大小 {declared} 超上限 {max}",
            declared = file.size(),
            max = max_bytes
        );
    }
    let mut buf = Vec::new();
    file.take(max_bytes as u64 + 1).read_to_end(&mut buf)?;
    if buf.len() > max_bytes {
        bail!(
            "zip 条目 {name} 解压后大小 {actual} 超上限 {max}",
            actual = buf.len(),
            max = max_bytes
        );
    }
    // 去除 UTF-8 BOM
    let bytes = buf.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&buf);
    let text = String::from_utf8_lossy(bytes);
    Ok(text.into_owned())
}

/// 从 container.xml 里解析 OPF 路径（rootfile full-path 属性）。
fn find_opf_path(container_xml: &str) -> Result<String> {
    let doc = parse_xml(container_xml).context("解析 container.xml 失败")?;
    for node in doc.descendants() {
        if node.has_tag_name("rootfile")
            && let Some(full) = node.attribute("full-path")
            && !full.trim().is_empty()
        {
            return Ok(full.trim().to_string());
        }
    }
    bail!("container.xml 里找不到 rootfile/full-path")
}

/// 解析 OPF：返回（标题, 作者, spine 章节 href 列表）。
///
/// spine 里每个 itemref 的 idref 指向 manifest 的 id；通过 manifest 映射到 href。
/// OPF 英寸 XML 命名空间无关紧要，按 tag 名（local name）匹配即可。
fn parse_opf(opf_xml: &str, opf_path: &str) -> Result<(String, String, Vec<String>)> {
    let doc = parse_xml(opf_xml).context("解析 OPF 失败")?;

    // 元数据
    let mut title = String::new();
    let mut author = String::new();
    for node in doc.descendants() {
        match node.tag_name().name() {
            "title" if title.is_empty() => {
                title = node_text(&node);
            }
            "creator" if author.is_empty() => {
                author = node_text(&node);
            }
            _ => {}
        }
    }

    // manifest：id → href
    let mut manifest: std::collections::HashMap<String, String> = Default::default();
    for node in doc.descendants() {
        if node.has_tag_name("item")
            && let (Some(id), Some(href)) = (node.attribute("id"), node.attribute("href"))
        {
            manifest.insert(id.to_string(), href.to_string());
        }
    }

    // spine：itemref idref 顺序 = 阅读顺序
    let mut spine_hrefs: Vec<String> = Vec::new();
    for node in doc.descendants() {
        if node.has_tag_name("itemref")
            && let Some(idref) = node.attribute("idref")
            && let Some(href) = manifest.get(idref)
        {
            // 过滤掉 ncx/封面等非 xhtml 项，只保留章节
            if href.ends_with(".html") || href.ends_with(".xhtml") || href.ends_with(".htm") {
                spine_hrefs.push(href.clone());
            }
        }
    }

    if spine_hrefs.is_empty() {
        bail!("OPF spine 不含任何章节：{opf_path}");
    }

    Ok((title, author, spine_hrefs))
}

/// 常见 HTML 命名实体 → Unicode 字面字符（供章节 XHTML 解析前预解码）。
///
/// roxmltree 只认 XML 预定义实体（`amp`/`lt`/`gt`/`quot`/`apos`）与数字字符引用
/// （`&#NNN;`/`&#xHH;`），真实 EPUB 正文常用 `&nbsp;`/`&mdash;`/`&ldquo;` 等 HTML
/// 命名实体，直接解析会整章 `Err` → 内容静默丢空。
///
/// 表**刻意不含**五个 XML 预定义实体与数字引用：它们交给 roxmltree 原地处理——
/// `&amp;nbsp;` 的 `&amp;` 保持原样 → roxmltree 解码为字面 `&nbsp;`，不会在预解码
/// 阶段被二次展开，杜绝「先解 `&amp;`→`&`、再把 `nbsp;` 当实体重解」的双重解码。
fn html_entity_value(name: &str) -> Option<&'static str> {
    Some(match name {
        "nbsp" => "\u{00A0}",   // 不换行空格
        "ensp" => "\u{2002}",   // 半角空格
        "emsp" => "\u{2003}",   // 全角空格
        "thinsp" => "\u{2009}", // 窄空格
        "mdash" => "\u{2014}",  // 破折号
        "ndash" => "\u{2013}",  // 短破折号
        "hellip" => "\u{2026}", // 省略号
        "ldquo" => "\u{201C}",
        "rdquo" => "\u{201D}",
        "lsquo" => "\u{2018}",
        "rsquo" => "\u{2019}",
        "bull" => "\u{2022}",
        "middot" => "\u{00B7}",
        "copy" => "\u{00A9}",
        "reg" => "\u{00AE}",
        "trade" => "\u{2122}",
        "sect" => "\u{00A7}",
        "para" => "\u{00B6}",
        "laquo" => "\u{00AB}",
        "raquo" => "\u{00BB}",
        "times" => "\u{00D7}",
        "divide" => "\u{00F7}",
        "plusmn" => "\u{00B1}",
        "micro" => "\u{00B5}",
        "deg" => "\u{00B0}",
        "ne" => "\u{2260}",
        "le" => "\u{2264}",
        "ge" => "\u{2265}",
        "frac12" => "\u{00BD}",
        "frac14" => "\u{00BC}",
        "frac34" => "\u{00BE}",
        "eacute" => "\u{00E9}",
        "egrave" => "\u{00E8}",
        "euml" => "\u{00EB}",
        "ocirc" => "\u{00F4}",
        "uuml" => "\u{00FC}",
        "aacute" => "\u{00E1}",
        "agrave" => "\u{00E0}",
        "ccedil" => "\u{00E7}",
        _ => return None,
    })
}

/// 一次性从左到右扫描，把章节 XHTML 里表中命中的 `&name;` 预解码为字面字符。
///
/// - 命中（表内，如 `&nbsp;`）→ 替换为对应字符，扫描从 `;` 之后继续；
/// - 未命中（`&amp;`/`&lt;`/`&gt;`/`&quot;`/`&apos;`、数字字符引用、未知实体）→
///   `&` 原样保留、只推进 1 字节，其后的字面字符不再当实体序列处理。
///
/// 因替换只发生在「完整 `&name;`」，且未命中时按单字符推进，`&amp;nbsp;` 的
/// `&amp;` 不会先变 `&` 再触发二次替换（`&amp;` 原样保留，`nbsp;` 只是后继字面）；
/// 输出中不会**新引入** `&`/`<`/`>`/`"`/`'`，不会破坏 XML 良构。
fn decode_html_named_entities(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < bytes.len() {
        // 找下一个 `&`：其前逐段原样拷贝；没有则剩余全部原样拷出。
        let amp = bytes[i..].iter().position(|&b| b == b'&').map(|p| i + p);
        let Some(a) = amp else {
            out.push_str(&text[i..]);
            break;
        };
        out.push_str(&text[i..a]);
        // 尝试匹配 `&name;`：名字为 ASCII 字母数字；数字引用（`&#…`）不匹配 → 保留。
        let mut name_end = a + 1;
        while name_end < bytes.len() && bytes[name_end].is_ascii_alphanumeric() {
            name_end += 1;
        }
        if name_end > a + 1
            && name_end < bytes.len()
            && bytes[name_end] == b';'
            && let Some(repl) = html_entity_value(&text[a + 1..name_end])
        {
            out.push_str(repl);
            i = name_end + 1; // 跳过 `;`，后继内容从字面继续
        } else {
            // 未命中：`&` 原样保留，推进 1 字节（防漏字符）
            out.push('&');
            i = a + 1;
        }
    }
    out
}

/// 把一棵 XHTML 章节转成 Markdown。
///
/// 支持：标题(h1..h6)、段落(p)、引述(blockquote)、列表(ul/ol/li)、
/// 换行(br)、图片(img)、忽略 head/style/script。EPUB 排版增强：
/// - `p.signature` / `div.float-r` / `div.float-l` → 行首加 `%%ALIGN:(RIGHT|LEFT)%%`
///   指令（渲染器据此右/左对齐，右对齐/浮段不做正文首行缩进）；
/// - 正文的 `<a id="noteN"><sup>[N]</sup></a>` 脚注标记 → `[^N]`（渲染器做悬浮卡片）；
/// - 「注释」章的 `p.note` 段落 → 抽出 (注记号 → 注释正文) 进 `notes`（正文仍照常渲染）。
///
/// `on_img`：`(src, alt) -> Option<绝对路径>`（把 zip 内图片落盘后返回路径，
/// 用于生成 `![alt](path)`；返回 None 时输出文本占位）。
fn xhtml_to_markdown(
    html: &str,
    on_img: &mut dyn FnMut(&str, &str) -> Option<String>,
    notes: &mut std::collections::BTreeMap<usize, String>,
) -> String {
    // roxmltree 不支持 DTD：真实 EPUB 的 XHTML 章节几乎都带 `<!DOCTYPE html
    // PUBLIC "…//DTD XHTML…dtd">`，直接解析会整章失败（"XML with DTD detected"）
    // → 每章输出空 → 整书空文档（打开黑屏无字）。解析前先剥离 DOCTYPE。
    let html = strip_doctype(html);
    // roxmltree 只认 5 个 XML 预定义实体 + 数字字符引用；正文的 HTML 命名实体
    // （`&nbsp;`/`&mdash;`/`&ldquo;` 等）预解码为字面字符，防整章解析 Err 静默丢空。
    let html = decode_html_named_entities(&html);
    let doc = match parse_xml(&html) {
        Ok(d) => d,
        Err(_) => return String::new(),
    };
    let mut out = String::new();
    walk_body(&doc.root_element(), &mut out, 0, 0, None, notes, on_img);
    // 修整：多个空行合并、去掉行尾空格
    let mut result = String::with_capacity(out.len());
    let mut blank = 0usize;
    for line in out.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        result.push_str(line);
        result.push('\n');
    }
    result
}

/// 剥离 `<!DOCTYPE …>` 声明（可跨行、可含内部子集 `[ … ]`；正文不需要 DTD）。
///
/// 返回新串：`<!DOCTYPE` 起始处（按字节，ASCII 大小写不敏感）被整体移除，
/// 声明之前（如 `<?xml …?>` 前导，roxmltree 可正常处理）与声明之后的内容保留。
fn strip_doctype(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let Some(start) = lower.find("<!doctype") else {
        return html.to_string();
    };
    let bytes = html.as_bytes();
    let mut i = start + "<!doctype".len();
    let mut bracket_depth = 0i32;
    while i < bytes.len() {
        match bytes[i] {
            b'[' => bracket_depth += 1,
            b']' => bracket_depth -= 1,
            b'>' if bracket_depth <= 0 => break,
            _ => {}
        }
        i += 1;
    }
    let rest = (i + 1).min(bytes.len());
    let mut out = String::with_capacity(html.len());
    out.push_str(&html[..start]);
    out.push_str(&html[rest..]);
    out
}

/// 递归遍历正文元素。
///
/// - `quoted`：当前引述嵌套深度；
/// - `align`：从祖先 `float-r`/`float-l` 继承的对齐（`p.signature` 会就地覆写为右）；
/// - `notes`：脚注表（「注释」章的 `p.note` 段落在此登记）。
fn walk_body(
    node: &roxmltree::Node,
    out: &mut String,
    depth: usize,
    quoted: usize,
    align: Option<RowAlign>,
    notes: &mut std::collections::BTreeMap<usize, String>,
    on_img: &mut dyn FnMut(&str, &str) -> Option<String>,
) {
    // P0-B2：递归深度硬上限——超限即截断（该子树不再下行），不 panic/不 abort/不无限递归。
    if depth > MAX_TREE_DEPTH {
        return;
    }
    match node.tag_name().name() {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => return emit_heading(node, out),
        "p" => return emit_paragraph(node, out, quoted, align, notes),
        "blockquote" => {
            out.push('\n');
        }
        "ul" | "ol" => {}
        "li" => return emit_list_item(node, out, quoted, align),
        "a" => {
            if emit_link_note(node, out) {
                return;
            }
        }
        "img" | "image" => return emit_image(node, out, on_img),
        "br" => {
            out.push('\n');
            return;
        }
        "sup" => return emit_sup(node, out),
        "title" | "style" | "script" | "head" | "meta" | "link" | "#comment" => return,
        _ => {}
    }

    let child_quoted = quoted + usize::from(node.has_tag_name("blockquote"));
    // `float-r` / `float-l` 容器内的段落下移该对齐（也用于取消正文首行缩进）。
    let child_align = if node.has_tag_name("div") {
        match node
            .attribute("class")
            .unwrap_or("")
            .split_whitespace()
            .find(|c| {
                *c == "float-r" || *c == "float-l" || *c == "float-right" || *c == "float-left"
            }) {
            Some("float-r") | Some("float-right") => Some(RowAlign::Right),
            Some("float-l") | Some("float-left") => Some(RowAlign::Left),
            _ => align,
        }
    } else {
        align
    };
    for child in node.children() {
        if child.is_element() || child.is_text() {
            walk_body(
                &child,
                out,
                depth + 1,
                child_quoted,
                child_align,
                notes,
                on_img,
            );
        }
    }
}

/// `h1..h6` → markdown 标题行。
fn emit_heading(node: &roxmltree::Node, out: &mut String) {
    let level = node.tag_name().name()[1..].parse::<u8>().unwrap_or(1);
    let text = node_text_epub(node, 0);
    out.push('\n');
    out.push_str(&"#".repeat(level as usize));
    out.push(' ');
    out.push_str(text.trim());
    out.push_str("\n\n");
}

/// `p` → 段落：落款右对齐/`<br>` 分行/「注释定义」改写 `[N]`→`[!N]` 并登记 `notes`/
/// 正文首行缩进 2 全角空格（引述/对齐行不缩进）。
fn emit_paragraph(
    node: &roxmltree::Node,
    out: &mut String,
    quoted: usize,
    align: Option<RowAlign>,
    notes: &mut std::collections::BTreeMap<usize, String>,
) {
    let cls = node.attribute("class").unwrap_or("");
    // 落款（签名）段落右对齐；其余沿用祖先对齐（float-r→右 / float-l→左）。
    let p_align = if cls.split_whitespace().any(|c| c == "signature") {
        Some(RowAlign::Right)
    } else {
        align
    };
    // 按 `<br>` 切开成多行（落款“…恩格斯<br/>1882年1月21日于伦敦”分行显示）。
    let mut segs = node_text_epub_lines(node);
    // 「注释」章：`p.note` 段落的 leading `<a id="footnoteN">[N]</a>` 是「注释定义」标记
    // → 改写成 `[!N]`（渲染层识别为定义标记，可逆跳到文内对应位置）；
    // 同时抽出 (注记号 → 注释正文) 进 `notes`（悬浮卡片数据源）。
    let note_def = leading_footnote_id(node).is_some();
    if note_def
        && let Some(first) = segs.iter_mut().find(|s| !s.trim().is_empty())
        && let Some(rel) = first.find('[')
    {
        let after = &first[rel + 1..];
        let d = after.chars().take_while(|c| c.is_ascii_digit()).count();
        if d > 0 && after[d..].starts_with(']') {
            first.insert(rel + 1, '!');
        }
    }
    let joined = segs.join("\n");
    let t = joined.trim().to_string();
    if note_def && let Some(n) = leading_footnote_id(node) {
        // 定义标记已被改写为 `[!N]`（上一段插入 `!`），正文提取要兼容
        // 改写后的前缀（先 `[!N]`、再回落裸 `[N]`），否则注释表永远为空。
        let def = format!("[!{n}]");
        let plain = format!("[{n}]");
        let rest = t
            .strip_prefix(def.as_str())
            .or_else(|| t.strip_prefix(plain.as_str()));
        if let Some(rest) = rest {
            notes
                .entry(n)
                .or_insert_with(|| rest.trim_start_matches('\u{3000}').trim().to_string());
        }
    }
    // 正文（`class="text"` 或空 class、且不在引述块内）首行缩进 2 字符：
    // 行首前置两个全角空格，渲染时折行后天然「首行缩进、次行顶格」；
    // 对齐行（落款/浮段）与引述/注释等段落不缩进。
    let is_body_text =
        quoted == 0 && (cls.split_whitespace().any(|c| c == "text") || cls.trim().is_empty());
    for seg in segs.into_iter().map(|s| s.trim().to_string()) {
        if seg.is_empty() {
            continue;
        }
        // 对齐段落每一 `br` 行都带对齐指令（多行落款整块靠右）。
        let line = match (p_align, is_body_text) {
            (Some(a), _) => {
                let mut s = String::with_capacity(seg.len() + 16);
                s.push_str(align_directive(a));
                s.push_str(&seg);
                s
            }
            (None, true) => format!("\u{3000}\u{3000}{seg}"),
            _ => seg,
        };
        if !line.trim().is_empty() {
            out.push_str(&quote_prefix(&line, quoted));
            out.push('\n');
        }
    }
}

/// `li` → 列表项（有序 `- ` / 无序 `• `，可带祖先对齐）。
fn emit_list_item(
    node: &roxmltree::Node,
    out: &mut String,
    quoted: usize,
    align: Option<RowAlign>,
) {
    let t = node_text_epub(node, 0);
    let t = t.trim().to_string();
    if !t.is_empty() {
        let line = match align {
            Some(a) => {
                let mut s = String::with_capacity(t.len() + 16);
                s.push_str(align_directive(a));
                s.push_str(&t);
                s
            }
            None => t,
        };
        let marker = if parent_is_ordered(node) {
            "- "
        } else {
            "• "
        };
        out.push_str(marker);
        out.push_str(&quote_prefix(&line, quoted));
        out.push('\n');
    }
}

/// `a` → 返回 true 表示它是正文脚注锚（已输出 `[^N]`，调用方不再下钻）；
/// false 表示普通链接，交给子节点递归。
fn emit_link_note(node: &roxmltree::Node, out: &mut String) -> bool {
    // 正文脚注标记：`<a id="noteN" href="...#footnoteN"><sup>[N]</sup></a>`
    // → `[^N]`（渲染层做悬浮卡片；通常已在 p/标题内被 [`node_text_epub`]
    // 处理，此处兜底裸露在 div/块外的锚）；普通链接原样走子节点文本。
    if let Some(n) = note_ref_at(node) {
        out.push_str(&format!(" [^{n}] "));
        return true;
    }
    false
}

/// `img` / SVG `image` → 内/外取址解析后交给 `on_img` 落 markdown 图片（失败退化为引用块）。
fn emit_image(
    node: &roxmltree::Node,
    out: &mut String,
    on_img: &mut dyn FnMut(&str, &str) -> Option<String>,
) {
    let alt = node.attribute("alt").unwrap_or("").to_string();
    // `<img src>` 与 SVG `<image xlink:href>` 两种取址方式。
    // 命名空间属性（href/xlink:href）逐个扫描，避免依赖限定名解析。
    let mut src = node.attribute("src").unwrap_or("").to_string();
    if src.is_empty() {
        for a in node.attributes() {
            let name = a.name();
            if name == "href" || name.ends_with(":href") || name == "xlink:href" {
                src = a.value().to_string();
                break;
            }
        }
    }
    match on_img(&src, &alt) {
        Some(path) => {
            let alt_esc = if alt.is_empty() {
                "图片".to_string()
            } else {
                alt.clone()
            };
            out.push_str(&format!("\n\n![{alt_esc}]({path})\n\n"));
        }
        None => {
            let label = if alt.is_empty() {
                "图片".to_string()
            } else {
                alt.clone()
            };
            out.push_str(&format!("\n\n> [图片：{label}]\n\n"));
        }
    }
}

/// `sup` → 可读文本（如 `[N]` 上标，转换为 ` [N] `）。
fn emit_sup(node: &roxmltree::Node, out: &mut String) {
    let t = node_text(node);
    let t = t.trim().to_string();
    if !t.is_empty() {
        out.push_str(&format!(" [{t}] "));
    }
}

/// `%%ALIGN:X%%` 指令串（渲染层据此对齐该行）。
fn align_directive(a: RowAlign) -> &'static str {
    match a {
        RowAlign::Left => "%%ALIGN:LEFT%%",
        RowAlign::Center => "%%ALIGN:CENTER%%",
        RowAlign::Right => "%%ALIGN:RIGHT%%",
    }
}

/// 「注释」段落的 leading 脚注锚 id：`footnoteN` → `N`（无则 `None`）。
fn leading_footnote_id(node: &roxmltree::Node) -> Option<usize> {
    let a = node
        .children()
        .find(|c| c.is_element() && c.has_tag_name("a"))?;
    let id = a.attribute("id")?;
    id.strip_prefix("footnote")?.parse().ok()
}

/// 从正文脚注锚的 href（如 `part0018.html#footnote1`）里取注记号。
fn note_ref_number(href: &str) -> Option<usize> {
    let after = href.split("#footnote").nth(1)?;
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// 找出 `s` 里全部脚注标记（正文引用 `[^N]` / 注释定义 `[!N]`）的字节区间与裸号。
/// 返回 `(起始字节, 结束字节(不含,含 `]`), 裸号)`；`s[start..end]` 即整个标记。
/// 非脚注方括号 / 多字节字符按字符边界跳过（防切在多字节 UTF-8 中间）。
fn note_marker_ranges(s: &str) -> Vec<(usize, usize, usize)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 1 < b.len() {
        if b[i] == b'[' && (b[i + 1] == b'^' || b[i + 1] == b'!') {
            let mut j = i + 2;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 2 && j < b.len() && b[j] == b']' {
                let local: usize = s[i + 2..j].parse().unwrap_or(0);
                out.push((i, j + 1, local));
                i = j + 1;
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap();
        i += ch.len_utf8();
    }
    out
}

/// 把 `s` 里全部脚注标记的裸号按 `renumber` 换算后改写（只换数字，标记形态
/// `[^N]`/`[!N]` 保持不变）。映射表没覆盖的号原样保留；无标记时直接返回原串。
fn renumber_note_markers(s: &str, renumber: &dyn Fn(usize) -> usize) -> String {
    let marks = note_marker_ranges(s);
    if marks.is_empty() {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut prev = 0usize;
    for (start, end, local) in marks {
        out.push_str(&s[prev..start]);
        out.push_str(&s[start..start + 2]); // `[^` 或 `[!`
        out.push_str(&renumber(local).to_string());
        out.push(']');
        prev = end;
    }
    out.push_str(&s[prev..]);
    out
}

/// 给引述层级加 `>` 前缀的辅助：每行前加对应数量 `> `。
fn quote_prefix(text: &str, quoted: usize) -> String {
    if quoted == 0 {
        return text.to_string();
    }
    let prefix = "> ".repeat(quoted);
    format!("{prefix}{text}")
}

/// 取节点及其后代的全部文本（含嵌套元素文本，忽略注释）。
fn node_text(node: &roxmltree::Node) -> String {
    let mut s = String::new();
    for n in node.descendants() {
        if n.is_text()
            && let Some(t) = n.text()
        {
            s.push_str(t);
        }
    }
    s
}

/// 取段落等**正文容器**的文本，但把正文脚注锚
/// （`<a id="noteN" href="...#footnoteN"><sup>[N]</sup></a>`）替换成 `[^N]` 标记
/// （渲染层据此做悬浮注释卡片）。普通链接/注释章锚（`id="footnoteN"`）按文本走。
fn node_text_epub(node: &roxmltree::Node, depth: usize) -> String {
    // P0-B2：文本递归同走深度上限，超限截断（不再下钻），防恶意 `<p>` 内深嵌套撑栈。
    if depth > MAX_TREE_DEPTH {
        return String::new();
    }
    let mut s = String::new();
    for child in node.children() {
        if child.is_text() {
            if let Some(t) = child.text() {
                s.push_str(t);
            }
        } else if child.is_element() {
            if child.has_tag_name("a")
                && let Some(n) = note_ref_at(&child)
            {
                s.push_str(&format!(" [^{n}] "));
                continue; // 跳过其 `<sup>[N]</sup>` 子节点
            }
            s.push_str(&node_text_epub(&child, depth + 1));
        }
    }
    s
}

/// 同 [`node_text_epub`]，但把段落里的 `<br>` 当成**换行**：返回按 `<br>` 切开的
/// 多段文本（EPUB 落款如“卡尔·马克思 弗里德里希·恩格斯<br/>1882年1月21日于伦敦”
/// 应分别成行，而不是挤在同一行）。
fn node_text_epub_lines(node: &roxmltree::Node) -> Vec<String> {
    let mut segs = vec![String::new()];
    walk_text_lines(node, 0, &mut segs);
    segs
}

fn walk_text_lines(node: &roxmltree::Node, depth: usize, segs: &mut Vec<String>) {
    // P0-B2：文本递归同走深度上限，超限截断（不再下钻）。
    if depth > MAX_TREE_DEPTH {
        return;
    }
    for child in node.children() {
        if child.is_text() {
            if let Some(t) = child.text() {
                segs.last_mut().unwrap().push_str(t);
            }
        } else if child.is_element() {
            if child.has_tag_name("br") {
                // 换行：收束当前段，新起一段。
                if !segs.last().unwrap().is_empty() {
                    segs.push(String::new());
                }
                continue;
            }
            if child.has_tag_name("a")
                && let Some(n) = note_ref_at(&child)
            {
                segs.last_mut().unwrap().push_str(&format!(" [^{n}] "));
                continue;
            }
            walk_text_lines(&child, depth + 1, segs);
        }
    }
}

/// `a` 是否正文脚注锚（`id^=note` + `href` 含 `#footnote`）；返回注记号。
fn note_ref_at(node: &roxmltree::Node) -> Option<usize> {
    if !node.has_tag_name("a") {
        return None;
    }
    let id = node.attribute("id").unwrap_or("");
    if !id.starts_with("note") {
        return None;
    }
    note_ref_number(node.attribute("href").unwrap_or(""))
}

/// 判断列表项是否有序（父级 `<ol>`）。
fn parent_is_ordered(node: &roxmltree::Node) -> bool {
    node.parent()
        .map(|p| p.has_tag_name("ol"))
        .unwrap_or(false)
}

/// OPF 所在目录（zip 内路径，用于拼接章节相对路径）。
fn parent_zip_dir(opf_path: &str) -> String {
    match opf_path.rfind('/') {
        Some(i) => opf_path[..i].to_string(),
        None => String::new(),
    }
}

/// 把 zip 内相对路径拼到 OPF 目录下（处理 `../`）。
fn join_zip_path(base_dir: &str, href: &str) -> String {
    if base_dir.is_empty() {
        return href.to_string();
    }
    // 简单合并，处理 ../
    let mut parts: Vec<&str> = base_dir.split('/').collect();
    for seg in href.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_epub_path() -> std::path::PathBuf {
        // P4：本 crate 位于 crates/epub，py 夹具在工作区根 target/测试epub.epub（上溯两级，
        // 不入库、手动装配）。
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/测试epub.epub")
    }

    #[test]
    fn parses_epub_title_and_author() {
        let doc = parse_epub_file(&test_epub_path()).expect("解析 测试epub.epub 应成功");
        assert_eq!(doc.title, "共产党宣言");
        assert!(doc.author.contains("马克思"));
    }

    #[test]
    fn extracts_chapters_in_spine_order() {
        let doc = parse_epub_file(&test_epub_path()).expect("解析 测试epub.epub 应成功");
        assert!(
            doc.chapter_count >= 5,
            "应有多个章节，实际={}",
            doc.chapter_count
        );
        assert!(!doc.markdown.is_empty());
        // 应包含著名开头标题之一（1888年英文版序言 或 第一章 资产者和无产者）
        let any = ["1888年英文版序言", "资产者和无产者"]
            .iter()
            .any(|kw| doc.markdown.contains(kw));
        assert!(any, "正文应包含书名/章标题之一");
    }

    #[test]
    fn extracts_epub_images_to_memory() {
        let doc = parse_epub_file(&test_epub_path()).expect("解析 测试epub.epub 应成功");
        // 测试epub.epub 有 6 张图；正文里以 `![...](epubimg://...)` 虚拟 src 引用
        let img_marks: Vec<&str> = doc
            .markdown
            .lines()
            .filter(|l| l.starts_with("!["))
            .collect();
        assert_eq!(
            img_marks.len(),
            6,
            "应提取 6 张图片（含 SVG 封面），实际段落:\n{}",
            doc.markdown
        );
        let mut found = 0usize;
        for line in &img_marks {
            // 形如 ![alt]( epubimg://... )
            if let (Some(start), Some(end)) = (line.find("]("), line.rfind(')')) {
                let vsrc = &line[start + 2..end];
                assert!(
                    vsrc.starts_with(EPUB_IMG_SCHEME),
                    "图片应为不落盘的内存虚拟 src：{vsrc}"
                );
                // 注册表里有该 src 且带字节
                let img = doc
                    .images
                    .get(vsrc)
                    .unwrap_or_else(|| panic!("内存图片表应含 {vsrc}"));
                assert!(!img.bytes.is_empty(), "内存图片字节不应为空");
                found += 1;
            }
        }
        assert_eq!(found, 6, "应命中 6 张图片，实际={found}");
        // 00001.jpeg 实为 PNG：按真实签名应为 Png，而不是 Jpeg（扩展名误导的脏数据）
        let png_bad_ext = doc
            .images
            .values()
            .filter(|img| img.format == gpui::ImageFormat::Png)
            .count();
        assert!(
            png_bad_ext >= 1,
            "应检出 1 张「扩展名 .jpeg 但实为 PNG」的图片（00001）"
        );
    }

    #[test]
    fn xhtml_to_markdown_preserves_heading_and_paragraph() {
        let html = "<html><body><h1>第一章</h1><p>正文内容。</p><blockquote><p>引述。</p></blockquote></body></html>";
        let no_img: Option<String> = None;
        let mut notes = std::collections::BTreeMap::new();
        let md = xhtml_to_markdown(html, &mut |_s, _a| no_img.clone(), &mut notes);
        assert!(md.contains("# 第一章"));
        assert!(md.contains("正文内容。"));
        assert!(md.contains("> 引述。"));
    }

    #[test]
    fn xhtml_signature_and_float_emit_align_directives() {
        // `p.signature` → 右对齐指令；`div.float-r` 内 p.text → 右对齐；`div.float-l` → 左对齐。
        let html = "<html><body>\
<p class=\"text\">普通正文段。</p>\
<p class=\"signature\">卡尔·马克思<br/>1872年6月24日于伦敦</p>\
<div class=\"float-r\"><p class=\"text\">原文是德文</p></div>\
<div class=\"float-l\"><p class=\"text\">卡·马克思写于1872年</p></div>\
</body></html>";
        let no: Option<String> = None;
        let mut notes = std::collections::BTreeMap::new();
        let md = xhtml_to_markdown(html, &mut |_s, _a| no.clone(), &mut notes);
        assert!(
            md.contains("%%ALIGN:RIGHT%%卡尔·马克思"),
            "签名应带右对齐指令：{md}"
        );
        // `<br>` 应把签名拆成两行，且每行都带右对齐指令（日期单独成行）。
        assert!(
            md.contains("%%ALIGN:RIGHT%%卡尔·马克思\n%%ALIGN:RIGHT%%1872年6月24日于伦敦"),
            "签名 `<br>` 应拆行且两行都右对齐：{md:?}"
        );
        assert!(
            !md.contains("卡尔·马克思1872年6月24日于伦敦"),
            "日期不得与姓名挤在一行: {md:?}"
        );
        assert!(
            md.contains("%%ALIGN:RIGHT%%原文是德文"),
            "float-r 应带右对齐指令：{md}"
        );
        assert!(
            md.contains("%%ALIGN:LEFT%%卡·马克思写于1872年"),
            "float-l 应带左对齐指令：{md}"
        );
        assert!(
            !md.contains("%%ALIGN:%普通正文段"),
            "普通正文不应带对齐指令：{md}"
        );
        assert!(
            md.contains("\u{3000}\u{3000}普通正文段。"),
            "正文（class=text）应首行缩进 2 全角空格：{md:?}"
        );
    }

    #[test]
    fn xhtml_footnote_markers_and_definitions() {
        // 正文标记 `[^1]`；注释章 p.note 定义进 notes 表且正文照常渲染。
        let html = "<html><body>\
<p class=\"text\">共产主义者同盟<a id=\"note2\" href=\"part0018.html#footnote2\" class=\"calibre2\"><sup class=\"suptext1\">[2]</sup></a>成立于1847年。</p>\
<p class=\"note\"><a id=\"footnote2\" href=\"part0002.html#note2\">[2]</a> 共产主义者同盟是历史上第一个无产阶级政党。</p>\
</body></html>";
        let no: Option<String> = None;
        let mut notes = std::collections::BTreeMap::new();
        let md = xhtml_to_markdown(html, &mut |_s, _a| no.clone(), &mut notes);
        assert!(md.contains("[^2]"), "正文脚注应转成 [^2] 标记：{md}");
        assert_eq!(
            notes.get(&2).map(|s| s.as_str()),
            Some("共产主义者同盟是历史上第一个无产阶级政党。"),
            "注释表应有第 2 条：{notes:?}"
        );
        // 注释章定义标记应改写为 `[!2]`（renderer 识别为定义、可逆跳），正文照常渲染。
        assert!(
            md.contains("[!2]") && md.contains("共产主义者同盟是历史上第一个无产阶级政党。"),
            "注释定义标记应为 [!2] 且正文照常渲染：{md}"
        );
    }

    #[test]
    fn xhtml_img_becomes_markdown_image() {
        let html = "<html><body><p>前文</p><img src=\"images/p1.jpeg\" alt=\"插图一\"/><p>后文</p></body></html>";
        let md = {
            let mut on_img = |src: &str, alt: &str| {
                assert_eq!(src, "images/p1.jpeg");
                assert_eq!(alt, "插图一");
                Some("/tmp/x/p1.jpeg".to_string())
            };
            let mut notes = std::collections::BTreeMap::new();
            xhtml_to_markdown(html, &mut on_img, &mut notes)
        };
        assert!(
            md.contains("![插图一](/tmp/x/p1.jpeg)"),
            "图片应转成 markdown 图片，实际={md:?}"
        );
    }

    #[test]
    fn xhtml_img_without_resolver_falls_back() {
        let html = "<html><body><img src=\"x.png\" alt=\"图A\"/></body></html>";
        let no: Option<String> = None;
        let mut notes = std::collections::BTreeMap::new();
        let md = xhtml_to_markdown(html, &mut |_s, _a| no.clone(), &mut notes);
        assert!(
            md.contains("[图片：图A]"),
            "无解析结果应输出占位，实际={md:?}"
        );
    }

    #[test]
    fn svg_image_becomes_markdown_image() {
        // 封面常用 `<svg><image xlink:href=...></svg>` 形式
        let html = "<html xmlns=\"http://www.w3.org/1999/xhtml\" xmlns:xlink=\"http://www.w3.org/1999/xlink\"><body><svg><image xlink:href=\"images/cover.jpeg\"/></svg></body></html>";
        let md = {
            let mut on_img = |src: &str, _alt: &str| {
                assert_eq!(src, "images/cover.jpeg");
                Some("epubimg://1.jpeg".to_string())
            };
            let mut notes = std::collections::BTreeMap::new();
            xhtml_to_markdown(html, &mut on_img, &mut notes)
        };
        assert!(
            md.contains("![图片](epubimg://1.jpeg)"),
            "SVG image 应转成 markdown 图片，实际={md:?}"
        );
    }

    #[test]
    fn join_zip_path_resolves_parent() {
        assert_eq!(
            join_zip_path("OEBPS", "text/ch1.xhtml"),
            "OEBPS/text/ch1.xhtml"
        );
        assert_eq!(
            join_zip_path("OEBPS", "../META-INF/container.xml"),
            "META-INF/container.xml"
        );
        assert_eq!(join_zip_path("", "text/ch1.xhtml"), "text/ch1.xhtml");
    }

    #[test]
    fn xhtml_with_doctype_still_extracts_text() {
        // 回归：真实 EPUB 的 XHTML 几乎都带 `<!DOCTYPE …>`（XHTML 1.1 DTD），roxmltree
        // 不支持 DTD 会整章解析失败（"XML with DTD detected"）→ 每章空 → 整书空文档
        // （打开一片黑、看不到文字）。剥离 DOCTYPE 后应照常提取标题/段落。
        let html = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.1//EN\"\n  \"http://www.w3.org/TR/xhtml11/DTD/xhtml11.dtd\">\n\
<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><h1>第一章</h1><p class=\"text\">正文内容。</p></body></html>";
        let no: Option<String> = None;
        let mut notes = std::collections::BTreeMap::new();
        let md = xhtml_to_markdown(html, &mut |_s, _a| no.clone(), &mut notes);
        assert!(
            md.contains("# 第一章"),
            "带 DOCTYPE 的 XHTML 应提取标题：{md:?}"
        );
        assert!(
            md.contains("正文内容。"),
            "带 DOCTYPE 的 XHTML 应提取段落：{md:?}"
        );
    }

    #[test]
    fn strip_doctype_removes_internal_subset() {
        // DOCTYPE 带内部子集 `[ … ]`（内含 `>`）不应提前截断；声明整体移除、正文保留。
        let html = "<!DOCTYPE html [ <!ENTITY x \"y\"> ]><html><body><p>正文</p></body></html>";
        let s = strip_doctype(html);
        assert!(!s.contains("<!DOCTYPE"), "DOCTYPE 应整体移除：{s:?}");
        assert!(s.starts_with("<html>"), "内部子集后的 <html> 应保留：{s:?}");
        assert!(s.contains("正文"), "声明前后的内容都应保留：{s:?}");
    }

    #[test]
    fn deep_nested_html_does_not_overflow_stack() {
        // 复现 P0：walk_body 递归遍历（lib.rs:423+）无深度上限 → 几万层嵌套 `<div>`
        // 撑爆测试线程栈（栈溢出 → 进程 abort / 0xC00000FD）。章节大小仅 ~6×N 字节，
        // 就在 16MiB 章节上限内，恶意章节可轻松触发。
        //
        // ⚠️ 修复前本测试会让测试二进制**栈溢出 abort**（整进程退出、同文件其它测试
        // 一并中止）——这是修复前真实崩溃的证据，接受；修复后遍历应在有界深度下
        // 优雅截断（不崩、不把深层子树全量展开），本测试恢复为普通 PASS。
        let n = 20000usize;
        let mut html = String::with_capacity(6 * n + 16);
        html.push_str("<html><body>");
        for _ in 0..n {
            html.push_str("<div>");
        }
        html.push('x');
        for _ in 0..n {
            html.push_str("</div>");
        }
        html.push_str("</body></html>");

        let no: Option<String> = None;
        let mut notes = std::collections::BTreeMap::new();
        // 固定入口：章节文本提取（章节 XHTML → Markdown）。
        let md = xhtml_to_markdown(&html, &mut |_s, _a| no.clone(), &mut notes);
        // 断言 1：应能正常返回（栈溢出即 abort，走不到这里——红测证据）。
        // 断言 2：输出有界，不得把深层子树全量展开。
        assert!(
            md.len() < html.len(),
            "深层嵌套输出应有界：md={}B html={}B",
            md.len(),
            html.len()
        );
        assert!(
            md.len() < 1024,
            "深层嵌套正文只含一个字符，输出应远小于 1KiB：{}B",
            md.len()
        );
    }

    #[test]
    fn deep_nested_spans_in_paragraph_does_not_overflow_stack() {
        // 复现 P0 第二条递归面：`<p>` 内的深嵌套 `<span>` → emit_paragraph →
        // node_text_epub（lib.rs:690+）对子元素递归无深度上限，同样把测试线程栈
        // 打爆 → abort（0xC00000FD）。修复前即红；修复后应在有界深度下优雅截断。
        let n = 20000usize;
        let mut html = String::with_capacity(6 * n + 20);
        html.push_str("<html><body><p>");
        for _ in 0..n {
            html.push_str("<span>");
        }
        html.push('x');
        for _ in 0..n {
            html.push_str("</span>");
        }
        html.push_str("</p></body></html>");

        let no: Option<String> = None;
        let mut notes = std::collections::BTreeMap::new();
        // 同上：栈溢出即 abort（红测证据）；能走到这里说明没有炸栈。
        let md = xhtml_to_markdown(&html, &mut |_s, _a| no.clone(), &mut notes);
        assert!(
            md.len() < html.len() && md.len() < 1024,
            "段落深嵌套输出应有界：md={}B html={}B",
            md.len(),
            html.len()
        );
    }

    #[test]
    fn deep_nested_unicode_name_does_not_overflow_stack() {
        // 复现评审 BLOCK（K173）：`max_xml_element_depth` 的名字起始判据
        // `is_xml_name_start` 只认 ASCII 字母/`_`/`:`，而非 ASCII 起始字节
        // （如 `α`=U+03B1 首字节 0xCE）不进谓词 → 落入裸 `<` 分支只跳 1 字节 →
        // 深树**零计数放行** → 绕过深度预扫描后 roxmltree 自身递归解析
        // 300+ 层 Unicode 名嵌套即 0xC00000FD 栈溢出。
        //
        // ⚠️ 修复前本测试会让测试二进制**栈溢出 abort**（整进程退出、同文件
        // 其它测试一并中止）——这是修复前真实崩溃的证据，接受；修复后深度
        // 预扫描在解析前拒掉（bail），走不到 roxmltree，输出为空。
        let n = 2000usize;
        let mut html = String::with_capacity(5 * n + 20);
        html.push_str("<html><body>");
        for _ in 0..n {
            html.push_str("<α>");
        }
        html.push('x');
        for _ in 0..n {
            html.push_str("</α>");
        }
        html.push_str("</body></html>");

        let no: Option<String> = None;
        let mut notes = std::collections::BTreeMap::new();
        // 防御路径必须触发：深度预扫描得看见 Unicode 名嵌套（修复前这里只数到
        // html/body 两层 → 断言失败即红，且不依赖运行环境的线程栈大小）。
        assert!(
            max_xml_element_depth(&html) > MAX_TREE_DEPTH,
            "深度预扫描应识别 Unicode 元素名嵌套：depth={}",
            max_xml_element_depth(&html)
        );
        // 栈溢出即 abort（红测证据）；能走到这里说明 pre-scan 已拒掉了深树。
        let md = xhtml_to_markdown(&html, &mut |_s, _a| no.clone(), &mut notes);
        assert!(
            md.len() < html.len() && md.len() < 1024,
            "Unicode 名深嵌套输出应有界：md={}B html={}B",
            md.len(),
            html.len()
        );
    }

    // ===== 解析安全上限（注入式小值，KB 级内存下验证各 DoS 闸）=====
    mod limits {
        use super::*;
        use std::io::{Seek, Write};
        use zip::write::SimpleFileOptions;

        /// 造一个最小合法 EPUB（container.xml + content.opf + 章节 + 可选图片），
        /// 返回可 Seek 的内存 zip 字节。`chapter` 为章节 XHTML 正文；
        /// `images` 为 `(zip 内路径, 字节)` 列表，正文引用它们（每张 `<img src=...>`）。
        fn make_epub(
            chapter: &str,
            images: &[(&str, &[u8])],
        ) -> std::io::Cursor<Vec<u8>> {
            let mut buf = std::io::Cursor::new(Vec::new());
            {
                let mut w = zip::ZipWriter::new(&mut buf);
                let opts =
                    SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
                let add = |w: &mut zip::ZipWriter<&mut std::io::Cursor<Vec<u8>>>,
                           name: &str,
                           data: &[u8]| {
                    w.start_file(name, opts).unwrap();
                    w.write_all(data).unwrap();
                };
                add(
                    &mut w,
                    "META-INF/container.xml",
                    br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#,
                );
                let mut manifest = String::from(
                    r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata><dc:title xmlns:dc="http://purl.org/dc/elements/1.1/">测试书</dc:title></metadata>
  <manifest><item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/>"#,
                );
                for (i, (img_path, _)) in images.iter().enumerate() {
                    manifest.push_str(&format!(
                        r#"<item id="img{i}" href="{img_path}" media-type="image/png"/>"#
                    ));
                }
                manifest.push_str("</manifest><spine><itemref idref=\"ch1\"/></spine></package>");
                add(&mut w, "OEBPS/content.opf", manifest.as_bytes());
                add(&mut w, "OEBPS/ch1.xhtml", chapter.as_bytes());
                for (img_path, data) in images {
                    add(&mut w, &format!("OEBPS/{img_path}"), data);
                }
                w.finish().unwrap();
            }
            buf.set_position(0);
            buf
        }

        /// 与 [`make_epub`] 同构造、但章节用 **Deflated 压缩**（高重复字节 → 压缩后极小）：
        /// 配合 [`patch_zip_uncompressed_size`] 造「声明小实大」zip bomb 测试夹具。
        fn make_epub_deflated(
            chapter: &str,
            images: &[(&str, &[u8])],
        ) -> std::io::Cursor<Vec<u8>> {
            let mut buf = std::io::Cursor::new(Vec::new());
            {
                let mut w = zip::ZipWriter::new(&mut buf);
                let opts = SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated);
                let add = |w: &mut zip::ZipWriter<&mut std::io::Cursor<Vec<u8>>>,
                           name: &str,
                           data: &[u8]| {
                    w.start_file(name, opts).unwrap();
                    w.write_all(data).unwrap();
                };
                add(
                    &mut w,
                    "META-INF/container.xml",
                    br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#,
                );
                let mut manifest = String::from(
                    r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata><dc:title xmlns:dc="http://purl.org/dc/elements/1.1/">测试书</dc:title></metadata>
  <manifest><item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/>"#,
                );
                for (i, (img_path, _)) in images.iter().enumerate() {
                    manifest.push_str(&format!(
                        r#"<item id="img{i}" href="{img_path}" media-type="image/png"/>"#
                    ));
                }
                manifest.push_str("</manifest><spine><itemref idref=\"ch1\"/></spine></package>");
                add(&mut w, "OEBPS/content.opf", manifest.as_bytes());
                add(&mut w, "OEBPS/ch1.xhtml", chapter.as_bytes());
                for (img_path, data) in images {
                    add(&mut w, &format!("OEBPS/{img_path}"), data);
                }
                w.finish().unwrap();
            }
            buf.set_position(0);
            buf
        }

        /// 二进制把 zip 里某条目的 `uncompressed_size`（本地头 + 中央目录两处 4 字节）
        /// 改成 `fake`——制造「声明小、实解压大」的压缩炸弹条目。zip 格式：
        /// - 本地文件头：sig(4)+ver(2)+flag(2)+method(2)+time(2)+date(2)+crc(4)+
        ///   compressed(4)+**uncompressed(4，偏移 18..22)**+name_len(2)+extra_len(2)+name
        /// - 中央目录头：sig(4)+made(2)+ver(2)+flag(2)+method(2)+time(2)+date(2)+crc(4)+
        ///   compressed(4)+**uncompressed(4，偏移 24..28)**+name_len(2)+extra_len(2)+comment_len(2)
        fn patch_zip_uncompressed_size(zip: &mut [u8], name: &str, fake: u32) {
            let fake_le = fake.to_le_bytes();
            // 本地文件头（PK\x03\x04）：uncompressed 在偏移 18。
            let mut i = 0usize;
            while i + 30 <= zip.len() {
                if zip[i..i + 4] == [0x50, 0x4b, 0x03, 0x04] {
                    let name_len =
                        u16::from_le_bytes([zip[i + 26], zip[i + 27]]) as usize;
                    let extra_len =
                        u16::from_le_bytes([zip[i + 28], zip[i + 29]]) as usize;
                    if i + 30 + name_len <= zip.len()
                        && &zip[i + 30..i + 30 + name_len] == name.as_bytes()
                    {
                        zip[i + 18..i + 22].copy_from_slice(&fake_le);
                    }
                    i += 30 + name_len + extra_len;
                } else {
                    i += 1;
                }
            }
            // 中央目录头（PK\x01\x02）：uncompressed 在偏移 24。
            let mut i = 0usize;
            while i + 46 <= zip.len() {
                if zip[i..i + 4] == [0x50, 0x4b, 0x01, 0x02] {
                    let name_len =
                        u16::from_le_bytes([zip[i + 28], zip[i + 29]]) as usize;
                    let extra_len =
                        u16::from_le_bytes([zip[i + 30], zip[i + 31]]) as usize;
                    let comment_len =
                        u16::from_le_bytes([zip[i + 32], zip[i + 33]]) as usize;
                    if i + 46 + name_len <= zip.len()
                        && &zip[i + 46..i + 46 + name_len] == name.as_bytes()
                    {
                        zip[i + 24..i + 28].copy_from_slice(&fake_le);
                    }
                    i += 46 + name_len + extra_len + comment_len;
                } else {
                    i += 1;
                }
            }
        }

        /// 章节声明大小 5B、实际内容 700B（压缩炸弹特征：声明小实大）。
        /// `read_zip_entry` 的「读中 take 限长」必须把解压峰值钳到 ~max+1 快速 Err，
        /// 而非先物化 700B 再复核（此处 max=20，700B 超 take 哨兵）。
        ///
        /// ⚠️ 真炸弹用 `Deflated` 高压缩比写 700B 高重复字节，再手工把中央目录 +
        /// 本地头的 `uncompressed_size` patch 成 5——这样 `file.size()`（声明）过预检、
        /// 解压实得 700B 才触 take 哨兵。旧实现误用 `Stored`（声明==实际 30B）导致
        /// 只走声明预检、从未命中 take/读后复核路径（K153 假绿，评审 NIT#1）。
        #[test]
        fn entry_declared_small_actual_large_peaks_bounded() {
            // Deflated 压缩大章节（"A"×700 高重复 → 压缩后 ~15B），patch 声明为 5。
            let chapter = "A".repeat(700);
            let mut zip = make_epub_deflated(&chapter, &[]).into_inner();
            // 声明小（5）≠ 实际 700，且小于 max=100。
            patch_zip_uncompressed_size(&mut zip, "OEBPS/ch1.xhtml", 5);
            let mut limits = EpubLimits::default();
            limits.max_entry_bytes = 100; // 声明(5) ≤ 100 过预检；实解压 700 > 100 触发 take
            let res = parse_epub_reader_limited(std::io::Cursor::new(zip.clone()), limits);
            assert!(
                res.is_err(),
                "声明小实大（zip bomb）应被 take 限长/读后复核拒绝"
            );
            // 对照：同一炸弹、上限放宽到 1000（>实解压 700）→ 不再拒绝（证明上面的
            // Err 确因 take/上限触发，而非解析器/CRC 等其它错误——K153 判别力）。
            let mut wide = EpubLimits::default();
            wide.max_entry_bytes = 1000;
            let wide_res = parse_epub_reader_limited(std::io::Cursor::new(zip), wide);
            assert!(
                wide_res.is_ok(),
                "上限放开后同一炸弹应解析成功（空文档），证明拒绝确因 take 限长触发"
            );
        }

        /// 单张图片解压字节上限：恶意大图拒绝，不进入内存。
        #[test]
        fn image_exceeds_single_max_rejected() {
            // 合法 PNG 头 + 大量填充（真实图片内容在此不重要——read 先到 max+1 即哨兵断）。
            let chapter = "<html><body><img src=\"images/big.png\"/></body></html>";
            let png = {
                let mut v = vec![0u8; 300];
                v[0..8].copy_from_slice(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n']);
                v
            };
            let zip = make_epub(chapter, &[("images/big.png", &png)]);
            let mut limits = EpubLimits::default();
            limits.max_image_bytes = 64; // 300B 图 > 64
            let res = parse_epub_reader_limited(zip, limits);
            // 图片超限：on_img 返回 None → 章内有占位而非图片 src；整书仍可解析。
            let doc = res.expect("图片超限应降级（占位）而非整书失败");
            assert!(
                doc.images.is_empty(),
                "超限图片不得进内存图片表"
            );
            assert!(
                doc.markdown.contains("[图片"),
                "超限图片应降级为 alt 占位，实际：{markdown}",
                markdown = doc.markdown
            );
        }

        /// 图片**张数**上限：超过 N 张后后续图片不进内存。
        #[test]
        fn image_count_cap_stops_collecting() {
            let mut chapter = String::from("<html><body>");
            for i in 0..3 {
                chapter.push_str(&format!(r#"<img src="images/p{i}.png"/>"#));
            }
            chapter.push_str("</body></html>");
            let png = [
                0x89u8, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n', 0, 0, 0, 0,
            ];
            let zip = make_epub(
                &chapter,
                &[
                    ("images/p0.png", &png),
                    ("images/p1.png", &png),
                    ("images/p2.png", &png),
                ],
            );
            let mut limits = EpubLimits::default();
            limits.max_image_count = 2; // 只收前 2 张
            limits.max_image_bytes = 1 << 20;
            let doc = parse_epub_reader_limited(zip, limits).expect("解析应成功");
            assert!(
                doc.images.len() <= 2,
                "图片张数超限后不得再收：{}",
                doc.images.len()
            );
        }

        /// 全图**总字节**上限：累计超限后不再收后续图片。
        #[test]
        fn image_total_bytes_cap_stops_collecting() {
            let mut chapter = String::from("<html><body>");
            for i in 0..3 {
                chapter.push_str(&format!(r#"<img src="images/p{i}.png"/>"#));
            }
            chapter.push_str("</body></html>");
            let png8 = [0x89u8, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n'];
            // 每张 8B（真实图片数据任意，read 到点位即融）——总量闸只需字节和。
            let zip = make_epub(
                &chapter,
                &[
                    ("images/p0.png", &png8),
                    ("images/p1.png", &png8),
                    ("images/p2.png", &png8),
                ],
            );
            let mut limits = EpubLimits::default();
            limits.max_image_bytes = 1 << 20;
            limits.max_image_total_bytes = 16; // 3×8=24 > 16 → 收 2 张后第 3 张拒
            let doc = parse_epub_reader_limited(zip, limits).expect("解析应成功");
            assert!(
                doc.images.len() <= 2,
                "总量超限后不得再收：{}",
                doc.images.len()
            );
        }

        /// spine **章节数**上限：超过 max 章直接整书 Err。
        #[test]
        fn chapter_count_cap_rejects_huge_spine() {
            // 造 3 章 epub（同一内容多份 manifest/spine 条目）。
            let mut buf = std::io::Cursor::new(Vec::new());
            {
                let mut w = zip::ZipWriter::new(&mut buf);
                let opts =
                    SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
                w.start_file("META-INF/container.xml", opts).unwrap();
                w.write_all(br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#).unwrap();
                let manifest = r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="2.0"><metadata><dc:title xmlns:dc="http://purl.org/dc/elements/1.1/">T</dc:title></metadata><manifest><item id="c0" href="c0.xhtml" media-type="application/xhtml+xml"/><item id="c1" href="c1.xhtml" media-type="application/xhtml+xml"/><item id="c2" href="c2.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="c0"/><itemref idref="c1"/><itemref idref="c2"/></spine></package>"#;
                w.start_file("OEBPS/content.opf", opts).unwrap();
                w.write_all(manifest.as_bytes()).unwrap();
                for i in 0..3 {
                    w.start_file(&format!("OEBPS/c{i}.xhtml"), opts).unwrap();
                    w.write_all(b"<html><body><p>x</p></body></html>").unwrap();
                }
                w.finish().unwrap();
            }
            buf.set_position(0);
            let mut limits = EpubLimits::default();
            limits.max_chapter_count = 2; // spine 3 章 > 2
            let res = parse_epub_reader_limited(buf, limits);
            assert!(res.is_err(), "章节数超限应整书拒绝");
        }

        /// 整书 Markdown 总量上限：拼接超限 → 显式 Err。
        #[test]
        fn markdown_total_cap_rejects_with_error() {
            let chapter = "<html><body><p>这是正文。</p></body></html>";
            let zip = make_epub(chapter, &[]);
            let mut limits = EpubLimits::default();
            limits.max_markdown_bytes = 2; // 章正文远大于 2B
            let res = parse_epub_reader_limited(zip, limits);
            assert!(
                res.is_err(),
                "整书 markdown 超限应显式 Err"
            );
        }
    }

    // ===== 2026-11-17 复现红测（docs/code-review-2026-11-17/25-epub.md M 级三条）=====
    // 本批只写失败断言、禁改生产码：全部应 RED，待实现流修绿。
    mod regression_2026_11_17 {
        use super::*;
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        /// 造一个两章节各在不同目录、且共用同一 `src="images/p.png"` 但指向 **不同**
        /// zip 内图片路径的最小合法 EPUB：`OEBPS/a/images/p.png` 与 `OEBPS/b/images/p.png`
        /// 字节不同（分别以 0xAA / 0xBB 结尾做标记）。
        ///
        /// 复现缺陷 1（lib.rs img_cache 按原始 src 而非解析后 zip 路径去重）：
        /// 两章 `src` 相同 → 后章被去重到首章条目 → 章节 b 渲染出章节 a 的字节（错图）。
        fn make_epub_same_src_diff_paths(img_a: &[u8], img_b: &[u8]) -> std::io::Cursor<Vec<u8>> {
            let mut buf = std::io::Cursor::new(Vec::new());
            {
                let mut w = zip::ZipWriter::new(&mut buf);
                let opts =
                    SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
                let add = |w: &mut zip::ZipWriter<&mut std::io::Cursor<Vec<u8>>>,
                           name: &str,
                           data: &[u8]| {
                    w.start_file(name, opts).unwrap();
                    w.write_all(data).unwrap();
                };
                add(
                    &mut w,
                    "META-INF/container.xml",
                    br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#,
                );
                let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata><dc:title xmlns:dc="http://purl.org/dc/elements/1.1/">测试书</dc:title></metadata>
  <manifest>
    <item id="c1" href="a/ch.xhtml" media-type="application/xhtml+xml"/>
    <item id="c2" href="b/ch.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="c1"/><itemref idref="c2"/></spine>
</package>"#;
                add(&mut w, "OEBPS/content.opf", opf.as_bytes());
                add(
                    &mut w,
                    "OEBPS/a/ch.xhtml",
                    "<html><body><p class=\"text\">章节甲</p><img src=\"images/p.png\" alt=\"图甲\"/></body></html>"
                        .as_bytes(),
                );
                add(
                    &mut w,
                    "OEBPS/b/ch.xhtml",
                    "<html><body><p class=\"text\">章节乙</p><img src=\"images/p.png\" alt=\"图乙\"/></body></html>"
                        .as_bytes(),
                );
                add(&mut w, "OEBPS/a/images/p.png", img_a);
                add(&mut w, "OEBPS/b/images/p.png", img_b);
                w.finish().unwrap();
            }
            buf.set_position(0);
            buf
        }

        /// 缺陷 1 红测：`src` 相同但 zip 内完整路径不同 → 必须各自成条目、各回读各自字节。
        #[test]
        fn img_cache_keys_by_zip_path_not_src() {
            // 两张 12B 图：PNG 魔数 + 区分标记（0xAA / 0xBB）。
            let img_a = [
                0x89u8, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n', 0xAA, 0xAA, 0xAA, 0xAA,
            ];
            let img_b = [
                0x89u8, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n', 0xBB, 0xBB, 0xBB, 0xBB,
            ];
            let zip = make_epub_same_src_diff_paths(&img_a, &img_b);
            let doc = parse_epub_reader(zip).expect("解析应成功");
            // 断言 1（红）：两个不同 zip 路径的图应分属两个内存条目。
            assert_eq!(
                doc.images.len(),
                2,
                "src 相同但 zip 路径不同的两张图应各自成条目，实际：{}",
                doc.images.len()
            );
            // 断言 2（红）：两章 markdown 应引用两个**不同**的虚拟 src。
            let img_lines: Vec<&str> = doc
                .markdown
                .lines()
                .filter(|l| l.starts_with("!["))
                .collect();
            assert_eq!(
                img_lines.len(),
                2,
                "两章各应有一张图，实际 markdown：{doc:?}"
            );
            let vsrcs: std::collections::HashSet<&str> = img_lines
                .iter()
                .filter_map(|l| {
                    let s = l.find("](")?;
                    let e = l.rfind(')')?;
                    Some(&l[s + 2..e])
                })
                .collect();
            assert_eq!(
                vsrcs.len(),
                2,
                "两章引用的虚拟 src 应不同（按 zip 路径区分），实际：{vsrcs:?}"
            );
            // 断言 3（红）：各虚拟 src 应回读各自 zip 路径的真实字节（去重错图即在此不匹配）。
            let mut markers: Vec<u8> = doc
                .images
                .values()
                .map(|img| img.bytes.last().copied().unwrap())
                .collect();
            markers.sort_unstable();
            assert_eq!(
                markers,
                vec![0xAA, 0xBB],
                "两图字节应分别保留，实际标记：{markers:?}"
            );
        }

        /// 缺陷 2 红测：`&nbsp;`（XML 未预定义**命名**实体）→ roxmltree parse Err →
        /// `xhtml_to_markdown` 的 `Err(_) => return String::new()` → 整章静默丢空。
        /// 期望：实体正常解码为空格、章不空、实体前后文本保留。
        #[test]
        fn chapter_with_nbsp_entity_not_emptied() {
            let html = "<html><body><p class=\"text\">第一句&nbsp;第二句</p></body></html>";
            let no: Option<String> = None;
            let mut notes = std::collections::BTreeMap::new();
            let md = xhtml_to_markdown(html, &mut |_s, _a| no.clone(), &mut notes);
            assert!(
                !md.trim().is_empty(),
                "含命名实体（&nbsp;）的章节不应被整章置空，实际输出：{md:?}"
            );
            assert!(
                md.contains("第一句") && md.contains("第二句"),
                "实体前后的文本都应保留，实际：{md:?}"
            );
            // 对照（判别力，K173）：数字字符引用 `&#160;` 是 XML 预定义 → 本应正常解码。
            // 该断言现为绿，证明「章不空」机制本身可用，红只源于命名实体 `&nbsp;`。
            let md_numeric = xhtml_to_markdown(
                "<html><body><p class=\"text\">第一句&#160;第二句</p></body></html>",
                &mut |_s, _a| no.clone(),
                &mut notes,
            );
            assert!(
                md_numeric.contains("第一句") && md_numeric.contains("第二句"),
                "数字字符引用（&#160;）不应影响章节文本，实际：{md_numeric:?}"
            );
        }

        /// 造一个条目总数远超上限（65536）的最小合法 EPUB：容器三件套 + `junk_count` 条
        /// Stored 空条目（不影响解析路径，仅撑大 zip 中央目录）。zip writer 对 >65535 条目
        /// 自动写 ZIP64 尾记录（zip write.rs:1539），读取端同样支持。
        fn make_epub_many_entries(junk_count: usize) -> std::io::Cursor<Vec<u8>> {
            let mut buf = std::io::Cursor::new(Vec::new());
            {
                let mut w = zip::ZipWriter::new(&mut buf);
                let opts =
                    SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
                let add = |w: &mut zip::ZipWriter<&mut std::io::Cursor<Vec<u8>>>,
                           name: &str,
                           data: &[u8]| {
                    w.start_file(name, opts).unwrap();
                    w.write_all(data).unwrap();
                };
                add(
                    &mut w,
                    "META-INF/container.xml",
                    br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#,
                );
                let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata><dc:title xmlns:dc="http://purl.org/dc/elements/1.1/">测试书</dc:title></metadata>
  <manifest><item id="ch1" href="ch1.xhtml" media-type="application/xhtml+xml"/></manifest>
  <spine><itemref idref="ch1"/></spine>
</package>"#;
                add(&mut w, "OEBPS/content.opf", opf.as_bytes());
                add(
                    &mut w,
                    "OEBPS/ch1.xhtml",
                    b"<html><body><p>x</p></body></html>",
                );
                for i in 0..junk_count {
                    add(&mut w, &format!("junk/junk{i:05}.bin"), b"");
                }
                w.finish().unwrap();
            }
            buf.set_position(0);
            buf
        }

        /// 缺陷 3 红测：zip 条目总数无上限（各量独立硬上界唯独漏条目数）→
        /// `ZipArchive::new` 全量物化中央目录 → 恶意海量条目大分配。
        /// 期望语义：条目总数硬上界（如 ≤ 65536，建议常量 `EPUB_MAX_ZIP_ENTRIES`），超限拒绝。
        #[test]
        fn zip_entry_count_cap_rejects_huge_archive() {
            const JUNK: usize = 65_536; // 65536 垃圾 + 3 结构条目 = 65539 > 65536（建议上限）
            let zip = make_epub_many_entries(JUNK);
            let res = parse_epub_reader(zip);
            assert!(
                res.is_err(),
                "条目总数超上限（{} 条 > 65536）应整档拒绝，实际解析通过",
                JUNK + 3
            );
        }
    }

    // ===== 2026-11-24 红测：epub 脚注跨章冲突（每章独立重新编号的书型）=====
    // 多数书每章脚注独立重新编号（两章都含 footnote1/2），正文 `[^1]` 引用本
    // 章 `p.note` 定义。旧实现只建一张全局 notes 表、`or_insert` first-wins →
    // 后章同名定义被丢弃，悬停/渲染显示前章脚注。本测试用内存 zip 造两章
    // 独立编号的脚注书，断言第二章引用解析到第二章自己的定义文本。
    mod footnote_cross_chapter {
        use super::*;
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        /// 造一本两章节、各自独立重新编号脚注的最小合法 EPUB：两章都含
        /// footnote1/2（正文 `[^1][^2]` + 本章 `p.note` 定义），但两章定义文本不同。
        fn make_two_chapter_epub() -> std::io::Cursor<Vec<u8>> {
            let mut buf = std::io::Cursor::new(Vec::new());
            {
                let mut w = zip::ZipWriter::new(&mut buf);
                let opts =
                    SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
                let add = |w: &mut zip::ZipWriter<&mut std::io::Cursor<Vec<u8>>>,
                           name: &str,
                           data: &[u8]| {
                    w.start_file(name, opts).unwrap();
                    w.write_all(data).unwrap();
                };
                add(
                    &mut w,
                    "META-INF/container.xml",
                    br#"<?xml version="1.0"?><container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
                );
                let opf = r#"<?xml version="1.0"?><package xmlns="http://www.idpf.org/2007/opf" version="2.0"><metadata><dc:title xmlns:dc="http://purl.org/dc/elements/1.1/">两章书</dc:title></metadata><manifest><item id="c1" href="c1.xhtml" media-type="application/xhtml+xml"/><item id="c2" href="c2.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="c1"/><itemref idref="c2"/></spine></package>"#;
                add(&mut w, "OEBPS/content.opf", opf.as_bytes());
                // 第一章：独立编号 1/2，定义文本「第一章…」
                add(
                    &mut w,
                    "OEBPS/c1.xhtml",
                    r#"<html><body><p class="text">第一章甲<a id="note1" href="c1.xhtml#footnote1"><sup>[1]</sup></a>、乙<a id="note2" href="c1.xhtml#footnote2"><sup>[2]</sup></a>。</p><p class="note"><a id="footnote1" href="c1.xhtml#note1">[1]</a> 第一章的脚注一。</p><p class="note"><a id="footnote2" href="c1.xhtml#note2">[2]</a> 第一章的脚注二。</p></body></html>"#.as_bytes(),
                );
                // 第二章：同样独立编号 1/2，但定义文本不同
                add(
                    &mut w,
                    "OEBPS/c2.xhtml",
                    r#"<html><body><p class="text">第二章甲<a id="note1" href="c2.xhtml#footnote1"><sup>[1]</sup></a>、乙<a id="note2" href="c2.xhtml#footnote2"><sup>[2]</sup></a>。</p><p class="note"><a id="footnote1" href="c2.xhtml#note1">[1]</a> 第二章的脚注一。</p><p class="note"><a id="footnote2" href="c2.xhtml#note2">[2]</a> 第二章的脚注二。</p></body></html>"#.as_bytes(),
                );
                w.finish().unwrap();
            }
            buf.set_position(0);
            buf
        }

        /// 找 `md` 里 `from` 之后第一个 `[^N]` 引用标记的裸号 N。
        fn next_ref(md: &str, from: &str) -> usize {
            let start = md.find(from).expect("应含定位文本");
            let rest = &md[start..];
            let p = rest.find("[^").expect("应含脚注引用 [^N]");
            let digits: String = rest[p + 2..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            assert!(!digits.is_empty(), "引用标记后应有数字：{rest:?}");
            digits.parse().unwrap()
        }

        #[test]
        fn cross_chapter_footnotes_do_not_collide() {
            let doc = parse_epub_reader(make_two_chapter_epub()).expect("两章脚注书解析应成功");
            // 第一章自身脚注：定义与引用应一致（既有行为，回归护栏）。
            let n1 = next_ref(&doc.markdown, "第一章甲");
            assert_eq!(
                doc.notes.get(&n1).map(|s| s.as_str()),
                Some("第一章的脚注一。"),
                "第一章 [^1] 应解析到第一章自己的定义文本"
            );
            // 红：旧实现 second-wins 被 first-wins 丢弃 → 第二章 [^1] 解析到第一章文本。
            let n2 = next_ref(&doc.markdown, "第二章甲");
            assert_eq!(
                doc.notes.get(&n2).map(|s| s.as_str()),
                Some("第二章的脚注一。"),
                "第二章 [^1] 应解析到第二章自己的定义文本"
            );
            // 红：两章各 1/2 号共 4 条定义都应保留（旧实现只有 2 条）。
            assert_eq!(
                doc.notes.len(),
                4,
                "两章各自 1/2 号脚注应互不覆盖：{:?}",
                doc.notes
            );
        }
    }
}
