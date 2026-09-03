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
//! 图片等二进制资源先忽略（输出占位文本），聚焦正文可读。

use std::io::Read;
use std::path::Path;

use anyhow::{bail, Context as _, Result};

/// 解析后的 EPUB 文档：元数据 + 按 spine 顺序的章节 Markdown 文本。
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

    // 3) 按 spine 顺序逐章读取 XHTML → 转 Markdown
    let mut markdown = String::new();
    let mut chapters: Vec<String> = Vec::new();
    let base_dir = parent_zip_dir(&opf_path);
    for href in &spine_hrefs {
        let entry_path = join_zip_path(&base_dir, href);
        let chapter_html = read_zip_entry(&mut archive, &entry_path)
            .with_context(|| format!("读取章节失败：{entry_path}"))?;
        let chapter_md = xhtml_to_markdown(&chapter_html);
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
    })
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
/// 换行(br)、脚注上标(sup/注记) 简化、图片(img) 占位、忽略 head/style/script。
fn xhtml_to_markdown(html: &str) -> String {
    let doc = match roxmltree::Document::parse(html) {
        Ok(d) => d,
        Err(_) => return String::new(),
    };
    let mut out = String::new();
    walk_body(&doc.root_element(), &mut out, 0);
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

/// 递归遍历正文元素，`quoted` 为当前引述嵌套深度。
fn walk_body(node: &roxmltree::Node, out: &mut String, quoted: usize) {
    match node.tag_name().name() {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = node.tag_name().name()[1..].parse::<u8>().unwrap_or(1);
            let text = node_text(node);
            out.push('\n');
            out.push_str(&"#".repeat(level as usize));
            out.push(' ');
            out.push_str(text.trim());
            out.push_str("\n\n");
            return;
        }
        "p" => {
            let t = node_text(node);
            let t = t.trim().to_string();
            if !t.is_empty() {
                out.push_str(&quote_prefix(&t, quoted));
                out.push('\n');
            }
            return;
        }
        "blockquote" => {
            out.push('\n');
        }
        "ul" | "ol" => {}
        "li" => {
            let t = node_text(node);
            let t = t.trim().to_string();
            if !t.is_empty() {
                let marker = if parent_is_ordered(node) { "- " } else { "• " };
                out.push_str(marker);
                out.push_str(&quote_prefix(&t, quoted));
                out.push('\n');
            }
            return;
        }
        "img" => {
            let alt = node.attribute("alt").unwrap_or("图");
            out.push_str(&format!("\n\n> [图片：{alt}]\n\n"));
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
    for child in node.children() {
        if child.is_element() || child.is_text() {
            walk_body(&child, out, child_quoted);
        }
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
    fn xhtml_to_markdown_preserves_heading_and_paragraph() {
        let html = "<html><body><h1>第一章</h1><p>正文内容。</p><blockquote><p>引述。</p></blockquote></body></html>";
        let md = xhtml_to_markdown(html);
        assert!(md.contains("# 第一章"));
        assert!(md.contains("正文内容。"));
        assert!(md.contains("> 引述。"));
    }

    #[test]
    fn join_zip_path_resolves_parent() {
        assert_eq!(join_zip_path("OEBPS", "text/ch1.xhtml"), "OEBPS/text/ch1.xhtml");
        assert_eq!(join_zip_path("OEBPS", "../META-INF/container.xml"), "META-INF/container.xml");
        assert_eq!(join_zip_path("", "text/ch1.xhtml"), "text/ch1.xhtml");
    }
}
