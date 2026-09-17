//! EPUB 读取模块。
//!
//! 一个 EPUB 是 OCF 容器（zip）：
//! - `mimetype`：固定首文件，`application/epub+zip`
//! - `META-INF/container.xml`：指出 OPF 路径
//! - `content.opf`：书籍元数据 + 清单(manifest) + 阅读顺序(spine)
//! - 每个章节是一个 XHTML 文件（spine 决定阅读顺序）
//!
//! 本模块把 EPUB 解析成**平铺的 XHTML 文本流**后转成 Markdown，交给
//! [`crate::markdown::MarkdownPreview`] 渲染（复用行级虚拟化预览）。
//! 图片以**内存字节**形式保存在 [`EpubDocument::images`]（虚拟 src → 字节），
//! 渲染时交给预览直接从内存解码显示，全程不写硬盘。

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context as _, Result};
use gpui::{Image, ImageFormat};

use crate::config::markdown::EPUB_IMG_SCHEME;
use crate::markdown::RowAlign;

/// 解析后的 EPUB 文档：元数据 + 按 spine 顺序的章节 Markdown 文本 + 内存图片。
#[derive(Debug, Clone, Default)]
pub struct EpubDocument {
    /// 书名（dc:title）
    pub title: String,
    /// 作者（dc:creator）
    pub author: String,
    /// 按阅读顺序拼接的整书 Markdown 文本（每章以章标题开始）
    pub markdown: String,
    /// 章节标题列表（预留：供目录跳转使用）
    #[allow(dead_code)]
    pub chapters: Vec<String>,
    /// 章节数
    pub chapter_count: usize,
    /// 内存图片表：markdown 里的虚拟 src（`epubimg://xxx`）→ 图片字节。
    /// 渲染时由预览查表直接解码，不落盘。
    pub images: HashMap<String, Arc<Image>>,
    /// 脚注表：注记号 → 注释正文（正文里 `[^n]` 悬浮卡片的数据源）。
    pub notes: std::collections::BTreeMap<usize, String>,
}

/// 从 `.epub` 文件路径解析文档。
pub fn parse_epub_file(path: &Path) -> Result<EpubDocument> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("无法打开 EPUB 文件：{}", path.display()))?;
    parse_epub_reader(file)
}

/// 从任意 `Read + Seek` 读取器解析 EPUB（zip 需要 seek）。
pub fn parse_epub_reader<R: Read + std::io::Seek>(reader: R) -> Result<EpubDocument> {
    let mut archive = zip::ZipArchive::new(reader).context("无效的 EPUB（zip 容器读取失败）")?;

    // 1) 定位 OPF：读 META-INF/container.xml
    let container_xml = read_zip_entry(&mut archive, "META-INF/container.xml")
        .context("EPUB 缺少 META-INF/container.xml")?;
    let opf_path = find_opf_path(&container_xml)?;

    // 2) 读取 OPF：解析 metadata + manifest + spine
    let opf_xml = read_zip_entry(&mut archive, &opf_path)
        .with_context(|| format!("读取 OPF 失败：{opf_path}"))?;
    let (title, author, spine_hrefs) = parse_opf(&opf_xml, &opf_path)?;

    // 3) 按 spine 顺序逐章读取 XHTML → 转 Markdown；图片保留在内存（不落盘）
    let mut markdown = String::new();
    let mut chapters: Vec<String> = Vec::new();
    let mut images: HashMap<String, Arc<Image>> = HashMap::new();
    let mut notes: std::collections::BTreeMap<usize, String> = std::collections::BTreeMap::new();
    let base_dir = parent_zip_dir(&opf_path);
    // 虚拟 src → 字段/条目的去重（同一 zip 内路径只解码一次进内存）
    let mut img_cache: HashMap<String, String> = HashMap::new();
    let mut img_counter = 0usize;
    for href in &spine_hrefs {
        let entry_path = join_zip_path(&base_dir, href);
        let chapter_html = read_zip_entry(&mut archive, &entry_path)
            .with_context(|| format!("读取章节失败：{entry_path}"))?;
        let chapter_dir = {
            let s = href.rsplitn(2, '/').nth(1).unwrap_or("").to_string();
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
            if let Some(vsrc) = img_cache.get(src) {
                return Some(vsrc.clone());
            }
            let full = join_zip_path(&chapter_dir, src);
            let mut file = archive.by_name(&full).ok()?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).ok()?;
            if bytes.is_empty() {
                return None;
            }
            img_counter += 1;
            // 以文件真实签名推断格式（扩展名可能误导：如 00001.jpeg 实为 PNG）
            let format = match image_format_for_bytes(&bytes) {
                Some(f) => f,
                None => return None, // 未知格式 → 走 alt 占位
            };
            let ext = format_ext(format);
            let vsrc = format!("{EPUB_IMG_SCHEME}{img_counter}.{ext}");
            // 内存持有字节：Image::from_bytes(format, bytes)
            images.insert(vsrc.clone(), Arc::new(Image::from_bytes(format, bytes)));
            img_cache.insert(src.to_string(), vsrc.clone());
            Some(vsrc)
        };
        let chapter_md = xhtml_to_markdown(&chapter_html, &mut on_img, &mut notes);
        if chapter_md.trim().is_empty() {
            continue;
        }
        // 提取首个标题作为章节名（供目录）
        let heading = first_heading(&chapter_md);
        if !heading.is_empty() {
            chapters.push(heading.to_string());
        }
        markdown.push_str(&chapter_md);
        markdown.push('\n');
    }

    Ok(EpubDocument {
        title,
        author,
        markdown,
        chapters,
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
        || data.get(..128).is_some_and(|s| String::from_utf8_lossy(s).contains("<svg"))
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

/// 从 zip 读取单个条目为 UTF-8 字符串。
fn read_zip_entry<R: Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
) -> Result<String> {
    let mut file = archive
        .by_name(name)
        .with_context(|| format!("zip 中不存在 {name}"))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    // 去除 UTF-8 BOM
    let bytes = buf.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&buf);
    let text = String::from_utf8_lossy(bytes);
    Ok(text.into_owned())
}

/// 从 container.xml 里解析 OPF 路径（rootfile full-path 属性）。
fn find_opf_path(container_xml: &str) -> Result<String> {
    let doc = roxmltree::Document::parse(container_xml).context("解析 container.xml 失败")?;
    for node in doc.descendants() {
        if node.has_tag_name("rootfile") {
            if let Some(full) = node.attribute("full-path") {
                if !full.trim().is_empty() {
                    return Ok(full.trim().to_string());
                }
            }
        }
    }
    bail!("container.xml 里找不到 rootfile/full-path")
}

/// 解析 OPF：返回（标题, 作者, spine 章节 href 列表）。
///
/// spine 里每个 itemref 的 idref 指向 manifest 的 id；通过 manifest 映射到 href。
/// OPF 英寸 XML 命名空间无关紧要，按 tag 名（local name）匹配即可。
fn parse_opf(opf_xml: &str, opf_path: &str) -> Result<(String, String, Vec<String>)> {
    let doc = roxmltree::Document::parse(opf_xml).context("解析 OPF 失败")?;

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
        if node.has_tag_name("item") {
            if let (Some(id), Some(href)) = (node.attribute("id"), node.attribute("href")) {
                manifest.insert(id.to_string(), href.to_string());
            }
        }
    }

    // spine：itemref idref 顺序 = 阅读顺序
    let mut spine_hrefs: Vec<String> = Vec::new();
    for node in doc.descendants() {
        if node.has_tag_name("itemref") {
            if let Some(idref) = node.attribute("idref") {
                if let Some(href) = manifest.get(idref) {
                    // 过滤掉 ncx/封面等非 xhtml 项，只保留章节
                    if href.ends_with(".html") || href.ends_with(".xhtml") || href.ends_with(".htm")
                    {
                        spine_hrefs.push(href.clone());
                    }
                }
            }
        }
    }

    if spine_hrefs.is_empty() {
        bail!("OPF spine 不含任何章节：{opf_path}");
    }

    Ok((title, author, spine_hrefs))
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
    let doc = match roxmltree::Document::parse(html) {
        Ok(d) => d,
        Err(_) => return String::new(),
    };
    let mut out = String::new();
    walk_body(&doc.root_element(), &mut out, 0, None, notes, on_img);
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

/// 递归遍历正文元素。
///
/// - `quoted`：当前引述嵌套深度；
/// - `align`：从祖先 `float-r`/`float-l` 继承的对齐（`p.signature` 会就地覆写为右）；
/// - `notes`：脚注表（「注释」章的 `p.note` 段落在此登记）。
fn walk_body(
    node: &roxmltree::Node,
    out: &mut String,
    quoted: usize,
    align: Option<RowAlign>,
    notes: &mut std::collections::BTreeMap<usize, String>,
    on_img: &mut dyn FnMut(&str, &str) -> Option<String>,
) {
    match node.tag_name().name() {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = node.tag_name().name()[1..].parse::<u8>().unwrap_or(1);
            let text = node_text_epub(node);
            out.push('\n');
            out.push_str(&"#".repeat(level as usize));
            out.push(' ');
            out.push_str(text.trim());
            out.push_str("\n\n");
            return;
        }
        "p" => {
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
            if note_def {
                if let Some(first) = segs.iter_mut().find(|s| !s.trim().is_empty()) {
                    if let Some(rel) = first.find('[') {
                        let after = &first[rel + 1..];
                        let d = after.chars().take_while(|c| c.is_ascii_digit()).count();
                        if d > 0 && after[d..].starts_with(']') {
                            first.insert(rel + 1, '!');
                        }
                    }
                }
            }
            let joined = segs.join("\n");
            let t = joined.trim().to_string();
            if note_def {
                if let Some(n) = leading_footnote_id(node) {
                    let marker = format!("[{n}]");
                    if let Some(rest) = t.strip_prefix(marker.as_str()) {
                        notes
                            .entry(n)
                            .or_insert_with(|| rest.trim_start_matches('\u{3000}').trim().to_string());
                    }
                }
            }
            // 正文（`class="text"` 或空 class、且不在引述块内）首行缩进 2 字符：
            // 行首前置两个全角空格，渲染时折行后天然「首行缩进、次行顶格」；
            // 对齐行（落款/浮段）与引述/注释等段落不缩进。
            let is_body_text = quoted == 0
                && (cls.split_whitespace().any(|c| c == "text") || cls.trim().is_empty());
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
            return;
        }
        "blockquote" => {
            out.push('\n');
        }
        "ul" | "ol" => {}
        "li" => {
            let t = node_text_epub(node);
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
                let marker = if parent_is_ordered(node) { "- " } else { "• " };
                out.push_str(marker);
                out.push_str(&quote_prefix(&line, quoted));
                out.push('\n');
            }
            return;
        }
        "a" => {
            // 正文脚注标记：`<a id="noteN" href="...#footnoteN"><sup>[N]</sup></a>`
            // → `[^N]`（渲染层做悬浮卡片；通常已在 p/标题内被 [`node_text_epub`]
            // 处理，此处兜底裸露在 div/块外的锚）；普通链接原样走子节点文本。
            if let Some(n) = note_ref_at(node) {
                out.push_str(&format!(" [^{n}] "));
                return;
            }
        }
        "img" | "image" => {
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
                    let alt_esc = if alt.is_empty() { "图片".to_string() } else { alt.clone() };
                    out.push_str(&format!("\n\n![{alt_esc}]({path})\n\n"));
                }
                None => {
                    let label = if alt.is_empty() { "图片".to_string() } else { alt.clone() };
                    out.push_str(&format!("\n\n> [图片：{label}]\n\n"));
                }
            }
            return;
        }
        "br" => {
            out.push('\n');
            return;
        }
        "sup" => {
            let t = node_text(node);
            let t = t.trim().to_string();
            if !t.is_empty() {
                out.push_str(&format!(" [{t}] "));
            }
            return;
        }
        "title" | "style" | "script" | "head" | "meta" | "link" | "#comment" => return,
        _ => {}
    }

    let child_quoted = quoted + usize::from(node.has_tag_name("blockquote"));
    // `float-r` / `float-l` 容器内的段落下移该对齐（也用于取消正文首行缩进）。
    let child_align = if node.has_tag_name("div") {
        match node.attribute("class").unwrap_or("").split_whitespace().find(|c| {
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
            walk_body(&child, out, child_quoted, child_align, notes, on_img);
        }
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
    let a = node.children().find(|c| c.is_element() && c.has_tag_name("a"))?;
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
        if n.is_text() {
            if let Some(t) = n.text() {
                s.push_str(t);
            }
        }
    }
    s
}

/// 取段落等**正文容器**的文本，但把正文脚注锚
/// （`<a id="noteN" href="...#footnoteN"><sup>[N]</sup></a>`）替换成 `[^N]` 标记
/// （渲染层据此做悬浮注释卡片）。普通链接/注释章锚（`id="footnoteN"`）按文本走。
fn node_text_epub(node: &roxmltree::Node) -> String {
    let mut s = String::new();
    for child in node.children() {
        if child.is_text() {
            if let Some(t) = child.text() {
                s.push_str(t);
            }
        } else if child.is_element() {
            if child.has_tag_name("a") {
                if let Some(n) = note_ref_at(&child) {
                    s.push_str(&format!(" [^{n}] "));
                    continue; // 跳过其 `<sup>[N]</sup>` 子节点
                }
            }
            s.push_str(&node_text_epub(&child));
        }
    }
    s
}

/// 同 [`node_text_epub`]，但把段落里的 `<br>` 当成**换行**：返回按 `<br>` 切开的
/// 多段文本（EPUB 落款如“卡尔·马克思 弗里德里希·恩格斯<br/>1882年1月21日于伦敦”
/// 应分别成行，而不是挤在同一行）。
fn node_text_epub_lines(node: &roxmltree::Node) -> Vec<String> {
    let mut segs = vec![String::new()];
    walk_text_lines(node, &mut segs);
    segs
}

fn walk_text_lines(node: &roxmltree::Node, segs: &mut Vec<String>) {
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
            if child.has_tag_name("a") {
                if let Some(n) = note_ref_at(&child) {
                    segs.last_mut().unwrap().push_str(&format!(" [^{n}] "));
                    continue;
                }
            }
            walk_text_lines(&child, segs);
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

/// 从章节 Markdown 里提取第一个标题（`# xxx`）。
fn first_heading(md: &str) -> &str {
    md.lines()
        .find(|l| l.starts_with('#'))
        .map(|l| l.trim_start_matches('#').trim())
        .unwrap_or("")
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
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/xy.epub")
    }

    #[test]
    fn parses_epub_title_and_author() {
        let doc = parse_epub_file(&test_epub_path()).expect("解析 xy.epub 应成功");
        assert_eq!(doc.title, "共产党宣言");
        assert!(doc.author.contains("马克思"));
    }

    #[test]
    fn extracts_chapters_in_spine_order() {
        let doc = parse_epub_file(&test_epub_path()).expect("解析 xy.epub 应成功");
        assert!(doc.chapter_count >= 5, "应有多个章节，实际={}", doc.chapter_count);
        assert!(!doc.markdown.is_empty());
        // 应包含著名开头标题之一（1888年英文版序言 或 第一章 资产者和无产者）
        let any = ["1888年英文版序言", "资产者和无产者"]
            .iter()
            .any(|kw| doc.markdown.contains(kw));
        assert!(any, "正文应包含书名/章标题之一");
    }

    #[test]
    fn extracts_epub_images_to_memory() {
        let doc = parse_epub_file(&test_epub_path()).expect("解析 xy.epub 应成功");
        // xy.epub 有 6 张图（正文 5 张 + SVG 封面 1 张）；正文里以 `![...](epubimg://...)` 虚拟 src 引用
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
        let md = xhtml_to_markdown(&html, &mut |_s, _a| no_img.clone(), &mut notes);
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
        let md = xhtml_to_markdown(&html, &mut |_s, _a| no.clone(), &mut notes);
        assert!(md.contains("%%ALIGN:RIGHT%%卡尔·马克思"), "签名应带右对齐指令：{md}");
        // `<br>` 应把签名拆成两行，且每行都带右对齐指令（日期单独成行）。
        assert!(
            md.contains("%%ALIGN:RIGHT%%卡尔·马克思\n%%ALIGN:RIGHT%%1872年6月24日于伦敦"),
            "签名 `<br>` 应拆行且两行都右对齐：{md:?}"
        );
        assert!(!md.contains("卡尔·马克思1872年6月24日于伦敦"), "日期不得与姓名挤在一行: {md:?}");
        assert!(md.contains("%%ALIGN:RIGHT%%原文是德文"), "float-r 应带右对齐指令：{md}");
        assert!(md.contains("%%ALIGN:LEFT%%卡·马克思写于1872年"), "float-l 应带左对齐指令：{md}");
        assert!(!md.contains("%%ALIGN:%普通正文段"), "普通正文不应带对齐指令：{md}");
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
        let md = xhtml_to_markdown(&html, &mut |_s, _a| no.clone(), &mut notes);
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
            xhtml_to_markdown(&html, &mut on_img, &mut notes)
        };
        assert!(md.contains("![插图一](/tmp/x/p1.jpeg)"), "图片应转成 markdown 图片，实际={md:?}");
    }

    #[test]
    fn xhtml_img_without_resolver_falls_back() {
        let html = "<html><body><img src=\"x.png\" alt=\"图A\"/></body></html>";
        let no: Option<String> = None;
        let mut notes = std::collections::BTreeMap::new();
        let md = xhtml_to_markdown(&html, &mut |_s, _a| no.clone(), &mut notes);
        assert!(md.contains("[图片：图A]"), "无解析结果应输出占位，实际={md:?}");
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
            xhtml_to_markdown(&html, &mut on_img, &mut notes)
        };
        assert!(
            md.contains("![图片](epubimg://1.jpeg)"),
            "SVG image 应转成 markdown 图片，实际={md:?}"
        );
    }

    #[test]
    fn join_zip_path_resolves_parent() {
        assert_eq!(join_zip_path("OEBPS", "text/ch1.xhtml"), "OEBPS/text/ch1.xhtml");
        assert_eq!(join_zip_path("OEBPS", "../META-INF/container.xml"), "META-INF/container.xml");
        assert_eq!(join_zip_path("", "text/ch1.xhtml"), "text/ch1.xhtml");
    }
}
