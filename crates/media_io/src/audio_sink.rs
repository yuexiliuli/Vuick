//! 共享音频输出 sink：CPAL 设备流 + 共享环形缓冲 + 播放位置。
//!
//! 同 music::sink / video::audio 的 AudioSink 设计合并（取含 `dropped_samples` 的统一版），
//! 本 crate 为 video / music 消费方共享复用（本批只沉淀组件、不迁消费方）：解码线程把
//! **F32 交错立体声** 样本 `push` 进有界环形缓冲；CPAL 输出回调按设备节奏 `pop` 样本送
//! 扬声器，并累加已播放帧数（[`AudioSink::position`] 即音频时钟）。缓冲耗尽时回填静音
//! （设备节奏不被解码卡顿拖垮）。`err` 标签作构造参数（传入 "[music]" / "[video]" 前缀，
//! 用于错误回调日志区分消费方）。

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use anyhow::{Context as _, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, I24, U24};

/// 环形缓冲上限（样本数）：约 2 秒 × 声道。**必须显著大于解码线程的回压阈值
///（1s）**——否则队列一满 backpressure 还没触发，`push` 就先丢最旧样本 = 持续
/// 丢音频（卡顿/碎裂声）。
const QUEUE_MAX_SAMPLES: usize = 48_000 * 2 * 2;

/// 音频输出流句柄（可跨线程引用内部缓冲/计数）。
#[derive(Clone)]
pub struct AudioSink {
    inner: Arc<SinkInner>,
}

struct SinkInner {
    stream: Mutex<cpal::Stream>,
    /// 待播放的 F32 交错样本（解码线程 producer，CPAL 回调 consumer）。
    queue: Arc<Mutex<VecDeque<f32>>>,
    /// 已交给设备播放的**帧数**（= 样本数 / 声道）。
    played_frames: Arc<AtomicU64>,
    sample_rate: u32,
    /// 音量（0..=1 的 f32 位模式；静音=0）。回调线程无锁直读。
    volume: Arc<AtomicU32>,
    /// 回调累计次数。
    cb_calls: Arc<AtomicU64>,
    /// 播放/丢弃健康：设备侧欠载样本数（回调时队列已空，填了静音）。
    cb_underrun: Arc<AtomicU64>,
    /// 设备侧锁竞争失败次数（整块静音）。
    cb_lockfail: Arc<AtomicU64>,
    /// **生产者侧**丢弃样本数（`push` 超过队列上限从队头弹出）。解码过快 + 回压不守
    /// 阈值时此值飙升 = 内容前跳（快进）+ 硬切杂音；正常回压下应恒 0。
    dropped_samples: Arc<AtomicU64>,
}

impl AudioSink {
    /// 打开默认输出设备并建立立体声输出流；设备不可用 / 采样格式不支持时返回错误
    /// （调用方将退化为纯状态播放）。默认 `err` 标签前缀 `[media]`。
    pub fn open() -> Result<Self> {
        Self::open_with_tag("[media]")
    }

    /// 同 [`Self::open`]，`tag` 用作错误回调日志前缀（如 "[music]" / "[video]"），
    /// 供多消费方共用时区分日志出处。
    pub fn open_with_tag(tag: impl Into<String>) -> Result<Self> {
        let tag = tag.into();
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| anyhow!("无可用音频输出设备"))?;
        let supported = device
            .default_output_config()
            .context("读取输出配置失败")?;
        let sample_rate = supported.sample_rate();
        let mut config: cpal::StreamConfig = supported.into();
        config.channels = 2; // 与解码输出（立体声交错）一致；绝大多数设备支持 2ch
        // 显式给足设备缓冲（约 2048 帧 ≈ 40ms+）：默认值在某些 WASAPI 端点上极小，
        // 一旦欠载即整块静音 → 滋滋/碎裂
        config.buffer_size = cpal::BufferSize::Fixed(2048);

        let queue = Arc::new(Mutex::new(VecDeque::<f32>::new()));
        let played = Arc::new(AtomicU64::new(0));
        let volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let cb_calls = Arc::new(AtomicU64::new(0));
        let cb_underrun = Arc::new(AtomicU64::new(0));
        let cb_lockfail = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicU64::new(0));
        let err_fn = move |e: cpal::Error| eprintln!("{tag} 音频输出流错误：{e}");

        let format = supported.sample_format();
        let stream: cpal::Stream = match format {
            SampleFormat::F32 => build::<f32>(&device, config, &queue, &played, &volume, &cb_calls, &cb_underrun, &cb_lockfail, err_fn)?,
            SampleFormat::F64 => build::<f64>(&device, config, &queue, &played, &volume, &cb_calls, &cb_underrun, &cb_lockfail, err_fn)?,
            SampleFormat::I8 => build::<i8>(&device, config, &queue, &played, &volume, &cb_calls, &cb_underrun, &cb_lockfail, err_fn)?,
            SampleFormat::I16 => build::<i16>(&device, config, &queue, &played, &volume, &cb_calls, &cb_underrun, &cb_lockfail, err_fn)?,
            SampleFormat::I24 => build::<I24>(&device, config, &queue, &played, &volume, &cb_calls, &cb_underrun, &cb_lockfail, err_fn)?,
            SampleFormat::I32 => build::<i32>(&device, config, &queue, &played, &volume, &cb_calls, &cb_underrun, &cb_lockfail, err_fn)?,
            SampleFormat::I64 => build::<i64>(&device, config, &queue, &played, &volume, &cb_calls, &cb_underrun, &cb_lockfail, err_fn)?,
            SampleFormat::U8 => build::<u8>(&device, config, &queue, &played, &volume, &cb_calls, &cb_underrun, &cb_lockfail, err_fn)?,
            SampleFormat::U16 => build::<u16>(&device, config, &queue, &played, &volume, &cb_calls, &cb_underrun, &cb_lockfail, err_fn)?,
            SampleFormat::U24 => build::<U24>(&device, config, &queue, &played, &volume, &cb_calls, &cb_underrun, &cb_lockfail, err_fn)?,
            SampleFormat::U32 => build::<u32>(&device, config, &queue, &played, &volume, &cb_calls, &cb_underrun, &cb_lockfail, err_fn)?,
            SampleFormat::U64 => build::<u64>(&device, config, &queue, &played, &volume, &cb_calls, &cb_underrun, &cb_lockfail, err_fn)?,
            other => return Err(anyhow!("不支持的输出采样格式：{other:?}")),
        };

        Ok(Self {
            inner: Arc::new(SinkInner {
                stream: Mutex::new(stream),
                queue,
                played_frames: played,
                sample_rate,
                volume,
                cb_calls,
                cb_underrun,
                cb_lockfail,
                dropped_samples: dropped,
            }),
        })
    }

    /// 输出采样率（解码线程据此重采样）。
    pub fn sample_rate(&self) -> u32 {
        self.inner.sample_rate
    }

    /// 播放位置（秒）= 已播帧数 / 采样率；seek 后 `flush` 归零。
    pub fn position(&self) -> f64 {
        self.inner.played_frames.load(Ordering::Relaxed) as f64
            / f64::from(self.inner.sample_rate)
    }

    /// 设置音量（0..=1；0 = 静音）。实时作用于回调：下一拍起生效，无需重建流。
    /// 静音/恢复走同一入口（`0.0` / 原音量），不额外维护布尔态。
    pub fn set_volume(&self, volume: f32) {
        let v = clamp_volume(volume);
        self.inner.volume.store(v.to_bits(), Ordering::Relaxed);
    }

    /// 待播放样本量（解码线程回压依据）。
    pub fn queued_len(&self) -> usize {
        self.inner.queue.lock().unwrap().len()
    }

    /// 解码线程入队一块 F32 交错样本（有界：超出上限丢弃最旧，保实时）。
    /// `dropped_samples` 计数队头丢弃量——**正常回压（worker 阻塞等队列下降）下恒 0**；
    /// 解码过快且不守回压时飙升（内容前跳 = 快进 + 硬切杂音，设备侧 underrun 数不到）。
    pub fn push(&self, samples: &[f32]) {
        push_samples(&self.inner.queue, &self.inner.dropped_samples, samples);
    }

    /// seek 时清空缓冲 + 播放位置归零。
    pub fn flush(&self) {
        self.inner.queue.lock().unwrap().clear();
        self.inner.played_frames.store(0, Ordering::Relaxed);
    }

    /// 播放进度/健康统计：`(回调次数, 欠载样本, 锁失败次数, 已播帧数, 生产者丢弃样本)`。
    /// 统一 5 元 stats（dropped_samples 纳入；video/music 消费方共用同一口径）。
    #[allow(dead_code)] // 调试/取证用（消费方未全部消费）
    pub fn stats(&self) -> (u64, u64, u64, u64, u64) {
        let i = &self.inner;
        (
            i.cb_calls.load(Ordering::Relaxed),
            i.cb_underrun.load(Ordering::Relaxed),
            i.cb_lockfail.load(Ordering::Relaxed),
            i.played_frames.load(Ordering::Relaxed),
            i.dropped_samples.load(Ordering::Relaxed),
        )
    }

    pub fn play(&self) -> Result<()> {
        let s = self.inner.stream.lock().unwrap();
        s.play().context("音频输出 play 失败")
    }

    pub fn pause(&self) {
        let s = self.inner.stream.lock().unwrap();
        let _ = s.pause();
    }
}

/// 音量收敛到 `[0, 1]`（0 = 静音、1 = 原音量；超界收敛）。**可纯测**（`set_volume`
/// 内部经此钳制后入原子）。
fn clamp_volume(volume: f32) -> f32 {
    volume.clamp(0.0, 1.0)
}

/// 解码线程入队一块 F32 交错样本（有界：超出上限丢弃最旧，保实时）。**抽取为纯函数**
/// 供 `AudioSink::push` 与守护测试共用；`dropped` 计数队头丢弃量（语义逐字同
/// music::sink `push`）。
fn push_samples(
    queue: &Arc<Mutex<VecDeque<f32>>>,
    dropped: &Arc<AtomicU64>,
    samples: &[f32],
) {
    let mut q = queue.lock().unwrap();
    if q.len() + samples.len() > QUEUE_MAX_SAMPLES {
        let drop = q.len() + samples.len() - QUEUE_MAX_SAMPLES;
        for _ in 0..drop {
            q.pop_front();
        }
        dropped.fetch_add(drop as u64, Ordering::Relaxed);
    }
    q.extend(samples);
}

/// 按目标采样格式建输出流（回调统一从 F32 队列取值并转目标格式）。
/// `build` 臂无法无设备单测：须真实 `cpal::Device` 才能建流——12 采样格式分支逐字
/// 照抄 music::sink / video::audio（headless 无设备，留真机路径）。
#[allow(clippy::too_many_arguments)]
fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    queue: &Arc<Mutex<VecDeque<f32>>>,
    played: &Arc<AtomicU64>,
    volume: &Arc<AtomicU32>,
    cb_calls: &Arc<AtomicU64>,
    cb_underrun: &Arc<AtomicU64>,
    cb_lockfail: &Arc<AtomicU64>,
    err_fn: impl Fn(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32> + Copy,
{
    let q = queue.clone();
    let p = played.clone();
    let v = volume.clone();
    let cc = cb_calls.clone();
    let cu = cb_underrun.clone();
    let cl = cb_lockfail.clone();
    let channels = config.channels as usize;
    let stream = device.build_output_stream(
        config,
        move |output: &mut [T], _: &cpal::OutputCallbackInfo| {
            pump::<T>(output, &q, &p, &v, &cc, &cu, &cl, channels);
        },
        err_fn,
        None,
    )?;
    Ok(stream)
}

/// CPAL 输出回调：从队列 pop 样本送到设备（耗尽回填静音），并累加已播帧数与
/// 健康计数。用 `try_lock` 避免在实时回调线程上阻塞；**拿不到锁时整块填静音**。
/// 音量在回调内统一应用（`volume` 无锁原子读，非 0/1 时逐样本乘）。
#[allow(clippy::too_many_arguments)]
fn pump<T>(
    data: &mut [T],
    queue: &Arc<Mutex<VecDeque<f32>>>,
    played: &Arc<AtomicU64>,
    volume: &Arc<AtomicU32>,
    cb_calls: &Arc<AtomicU64>,
    cb_underrun: &Arc<AtomicU64>,
    cb_lockfail: &Arc<AtomicU64>,
    channels: usize,
) where
    T: cpal::Sample + SizedSample + FromSample<f32> + Copy,
{
    let frame_count = data.len() / channels;
    cb_calls.fetch_add(1, Ordering::Relaxed);
    let vol = f32::from_bits(volume.load(Ordering::Relaxed));
    let Ok(mut q) = queue.try_lock() else {
        // 竞态（主线程推送/清空持锁短暂窗口）：本拍静音，不计数
        for slot in data.iter_mut() {
            *slot = T::EQUILIBRIUM;
        }
        cb_lockfail.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let mut underrun = 0u64;
    for slot in data.iter_mut() {
        match q.pop_front() {
            Some(v) => *slot = T::from_sample(v * vol),
            None => {
                *slot = T::from_sample(0.0);
                underrun += 1;
            }
        }
    }
    if underrun > 0 {
        cb_underrun.fetch_add(underrun, Ordering::Relaxed);
    }
    played.fetch_add(frame_count as u64, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 欠载填静音 + 健康计数（**无设备**，直接驱动 `pump::<f32>` 纯函数）：
    /// 队列仅有 2 样本、3 帧×2 声道 → 后 4 槽补静音，underrun=4、played=3 帧。
    #[test]
    fn pump_fills_silence_on_underrun_and_counts() {
        let queue = Arc::new(Mutex::new(VecDeque::<f32>::new()));
        queue.lock().unwrap().extend([0.5, 0.25]); // 2 样本 = 1 帧（2 声道）
        let played = Arc::new(AtomicU64::new(0));
        let volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let cb_calls = Arc::new(AtomicU64::new(0));
        let cb_underrun = Arc::new(AtomicU64::new(0));
        let cb_lockfail = Arc::new(AtomicU64::new(0));

        let mut data = [0f32; 6]; // 3 帧 × 2 声道
        pump(&mut data, &queue, &played, &volume, &cb_calls, &cb_underrun, &cb_lockfail, 2);

        assert_eq!(data, [0.5, 0.25, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(cb_calls.load(Ordering::Relaxed), 1);
        assert_eq!(cb_underrun.load(Ordering::Relaxed), 4);
        assert_eq!(cb_lockfail.load(Ordering::Relaxed), 0);
        assert_eq!(played.load(Ordering::Relaxed), 3);
    }

    /// 音量统一应用：`volume=0.5` 时逐样本乘（回调内 `v * vol` 语义）。
    #[test]
    fn pump_applies_volume_per_sample() {
        let queue = Arc::new(Mutex::new(VecDeque::<f32>::new()));
        queue.lock().unwrap().extend([0.8f32, 0.4]);
        let played = Arc::new(AtomicU64::new(0));
        let volume = Arc::new(AtomicU32::new(0.5f32.to_bits()));
        let cb_calls = Arc::new(AtomicU64::new(0));
        let cb_underrun = Arc::new(AtomicU64::new(0));
        let cb_lockfail = Arc::new(AtomicU64::new(0));

        let mut data = [0f32; 2];
        pump(&mut data, &queue, &played, &volume, &cb_calls, &cb_underrun, &cb_lockfail, 2);

        let expect = [0.8f32 * 0.5, 0.4 * 0.5];
        for (got, exp) in data.iter().zip(expect) {
            assert!((got - exp).abs() < 1e-6, "音量乘应用失败：{got} != {exp}");
        }
        assert_eq!(played.load(Ordering::Relaxed), 1);
    }

    /// 锁失败路径：主线程持锁（模拟 push/flush 短暂窗口）→ 整块 `EQUILIBRIUM` 静音、
    /// lockfail +1 且**不**累加已播帧数。
    #[test]
    fn pump_fills_silence_on_lock_fail_and_counts() {
        let queue = Arc::new(Mutex::new(VecDeque::<f32>::new()));
        let _guard = queue.lock().unwrap(); // 模拟主线程持锁窗口
        let played = Arc::new(AtomicU64::new(42));
        let volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let cb_calls = Arc::new(AtomicU64::new(0));
        let cb_underrun = Arc::new(AtomicU64::new(0));
        let cb_lockfail = Arc::new(AtomicU64::new(0));

        let mut data = [1f32; 4];
        pump(&mut data, &queue, &played, &volume, &cb_calls, &cb_underrun, &cb_lockfail, 2);

        assert_eq!(data, [0.0; 4]);
        assert_eq!(cb_calls.load(Ordering::Relaxed), 1);
        assert_eq!(cb_lockfail.load(Ordering::Relaxed), 1);
        assert_eq!(cb_underrun.load(Ordering::Relaxed), 0);
        assert_eq!(played.load(Ordering::Relaxed), 42); // 锁失败不累加帧数
    }

    /// push 有界队列：超出上限丢最旧 + `dropped_samples` 计数（纯函数 `push_samples`）。
    #[test]
    fn push_samples_drops_oldest_on_overflow_and_counts() {
        let queue = Arc::new(Mutex::new(VecDeque::<f32>::new()));
        let dropped = Arc::new(AtomicU64::new(0));

        // 先填满上限（队头标记 7.0 为最旧样本）
        let mut big = vec![1.0f32; QUEUE_MAX_SAMPLES];
        big[0] = 7.0;
        push_samples(&queue, &dropped, &big);
        assert_eq!(queue.lock().unwrap().len(), QUEUE_MAX_SAMPLES);
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        assert_eq!(*queue.lock().unwrap().front().unwrap(), 7.0);

        // 再推 1 个 → 丢 1 个最旧（7.0 弹出）、队尾入 2.0
        push_samples(&queue, &dropped, &[2.0]);
        let q = queue.lock().unwrap();
        assert_eq!(q.len(), QUEUE_MAX_SAMPLES);
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        assert_eq!(*q.front().unwrap(), 1.0);
        assert_eq!(*q.back().unwrap(), 2.0);
    }

    /// 音量钳制 `[0,1]`（`set_volume` 内部语义；超界收敛、界内原值）。
    #[test]
    fn clamp_volume_bounds() {
        assert_eq!(clamp_volume(-0.5), 0.0);
        assert_eq!(clamp_volume(3.0), 1.0);
        assert_eq!(clamp_volume(0.7), 0.7);
    }
}
