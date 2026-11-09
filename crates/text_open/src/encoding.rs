//! 文本编码检测与「按原编码读写」：打开非 UTF-8（GBK/GB18030/UTF-16 BOM 等）文本、
//! 保存按源编码转回写盘。打开解码顺序（用户确认）：
//! 1. **BOM 优先**：UTF-8 BOM（EF BB BF）→ UTF-8 去 BOM；UTF-16 LE/BE BOM（FF FE / FE FF）
//!    → 对应 UTF-16 去 BOM；
//! 2. **合法 UTF-8**（`std::str::from_utf8` 前缀全通过）→ UTF-8 原样（现状路径不变）；
//! 3. **否则 → GB18030**（`encoding_rs::GB18030` 覆盖 GBK/GB2312 旧中文文本；保存按
//!    GB18030 label 回写）。Big5/Shift_JIS 不特判（首版不误匹配）。
//!
//! 解码遇到 malformed（非 UTF-8 拒收，含二进制/截断多字节）→ `InvalidData`，
//! 保「打开失败」既有语义（GB18030 对非法字节序列确实拒收：探针已验证 0xFF / 截断
//! UTF-8 均 `decode_without_replacement` 返回 None）。
//!
//! `source_encoding: Option<&'static Encoding>`（`None`=纯 UTF-8 新建/打开）随选项卡
//! 保存：`Some(UTF_8)` 表示带 BOM 的 UTF-8、`Some(UTF_16LE/BE)` 表示带对应 BOM 的
//! UTF-16、`Some(GB18030)` 表示 GB18030 文本——保存均按编码（+ BOM）回写。

use encoding_rs::{Encoding, GB18030, UTF_16BE, UTF_16LE, UTF_8};
use std::io;

/// 从字节前缀探测文本编码。返回 `(编码, BOM 字节数)`；`None` = 纯 UTF-8（无 BOM）。
/// 空输入 → `(None, 0)`。
pub fn detect_source_encoding(bytes: &[u8]) -> (Option<&'static Encoding>, usize) {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        (Some(UTF_8), 3)
    } else if bytes.starts_with(&[0xFF, 0xFE]) {
        (Some(UTF_16LE), 2)
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        (Some(UTF_16BE), 2)
    } else if std::str::from_utf8(bytes).is_ok() {
        (None, 0) // 纯 UTF-8（现状路径）
    } else {
        (Some(GB18030), 0) // 非 UTF-8：GB18030 兜底（覆盖 GBK/GB2312）
    }
}

/// 整篇解码（小文件同步打开路径）：把字节按探测编码转回 UTF-8 **String**（BOM 已去），
/// 返回 `(解码文本, 源编码)`。非 UTF-8 遇 malformed → `InvalidData`（不静默替换）。
pub fn decode_bytes(
    bytes: &[u8],
) -> io::Result<(String, Option<&'static Encoding>)> {
    let (enc, bom) = detect_source_encoding(bytes);
    let raw = &bytes[bom..];
    match enc {
        None => {
            let s = std::str::from_utf8(raw).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, e.to_string())
            })?;
            Ok((s.to_string(), None))
        }
        Some(e) => {
            // 无 BOM 将按 GB18030 处理时先排除「截断的 UTF-8」：`error_len==None` 是
            // 文件在 UTF-8 多字节序列中截断（坏 UTF-8 而非 GB18030），保「截断=失败」
            // 既有语义（不落 GB18030 误判成乱码）。
            if e == GB18030
                && bom == 0
                && is_truncated_utf8_tail(raw)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "stream ended in the middle of a UTF-8 sequence",
                ));
            }
            match e.decode_without_bom_handling_and_without_replacement(raw) {
                Some(s) => Ok((s.into_owned(), Some(e))),
                None => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "stream did not match detected encoding",
                )),
            }
        },
    }
}

/// 是否「合法 UTF-8 前缀 + 多字节序列中途截断」（`from_utf8` 报 `error_len()==None`）。
/// 流式首块探针：块边界劈开多字节是常态（跨块切分，非错误）；整篇同步读取 & EOF：
/// 则意味着文件在 UTF-8 序列中截断，应按 `read_to_string` 既有「截断=失败」语义报错。
pub fn is_truncated_utf8_tail(bytes: &[u8]) -> bool {
    matches!(std::str::from_utf8(bytes), Err(e) if e.error_len().is_none())
}

/// 按源编码把 UTF-8 缓冲文本转回字节写盘：`None` = 纯 UTF-8 原样（现状）；
/// `Some(UTF_8)`/`Some(UTF_16LE)`/`Some(UTF_16BE)` = 补相应 BOM 再编码；`Some(GB18030)`
/// = GB18030 编码（无 BOM）。GB18030/UTF-16 编码器对 Unicode 全集可表达（含 4 字节
/// GB18030 / 代理对），`had_errors` 仅防御：若出现（理论不该有）以库默认替换写回。
///
/// **encoding_rs 陷阱**：`Encoding::encode()` 对 UTF-16LE/BE **降级输出 UTF-8**（WHATWG
/// 未定义其传统 encoder），UTF-16 保存必须手写码元 + BOM 还原（探针实证：`UTF_16LE.encode`
/// 给出 UTF-8 字节）。解码侧的 `decode_without_bom_handling*` 无此问题。
pub fn encode_for_save(text: &str, enc: Option<&'static Encoding>) -> Vec<u8> {
    match enc {
        None => text.as_bytes().to_vec(),
        // UTF-16：自带 BOM + UTF-16 码元（LE/BE 按平台字节序写码元）。
        Some(e) if e == UTF_16LE || e == UTF_16BE => {
            let le = e == UTF_16LE;
            let mut out = Vec::with_capacity(text.len() * 2 + 2);
            out.extend_from_slice(if le { &[0xFF, 0xFE] } else { &[0xFE, 0xFF] });
            for u in text.encode_utf16() {
                let b = if le { u.to_le_bytes() } else { u.to_be_bytes() };
                out.extend_from_slice(&b);
            }
            out
        }
        Some(e) => {
            let (body, _, _) = e.encode(text);
            let mut out = Vec::with_capacity(body.len() + 3);
            if e == UTF_8 {
                out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
            }
            out.extend_from_slice(&body);
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 从指定编码构造测试字节（内存构造，无磁盘 fixture）。注意：`Encoding::encode()`
    /// 对 UTF-16 会降级输出 UTF-8，故 UTF-16 统一走 [`utf16_bytes`] 手写码元。
    fn bytes_for(enc: &'static Encoding, s: &str) -> Vec<u8> {
        enc.encode(s).0.into_owned()
    }

    /// 构造真实 UTF-16（LE/BE）**无 BOM** 字节（避开 encoding_rs `encode()` 的 UTF-8 陷阱）。
    fn utf16_bytes(enc: &'static Encoding, s: &str) -> Vec<u8> {
        let le = enc == UTF_16LE;
        let mut v = Vec::with_capacity(s.len() * 2);
        for u in s.encode_utf16() {
            let b = if le { u.to_le_bytes() } else { u.to_be_bytes() };
            v.extend_from_slice(&b);
        }
        v
    }

    #[test]
    fn detect_bom_and_utf8_and_gb18030() {
        assert_eq!(detect_source_encoding(b"\xEF\xBB\xBFabc"), (Some(UTF_8), 3));
        assert_eq!(detect_source_encoding(b"\xFF\xFEa\x00"), (Some(UTF_16LE), 2));
        assert_eq!(detect_source_encoding(b"\xFE\xFF\x00a"), (Some(UTF_16BE), 2));
        assert_eq!(detect_source_encoding(b"plain ascii"), (None, 0));
        assert_eq!(detect_source_encoding(&[]), (None, 0));
        // 中文 GBK 双字节（自编）：不是合法 UTF-8 → GB18030。
        let gbk = bytes_for(GB18030, "从前有座寻仙山");
        assert_eq!(detect_source_encoding(&gbk), (Some(GB18030), 0));
        assert!(std::str::from_utf8(&gbk).is_err(), "GBK 字节应非合法 UTF-8");
    }

    #[test]
    fn decode_roundtrip_gbk_and_utf16() {
        let text = "从前有座寻仙山，山上有座庙……\n第二行 中文测试\n";
        let gbk = bytes_for(GB18030, text);
        let (decoded, enc) = decode_bytes(&gbk).unwrap();
        assert_eq!(decoded, text, "GB18030 解码应还原 UTF-8 文本");
        assert_eq!(enc, Some(GB18030));

        // UTF-16 必须用真码元（`Encoding::encode()` 给 UTF-8，见 utf16_bytes 注释）。
        let mut u16le = vec![0xFF, 0xFE];
        u16le.extend(utf16_bytes(UTF_16LE, text));
        assert_eq!(u16le[2..4], [0xCEu8, 0x4E], "「从」(U+4ECE) LE 真码元应是 CE 4E");
        let (d2, e2) = decode_bytes(&u16le).unwrap();
        assert_eq!(d2, text, "UTF-16 LE 应去 BOM 并正确解码");
        assert_eq!(e2, Some(UTF_16LE));

        let mut u16be = vec![0xFE, 0xFF];
        u16be.extend(utf16_bytes(UTF_16BE, text));
        let (d3, e3) = decode_bytes(&u16be).unwrap();
        assert_eq!(d3, text, "UTF-16 BE 应去 BOM 并正确解码");
        assert_eq!(e3, Some(UTF_16BE));

        // UTF-8 BOM：去 BOM。
        let mut u8bom = vec![0xEF, 0xBB, 0xBF];
        u8bom.extend_from_slice(text.as_bytes());
        let (d4, e4) = decode_bytes(&u8bom).unwrap();
        assert_eq!(d4, text, "UTF-8 BOM 应去 BOM");
        assert_eq!(e4, Some(UTF_8));
    }

    #[test]
    fn encode_for_save_roundtrips_original_bytes() {
        let text = "从前有座寻仙山……世界你好";
        let gbk = bytes_for(GB18030, text);
        let (decoded, enc) = decode_bytes(&gbk).unwrap();
        assert_eq!(encode_for_save(&decoded, enc), gbk, "GB18030 保存应还原原字节");
        // UTF-16 LE 带回 BOM（真码元）。
        let mut u16le = vec![0xFF, 0xFE];
        u16le.extend(utf16_bytes(UTF_16LE, text));
        let (d, e) = decode_bytes(&u16le).unwrap();
        assert_eq!(encode_for_save(&d, e), u16le, "UTF-16LE 保存应还原含 BOM 原字节");
        // UTF-8 带 BOM。
        let mut u8bom = vec![0xEF, 0xBB, 0xBF];
        u8bom.extend_from_slice(text.as_bytes());
        let (d2, e2) = decode_bytes(&u8bom).unwrap();
        assert_eq!(encode_for_save(&d2, e2), u8bom, "UTF-8 BOM 保存应还原含 BOM 原字节");
        // 纯 UTF-8：None → 原样。
        assert_eq!(encode_for_save("abc", None), b"abc");
    }

    #[test]
    fn probe_encoding_rs_traps_and_surrogate_pairs() {
        // A: encoding_rs `Encoding::encode` 对 UTF-16 降级输出 UTF-8（WHATWG 未定义其传统
        // encoder）——直接钉死该陷阱：UTF_16LE.encode("从") 返回 UTF-8 字节而非码元。
        let utf8_of_cong = UTF_16LE.encode("从").0.into_owned();
        assert_eq!(
            utf8_of_cong, b"\xE4\xBB\x8E",
            "encoding_rs UTF-16 encode 会把码元降级成 UTF-8 字节（源码注释所述陷阱）"
        );
        // B: `\xe4\xb8`（UTF-8「中」的前 2 字节）是**合法 GB18030 双字节**（实测解码成
        // 「涓」字面）——无 `is_truncated_utf8_tail` 守卫，坏 UTF-8（截断尾）会被 GB18030
        // 解码成乱码而非报错。注意 `decode_to_string_without_replacement` 不扩容（当前
        // 容量即输出上限），必须给足容量再判，否则空 String 会因容量 0 误报 OutputFull。
        for last in [false, true] {
            let mut dbg = String::with_capacity(64);
            let (r2, _) = GB18030
                .new_decoder_without_bom_handling()
                .decode_to_string_without_replacement(b"\xe4\xb8", &mut dbg, last);
            assert!(
                matches!(r2, encoding_rs::DecoderResult::InputEmpty),
                "\\xe4\\xb8 应被 GB18030 吸收为合法字面（守卫必要，last={last}）"
            );
            assert!(!dbg.is_empty(), "GB18030 对该双字节应产出字面字符");
        }
        // C: 手写 encode_utf16 对代理对（😀 U+1F600）的输出与解码往返一致。
        let text = "A😀B";
        for enc in [UTF_16LE, UTF_16BE] {
            let le = enc == UTF_16LE;
            let mut bytes = vec![
                if le { 0xFF } else { 0xFE },
                if le { 0xFE } else { 0xFF },
            ];
            for u in text.encode_utf16() {
                let b = if le { u.to_le_bytes() } else { u.to_be_bytes() };
                bytes.extend_from_slice(&b);
            }
            let (decoded, e) = decode_bytes(&bytes).unwrap();
            assert_eq!(decoded, text, "UTF-16 代理对解码应还原原文");
            assert_eq!(e, Some(enc));
            assert_eq!(
                encode_for_save(&decoded, e),
                bytes,
                "代理对保存回写应与原字节一致"
            );
        }
    }

    #[test]
    fn decode_rejects_binary_and_truncated() {
        // 二进制 0xFF / 截断 UTF-8 / GBK 非法对 → 拒收（InvalidData），不静默替换。
        for bad in [
            &vec![0xFFu8; 4][..],
            b"abc\xf0\x9f\x98".as_slice(), // 截断的 4 字节 UTF-8
            &[0x81u8, 0x20, b'a'][..],
        ] {
            assert!(
                decode_bytes(bad).is_err(),
                "非 UTF-8 且 GB18030 拒收的字节应报错，实际={bad:02X?}"
            );
        }
        // 截断尾防线：`\xe4\xb8`（UTF-8「中」的前 2 字节）恰是合法 GB18030 双字节——
        // 若不拦，坏 UTF-8 会被误判成 GB18030 乱码打开（回归：read_to_string「截断=失败」）。
        let mut truncated = "汉字".as_bytes().to_vec();
        truncated.extend_from_slice(&[0xE4u8, 0xB8]);
        assert!(
            is_truncated_utf8_tail(&truncated),
            "合法前缀 + 截断多字节尾应判为截断 UTF-8"
        );
        assert!(
            decode_bytes(&truncated).is_err(),
            "整篇同步读取时截断 UTF-8 应报 InvalidData（不落 GB18030 乱码）"
        );
        assert!(
            !is_truncated_utf8_tail(b"plain\xff"),
            "确定性非法字节（0xFF）不是跨块截断尾"
        );
    }
}
