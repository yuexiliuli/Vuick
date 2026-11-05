//! 帧解码器（H.265 §7.3.8）：解码一张帧内编码的 HEVC 图画（其全部切片段）为 [`HevcPicture`]：
//! 切片数据解析（§7.3.8）、帧内重建（§8.4、§8.6）。逐函数移植自 C#
//! `Hevc\HevcFrameDecoder.cs`，与 C# 字节恒等。
//!
//! 每张图画的辅助信息按 4×4 亮度粒度存放（下标 `(y >> 2) * w4 + (x >> 2)`）：编码树深度、
//! 亮度帧内模式、QpY、transquant-bypass CU 的 noLoopFilter 标志以及去块边强度。tile 与 PCM
//! 不支持，构造器直接拒绝。
//!
//! **Phase-3 已接**：`filter_picture` 经 `loop_filter` 模块跑 C# `HevcLoopFilter.cs` 的
//! `Deblock()`/`ApplySao()`（§8.7）。本阶段输出 = 带 in-loop 滤波的帧内重建，
//! 与 oracle `_t0.yuv` 逐字节一致；`set_skip_filters` 消融 = `_t0_nofilter.yuv`。
//!
//! 字节恒等纪律：`<<`/`>>` 越位宽按 C# 的 mod-位数语义用 `wrapping_shl/shr` 或显式分支保恒等；
//! `unchecked`/`%` 算术按 C# 语义对齐（C# `%` 对非负操作数与 Rust `%` 一致，参数恒非负处直接用 `%`）；
//! 所有下标先守卫（`get_mut`/`min`/条件短路），不可信输入零 panic；C# `throw` → `Err(HeifError::invalid(中文))`。

use std::sync::LazyLock;

use crate::codecs::heif::error::{HeifError, HeifResult};
use crate::codecs::heif::hevc::cabac::{HevcCabac, HevcCabacTables};
use crate::codecs::heif::hevc::intra_pred::IntraPred;
use crate::codecs::heif::hevc::parameter_sets::{HevcPps, HevcScan, HevcSps};
use crate::codecs::heif::hevc::picture::HevcPicture;
use crate::codecs::heif::hevc::slice_header::HevcSliceHeader;
use crate::codecs::heif::hevc::transform::{HevcTransform, Scaler};

/// §6.5.3–6.5.5 的扫描序（log2Size 0..3 × scanIdx 0..2），懒构建一次，解码所有 TU 共用。
static SCANS: LazyLock<Vec<Vec<Vec<u8>>>> = LazyLock::new(HevcScan::order);

const MAX_TB_SIZE: usize = 32;

/// Table 8-10（ChromaArrayType 1）的 ctxIdxMap（9.3.4.2.3）：4×4 块的 sig_coeff_flag 上下文。
const CTX_IDX_MAP: [u8; 16] = [0, 1, 4, 5, 2, 3, 4, 5, 6, 6, 8, 8, 7, 7, 8, 8];

/// 每 CTB 的 SAO 参数（C# 同名字步类型，供 Phase-3 SAO 应用使用）。
#[derive(Debug, Clone, Copy, Default)]
pub struct SaoParams {
    pub type_y: u8,
    pub type_cb: u8,
    pub type_cr: u8,
    pub band_y: u8,
    pub band_cb: u8,
    pub band_cr: u8,
    pub class_y: u8,
    pub class_cb: u8,
    pub class_cr: u8,
    /// SaoOffsetVal[c][1..4]。
    pub o: [i8; 12],
}

impl SaoParams {
    /// C# `SaoParams.Set(c, type, band, eoClass, offsets)`。
    pub fn set(&mut self, c: usize, ty: i32, band: i32, eo_class: i32, offsets: &[i32; 4]) {
        match c {
            0 => {
                self.type_y = ty as u8;
                self.band_y = band as u8;
                self.class_y = eo_class as u8;
                self.o[0] = offsets[0] as i8;
                self.o[1] = offsets[1] as i8;
                self.o[2] = offsets[2] as i8;
                self.o[3] = offsets[3] as i8;
            }
            1 => {
                self.type_cb = ty as u8;
                self.band_cb = band as u8;
                self.class_cb = eo_class as u8;
                self.o[4] = offsets[0] as i8;
                self.o[5] = offsets[1] as i8;
                self.o[6] = offsets[2] as i8;
                self.o[7] = offsets[3] as i8;
            }
            _ => {
                self.type_cr = ty as u8;
                self.band_cr = band as u8;
                self.class_cr = eo_class as u8;
                self.o[8] = offsets[0] as i8;
                self.o[9] = offsets[1] as i8;
                self.o[10] = offsets[2] as i8;
                self.o[11] = offsets[3] as i8;
            }
        }
    }
    pub fn ty(&self, c: usize) -> u8 {
        match c {
            0 => self.type_y,
            1 => self.type_cb,
            _ => self.type_cr,
        }
    }
    pub fn band(&self, c: usize) -> u8 {
        match c {
            0 => self.band_y,
            1 => self.band_cb,
            _ => self.band_cr,
        }
    }
    pub fn class_(&self, c: usize) -> u8 {
        match c {
            0 => self.class_y,
            1 => self.class_cb,
            _ => self.class_cr,
        }
    }
    pub fn offset(&self, c: usize, i: usize) -> i8 {
        self.o[(c * 4 + i).min(11)]
    }
}

/// 解码一张帧内 HEVC 图画为 [`HevcPicture`]。
///
/// `decode` 期间 CABAC 引擎借用当前切片的去转义 RBSP（`HevcCabac` 带生命周期），由 `decode` 循环
/// 以 `&mut HevcCabac` 传入各语法方法；当前切片头与 CU 状态存于此结构体字段。
pub struct HevcFrameDecoder {
    pub(crate) sps: HevcSps,
    pub(crate) pps: HevcPps,
    pub(crate) picture: HevcPicture,

    pub(crate) w4: i32,
    ct_depth: Vec<u8>,
    intra_mode: Vec<u8>,
    pub(crate) qp_y: Vec<i8>,
    pub(crate) no_filter: Vec<u8>,
    // 每个已重建 4×4 亮度块的 SliceAddrRs，-1 表示尚未解码（帧内参考可用性）。
    decoded: Vec<i32>,
    // bS：每个 4×4 块左缘 / 上缘的变换边强度（去块用）。
    pub(crate) edge_ver: Vec<u8>,
    pub(crate) edge_hor: Vec<u8>,
    // 每个 CTB 所属切片的 SliceAddrRs，-1 表示未解码。
    pub(crate) ctb_slice_addr: Vec<i32>,
    // 每个 CTB 所属切片头（C# `_ctbSlice`）：去块 β/tC 偏移与 SAO 标志按 CTB 查。
    pub(crate) ctb_slice: Vec<HevcSliceHeader>,
    // SAO 参数（语法已在 ParseSao 全量读走）。
    pub(crate) sao: Vec<SaoParams>,
    // 滤波消融（oracle `_nofilter` 对照）：真 = 跳过对应环节。
    skip_deblock: bool,
    skip_sao: bool,
    // CTB 内最小编码变换块的 morton 序（§6.5.2）。
    zscan: Vec<i32>,
    zscan_shift: i32,
    qp_bd_offset_y: i32,
    qp_bd_offset_c: i32,
    log2_min_cu_qp_delta_size: i32,

    // 当前切片 / CTU / CU 状态。
    slice: Option<HevcSliceHeader>,
    ctb_addr_rs: i32,
    cu_transquant_bypass: bool,
    intra_chroma_mode: i32,
    qp_y_pred: i32,
    qp_y_current: i32,
    last_cu_qp_y: i32,
    first_qg_in_slice: bool,
    first_qg_in_ctb_row: bool,
    is_cu_qp_delta_coded: bool,
    cu_qp_delta_val: i32,

    // 草稿缓冲。
    coeffs: [i32; MAX_TB_SIZE * MAX_TB_SIZE],
    residual: [i32; MAX_TB_SIZE * MAX_TB_SIZE],
    refs: [i32; 4 * MAX_TB_SIZE + 1],
    ref_available: [bool; 4 * MAX_TB_SIZE + 1],
    csbf: [u8; 64],
    wpp_contexts: Vec<u8>,
    ds_contexts: Vec<u8>,
    wpp_saved: bool,
}

impl HevcFrameDecoder {
    /// 编码深度 `depth` 与 transquant-bypass 之外的辅助字段为 Phase-3 滤波专用，此刻只写不读。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sps: HevcSps,
        pps: HevcPps,
        _pooled: bool,
        _parallel: bool,
    ) -> HeifResult<HevcFrameDecoder> {
        if pps.tiles_enabled {
            return Err(HeifError::unsupported("HEVC tiles 不受支持。"));
        }
        if sps.pcm_enabled {
            return Err(HeifError::unsupported("HEVC PCM 编码不受支持。"));
        }
        if sps.chroma_format_idc != 1 {
            return Err(HeifError::unsupported(format!(
                "HEVC chroma_format_idc {} 不受支持；只支持 4:2:0。",
                sps.chroma_format_idc
            )));
        }
        if sps.bit_depth_y > 10 || sps.bit_depth_c > 10 {
            return Err(HeifError::unsupported("HEVC 位深超过 10 不受支持。"));
        }
        let log2_min_cu_qp_delta_size = sps.ctb_log2_size_y - pps.diff_cu_qp_delta_depth;
        if log2_min_cu_qp_delta_size < sps.min_cb_log2_size_y {
            return Err(HeifError::invalid("diff_cu_qp_delta_depth 越界。"));
        }

        let mut picture = HevcPicture::new(
            sps.width,
            sps.height,
            sps.chroma_format_idc,
            sps.bit_depth_y,
            sps.bit_depth_c,
        );
        picture.crop_left = sps.conf_left;
        picture.crop_right = sps.conf_right;
        picture.crop_top = sps.conf_top;
        picture.crop_bottom = sps.conf_bottom;

        let w4 = (sps.width + 3) >> 2;
        let h4 = (sps.height + 3) >> 2;
        let blocks = (w4 * h4) as usize;
        let ctbs = (sps.pic_width_in_ctbs() * sps.pic_height_in_ctbs()) as usize;
        let qp_bd_offset_y = 6 * (sps.bit_depth_y - 8);
        let qp_bd_offset_c = 6 * (sps.bit_depth_c - 8);

        // 6.5.2：一个 CTB 内最小变换块的 z-scan 序。
        let zscan_shift = sps.ctb_log2_size_y - sps.min_tb_log2_size_y;
        let side = 1 << zscan_shift;
        let mut zscan = vec![0i32; (side * side) as usize];
        for y in 0..side {
            for x in 0..side {
                let mut z = 0;
                for bit in 0..zscan_shift {
                    z |= ((x >> bit) & 1) << (2 * bit);
                    z |= ((y >> bit) & 1) << (2 * bit + 1);
                }
                zscan[(y * side + x) as usize] = z;
            }
        }

        Ok(HevcFrameDecoder {
            sps,
            pps,
            picture,
            w4,
            ct_depth: vec![0u8; blocks],
            intra_mode: vec![0u8; blocks],
            qp_y: vec![0i8; blocks],
            no_filter: vec![0u8; blocks],
            decoded: vec![-1i32; blocks],
            edge_ver: vec![0u8; blocks],
            edge_hor: vec![0u8; blocks],
            ctb_slice_addr: vec![-1i32; ctbs],
            ctb_slice: vec![HevcSliceHeader::default(); ctbs],
            sao: vec![SaoParams::default(); ctbs],
            skip_deblock: false,
            skip_sao: false,
            zscan,
            zscan_shift,
            qp_bd_offset_y,
            qp_bd_offset_c,
            log2_min_cu_qp_delta_size,
            slice: None,
            ctb_addr_rs: 0,
            cu_transquant_bypass: false,
            intra_chroma_mode: 0,
            qp_y_pred: 0,
            qp_y_current: 0,
            last_cu_qp_y: 0,
            first_qg_in_slice: false,
            first_qg_in_ctb_row: false,
            is_cu_qp_delta_coded: false,
            cu_qp_delta_val: 0,
            coeffs: [0; MAX_TB_SIZE * MAX_TB_SIZE],
            residual: [0; MAX_TB_SIZE * MAX_TB_SIZE],
            refs: [0; 4 * MAX_TB_SIZE + 1],
            ref_available: [false; 4 * MAX_TB_SIZE + 1],
            csbf: [0; 64],
            wpp_contexts: vec![0u8; HevcCabacTables::Count],
            ds_contexts: vec![0u8; HevcCabacTables::Count],
            wpp_saved: false,
        })
    }

    /// 当前解码出的图画。
    pub fn picture(&self) -> &HevcPicture {
        &self.picture
    }

    /// 当前切片头的访问器（`decode` 在进入任何 CTU 前已设置；内部不变式，非输入数据）。
    fn slice(&self) -> &HevcSliceHeader {
        self.slice
            .as_ref()
            .expect("切片头已在 decode 时设置（内部不变式）")
    }
    fn slice_addr(&self) -> i32 {
        self.slice().slice_addr_rs
    }

    /// 解码 `(nal_type, 去转义 RBSP)` 切片（同图画所有切片段）为图画。
    pub fn decode(&mut self, slices: Vec<(u8, Vec<u8>)>) -> HeifResult<HevcPicture> {
        let mut previous: Option<HevcSliceHeader> = None;
        let mut decoded_ctbs = 0i32;
        let width_in_ctbs = self.sps.pic_width_in_ctbs();
        let total = self.ctb_slice_addr.len();
        for (nal_type, rbsp) in slices {
            let header = HevcSliceHeader::parse(&rbsp, nal_type, &self.sps, &self.pps, previous.as_ref())?;
            if previous.is_none() && !header.first_slice_segment_in_pic {
                return Err(HeifError::invalid("图画的第一切片段缺失。"));
            }
            let mut cabac = HevcCabac::new(&rbsp, header.data_offset);
            decoded_ctbs += self.decode_slice_segment(&mut cabac, &header, width_in_ctbs, total)? as i32;
            previous = Some(header);
        }
        if decoded_ctbs != self.ctb_slice_addr.len() as i32
            || self.ctb_slice_addr.iter().any(|&a| a == -1)
        {
            return Err(HeifError::invalid("图画的切片没有覆盖每个 CTB。"));
        }
        self.filter_picture()?;
        let mut dummy = HevcPicture::new(0, 0, 0, 0, 0);
        std::mem::swap(&mut self.picture, &mut dummy);
        Ok(dummy)
    }

    /// 全图 in-loop 滤波：先去块，再 SAO（§8.7）。移植自 C# `HevcLoopFilter.cs` 的
    /// `Deblock()`/`ApplySao()`（`skip_*` 供 oracle `_nofilter` 消融对照）。
    fn filter_picture(&mut self) -> HeifResult<()> {
        if !self.skip_deblock {
            self.deblock()?;
        }
        if !self.skip_sao {
            self.apply_sao()?;
        }
        Ok(())
    }

    /// 查询/开启滤波消融（测试用）：`skip_all = true` 时去块与 SAO 都跳过，
    /// 输出 = `_t0_nofilter.yuv` 的 C# `-deblock -sao` 消融。
    pub fn set_skip_filters(&mut self, skip_deblock: bool, skip_sao: bool) {
        self.skip_deblock = skip_deblock;
        self.skip_sao = skip_sao;
    }

    // ───────────────────────────── 切片数据（7.3.8.1） ─────────────────────────────

    fn decode_slice_segment(
        &mut self,
        cabac: &mut HevcCabac,
        header: &HevcSliceHeader,
        width_in_ctbs: i32,
        total: usize,
    ) -> HeifResult<usize> {
        self.slice = Some(header.clone());
        self.ctb_addr_rs = header.segment_address;
        if self.ctb_slice_addr[self.ctb_addr_rs as usize] != -1 {
            return Err(HeifError::invalid("切片段重叠。"));
        }
        if !header.dependent_slice_segment {
            self.first_qg_in_slice = true;
            self.last_cu_qp_y = header.slice_qp_y;
        }

        let mut count = 0usize;
        let mut first = true;
        loop {
            if self.ctb_addr_rs as usize >= total {
                return Err(HeifError::invalid("切片数据越过最后一个 CTB。"));
            }
            if self.ctb_slice_addr[self.ctb_addr_rs as usize] != -1 {
                return Err(HeifError::invalid("切片段重叠。"));
            }
            let ctb_x = self.ctb_addr_rs % width_in_ctbs;
            let ctb_y = self.ctb_addr_rs / width_in_ctbs;

            // 9.3.1 / 9.3.2.1：在切片段起点与每个 WPP 行起点初始化上下文。
            if first || (self.pps.entropy_coding_sync_enabled && ctb_x == 0) {
                self.initialize_contexts_for_ctu(cabac, ctb_x, ctb_y, first)?;
            }
            if self.pps.entropy_coding_sync_enabled && ctb_x == 0 {
                self.first_qg_in_ctb_row = true;
            }
            first = false;

            self.ctb_slice_addr[self.ctb_addr_rs as usize] = header.slice_addr_rs;
            self.ctb_slice[self.ctb_addr_rs as usize] = header.clone();
            self.coding_tree_unit(cabac, ctb_x, ctb_y)?;
            count += 1;

            // 9.3.2.4：一行第二个 CTB 之后保存 WPP 上下文。
            if self.pps.entropy_coding_sync_enabled && ctb_x == 1 {
                cabac.save_contexts(&mut self.wpp_contexts)?;
                self.wpp_saved = true;
            }

            let end_of_slice = cabac.decode_terminate()? == 1;
            self.ctb_addr_rs += 1;
            if end_of_slice {
                if self.pps.dependent_slice_segments_enabled {
                    cabac.save_contexts(&mut self.ds_contexts)?;
                }
                return Ok(count);
            }
            if self.pps.entropy_coding_sync_enabled && self.ctb_addr_rs % width_in_ctbs == 0 {
                if cabac.decode_terminate()? != 1 {
                    return Err(HeifError::invalid("end_of_subset_one_bit 不是 1。"));
                }
                // byte_alignment()：终止 bin 已吃掉 1 位；跳到字节边界重启引擎。
                let pos = cabac.bit_position();
                cabac.init_engine(((pos + 7) >> 3) as usize);
            }
        }
    }

    fn initialize_contexts_for_ctu(
        &mut self,
        cabac: &mut HevcCabac,
        ctb_x: i32,
        ctb_y: i32,
        slice_segment_start: bool,
    ) -> HeifResult<()> {
        if self.ctb_addr_rs == 0 {
            let qp = self.slice().slice_qp_y;
            cabac.init_contexts(qp, 0)?;
            return Ok(());
        }
        if self.pps.entropy_coding_sync_enabled && ctb_x == 0 {
            // 空间邻居 T = (x0 + CtbSizeY, y0 - CtbSizeY)。
            let ctb_size = self.sps.ctb_size_y();
            let x_t = (ctb_x + 1) * ctb_size;
            let y_t = ctb_y * ctb_size - ctb_size;
            let available = self.wpp_saved
                && x_t < self.sps.width
                && y_t >= 0
                && self.ctb_slice_addr
                    [((ctb_y - 1) * self.sps.pic_width_in_ctbs() + ctb_x + 1) as usize]
                    == self.slice_addr();
            if available {
                cabac.load_contexts(&self.wpp_contexts)?;
            } else {
                let qp = self.slice().slice_qp_y;
                cabac.init_contexts(qp, 0)?;
            }
            return Ok(());
        }
        if slice_segment_start && self.slice().dependent_slice_segment {
            cabac.load_contexts(&self.ds_contexts)?;
            return Ok(());
        }
        if slice_segment_start {
            let qp = self.slice().slice_qp_y;
            cabac.init_contexts(qp, 0)?;
        }
        Ok(())
    }

    // ───────────────────────────── 可用性（6.4.1） ─────────────────────────────

    fn z_address(&self, x: i32, y: i32) -> i32 {
        let ctb_log2 = self.sps.ctb_log2_size_y;
        let ctb_addr = (y >> ctb_log2) * self.sps.pic_width_in_ctbs() + (x >> ctb_log2);
        let mask = (1 << ctb_log2) - 1;
        let min_tb = self.sps.min_tb_log2_size_y;
        let inner = self.zscan
            [(((y & mask) >> min_tb) << self.zscan_shift) as usize + ((x & mask) >> min_tb) as usize];
        (ctb_addr << (2 * self.zscan_shift)) + inner
    }

    fn available(&self, x_curr: i32, y_curr: i32, x_n: i32, y_n: i32) -> bool {
        if x_n < 0 || y_n < 0 || x_n >= self.sps.width || y_n >= self.sps.height {
            return false;
        }
        if self.z_address(x_n, y_n) > self.z_address(x_curr, y_curr) {
            return false;
        }
        let ctb_log2 = self.sps.ctb_log2_size_y;
        let ctb = (y_n >> ctb_log2) * self.sps.pic_width_in_ctbs() + (x_n >> ctb_log2);
        self.ctb_slice_addr[ctb as usize] == self.slice_addr()
    }

    fn block(&self, x: i32, y: i32) -> usize {
        (y >> 2) as usize * self.w4 as usize + (x >> 2) as usize
    }

    // ───────────────────────────── CTU / SAO 语法（7.3.8.2, 7.3.8.3） ─────────────────────────────

    fn coding_tree_unit(&mut self, cabac: &mut HevcCabac, ctb_x: i32, ctb_y: i32) -> HeifResult<()> {
        let s = self.slice();
        if s.sao_luma || s.sao_chroma {
            self.parse_sao(cabac, ctb_x, ctb_y)?;
        }
        self.coding_quadtree(
            cabac,
            ctb_x << self.sps.ctb_log2_size_y,
            ctb_y << self.sps.ctb_log2_size_y,
            self.sps.ctb_log2_size_y,
            0,
        )
    }

    fn parse_sao(&mut self, cabac: &mut HevcCabac, rx: i32, ry: i32) -> HeifResult<()> {
        let width = self.sps.pic_width_in_ctbs();
        let slice_addr = self.slice_addr();
        let slice_sao_luma = self.slice().sao_luma;
        let slice_sao_chroma = self.slice().sao_chroma;
        let bit_depth_y = self.sps.bit_depth_y;
        let bit_depth_c = self.sps.bit_depth_c;

        let mut merge_left = false;
        let mut merge_up = false;
        if rx > 0 && self.ctb_addr_rs - 1 >= slice_addr {
            merge_left = cabac.decode_bin(HevcCabacTables::SaoMerge)? == 1;
        }
        if ry > 0 && !merge_left && self.ctb_addr_rs - width >= slice_addr {
            merge_up = cabac.decode_bin(HevcCabacTables::SaoMerge)? == 1;
        }
        if merge_left {
            self.sao[self.ctb_addr_rs as usize] = self.sao[(self.ctb_addr_rs - 1) as usize];
            return Ok(());
        }
        if merge_up {
            self.sao[self.ctb_addr_rs as usize] = self.sao[(self.ctb_addr_rs - width) as usize];
            return Ok(());
        }

        let mut p = SaoParams::default();
        let mut chroma_type = 0;
        let mut chroma_class = 0;
        for c in 0..3usize {
            let mut offsets = [0i32; 4];
            if (c == 0 && !slice_sao_luma) || (c > 0 && !slice_sao_chroma) {
                p.set(c, 0, 0, 0, &offsets);
                continue;
            }
            let ty: i32;
            if c == 2 {
                ty = chroma_type;
            } else {
                // sao_type_idx：TR，cMax = 2，首 bin 上下文编码，第二 bin 旁路。
                ty = if cabac.decode_bin(HevcCabacTables::SaoType)? == 0 {
                    0
                } else if cabac.decode_bypass()? == 0 {
                    1
                } else {
                    2
                };
                if c == 1 {
                    chroma_type = ty;
                }
            }
            if ty == 0 {
                p.set(c, 0, 0, 0, &offsets);
                continue;
            }

            let bit_depth = if c == 0 { bit_depth_y } else { bit_depth_c };
            let c_max = (1 << (bit_depth.min(10) - 5)) - 1;
            for i in 0..4usize {
                let mut abs = 0;
                while abs < c_max && cabac.decode_bypass()? == 1 {
                    abs += 1;
                }
                offsets[i] = abs;
            }

            let mut band = 0;
            let mut eo_class = 0;
            if ty == 1 {
                for i in 0..4usize {
                    if offsets[i] != 0 && cabac.decode_bypass()? == 1 {
                        offsets[i] = -offsets[i];
                    }
                }
                band = cabac.decode_bypass_bits(5)? as i32;
            } else {
                // 边界偏移：1、2 类为正，3、4 类为负（7.4.9.3.2）。
                offsets[2] = -offsets[2];
                offsets[3] = -offsets[3];
                if c == 0 {
                    eo_class = cabac.decode_bypass_bits(2)? as i32;
                } else if c == 1 {
                    eo_class = cabac.decode_bypass_bits(2)? as i32;
                    chroma_class = eo_class;
                } else {
                    eo_class = chroma_class;
                }
            }
            p.set(c, ty, band, eo_class, &offsets);
        }
        self.sao[self.ctb_addr_rs as usize] = p;
        Ok(())
    }

    // ───────────────────────────── 编码四叉树 / 单元（7.3.8.4, 7.3.8.5） ─────────────────────────────

    fn coding_quadtree(
        &mut self,
        cabac: &mut HevcCabac,
        x0: i32,
        y0: i32,
        log2_cb_size: i32,
        depth: i32,
    ) -> HeifResult<()> {
        let size = 1 << log2_cb_size;
        let split: bool;
        if x0 + size <= self.sps.width
            && y0 + size <= self.sps.height
            && log2_cb_size > self.sps.min_cb_log2_size_y
        {
            let mut ctx_inc = 0;
            if self.available(x0, y0, x0 - 1, y0) && self.ct_depth[self.block(x0 - 1, y0)] as i32 > depth {
                ctx_inc += 1;
            }
            if self.available(x0, y0, x0, y0 - 1) && self.ct_depth[self.block(x0, y0 - 1)] as i32 > depth {
                ctx_inc += 1;
            }
            split = cabac.decode_bin(HevcCabacTables::SplitCu + ctx_inc)? == 1;
        } else {
            split = log2_cb_size > self.sps.min_cb_log2_size_y;
        }

        if log2_cb_size >= self.log2_min_cu_qp_delta_size {
            self.is_cu_qp_delta_coded = false;
            self.cu_qp_delta_val = 0;
            self.start_quantization_group(x0, y0);
        }

        if split {
            let half = size >> 1;
            let x1 = x0 + half;
            let y1 = y0 + half;
            self.coding_quadtree(cabac, x0, y0, log2_cb_size - 1, depth + 1)?;
            if x1 < self.sps.width {
                self.coding_quadtree(cabac, x1, y0, log2_cb_size - 1, depth + 1)?;
            }
            if y1 < self.sps.height {
                self.coding_quadtree(cabac, x0, y1, log2_cb_size - 1, depth + 1)?;
            }
            if x1 < self.sps.width && y1 < self.sps.height {
                self.coding_quadtree(cabac, x1, y1, log2_cb_size - 1, depth + 1)?;
            }
        } else {
            self.coding_unit(cabac, x0, y0, log2_cb_size, depth)?;
        }
        Ok(())
    }

    /// 起始于 (xQg, yQg) 的量化组的 qPY_PRED，§8.6.1。
    fn start_quantization_group(&mut self, x_qg: i32, y_qg: i32) {
        // 第一个 CU 解码时清掉这些旗标，所以每个打开切片/行首个量化组的四叉树层都看到
        // qPY_PREV = SliceQpY。
        let prev = if self.first_qg_in_slice || self.first_qg_in_ctb_row {
            self.slice().slice_qp_y
        } else {
            self.last_cu_qp_y
        };

        let ctb_mask = self.sps.ctb_size_y() - 1;
        // qPY_A / qPY_B：邻居可用且同 CTB 时取邻居 QpY，否则 qPY_PREV。
        let qp_a = if (x_qg & ctb_mask) != 0 && self.available(x_qg, y_qg, x_qg - 1, y_qg) {
            self.qp_y[self.block(x_qg - 1, y_qg)] as i32
        } else {
            prev
        };
        let qp_b = if (y_qg & ctb_mask) != 0 && self.available(x_qg, y_qg, x_qg, y_qg - 1) {
            self.qp_y[self.block(x_qg, y_qg - 1)] as i32
        } else {
            prev
        };
        self.qp_y_pred = (qp_a + qp_b + 1) >> 1;
    }

    ///（8-283）。
    fn update_cu_qp(&mut self) {
        let span = 52 + self.qp_bd_offset_y;
        self.qp_y_current = ((self.qp_y_pred + self.cu_qp_delta_val + 52 + 2 * self.qp_bd_offset_y)
            % span)
            - self.qp_bd_offset_y;
    }

    fn coding_unit(
        &mut self,
        cabac: &mut HevcCabac,
        x0: i32,
        y0: i32,
        log2_cb_size: i32,
        depth: i32,
    ) -> HeifResult<()> {
        let size = 1 << log2_cb_size;
        self.cu_transquant_bypass = self.pps.transquant_bypass_enabled
            && cabac.decode_bin(HevcCabacTables::TransquantBypass)? == 1;
        self.update_cu_qp();

        // I 片的帧内 CU：part_mode 只在最小 CB 尺寸出现，1 → PART_2Nx2N，0 → PART_NxN。
        let part_nxn = log2_cb_size == self.sps.min_cb_log2_size_y
            && cabac.decode_bin(HevcCabacTables::PartMode)? == 0;

        // 先填 CU 辅助信息：后续 PB 的亮度模式推导要读它。
        let pb_offset = if part_nxn { size >> 1 } else { size };
        let x_end = (x0 + size).min(self.sps.width);
        let y_end = (y0 + size).min(self.sps.height);
        let mut y = y0;
        while y < y_end {
            let mut x = x0;
            while x < x_end {
                let b = self.block(x, y);
                self.ct_depth[b] = depth as u8;
                self.no_filter[b] = if self.cu_transquant_bypass { 1 } else { 0 };
                x += 4;
            }
            y += 4;
        }

        // 每个 PB 的 prev_intra_luma_pred_flag，随后 mpm_idx / rem_intra_luma_pred_mode（7.3.8.5）。
        let pb_count = if part_nxn { 4 } else { 1 };
        let mut prev_flags = [false; 4];
        for i in 0..pb_count {
            prev_flags[i] = cabac.decode_bin(HevcCabacTables::PrevIntraLuma)? == 1;
        }
        for i in 0..pb_count {
            let x_pb = x0 + (i & 1) as i32 * pb_offset;
            let y_pb = y0 + ((i >> 1) as i32) * pb_offset;
            let mode: i32;
            if prev_flags[i] {
                let mpm_idx = if cabac.decode_bypass()? == 0 {
                    0
                } else if cabac.decode_bypass()? == 0 {
                    1
                } else {
                    2
                };
                mode = self.derive_luma_mode(x_pb, y_pb, mpm_idx, -1);
            } else {
                mode = self.derive_luma_mode(x_pb, y_pb, -1, cabac.decode_bypass_bits(5)? as i32);
            }
            let pb_end_x = (x_pb + pb_offset).min(self.sps.width);
            let pb_end_y = (y_pb + pb_offset).min(self.sps.height);
            let mut y = y_pb;
            while y < pb_end_y {
                let mut x = x_pb;
                while x < pb_end_x {
                    let b = self.block(x, y);
                    self.intra_mode[b] = mode as u8;
                    x += 4;
                }
                y += 4;
            }
        }

        // intra_chroma_pred_mode（4:2:0：每个 CU 一个）→ IntraPredModeC（表 8-2）。
        let chroma_syntax = if cabac.decode_bin(HevcCabacTables::IntraChroma)? == 0 {
            4
        } else {
            cabac.decode_bypass_bits(2)? as i32
        };
        let luma_mode = self.intra_mode[self.block(x0, y0)] as i32;
        if chroma_syntax == 4 {
            self.intra_chroma_mode = luma_mode;
        } else {
            let candidate = match chroma_syntax {
                0 => 0,
                1 => 26,
                2 => 10,
                _ => 1,
            };
            self.intra_chroma_mode = if candidate == luma_mode { 34 } else { candidate };
        }

        let max_trafo_depth =
            self.sps.max_transform_hierarchy_depth_intra + if part_nxn { 1 } else { 0 };
        self.transform_tree(
            cabac,
            x0,
            y0,
            x0,
            y0,
            log2_cb_size,
            0,
            0,
            max_trafo_depth,
            part_nxn,
            true,
            true,
        )?;

        // CU 的 QpY（8-283，含其中解出的 CuQpDeltaVal），供预测与去块。
        self.update_cu_qp();
        let mut y = y0;
        while y < y_end {
            let mut x = x0;
            while x < x_end {
                let b = self.block(x, y);
                self.qp_y[b] = self.qp_y_current as i8;
                x += 4;
            }
            y += 4;
        }
        self.last_cu_qp_y = self.qp_y_current;
        self.first_qg_in_slice = false;
        self.first_qg_in_ctb_row = false;
        Ok(())
    }

    /// 8.4.2：从候选表（`mpm_idx` ≥ 0）或 rem_intra_luma_pred_mode 得 IntraPredModeY。
    fn derive_luma_mode(&self, x_pb: i32, y_pb: i32, mpm_idx: i32, rem_mode: i32) -> i32 {
        let cand_a = if self.available(x_pb, y_pb, x_pb - 1, y_pb) {
            self.intra_mode[self.block(x_pb - 1, y_pb)] as i32
        } else {
            1
        };
        let mut cand_b = 1;
        let ctb_top = (y_pb >> self.sps.ctb_log2_size_y) << self.sps.ctb_log2_size_y;
        if y_pb - 1 >= ctb_top && self.available(x_pb, y_pb, x_pb, y_pb - 1) {
            cand_b = self.intra_mode[self.block(x_pb, y_pb - 1)] as i32;
        }

        let mut list = [0i32; 3];
        if cand_a == cand_b {
            if cand_a < 2 {
                list[0] = 0;
                list[1] = 1;
                list[2] = 26;
            } else {
                list[0] = cand_a;
                list[1] = 2 + ((cand_a + 29) % 32);
                list[2] = 2 + ((cand_a - 2 + 1) % 32);
            }
        } else {
            list[0] = cand_a;
            list[1] = cand_b;
            list[2] = if cand_a != 0 && cand_b != 0 {
                0
            } else if cand_a != 1 && cand_b != 1 {
                1
            } else {
                26
            };
        }

        if mpm_idx >= 0 {
            return list[mpm_idx as usize];
        }

        if list[0] > list[1] {
            list.swap(0, 1);
        }
        if list[0] > list[2] {
            list.swap(0, 2);
        }
        if list[1] > list[2] {
            list.swap(1, 2);
        }
        let mut mode = rem_mode;
        for i in 0..3 {
            if mode >= list[i] {
                mode += 1;
            }
        }
        mode
    }

    // ───────────────────────────── 变换树 / 单元（7.3.8.8, 7.3.8.10） ─────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn transform_tree(
        &mut self,
        cabac: &mut HevcCabac,
        x0: i32,
        y0: i32,
        x_base: i32,
        y_base: i32,
        log2_size: i32,
        depth: i32,
        blk_idx: i32,
        max_trafo_depth: i32,
        intra_split: bool,
        parent_cbf_cb: bool,
        parent_cbf_cr: bool,
    ) -> HeifResult<()> {
        let split: bool;
        if log2_size <= self.sps.max_tb_log2_size_y
            && log2_size > self.sps.min_tb_log2_size_y
            && depth < max_trafo_depth
            && !(intra_split && depth == 0)
        {
            split =
                cabac.decode_bin(HevcCabacTables::SplitTransform + (5 - log2_size) as usize)? == 1;
        } else {
            split = log2_size > self.sps.max_tb_log2_size_y || (intra_split && depth == 0);
        }

        let (cbf_cb, cbf_cr): (bool, bool);
        if log2_size > 2 {
            let ctx = HevcCabacTables::CbfChroma + depth.min(4) as usize;
            cbf_cb = (depth == 0 || parent_cbf_cb) && cabac.decode_bin(ctx)? == 1;
            cbf_cr = (depth == 0 || parent_cbf_cr) && cabac.decode_bin(ctx)? == 1;
        } else {
            // 4:2:0 的 4×4 亮度块：色度块属于父块（cbf 取自 depth - 1）。
            cbf_cb = parent_cbf_cb;
            cbf_cr = parent_cbf_cr;
        }

        if split {
            let half = 1 << (log2_size - 1);
            self.transform_tree(
                cabac, x0, y0, x0, y0, log2_size - 1, depth + 1, 0, max_trafo_depth, intra_split,
                cbf_cb, cbf_cr,
            )?;
            self.transform_tree(
                cabac, x0 + half, y0, x0, y0, log2_size - 1, depth + 1, 1, max_trafo_depth,
                intra_split, cbf_cb, cbf_cr,
            )?;
            self.transform_tree(
                cabac, x0, y0 + half, x0, y0, log2_size - 1, depth + 1, 2, max_trafo_depth,
                intra_split, cbf_cb, cbf_cr,
            )?;
            self.transform_tree(
                cabac, x0 + half, y0 + half, x0, y0, log2_size - 1, depth + 1, 3, max_trafo_depth,
                intra_split, cbf_cb, cbf_cr,
            )?;
            return Ok(());
        }

        let cbf_luma = cabac.decode_bin(HevcCabacTables::CbfLuma + if depth == 0 { 1 } else { 0 })?
            == 1;
        self.transform_unit(
            cabac,
            x0,
            y0,
            x_base,
            y_base,
            log2_size,
            blk_idx,
            cbf_luma,
            cbf_cb,
            cbf_cr,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn transform_unit(
        &mut self,
        cabac: &mut HevcCabac,
        x0: i32,
        y0: i32,
        x_base: i32,
        y_base: i32,
        log2_size: i32,
        blk_idx: i32,
        cbf_luma: bool,
        cbf_cb: bool,
        cbf_cr: bool,
    ) -> HeifResult<()> {
        if (cbf_luma || cbf_cb || cbf_cr) && self.pps.cu_qp_delta_enabled && !self.is_cu_qp_delta_coded
        {
            // cu_qp_delta_abs：前缀 TU（cMax 5，ctxInc 0 然后 1），后缀 EG0 旁路；然后符号。
            let mut abs = 0;
            if cabac.decode_bin(HevcCabacTables::CuQpDeltaAbs)? == 1 {
                abs = 1;
                while abs < 5 && cabac.decode_bin(HevcCabacTables::CuQpDeltaAbs + 1)? == 1 {
                    abs += 1;
                }
                if abs == 5 {
                    abs += cabac.decode_exp_golomb(0)?.min(1000) as i32;
                }
            }
            self.is_cu_qp_delta_coded = true;
            let delta = if abs != 0 && cabac.decode_bypass()? == 1 {
                -abs
            } else {
                abs
            };
            // 7.4.9.14：CuQpDeltaVal ∈ [-(26 + QpBdOffsetY / 2), 25 + QpBdOffsetY / 2]。
            let limit = 26 + self.qp_bd_offset_y / 2;
            if delta < -limit || delta > limit - 1 {
                return Err(HeifError::invalid("CuQpDeltaVal 越界。"));
            }
            self.cu_qp_delta_val = delta;
            self.update_cu_qp();
        }

        // 亮度：先预测，再加残差。
        let luma_mode = self.intra_mode[self.block(x0, y0)] as i32;
        self.predict_intra(0, x0, y0, log2_size, luma_mode)?;
        if cbf_luma {
            self.residual_and_reconstruct(cabac, 0, x0, y0, log2_size, luma_mode)?;
        }
        self.mark_transform_edges(x0, y0, 1 << log2_size);
        self.mark_decoded(x0, y0, 1 << log2_size);

        if log2_size > 2 {
            let x_c = x0 >> 1;
            let y_c = y0 >> 1;
            self.predict_intra(1, x_c, y_c, log2_size - 1, self.intra_chroma_mode)?;
            if cbf_cb {
                self.residual_and_reconstruct(cabac, 1, x_c, y_c, log2_size - 1, self.intra_chroma_mode)?;
            }
            self.predict_intra(2, x_c, y_c, log2_size - 1, self.intra_chroma_mode)?;
            if cbf_cr {
                self.residual_and_reconstruct(cabac, 2, x_c, y_c, log2_size - 1, self.intra_chroma_mode)?;
            }
        } else if blk_idx == 3 {
            let x_c = x_base >> 1;
            let y_c = y_base >> 1;
            self.predict_intra(1, x_c, y_c, 2, self.intra_chroma_mode)?;
            if cbf_cb {
                self.residual_and_reconstruct(cabac, 1, x_c, y_c, 2, self.intra_chroma_mode)?;
            }
            self.predict_intra(2, x_c, y_c, 2, self.intra_chroma_mode)?;
            if cbf_cr {
                self.residual_and_reconstruct(cabac, 2, x_c, y_c, 2, self.intra_chroma_mode)?;
            }
        }
        Ok(())
    }

    /// 把一个亮度 TB 的 bS = 2 变换边记到 8×8 网格上（8.7.2.2–8.7.2.4；每个 CU 都是帧内）。
    fn mark_transform_edges(&mut self, x0: i32, y0: i32, size: i32) {
        if self.slice().deblocking_disabled {
            return;
        }
        let x_end = (x0 + size).min(self.sps.width);
        let y_end = (y0 + size).min(self.sps.height);
        // 竖直边：该 TB 左缘自 y0 起每 4 行一块（Block(x0, y) 逐行跳 w4），C# 逐 y 重算下标。
        if (x0 & 7) == 0 && x0 > 0 && self.edge_allowed(x0 - 1, y0) {
            let mut y = y0;
            while y < y_end {
                let b = self.block(x0, y);
                if b < self.edge_ver.len() {
                    self.edge_ver[b] = 2;
                }
                y += 4;
            }
        }
        if (y0 & 7) == 0 && y0 > 0 && self.edge_allowed(x0, y0 - 1) {
            let mut x = x0;
            while x < x_end {
                let b = self.block(x, y0);
                if b < self.edge_hor.len() {
                    self.edge_hor[b] = 2;
                }
                x += 4;
            }
        }
    }

    /// (xP, yP) 落在另一个切片且当前切片不跨切片滤波时为假。
    fn edge_allowed(&self, x_p: i32, y_p: i32) -> bool {
        if self.slice().loop_filter_across_slices {
            return true;
        }
        let ctb_log2 = self.sps.ctb_log2_size_y;
        let cb = self.ctb_slice_addr
            [((y_p >> ctb_log2) * self.sps.pic_width_in_ctbs() + (x_p >> ctb_log2)) as usize];
        cb == self.slice_addr()
    }

    // ───────────────────────────── 帧内预测（8.4.4.2） ─────────────────────────────

    /// 把 (x0, y0) 的亮度 TB 标记为已重建。帧内预测只在样本重建后才读它：解码顺序（CTB 栅格、
    /// 内部 z-scan）里，块外的样本恰在它之前出现（6.4.1），"标记为当前切片地址"即 6.4.1 的帧内
    /// 参考可用性。
    fn mark_decoded(&mut self, x0: i32, y0: i32, size: i32) {
        let x_end = (x0 + size).min(self.sps.width);
        let y_end = (y0 + size).min(self.sps.height);
        let slice_addr = self.slice_addr();
        let span_w = ((x_end - x0 + 3) >> 2) as usize;
        let mut y = y0;
        while y < y_end {
            let b = self.block(x0, y);
            let end = (b + span_w).min(self.decoded.len());
            self.decoded[b..end].fill(slice_addr);
            y += 4;
        }
    }

    /// 亮度样本 (xN, yN) 作为帧内参考的可用性（6.4.1）；见 [`Self::mark_decoded`]。
    /// 帧内预测（ff：见 `predict_intra` 的内联闭包）直接使用该判定，这里不复用为方法。

    /// 预测 cIdx 分量在 (xTb, yTb) 的块（该分量的采样坐标）。
    fn predict_intra(
        &mut self,
        c_idx: usize,
        x_tb: i32,
        y_tb: i32,
        log2_size: i32,
        mode: i32,
    ) -> HeifResult<()> {
        let n = 1usize << log2_size;
        let shift = if c_idx == 0 { 0 } else { 1 }; // 分量 → 亮度坐标（4:2:0）
        let bit_depth = if c_idx == 0 {
            self.sps.bit_depth_y
        } else {
            self.sps.bit_depth_c
        };
        let unit = if c_idx == 0 { 4 } else { 2 }; // 每个 4×4 亮度可用性单元的样本数
        let len = 4 * n + 1;
        let corner = 2 * n;
        let w4 = self.w4;
        let (width, height) = (self.sps.width, self.sps.height);
        let slice_addr = self.slice_addr();
        let stride = self.picture.plane_width(c_idx);
        let ra = |x_n: i32, y_n: i32| -> bool {
            if x_n < 0 || y_n < 0 || x_n >= width || y_n >= height {
                return false;
            }
            let idx = (y_n >> 2) as usize * w4 as usize + (x_n >> 2) as usize;
            self.decoded[idx] == slice_addr
        };

        let refs = &mut self.refs[..len];
        let available = &mut self.ref_available[..len];
        let plane = &self.picture.planes[c_idx];
        let planewise = stride as usize;
        let mut count = 0usize;

        // 左列，自下而上（下标 0 = p[-1][2N-1] … 下标 2N-1 = p[-1][0]）。
        // 地址用 i64 计算：ok 时才下标（ok 保证坐标在平面内），避免 x_left=-1/顶行 -1 的
        // 无符号回绕溢出（C# 只在 ok 时解引用 int 下标，负值不触碰）。
        let x_left = x_tb - 1;
        let mut y = 0i32;
        while (y as usize) < 2 * n {
            let ok = ra(x_left << shift, (y_tb + y) << shift);
            let at = (y_tb + y) as i64 * planewise as i64 + x_left as i64;
            for k in 0..unit {
                let index = corner - 1 - (y as usize + k);
                available[index] = ok;
                if ok {
                    let i = (at + k as i64 * planewise as i64) as usize;
                    refs[index] = i32::from(plane[i]);
                }
            }
            if ok {
                count += unit;
            }
            y += unit as i32;
        }

        // 角点。
        let corner_ok = ra(x_left << shift, (y_tb - 1) << shift);
        available[corner] = corner_ok;
        if corner_ok {
            let i = ((y_tb - 1) as i64 * planewise as i64 + x_left as i64) as usize;
            refs[corner] = i32::from(plane[i]);
            count += 1;
        }

        // 顶行，自左向右。
        let top = (y_tb as i64 - 1) * planewise as i64 + x_tb as i64;
        let mut x = 0i32;
        while (x as usize) < 2 * n {
            let ok = ra((x_tb + x) << shift, (y_tb - 1) << shift);
            let index = corner + 1 + x as usize;
            for k in 0..unit {
                available[index + k] = ok;
                if ok {
                    let i = (top + (x as i64 + k as i64)) as usize;
                    refs[index + k] = i32::from(plane[i]);
                }
            }
            if ok {
                count += unit;
            }
            x += unit as i32;
        }

        if count != len {
            IntraPred::substitute(refs, available, count > 0, bit_depth)?;
        }

        let plane = &mut self.picture.planes[c_idx];
        let base = y_tb as usize * planewise + x_tb as usize;
        let dst = plane
            .get_mut(base..)
            .ok_or_else(|| HeifError::invalid("帧内预测目标越界。"))?;
        IntraPred::predict(
            refs,
            log2_size,
            mode,
            c_idx == 0,
            c_idx == 0,
            self.sps.strong_intra_smoothing_enabled,
            bit_depth,
            dst,
            stride,
        )
    }

    // ───────────────────────────── 残差编码（7.3.8.11）与重建 ─────────────────────────────

    fn residual_and_reconstruct(
        &mut self,
        cabac: &mut HevcCabac,
        c_idx: usize,
        x_tb: i32,
        y_tb: i32,
        log2_size: i32,
        pred_mode_intra: i32,
    ) -> HeifResult<()> {
        let n = 1usize << log2_size;
        // _coeffs 在块之间全零：每块清掉它写下的矩形。
        let packed = self.residual_coding(cabac, c_idx, log2_size, pred_mode_intra)?;
        let transform_skip = (packed & 1) != 0;
        let max_x = (packed >> 8) & 0xFF;
        let max_y = packed >> 16;

        let bit_depth = if c_idx == 0 {
            self.sps.bit_depth_y
        } else {
            self.sps.bit_depth_c
        };
        let coeffs = &mut self.coeffs[..n * n];
        let stride = self.picture.plane_width(c_idx);
        let base = y_tb as usize * stride as usize + x_tb as usize;
        let plane = &mut self.picture.planes[c_idx];
        let dst = plane
            .get_mut(base..)
            .ok_or_else(|| HeifError::invalid("残差重建目标越界。"))?;
        if self.cu_transquant_bypass {
            HevcTransform::add_residual(coeffs, log2_size, bit_depth, dst, stride)?;
        } else if transform_skip {
            HevcTransform::transform_skip(coeffs, log2_size, bit_depth)?;
            HevcTransform::add_residual(coeffs, log2_size, bit_depth, dst, stride)?;
        } else {
            HevcTransform::inverse_add(
                coeffs,
                &mut self.residual[..n * n],
                log2_size,
                c_idx == 0 && n == 4,
                max_x,
                max_y,
                bit_depth,
                dst,
                stride,
            )?;
        }

        // 变换跳过把零映射到零，所以只有编码系数的矩形非零。
        let span = (max_x as usize + 1).min(n);
        let last_row = (max_y as usize).min(n - 1);
        for y in 0..=last_row {
            let start = y * n;
            let end = (start + span).min(n * n);
            coeffs[start..end].fill(0);
        }
        Ok(())
    }

    /// 当前 CU 的 qP（8-291 .. 8-293）。
    fn component_qp(&self, c_idx: usize) -> i32 {
        if c_idx == 0 {
            return self.qp_y_current + self.qp_bd_offset_y;
        }
        let offset = if c_idx == 1 {
            self.pps.cb_qp_offset + self.slice().cb_qp_offset
        } else {
            self.pps.cr_qp_offset + self.slice().cr_qp_offset
        };
        let q_pi = (self.qp_y_current + offset).clamp(-self.qp_bd_offset_c, 57);
        Self::chroma_qp_table(q_pi) + self.qp_bd_offset_c
    }

    /// 表 8-10（ChromaArrayType 1）。`pub(crate)`：`loop_filter` 模块去块色度边复用。
    pub(crate) fn chroma_qp_table(q_pi: i32) -> i32 {
        const TABLE: [i32; 14] = [29, 30, 31, 32, 33, 33, 34, 34, 35, 35, 36, 36, 37, 37];
        if q_pi < 30 {
            return q_pi;
        }
        if q_pi > 43 {
            return q_pi - 6;
        }
        TABLE[(q_pi - 30) as usize]
    }

    /// 把 residual_coding() 解析进 `coeffs`（行主 N×N），除非 CU 是 transquant-bypass，
    /// 按 (8-309) 缩放（变换跳过平移跳过）。返回 bit 0 = transform_skip_flag，
    /// bits 8..15 / 16..23 = 非零系数的最大 x / y。
    fn residual_coding(
        &mut self,
        cabac: &mut HevcCabac,
        c_idx: usize,
        log2_size: i32,
        pred_mode_intra: i32,
    ) -> HeifResult<i32> {
        let n = 1 << log2_size;
        let mut transform_skip = false;
        if self.pps.transform_skip_enabled
            && !self.cu_transquant_bypass
            && log2_size <= self.pps.log2_max_transform_skip_size
        {
            let base = HevcCabacTables::TransformSkip + if c_idx > 0 { 1 } else { 0 };
            transform_skip = cabac.decode_bin(base)? == 1;
        }

        // 缩放（8.6.3），逐系数解码时应用。
        let bypass = self.cu_transquant_bypass;
        let scaler = if bypass {
            None
        } else {
            let bit_depth = if c_idx == 0 {
                self.sps.bit_depth_y
            } else {
                self.sps.bit_depth_c
            };
            Some(Scaler::new(log2_size, self.component_qp(c_idx), bit_depth)?)
        };
        let scaling = self.pps.scaling_list.as_ref().or(self.sps.scaling_list.as_ref());
        // C# `_pps.ScalingList ?? _sps.ScalingList`：PPS 优先，否则 SPS。
        let factors: Option<&[u8]> = if self.sps.scaling_list_enabled
            && let Some(scaling_list) = scaling
            && !(transform_skip && n > 4)
        {
            Some(&scaling_list.factors[(log2_size - 2) as usize][c_idx])
        } else {
            None
        };

        let mut max_x = 0;
        let mut max_y = 0;

        // last_sig_coeff_{x,y}_prefix（9.3.4.2.3），然后后缀。
        let (ctx_offset, ctx_shift): (i32, i32) = if c_idx == 0 {
            (3 * (log2_size - 2) + ((log2_size - 1) >> 2), (log2_size + 1) >> 2)
        } else {
            (15, log2_size - 2)
        };
        let c_max = (log2_size << 1) - 1;
        let mut prefix_x = 0;
        while prefix_x < c_max
            && cabac.decode_bin(HevcCabacTables::LastX + (ctx_offset + (prefix_x >> ctx_shift)) as usize)?
                == 1
        {
            prefix_x += 1;
        }
        let mut prefix_y = 0;
        while prefix_y < c_max
            && cabac.decode_bin(HevcCabacTables::LastY + (ctx_offset + (prefix_y >> ctx_shift)) as usize)?
                == 1
        {
            prefix_y += 1;
        }
        let mut last_x = prefix_x;
        if prefix_x > 3 {
            let bits = (prefix_x >> 1) - 1;
            last_x = (1 << bits) * (2 + (prefix_x & 1)) + cabac.decode_bypass_bits(bits as usize)? as i32;
        }
        let mut last_y = prefix_y;
        if prefix_y > 3 {
            let bits = (prefix_y >> 1) - 1;
            last_y = (1 << bits) * (2 + (prefix_y & 1)) + cabac.decode_bypass_bits(bits as usize)? as i32;
        }

        // scanIdx（7.4.9.11）：4×4 块与 8×8 亮度块的模式相关扫描。
        let mut scan_idx = 0;
        if log2_size == 2 || (log2_size == 3 && c_idx == 0) {
            if (6..=14).contains(&pred_mode_intra) {
                scan_idx = 2;
            } else if (22..=30).contains(&pred_mode_intra) {
                scan_idx = 1;
            }
        }
        if scan_idx == 2 {
            std::mem::swap(&mut last_x, &mut last_y);
        }
        if last_x >= n || last_y >= n {
            return Err(HeifError::invalid("最后一个有效系数落在变换块外。"));
        }

        let log2_sb = log2_size - 2;
        let sb_width = 1 << log2_sb;
        let scans = &SCANS;
        let sub_scan = &scans[log2_sb as usize][scan_idx as usize];
        let scan4 = &scans[2][scan_idx as usize];

        // 定位最后一个子块与其中最后一个位置。
        let mut last_sub_block = sb_width * sb_width - 1;
        let last_sb_packed = (last_x >> 2) | ((last_y >> 2) << 4);
        while sub_scan[last_sub_block as usize] != last_sb_packed as u8 {
            last_sub_block -= 1;
        }
        let last_inner = (last_x & 3) | ((last_y & 3) << 4);
        let mut last_scan_pos = 15;
        while scan4[last_scan_pos as usize] != last_inner as u8 {
            last_scan_pos -= 1;
        }

        let sb_count = (sb_width * sb_width) as usize;
        let csbf = &mut self.csbf[..sb_count];
        csbf.fill(0);
        // 系数目标（行主 N×N）；与 csbf 同属本函数内 disjoint 字段借用。
        let coeffs = &mut self.coeffs[..(n * n) as usize];
        let sign_hiding_enabled = self.pps.sign_data_hiding_enabled && !bypass;
        let greater1_base = HevcCabacTables::Greater1 + if c_idx > 0 { 16 } else { 0 };
        let greater2_base = HevcCabacTables::Greater2 + if c_idx > 0 { 4 } else { 0 };
        let sig_base = HevcCabacTables::Sig + if c_idx > 0 { 27 } else { 0 };

        let mut sig_pos = [0i32; 16]; // 有效系数的扫描位置 n，降序
        let mut abs_level = [0i32; 16];
        let mut any_sub_block_processed = false;
        let mut greater1_ctx = 1usize;

        for i in (0..=last_sub_block).rev() {
            let x_s = (sub_scan[i as usize] & 15) as i32;
            let y_s = (sub_scan[i as usize] >> 4) as i32;
            let mut infer_sb_dc = false;
            let right = if x_s < sb_width - 1 {
                csbf[(y_s * sb_width + x_s + 1) as usize]
            } else {
                0
            };
            let below = if y_s < sb_width - 1 {
                csbf[((y_s + 1) * sb_width + x_s) as usize]
            } else {
                0
            };
            if i < last_sub_block && i > 0 {
                let ctx_inc = (right + below).min(1) + if c_idx > 0 { 2 } else { 0 };
                csbf[(y_s * sb_width + x_s) as usize] =
                    cabac.decode_bin(HevcCabacTables::CodedSubBlock + ctx_inc as usize)? as u8;
                infer_sb_dc = true;
            } else {
                csbf[(y_s * sb_width + x_s) as usize] = 1;
            }
            let coded = csbf[(y_s * sb_width + x_s) as usize] != 0;

            // sig_coeff_flag。
            let mut count = 0usize;
            let mut start_n = 15i32;
            if i == last_sub_block {
                sig_pos[count] = last_scan_pos;
                count += 1;
                start_n = last_scan_pos - 1;
            }
            if coded {
                let prev_csbf = right | (below << 1);
                for pos in (0..=start_n).rev() {
                    let x_p = (scan4[pos as usize] & 15) as i32;
                    let y_p = (scan4[pos as usize] >> 4) as i32;
                    if pos > 0 || !infer_sb_dc {
                        let x_c = (x_s << 2) + x_p;
                        let y_c = (y_s << 2) + y_p;
                        let mut sig_ctx: usize;
                        if log2_size == 2 {
                            sig_ctx = CTX_IDX_MAP[((y_c << 2) + x_c) as usize] as usize;
                        } else if x_c + y_c == 0 {
                            sig_ctx = 0;
                        } else {
                            sig_ctx = match prev_csbf {
                                0 => {
                                    if x_p + y_p == 0 {
                                        2
                                    } else if x_p + y_p < 3 {
                                        1
                                    } else {
                                        0
                                    }
                                }
                                1 => {
                                    if y_p == 0 {
                                        2
                                    } else if y_p == 1 {
                                        1
                                    } else {
                                        0
                                    }
                                }
                                2 => {
                                    if x_p == 0 {
                                        2
                                    } else if x_p == 1 {
                                        1
                                    } else {
                                        0
                                    }
                                }
                                _ => 2,
                            };
                            if c_idx == 0 {
                                if x_s + y_s > 0 {
                                    sig_ctx += 3;
                                }
                                sig_ctx += if log2_size == 3 {
                                    if scan_idx == 0 {
                                        9
                                    } else {
                                        15
                                    }
                                } else {
                                    21
                                };
                            } else {
                                sig_ctx += if log2_size == 3 { 9 } else { 12 };
                            }
                        }
                        if cabac.decode_bin(sig_base + sig_ctx)? == 1 {
                            sig_pos[count] = pos;
                            count += 1;
                            infer_sb_dc = false;
                        }
                    } else {
                        // 编码子块的其余 15 个旗标全 0 时的 DC：推断有效。
                        sig_pos[count] = 0;
                        count += 1;
                    }
                }
            }
            if count == 0 {
                continue;
            }

            // coeff_abs_level_greater1_flag（9.3.4.2.6）。
            let mut ctx_set = if i == 0 || c_idx > 0 { 0 } else { 2 };
            if any_sub_block_processed && greater1_ctx == 0 {
                ctx_set += 1;
            }
            any_sub_block_processed = true;
            greater1_ctx = 1;
            let mut first_g1 = -1i32;
            let g1_count = count.min(8);
            for k in 0..count {
                abs_level[k] = 1;
            }
            for k in 0..g1_count {
                let flag =
                    cabac.decode_bin(greater1_base + (ctx_set as usize) * 4 + greater1_ctx.min(3))?;
                if flag == 1 {
                    abs_level[k] = 2;
                    if first_g1 < 0 {
                        first_g1 = k as i32;
                    }
                    greater1_ctx = 0;
                } else if greater1_ctx > 0 {
                    greater1_ctx += 1;
                }
            }
            if first_g1 >= 0 && cabac.decode_bin(greater2_base + ctx_set as usize)? == 1 {
                abs_level[first_g1 as usize] = 3;
            }

            // 符号（旁路），带扫描序末位系数的符号隐藏。
            let last_sig_scan_pos = sig_pos[0];
            let first_sig_scan_pos = sig_pos[count - 1];
            let sign_hidden =
                sign_hiding_enabled && last_sig_scan_pos - first_sig_scan_pos > 3;
            let sign_bits = if sign_hidden { count - 1 } else { count };
            let mut signs: u32 = if sign_bits == 0 {
                0
            } else {
                cabac.decode_bypass_bits(sign_bits)? << (32 - sign_bits)
            };

            // coeff_abs_level_remaining。
            let mut num_sig_coeff = 0usize;
            let mut rice_param = 0usize;
            let mut sum_abs = 0i64;
            for k in 0..count {
                let base_level = abs_level[k];
                let threshold = if num_sig_coeff < 8 {
                    if k as i32 == first_g1 {
                        3
                    } else {
                        2
                    }
                } else {
                    1
                };
                let mut level = base_level;
                if base_level == threshold {
                    let remaining = decode_abs_level_remaining(cabac, rice_param)?;
                    level = (base_level as i64 + remaining).min(32767) as i32;
                    if level > 3 * (1 << rice_param) {
                        rice_param = (rice_param + 1).min(4);
                    }
                }
                num_sig_coeff += 1;

                let pos = sig_pos[k];
                let x_c = (x_s << 2) + (scan4[pos as usize] & 15) as i32;
                let y_c = (y_s << 2) + ((scan4[pos as usize] >> 4) as i32);
                let value: i32;
                if sign_hidden && k == count - 1 {
                    sum_abs += level as i64;
                    value = if (sum_abs & 1) != 0 { -level } else { level };
                } else {
                    sum_abs += level as i64;
                    value = if (signs & 0x8000_0000) != 0 { -level } else { level };
                    signs <<= 1;
                }
                let index = (y_c * n + x_c) as usize;
                let m = match factors {
                    Some(f) => f[index] as i32,
                    None => 16,
                };
                coeffs[index] = match &scaler {
                    Some(sc) => sc.scale(value, m),
                    None => value,
                };
                max_x = max_x.max(x_c);
                max_y = max_y.max(y_c);
            }
        }
        Ok((if transform_skip { 1 } else { 0 }) | (max_x << 8) | (max_y << 16))
    }
}

/// coeff_abs_level_remaining，Rice 参数 k（9.3.3.11）：TR 前缀（cMax 4 << k），后跟 EG(k+1)。
/// 纯 CABAC 消费者，取 `&mut HevcCabac`（不碰解码器状态）。
fn decode_abs_level_remaining(cabac: &mut HevcCabac, k: usize) -> HeifResult<i64> {
    let mut prefix = 0;
    while prefix < 4 && cabac.decode_bypass()? == 1 {
        prefix += 1;
    }
    if prefix < 4 {
        return Ok(((prefix << k) + if k > 0 { cabac.decode_bypass_bits(k)? as i32 } else { 0 })
            as i64);
    }
    // 后缀：k+1 阶 Exp-Golomb 取 (value - cMax)。
    let mut value: i64 = 0;
    let mut order = k + 1;
    while cabac.decode_bypass()? == 1 {
        value += 1i64 << order;
        order += 1;
        if order > 31 {
            return Err(HeifError::invalid("coeff_abs_level_remaining 转义码太长。"));
        }
    }
    value += cabac.decode_bypass_bits(order)? as i64;
    Ok((4i64 << k) + value)
}
