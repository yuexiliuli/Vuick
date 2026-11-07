//! oracle 测试骨架：读 `target/heif/oracle/manifest.csv`（`md5,file,w,h,bitdepth,tiles`），
//! 每文件读 `<md5>.info.json`，用 `heif_probe` 解析 `Benchmarks/corpus/heif/<file>` 并逐字段断言
//! （w,h,bd,spsW,spsH,bitY,bitC,chroma,profile,ctbLog2,grid,cols,rows,tiles,conf）。
//! 目录/文件缺失 → 打印 `SKIP: missing …` 后 return（SKIP 是常态不是失败）。
//! 运行：`cargo test -p kimg_heif --test oracle -- --nocapture --test-threads=1`

use std::path::PathBuf;

use kimg_heif::codecs::heif::probe::heif_probe;

const ORACLE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/heif/oracle");
const CORPUS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Benchmarks/corpus/heif");

#[allow(non_snake_case)]
#[derive(Debug, serde::Deserialize)]
struct InfoJson {
    w: i64,
    h: i64,
    #[serde(rename = "bd")]
    bd: i64,
    spsW: i64,
    spsH: i64,
    bitY: i64,
    bitC: i64,
    chroma: i64,
    profile: i64,
    ctbLog2: i64,
    grid: String,
    cols: i64,
    rows: i64,
    tiles: i64,
    conf: Vec<i64>,
    #[serde(default)]
    nclx: bool,
    #[serde(default)]
    icc: bool,
    #[serde(default)]
    transforms: i64,
}

#[test]
fn oracle_matches_info_json() {
    let manifest_path = PathBuf::from(ORACLE_DIR).join("manifest.csv");
    let manifest = match std::fs::read_to_string(&manifest_path) {
        Ok(m) => m,
        Err(e) => {
            println!("SKIP: missing {manifest_path:?} ({e})");
            return;
        }
    };

    let mut checked = 0usize;
    let mut skipped = 0usize;
    for (line_no, line) in manifest.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("md5,") {
            continue;
        }
        let cols: Vec<&str> = line.split(',').collect();
        if cols.len() < 6 {
            println!("note: manifest 第 {} 行字段不足，跳过", line_no + 1);
            continue;
        }
        let md5 = cols[0];
        let file = cols[1];
        // ERR 行（w="ERR"）无 info.json；C# 侧拒绝 profile-4，本测试不覆盖。
        if cols[2] == "ERR" {
            println!("note: {} 是拒绝条目（{file}），跳过", line_no + 1);
            continue;
        }
        let info_path = PathBuf::from(ORACLE_DIR).join(format!("{md5}.info.json"));
        let info: InfoJson = match std::fs::read(&info_path)
            .map_err(|e| e)
            .and_then(|b| serde_json::from_slice(&b).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
        {
            Ok(v) => v,
            Err(e) => {
                println!("SKIP: missing/bad {info_path:?} ({e})");
                skipped += 1;
                continue;
            }
        };

        let corpus_file = PathBuf::from(CORPUS_DIR).join(file);
        let data = match std::fs::read(&corpus_file) {
            Ok(d) => d,
            Err(e) => {
                println!("SKIP: missing corpus {corpus_file:?} ({e})");
                skipped += 1;
                continue;
            }
        };

        let p = match heif_probe(&data) {
            Ok(p) => p,
            Err(e) => {
                panic!("{file}: heif_probe 失败：{e}");
            }
        };
        assert_eq!(i64::from(p.width), info.w, "{file}: width");
        assert_eq!(i64::from(p.height), info.h, "{file}: height");
        assert_eq!(i64::from(p.bitdepth), info.bd, "{file}: bitdepth");
        assert_eq!(i64::from(p.sps_w), info.spsW, "{file}: spsW");
        assert_eq!(i64::from(p.sps_h), info.spsH, "{file}: spsH");
        assert_eq!(i64::from(p.bit_y), info.bitY, "{file}: bitY");
        assert_eq!(i64::from(p.bit_c), info.bitC, "{file}: bitC");
        assert_eq!(i64::from(p.chroma), info.chroma, "{file}: chroma");
        assert_eq!(i64::from(p.profile), info.profile, "{file}: profile");
        assert_eq!(i64::from(p.ctb_log2), info.ctbLog2, "{file}: ctbLog2");
        assert_eq!(p.grid, info.grid, "{file}: grid");
        assert_eq!(i64::from(p.cols), info.cols, "{file}: cols");
        assert_eq!(i64::from(p.rows), info.rows, "{file}: rows");
        assert_eq!(p.tiles as i64, info.tiles, "{file}: tiles");
        assert_eq!(p.conf.len(), info.conf.len(), "{file}: conf 长度");
        for (a, b) in p.conf.iter().zip(info.conf.iter()) {
            assert_eq!(i64::from(*a), *b, "{file}: conf");
        }
        // nclx/icc/transforms 在 ProbeInfo 里同样可测，一并断言（字段与 C# 输出对齐）。
        assert_eq!(p.nclx, info.nclx, "{file}: nclx");
        assert_eq!(p.icc, info.icc, "{file}: icc");
        assert_eq!(p.transforms as i64, info.transforms, "{file}: transforms");
        checked += 1;
        println!("PASS {file}: {}x{} bd={} tiles={} grid={}", p.width, p.height, p.bitdepth, p.tiles, p.grid);
    }
    println!("oracle: checked={checked} skipped={skipped} (manifest 共 {} 数据行)", manifest.lines().count().saturating_sub(1));
}
