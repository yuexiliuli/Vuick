//! 音频输出：CPAL 设备流 + 共享环形缓冲 + 播放位置。
//!
//! 设计：解码线程把 **F32 交错立体声** 样本 `push` 进有界环形缓冲；CPAL 输出回调
//! 按设备节奏 `pop` 样本送扬声器，并累加已播放帧数（[`AudioSink::position`] 即
//! 音频时钟，供视频 A/V 对齐）。缓冲耗尽时回填静音（设备节奏不被解码卡顿拖垮）。

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use anyhow::{Context as _, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, I24, U24};

/// 环形缓冲上限（样本数）：约 1 秒 × 声道，防止解码线程提前跑完整个文件塞爆内存。
const QUEUE_MAX_SAMPLES: usize = 48_000 * 2;

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
    /// 输出声道数。
    #[allow(dead_code)] // M4 音频时钟/多声道接入后启用
    channels: u32,
}

impl AudioSink {
    /// 打开默认输出设备并建立立体声输出流；设备不可用 / 采样格式不支持时返回错误
    /// （调用方将退化为纯画面 + 墙钟播放）。
    pub fn open() -> Result<Self> {
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

        let queue = Arc::new(Mutex::new(VecDeque::<f32>::new()));
        let played = Arc::new(AtomicU64::new(0));
        let err_fn = |e: cpal::Error| eprintln!("[video] 音频输出流错误：{e}");

        let format = supported.sample_format();
        let stream: cpal::Stream = match format {
            SampleFormat::F32 => build::<f32>(&device, config, &queue, &played, err_fn)?,
            SampleFormat::F64 => build::<f64>(&device, config, &queue, &played, err_fn)?,
            SampleFormat::I8 => build::<i8>(&device, config, &queue, &played, err_fn)?,
            SampleFormat::I16 => build::<i16>(&device, config, &queue, &played, err_fn)?,
            SampleFormat::I24 => build::<I24>(&device, config, &queue, &played, err_fn)?,
            SampleFormat::I32 => build::<i32>(&device, config, &queue, &played, err_fn)?,
            SampleFormat::I64 => build::<i64>(&device, config, &queue, &played, err_fn)?,
            SampleFormat::U8 => build::<u8>(&device, config, &queue, &played, err_fn)?,
            SampleFormat::U16 => build::<u16>(&device, config, &queue, &played, err_fn)?,
            SampleFormat::U24 => build::<U24>(&device, config, &queue, &played, err_fn)?,
            SampleFormat::U32 => build::<u32>(&device, config, &queue, &played, err_fn)?,
            SampleFormat::U64 => build::<u64>(&device, config, &queue, &played, err_fn)?,
            other => return Err(anyhow!("不支持的输出采样格式：{other:?}")),
        };

        Ok(Self {
            inner: Arc::new(SinkInner {
                stream: Mutex::new(stream),
                queue,
                played_frames: played,
                sample_rate,
                channels: 2,
            }),
        })
    }

    /// 输出采样率（解码线程据此重采样）。
    pub fn sample_rate(&self) -> u32 {
        self.inner.sample_rate
    }

    /// 输出声道数。
    #[allow(dead_code)] // M4 音频时钟/多声道接入后启用
    pub fn channels(&self) -> u32 {
        self.inner.channels
    }

    /// 播放位置（秒）= 已播帧数 / 采样率；seek 后 `flush` 归零。
    #[allow(dead_code)] // M4 音频时钟接入后启用（当前墙钟播放）
    pub fn position(&self) -> f64 {
        self.inner.played_frames.load(Ordering::Relaxed) as f64
            / f64::from(self.inner.sample_rate)
    }

    /// 待播放样本量（解码线程回压依据）。
    pub fn queued_len(&self) -> usize {
        self.inner.queue.lock().unwrap().len()
    }

    /// 解码线程入队一块 F32 交错样本（有界：超出上限丢弃最旧，保实时）。
    pub fn push(&self, samples: &[f32]) {
        let mut q = self.inner.queue.lock().unwrap();
        if q.len() + samples.len() > QUEUE_MAX_SAMPLES {
            let drop = q.len() + samples.len() - QUEUE_MAX_SAMPLES;
            for _ in 0..drop {
                q.pop_front();
            }
        }
        q.extend(samples);
    }

    /// seek 时清空缓冲 + 播放位置归零。
    pub fn flush(&self) {
        self.inner.queue.lock().unwrap().clear();
        self.inner.played_frames.store(0, Ordering::Relaxed);
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

/// 按目标采样格式建输出流（回调统一从 F32 队列取值并转目标格式）。
fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    queue: &Arc<Mutex<VecDeque<f32>>>,
    played: &Arc<AtomicU64>,
    err_fn: impl Fn(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32> + Copy,
{
    let q = queue.clone();
    let p = played.clone();
    let channels = config.channels as usize;
    let stream = device.build_output_stream(
        config,
        move |output: &mut [T], _: &cpal::OutputCallbackInfo| {
            pump::<T>(output, &q, &p, channels);
        },
        err_fn,
        None,
    )?;
    Ok(stream)
}

/// CPAL 输出回调：从队列 pop 样本送到设备（耗尽回填静音），并累加已播帧数。
/// 用 `try_lock` 避免在实时回调线程上阻塞；拿不到锁时输出静音、不计数。
fn pump<T>(data: &mut [T], queue: &Arc<Mutex<VecDeque<f32>>>, played: &Arc<AtomicU64>, channels: usize)
where
    T: SizedSample + FromSample<f32> + Copy,
{
    let frame_count = data.len() / channels;
    let Ok(mut q) = queue.try_lock() else {
        return; // 主线程持锁中：本拍静音并跳过计数
    };
    for slot in data {
        let v = q.pop_front().unwrap_or(0.0_f32);
        *slot = T::from_sample(v);
    }
    played.fetch_add(frame_count as u64, Ordering::Relaxed);
}
