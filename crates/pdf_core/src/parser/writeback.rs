//! 写回 / 页操作（Phase A）簇：worker 单线程内的低层 FPDF 调用。
//!
//! 关键原则（单工作线程不变式，勿破坏）：所有 FPDF_* 调用（含 FPDF_SaveAsCopy /
//! FPDFPage_CreateAnnot / FPDFPage_New/Delete / FPDF_ImportPages）全部发生在 worker
//! 线程内、经进程级 bindings（`fpdf`）进行；viewer 线程只发消息收结果，绝不触碰 FPDF。
//!
//! 原生句柄生命周期：
//!   FPDF_LoadMemDocument64 → FPDF_DOCUMENT（结束须 FPDF_CloseDocument）
//!   FPDF_LoadPage → FPDF_PAGE（结束须 FPDF_ClosePage）
//!   FPDFPage_CreateAnnot → FPDF_ANNOTATION（结束须 FPDFPage_CloseAnnot）
//!   FPDF_CreateNewDocument → FPDF_DOCUMENT（结束须 FPDF_CloseDocument，含失败分支）
//! 任一环节失败都在归还前 Close 已打开的更内层句柄（防柄泄漏；参照 pdfium-render drop 语义）。
//!
//! 本簇含：自制 FPDF_FILEWRITE 回调（落盘 [`RawFileWrite`] / 内存 [`RawFileWriteVec`]）、
//! 原子替换写盘（`save_raw_doc_to_file`）、内存工作副本操作（[`writeback_edit_mem`] /
//! [`reorder_edit_mem`]）、多页导出/拆分/合并（[`export_pages_to_file`]）、注释写回与读回
//! （[`apply_annotations_raw`] / [`read_annotations`]）、书签抽取（[`extract_outline`]）。
//!
//! 跨簇可见性（模块重组）：除对外 API 外，被根 `run` 调用的骨架/入口升 `pub(crate)`；
//! `fpdf_last_error`（错误码文案）仍驻根模块，本簇经 `super::fpdf_last_error` 访问。

use std::collections::HashMap;
use std::io::Write;
use std::os::raw::{c_int, c_ulong, c_void};
use std::path::Path;
use std::sync::Arc;
// pub(crate) 重导出：`TMP_SEQ` 与原子类型（Ordering）供 `parser/tests.rs` 经根模块
// `use super::*` 取用（根不再直接 import 原子类型，放在本簇顺带可及）。
pub(crate) use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, Result};
use pdfium_render::prelude::*;

use super::{
    AnnotKind, AnnotationInfo, AnnotateItem, DocId, PdfOutlineEntry, QuadPt, fpdf_last_error,
};

/// 把内存 bytes 载入**自有 raw 句柄**文档（写回/提取的独立副本，不动渲染中 PdfDocument）。
pub(crate) unsafe fn load_raw_doc(
    bindings: &dyn PdfiumLibraryBindings,
    bytes: &[u8],
) -> Result<FPDF_DOCUMENT> {
    let h = unsafe { bindings.FPDF_LoadMemDocument64(bytes, None) };
    if h.is_null() {
        return Err(anyhow!("载入 PDF 失败（FPDF_GetLastError={}）", fpdf_last_error(bindings)));
    }
    Ok(h)
}

/// 自制 `FPDF_FILEWRITE` 回调结构（与 pdfium-render 的 `FpdfFileWriteExt` / bindgen
/// `FPDF_FILEWRITE_` 同内存布局：`version` + `write_block` 前置两字段，pdfium 只读这两
/// 字段；后缀自定义 `file` 字段由回调经指针还原访问）。禁止改动首两字段的类型/顺序。
#[repr(C)]
struct RawFileWrite<'a> {
    version: c_int,
    write_block: Option<unsafe extern "C" fn(*mut FPDF_FILEWRITE, *const c_void, c_ulong) -> c_int>,
    file: &'a mut std::fs::File,
}

/// FPDF_SaveAsCopy 的写块回调：把 pdfium 产出的字节流直接写进目标临时文件（同步立即）。
unsafe extern "C" fn raw_file_write_block(
    this_: *mut FPDF_FILEWRITE,
    data: *const c_void,
    size: c_ulong,
) -> c_int {
    let this = unsafe { &mut *(this_ as *mut RawFileWrite) };
    match this
        .file
        .write_all(unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) })
    {
        Ok(()) => 1,
        Err(_) => 0,
    }
}

/// 写回目标「被占用/只读」时的可操作指引文案（R1 专用）：唯一判据 = 用户可读句子，
/// 供 [`is_writeback_busy_error`] 匹配 `last_modify_error`（改措辞须同步测试与 shell 层）。
const WRITEBACK_BUSY_INSTRUCTION: &str =
    "PDF 正被其他程序打开或文件只读；请关闭占用它的程序后重试";

/// 判断 `last_modify_error` 文案是否属「目标文件被其他程序占用/只读」（shell 层 toast
/// 据此补第二条路：另存为新副本）。
pub fn is_writeback_busy_error(msg: &str) -> bool {
    msg.contains(WRITEBACK_BUSY_INSTRUCTION)
}

/// Windows `rename` 覆盖被占用（无 FILE_SHARE_DELETE）/只读目标返回 ERROR_ACCESS_DENIED
/// （std 映射 `io::ErrorKind::PermissionDenied`）：杀软/预览器可能瞬时锁住 → 短重试；
/// 常驻占用与非「被占用/只读」类错误（磁盘满/网络/目录无权限）不重试、直接上抛。
pub(crate) fn is_access_denied(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(5)
}

/// 原子替换前的短重试（R2）：首跑 + ≤2 次重试、间隔 50ms（应对杀软/预览器瞬时锁）。
/// 重试只对「被占用/只读」类错误生效；重试耗尽即最终失败，交 [`map_write_io_error`] 呈现。
pub(crate) fn rename_with_retry(tmp: &Path, target: &Path) -> std::io::Result<()> {
    const EXTRA_RETRIES: usize = 2;
    const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);
    for attempt in 0..=EXTRA_RETRIES {
        match std::fs::rename(tmp, target) {
            Ok(()) => return Ok(()),
            Err(e) if is_access_denied(&e) && attempt < EXTRA_RETRIES => {
                std::thread::sleep(RETRY_DELAY);
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("重试循环必然 return")
}

/// 把 rename/写盘 io 错误映射成用户可操作中文（R1）：ERROR_ACCESS_DENIED（被占用/只读）
/// → 专属指引（注明不破坏原文件）；其余 io 错误保原文案、前缀「写回 PDF 失败：」。
pub(crate) fn map_write_io_error(e: std::io::Error) -> String {
    if is_access_denied(&e) {
        format!("{WRITEBACK_BUSY_INSTRUCTION}（原文件未受影响、不会被破坏；修改仍在本应用内）")
    } else {
        format!("写回 PDF 失败：{e}")
    }
}

/// 原子替换提交（R0a）：rename 成功则临时文件已随操作移走；失败则清理临时文件
/// （防 `.pid.seq.tmp.pdf` 点文件残留）并映射错误文案后上抛。
pub(crate) fn commit_rename(tmp: &Path, target: &Path) -> Result<()> {
    match rename_with_retry(tmp, target) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(tmp);
            Err(anyhow!("{}", map_write_io_error(e)))
        }
    }
}

/// 自制 `FPDF_FILEWRITE` 回调结构（写进 `Vec<u8>` 内存缓冲）：与 [`RawFileWrite`] 同
/// 内存布局（`version` + `write_block` 前置两字段），只把产出字节追加进缓冲而非文件。
/// 供「待保存」内存工作副本的页面操作（插页/删页/撤销恢复）使用——不动磁盘。
#[repr(C)]
struct RawFileWriteVec<'a> {
    version: c_int,
    write_block: Option<unsafe extern "C" fn(*mut FPDF_FILEWRITE, *const c_void, c_ulong) -> c_int>,
    buf: &'a mut Vec<u8>,
}

/// `FPDF_SaveAsCopy` 的写块回调：把 pdfium 产出的字节流直接追加进内存 `Vec<u8>`。
unsafe extern "C" fn raw_file_write_vec_block(
    this_: *mut FPDF_FILEWRITE,
    data: *const c_void,
    size: c_ulong,
) -> c_int {
    let this = unsafe { &mut *(this_ as *mut RawFileWriteVec) };
    this.buf
        .extend_from_slice(unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) });
    1
}

/// T3：**惰性物化 stores 基线**——打开不再镜像源字节（[`WorkerMsg::Open`] stores 留空），
/// 首个写/存/导出/读回经本 helper 从 live 渲染 doc `SaveAsCopy` 物化一次并落 stores；此后
/// 命中缓存零重复。语义与现状「Open 即 clone 源字节」的差异：物化产物 = `FPDF_SaveAsCopy`
/// （重编码），非原始源字节——写回/撤销/另存的 `before` 基线由「源字节」变为「重编码」，页/
/// 内容级保真不变（既有写路径测试覆盖），字节级不恒等（设计评审已定可接受）。
///
/// 仅当 stores 缺失（= 文档刚 Open / 刚经 viewer 重载）时才谈得上物化；此时 `docs` 必为
/// 最新 live 态（Open 载入 / open_from_bytes 重载后的解析结果），与「stores 本应持有的
/// 字节」同源，不会物化出陈旧内容。
pub(crate) fn materialize_stores(
    stores: &mut HashMap<DocId, Arc<Vec<u8>>>,
    docs: &HashMap<DocId, PdfDocument<'_>>,
    doc_id: DocId,
) -> Result<Arc<Vec<u8>>> {
    if let Some(b) = stores.get(&doc_id) {
        return Ok(b.clone());
    }
    let doc = docs
        .get(&doc_id)
        .ok_or_else(|| anyhow!("文档 {doc_id} 不存在（无法物化 stores）"))?;
    let bytes = doc
        .save_to_bytes()
        .map_err(|e| anyhow!("物化 stores 失败（SaveAsCopy：{e}）"))?;
    let bytes = Arc::new(bytes);
    stores.insert(doc_id, bytes.clone());
    Ok(bytes)
}

/// 把 raw 句柄文档重新编码成内存 bytes（等效 [`save_raw_doc_to_file`] 但零磁盘）。
/// 供 worker 内存工作副本（stores）的页面操作产出新镜像，由 viewer 内存级重载。
unsafe fn save_raw_doc_to_bytes(
    bindings: &dyn PdfiumLibraryBindings,
    doc_handle: FPDF_DOCUMENT,
) -> Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::new();
    // w 持有 `&mut buf`：块作用域收尾即结束借用（回调同步返回后无需显式 drop）。
    let ok = unsafe {
        let mut w = RawFileWriteVec {
            version: 1,
            write_block: Some(raw_file_write_vec_block),
            buf: &mut buf,
        };
        bindings.FPDF_SaveAsCopy(doc_handle, (&mut w as *mut RawFileWriteVec).cast(), 0)
    };
    if !bindings.is_true(ok) {
        return Err(anyhow!("FPDF_SaveAsCopy 失败（err={}）", fpdf_last_error(bindings)));
    }
    Ok(buf)
}

/// 把 raw 句柄文档保存到 `target`：先写**同目录临时文件**，成功后再原子替换目标
/// （Windows 上 `std::fs::rename` = `MoveFileExW + MOVEFILE_REPLACE_EXISTING`，可覆盖已存
/// 文件）。任一步失败都保留原目标文件完好（先落盘再替换，防中途损坏原文件）。
pub(crate) unsafe fn save_raw_doc_to_file(
    bindings: &dyn PdfiumLibraryBindings,
    doc_handle: FPDF_DOCUMENT,
    target: &Path,
) -> Result<()> {
    let dir = match target.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => std::env::temp_dir(),
    };
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    // 同目录 . 前缀隐藏文件，随进程 id + 序号唯一；同名写回/重载无碰撞风险
    let tmp = dir.join(format!(".{}.{seq}.tmp.pdf", std::process::id()));
    let mut file = std::fs::File::create(&tmp)?;
    // w 持有 `&mut file`：块作用域收尾即结束借用（回调同步返回后无需显式 drop）。
    let ok = unsafe {
        let mut w = RawFileWrite {
            version: 1,
            write_block: Some(raw_file_write_block),
            file: &mut file,
        };
        bindings.FPDF_SaveAsCopy(doc_handle, (&mut w as *mut RawFileWrite).cast(), 0)
    };
    file.sync_all().ok();
    drop(file); // 释放句柄后才能 rename（Windows 语义）
    if !bindings.is_true(ok) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow!("FPDF_SaveAsCopy 失败（err={}）", fpdf_last_error(bindings)));
    }
    // rename 成功 → 临时文件随操作移走；失败（被占用/只读/磁盘满等） → commit_rename
    // 内清理临时文件并映射成用户可操作的中文文案（防 `.pid.seq.tmp.pdf` 点文件残留）。
    commit_rename(&tmp, target)
}

/// worker 内“载入 raw 句柄 → edit 修改 → **内存**产出新 bytes → 更新 stores 镜像”。
///
/// 待保存模型（P1）的页面操作：只改 worker 内内存工作副本（stores，`FPDF_SaveAsCopy`
/// 到 `Vec`），**绝不碰磁盘**。`before` = 操作前 stores 快照（撤销恢复用），`after` =
/// 操作后新镜像，两者都由 viewer 持有/消费。T3：`before` 先经 [`materialize_stores`]
/// 惰性取（缺镜像即从 live doc SaveAsCopy 物化一次）。
pub(crate) fn writeback_edit_mem(
    stores: &mut HashMap<DocId, Arc<Vec<u8>>>,
    docs: &HashMap<DocId, PdfDocument<'_>>,
    bindings: &dyn PdfiumLibraryBindings,
    doc_id: DocId,
    edit: impl FnOnce(&dyn PdfiumLibraryBindings, FPDF_DOCUMENT) -> Result<usize>,
) -> Result<MemOpResult> {
    let before = materialize_stores(stores, docs, doc_id)?.to_vec();
    unsafe {
        let raw = load_raw_doc(bindings, &before)?;
        let count = match edit(bindings, raw) {
            Ok(n) => n,
            Err(e) => {
                bindings.FPDF_CloseDocument(raw);
                return Err(e);
            }
        };
        let after = match save_raw_doc_to_bytes(bindings, raw) {
            Ok(b) => b,
            Err(e) => {
                bindings.FPDF_CloseDocument(raw);
                return Err(e);
            }
        };
        bindings.FPDF_CloseDocument(raw);
        stores.insert(doc_id, Arc::new(after.clone()));
        Ok(MemOpResult { before, after, count })
    }
}

/// worker 内「重排页序」专用骨架（[`WorkerMsg::MemOpReorder`]）：新建 raw 文档 → 按新序
/// 逐段 `FPDF_ImportPages` → 产出新镜像 → 更新 stores。与 [`writeback_edit_mem`] 的区别：
/// 重排要把「新文档」写回（非原地编辑 `raw`），故走独立函数而非 edit 闭包。
///
/// 严格校验（K122/K124）：`new_order` 必须为 0 基**全排列**（长度 == 当前页数、每值 < n、
/// 0..n 各恰一次）；任何不合法 → 立即 Err，文档不变、无副作用，不提供任何自造超集便利/
/// 静默容错。生效机制：`FPDF_CreateNewDocument` + 按新序「值连续的升序段」分组、逐段
/// `FPDF_ImportPages`（pagerange 只能表达升序连续段；任意排列按段序多次 import 追加成
/// 目标序，例如 0 基 [2,4,1,3] → 1 基段 "3","5","2","4" 逐段 import 成 [2,4,1,3]）——
/// 与 [`WorkerMsg::ExtractPage`] 同 fps 精度/保真口径（pdfium 重新编码，不做字节级保真）。
pub(crate) fn reorder_edit_mem(
    stores: &mut HashMap<DocId, Arc<Vec<u8>>>,
    docs: &HashMap<DocId, PdfDocument<'_>>,
    bindings: &dyn PdfiumLibraryBindings,
    doc_id: DocId,
    new_order: Vec<usize>,
) -> Result<MemOpResult> {
    let before = materialize_stores(stores, docs, doc_id)?.to_vec();
    unsafe {
        let raw = load_raw_doc(bindings, &before)?;
        let count = bindings.FPDF_GetPageCount(raw) as usize;
        let fail = |bindings: &dyn PdfiumLibraryBindings, raw: FPDF_DOCUMENT, e: anyhow::Error| {
            bindings.FPDF_CloseDocument(raw);
            Err(e)
        };
        // ① 长度校验
        if new_order.len() != count {
            return fail(
                bindings,
                raw,
                anyhow!("重排页序长度应为 {count}（当前页数），实际 {}", new_order.len()),
            );
        }
        // ② 每值 < n 且 0..n 各恰一次（全排列校验：重复或缺失都拒绝）
        let mut seen = vec![false; count];
        for &p in new_order.iter() {
            if p >= count {
                return fail(
                    bindings,
                    raw,
                    anyhow!("重排页序含越界页 {p}（共 {count} 页）"),
                );
            }
            if seen[p] {
                return fail(
                    bindings,
                    raw,
                    anyhow!("重排页序含重复页 {p}（须为全排列，0..{count} 各恰一次）"),
                );
            }
            seen[p] = true;
        }
        // ③ 建新文档，把校验后的 `new_order` 当目标序交给 `new_doc_importing_seq`（与拆分/
        // 提取共用同一段「值连续升序段 → 1 基 pagerange → 逐段 append」导入循环，消除重复）。
        //    `FPDF_ImportPages` 的 `index` 是**插入位置**：导入循环内部传「目标文档当前页数」
        //    逐段追加（传 0 = 每次插到开头，会把最后一段 prepend 到最前，等于没重排）。
        let dst = match new_doc_importing_seq(bindings, raw, &new_order) {
            Ok(d) => d,
            // `new_doc_importing_seq` 失败时已自行 `FPDF_CloseDocument(dst)` → 只需关闭 raw。
            Err(e) => return fail(bindings, raw, e),
        };
        let after = match save_raw_doc_to_bytes(bindings, dst) {
            Ok(b) => b,
            Err(e) => {
                bindings.FPDF_CloseDocument(dst);
                return fail(bindings, raw, e);
            }
        };
        bindings.FPDF_CloseDocument(dst);
        bindings.FPDF_CloseDocument(raw);
        // 重排只改变页序，总页数不变。
        let new_count = count;
        stores.insert(doc_id, Arc::new(after.clone()));
        Ok(MemOpResult { before, after, count: new_count })
    }
}

/// 合并源文件的**大小硬上界**（wave7b 输入护栏，K42/K124）：`pub` 供 view 侧
/// `merge_pdf_file` 读端做「元数据预检 + `Read::take` 真边界」的有界读盘（超限在**读端**
/// 拦截，绝不把超限字节送进 pdfium worker）；本文件不再在 worker 侧按路径读盘防 `fs::read`
/// 一次性 OOM。量级对齐同仓图片浏览器的 `DECODE_DISPLAY_MAX_BYTES`（512MB）。
pub const PDF_MERGE_MAX_SRC_BYTES: u64 = 512 * 1024 * 1024;

/// 按「0 基页数组 `seq`（目标输出序）」把 `raw_src` 的页逐「值连续的升序段」import 追加进
/// **新建** raw 文档，成功回传新文档句柄（调用方负责 `FPDF_CloseDocument`；失败分支内部已
/// 关闭）。`seq` 须已校验（逐值 < src 页数、去重、非空）。`FPDF_ImportPages` 的 `index` 是
/// **插入位置**：必须传「目标文档当前页数」才能逐段追加（传 0 = 每次插到开头，会把最后
/// 一段 prepend 到最前）——对齐 [`reorder_edit_mem`] 的重排分段姿势。
pub(crate) fn new_doc_importing_seq(
    bindings: &dyn PdfiumLibraryBindings,
    raw_src: FPDF_DOCUMENT,
    seq: &[usize],
) -> Result<FPDF_DOCUMENT> {
    unsafe {
        let dst = bindings.FPDF_CreateNewDocument();
        if dst.is_null() {
            return Err(anyhow!("FPDF_CreateNewDocument 失败"));
        }
        let mut ok = true;
        let mut i = 0usize;
        let mut dest_count: c_int = 0;
        while i < seq.len() {
            let start = seq[i];
            let mut j = i;
            while j + 1 < seq.len() && seq[j + 1] == seq[j] + 1 {
                j += 1;
            }
            // pagerange 为 1 基页码串：单页段 "a"，多页连续段 "a-b"（升序连续段，符合
            // FPDF_ImportPages 的 pagerange 语法）。
            let start1 = start + 1;
            let end1 = seq[j] + 1;
            let range = if start1 == end1 {
                format!("{start1}")
            } else {
                format!("{start1}-{end1}")
            };
            let r = bindings.FPDF_ImportPages(dst, raw_src, &range, dest_count);
            if !bindings.is_true(r) {
                ok = false;
                break;
            }
            // 段长 = j - i + 1（值连续升序段），已 append 进目标文档。
            dest_count += (j - i + 1) as c_int;
            i = j + 1;
        }
        if !ok {
            bindings.FPDF_CloseDocument(dst);
            return Err(anyhow!("提取导入失败（FPDF_GetLastError={}）", fpdf_last_error(bindings)));
        }
        Ok(dst)
    }
}

/// worker 内「多页提取到新文件」（[`WorkerMsg::ExportPages`]）：严格校验（每页 < 当前页数、
/// 去重、非空；非法 → Err）→ 新建 raw 文档按给定序逐段 import → 落盘 `out_path`。
/// **不改变当前文档 / 不更新 stores**（副本语义）。成功回传写入文件的页数（= 去重后页数）。
pub(crate) fn export_pages_to_file(
    stores: &mut HashMap<DocId, Arc<Vec<u8>>>,
    docs: &HashMap<DocId, PdfDocument<'_>>,
    bindings: &dyn PdfiumLibraryBindings,
    doc_id: DocId,
    pages: &[usize],
    out: &Path,
) -> Result<usize> {
    let bytes = materialize_stores(stores, docs, doc_id)?;
    unsafe {
        let raw = load_raw_doc(bindings, &bytes)?;
        let count = bindings.FPDF_GetPageCount(raw) as usize;
        let fail = |bindings: &dyn PdfiumLibraryBindings, raw: FPDF_DOCUMENT, e: anyhow::Error| {
            bindings.FPDF_CloseDocument(raw);
            Err(e)
        };
        // 严格校验（K122/K124）：逐页 < 当前页数、去重（保留首次出现序）、非空。
        let mut seq: Vec<usize> = Vec::with_capacity(pages.len());
        for &p in pages.iter() {
            if p >= count {
                return fail(
                    bindings,
                    raw,
                    anyhow!("提取页码越界：第 {p} 页（共 {count} 页）"),
                );
            }
            if !seq.contains(&p) {
                seq.push(p);
            }
        }
        if seq.is_empty() {
            return fail(bindings, raw, anyhow!("提取页清单为空（须至少 1 页）"));
        }
        let dst = match new_doc_importing_seq(bindings, raw, &seq) {
            Ok(d) => d,
            Err(e) => return fail(bindings, raw, e),
        };
        let res = save_raw_doc_to_file(bindings, dst, out);
        bindings.FPDF_CloseDocument(dst);
        bindings.FPDF_CloseDocument(raw);
        res.map(|()| seq.len())
    }
}

/// 页面操作的结果载荷（[`WorkerMsg::MemOpInsert`] / [`WorkerMsg::MemOpDelete`]）：含操作前
/// 内存镜像（撤销 / 测试 MD5 用）、操作后新镜像（viewer 内存级重载）与新页数。
#[derive(Debug, Clone)]
pub struct MemOpResult {
    /// 操作前的内存工作副本（撤销恢复用）。
    pub before: Vec<u8>,
    /// 操作后的新内存工作副本（viewer 重载显示用）。
    pub after: Vec<u8>,
    /// 操作后的页数。
    pub count: usize,
}

/// worker 内“载入 raw 句柄 → edit 修改 → 落盘 → 收尾”的统一骨架。
///
/// - `edit` 闭包在 raw 句柄上做修改并返回「结果数」（注释条数 / 新页数）；
/// - 成功路径：`FPDF_SaveAsCopy` 落到 `output_path`（原子替换），然后**更新内存 bytes**
///   镜像（后续操作基于新内容）；
/// - 失败路径：关闭 raw 句柄后原样上抛，目标文件不受影响。
///
/// T3：源镜像先经 [`materialize_stores`] 惰性取（缺镜像即从 live doc SaveAsCopy 物化一次）。
pub(crate) fn writeback_edit(
    stores: &mut HashMap<DocId, Arc<Vec<u8>>>,
    docs: &HashMap<DocId, PdfDocument<'_>>,
    bindings: &dyn PdfiumLibraryBindings,
    doc_id: DocId,
    output_path: &Path,
    edit: impl FnOnce(&dyn PdfiumLibraryBindings, FPDF_DOCUMENT) -> Result<usize>,
) -> Result<usize> {
    let bytes = materialize_stores(stores, docs, doc_id)?;
    unsafe {
        let raw = load_raw_doc(bindings, &bytes)?;
        let result_out = match edit(bindings, raw) {
            Ok(n) => {
                if let Err(e) = save_raw_doc_to_file(bindings, raw, output_path) {
                    bindings.FPDF_CloseDocument(raw);
                    return Err(e);
                }
                // 读回已写入的新文件 bytes 作内存镜像（后续写回/重载基于新内容）
                if let Ok(new_bytes) = std::fs::read(output_path) {
                    stores.insert(doc_id, Arc::new(new_bytes));
                }
                n
            }
            Err(e) => {
                bindings.FPDF_CloseDocument(raw);
                return Err(e);
            }
        };
        bindings.FPDF_CloseDocument(raw);
        Ok(result_out)
    }
}

/// 在 raw 句柄文档上逐条创建标准注释（Highlight / StrikeOut / Underline）并回填几何。
///
/// 坐标约定：`QuadsPt` 为 PDF 用户空间坐标（y 向上、原点页左下）。PDF 的 QuadPoints
/// 顶点序按 pdfium-render 官方姿势（自底左起逆时针：BL→BR→TR→TL，见其
/// create_squiggly_annotation_under_object）；注释外接矩形 = 全部 quad 的包围盒。
pub(crate) fn apply_annotations_raw(
    bindings: &dyn PdfiumLibraryBindings,
    doc_handle: FPDF_DOCUMENT,
    items: &[AnnotateItem],
) -> Result<usize> {
    for item in items {
        let page = unsafe { bindings.FPDF_LoadPage(doc_handle, item.page_index as c_int) };
        if page.is_null() {
            return Err(anyhow!("写回注释：无法加载第 {} 页", item.page_index));
        }
        let annot = unsafe { bindings.FPDFPage_CreateAnnot(page, item.kind.fpdf_subtype()) };
        if annot.is_null() {
            unsafe { bindings.FPDF_ClosePage(page) };
            return Err(anyhow!(
                "写回注释：FPDFPage_CreateAnnot 失败（第 {} 页）",
                item.page_index
            ));
        }
        // 默认色（Phase B 由 UI 定制）；注释级透明度 /CA：高亮半透明、删除线/下划线不透明
        let (r, g, b) = item.kind.default_color();
        let ca_alpha = if item.kind == AnnotKind::Highlight {
            APOBJ_HIGHLIGHT_A
        } else {
            255
        };
        unsafe {
            bindings.FPDFAnnot_SetColor(annot, FPDFANNOT_COLORTYPE_COLOR, r, g, b, ca_alpha);
        }
        // 外接矩形（FS_RECTF 字段为 left/top/right/bottom，PDF 用户空间 y 向上）
        let mut left = f32::INFINITY;
        let mut right = f32::NEG_INFINITY;
        let mut bottom = f32::INFINITY;
        let mut top = f32::NEG_INFINITY;
        for q in &item.quads {
            left = left.min(q.x0.min(q.x1));
            right = right.max(q.x0.max(q.x1));
            bottom = bottom.min(q.y0.min(q.y1));
            top = top.max(q.y0.max(q.y1));
        }
        if !(left.is_finite() && right.is_finite() && bottom.is_finite() && top.is_finite()) {
            unsafe { bindings.FPDFPage_CloseAnnot(annot) };
            unsafe { bindings.FPDF_ClosePage(page) };
            return Err(anyhow!("写回注释：quad 坐标非法（第 {} 页）", item.page_index));
        }
        let rect = FS_RECTF {
            left,
            top,
            right,
            bottom,
        };
        unsafe {
            bindings.FPDFAnnot_SetRect(annot, &rect);
            for q in &item.quads {
                let qp = FS_QUADPOINTSF {
                    x1: q.x0, y1: q.y0, // 左下
                    x2: q.x1, y2: q.y0, // 右下
                    x3: q.x1, y3: q.y1, // 右上
                    x4: q.x0, y4: q.y1, // 左上
                };
                bindings.FPDFAnnot_AppendAttachmentPoints(annot, &qp);
            }
            // 默认可见 + 随打印输出（PDF 标准 AnnotationFlag 的 Print 位）
            bindings.FPDFAnnot_SetFlags(annot, FPDF_ANNOT_FLAG_PRINT);
        }
        // 生成外观流（关键，诊断实证）：只设 Rect/QuadPoints/Color 的话，存盘后注释字典里
        // **没有 /AP**（文件里有 /Annots+/Subtype+/QuadPoints 但无 /AP），重新打开渲染时
        // pdfium 对无外观流的注释整页像素零差异；FPDFAnnot_AppendObject 仅支持 ink/stamp
        // 子类型（Highlight/Underline/StrikeOut 都会失败）→ 只能用 `FPDFAnnot_SetAP` 手写
        // 外观内容。实测本机 pdfium(.8035)：SetAP 生成的 /AP/N Form XObject 的 **BBox =
        // 注释矩形（页用户空间坐标）**、无 /Matrix，即内容按**页坐标**绘制（不是 BBox 的
        // 0..W×0..H 局部坐标）——若按局部坐标写会被裁剪在注释矩形外、整页像素零差异。
        let (fr, fg, fb) = item.kind.default_color();
        // 高亮半透明：pdfium 的 SetAP 已自动在 AP 的 /Resources 里生成
        // `ExtGState/GS (CA=.431)`（因为 SetColor 传了 alpha<255），内容里须显式 `/GS gs`
        // 激活它才会半透明（否则 ExtGState 定义但未被引用 = 纯色填充覆盖正文）；不透明类型无 gs。
        let mut content = String::from("q\n");
        if item.kind == AnnotKind::Highlight {
            content.push_str("/GS gs\n");
        }
        content.push_str(&format!(
            "{} {} {} rg\n",
            fr as f32 / 255.0,
            fg as f32 / 255.0,
            fb as f32 / 255.0
        ));
        for q in &item.quads {
            let (qx0, qy0) = (q.x0.min(q.x1), q.y0.min(q.y1));
            let (qx1, qy1) = (q.x0.max(q.x1), q.y0.max(q.y1));
            let (qw, qh) = ((qx1 - qx0).max(1.0), (qy1 - qy0).max(1.0));
            let (ly, lh) = match item.kind {
                // 高亮整块填充；删除线居中细条；下划线贴底——全部用页坐标
                AnnotKind::Highlight => (qy0, qh),
                AnnotKind::Strikeout => {
                    let c = (qy0 + qy1) * 0.5;
                    (c - APOBJ_LINE_H * 0.5, APOBJ_LINE_H)
                }
                AnnotKind::Underline => (qy0 - APOBJ_LINE_H * 0.25, APOBJ_LINE_H),
            };
            content.push_str(&format!("{} {} {} {} re f\n", qx0, ly, qw, lh));
        }
        content.push('Q');
        // FPDF_WIDESTRING（FPDFAnnot_SetAP 只收指针无长度）按 pdfium C 约定必须 C 风格
        // **u16 零结尾**（对照 pdfium-render utils::utf16le 注释「expects C-style null
        // termination」）——补尾零防 pdfium 端 wcslen 越过缓冲区读到随机零（隐性越界）。
        let mut wide: Vec<u16> = content.encode_utf16().collect();
        wide.push(0);
        let ok = unsafe {
            bindings.FPDFAnnot_SetAP(
                annot,
                0, // FPDF_ANNOT_APPEARANCEMODE_NORMAL
                wide.as_ptr(),
            )
        };
        if !bindings.is_true(ok) {
            unsafe {
                bindings.FPDFPage_CloseAnnot(annot);
                bindings.FPDF_ClosePage(page);
            }
            return Err(anyhow!("写回注释：FPDFAnnot_SetAP 失败（第 {} 页）", item.page_index));
        }
        unsafe {
            bindings.FPDFPage_CloseAnnot(annot);
            bindings.FPDF_ClosePage(page);
        }
    }
    Ok(items.len())
}

/// FPDF_ANNOT_FLAG_PRINT：注释随打印输出（去掉了默认的 INVISIBLE/HIDDEN 显隐影响，
/// 默认 0 = 可见 + 可打印位置 1）。
const FPDF_ANNOT_FLAG_PRINT: c_int = 4;
/// FPDFANNOT_COLORTYPE_Color：注释描边/填充色（0）。
const FPDFANNOT_COLORTYPE_COLOR: u32 = 0;

/// 写回落盘临时文件序号（同目录并发写回防撞名；worker 单线程串行本不应并发，兜底）。
///
/// 跨可见性（模块重组）：`parser/tests.rs` 引用（测验原子写盘序号）→ 升 `pub(crate)`。
pub(crate) static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 删除线/下划线细条的视觉厚度（PDF 点，~1px@72dpi；与待存叠加层 `render` 的 ~1.5pt 对齐）。
const APOBJ_LINE_H: f32 = 1.5;
/// 高亮注释注释级透明度（/CA，0-255）：≈0.43 半透明，正文不被遮挡。
const APOBJ_HIGHLIGHT_A: u32 = 110;

/// 读回某页注释（only-read，走 pdfium-render 高层 `page.annotations()`；供测试断言/Phase B）。
/// 只摘 Highlight/Underline/Strikeout 的几何：每个附件点多边形折算成 quad 包围盒（PDF 点）。
pub(crate) fn read_annotations(doc: &PdfDocument<'_>, page_index: usize) -> Option<Vec<AnnotationInfo>> {
    use PdfPageAnnotationType as T;
    let page = doc.pages().get(page_index as i32).ok()?;
    let mut out = Vec::new();
    for a in page.annotations().iter() {
        let kind = match a.annotation_type() {
            T::Highlight => AnnotKind::Highlight,
            T::Underline => AnnotKind::Underline,
            T::Strikeout => AnnotKind::Strikeout,
            _ => continue,
        };
        let mut quads = Vec::new();
        for qp in a.attachment_points().iter() {
            let (l, r) = (qp.left().value.min(qp.right().value), qp.left().value.max(qp.right().value));
            let (b, t) = (qp.bottom().value.min(qp.top().value), qp.bottom().value.max(qp.top().value));
            quads.push(QuadPt {
                x0: l,
                y0: b,
                x1: r,
                y1: t,
            });
        }
        if quads.is_empty() {
            continue;
        }
        out.push(AnnotationInfo { kind, quads });
    }
    Some(out)
}

/// 从文档书签树提取自包含的目录条目（标题 + 层级 + 目标页码）。
///
/// 深度优先先序展开（pdfium `bookmarks().iter()` 即 DF 先序）。每个书签的层级 =
/// 其父链长度（`parent()` 链递推）；目标页码取书签指向的 destination，
/// 解析失败（如指向外部/无页码）记为 `usize::MAX`。
///
/// 跨簇可见性（模块重组）：根 `run` 的 Open 分支调用 → 升 `pub(crate)`。
pub(crate) fn extract_outline(doc: &PdfDocument<'_>) -> Vec<PdfOutlineEntry> {
    let mut out = Vec::new();
    for bookmark in doc.bookmarks().iter() {
        let title = bookmark.title().unwrap_or_default();
        if title.trim().is_empty() {
            continue;
        }
        let page_index = bookmark
            .destination()
            .and_then(|d| d.page_index().ok().map(|i| i as usize))
            .unwrap_or(usize::MAX);
        // 层级 = 父链长度
        let mut level = 0usize;
        let mut parent = bookmark.parent();
        while let Some(p) = parent {
            level += 1;
            parent = p.parent();
        }
        out.push(PdfOutlineEntry {
            title,
            level,
            page_index,
        });
    }
    out
}
