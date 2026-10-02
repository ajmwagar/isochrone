//! Narrow blocking audio-device interfaces.
//!
//! Blocking one-period reads and writes are deliberate for the first I/O
//! path: they let the hardware clock pace each endpoint without teaching the
//! transport about ALSA's internals. Callback APIs can implement the same
//! traits later without changing packet or playout policy.

use std::io;

pub trait Capture {
    fn read_period(&mut self, interleaved: &mut [f32]) -> io::Result<()>;
}

pub trait Playback {
    fn write_period(&mut self, interleaved: &[f32]) -> io::Result<()>;
}

#[cfg(any(target_os = "linux", test))]
fn sample_to_s32(sample: f32) -> i32 {
    (sample.clamp(-1.0, 1.0) * 2_147_483_647.0).round() as i32
}

#[cfg(any(target_os = "linux", test))]
fn s32_to_sample(sample: i32) -> f32 {
    sample as f32 / 2_147_483_648.0
}

#[cfg(target_os = "macos")]
pub mod coreaudio {
    use super::{Capture, Playback};
    use cpal::{
        BufferSize, SampleFormat, Stream, StreamConfig,
        traits::{DeviceTrait, HostTrait, StreamTrait},
    };
    use std::{
        collections::VecDeque,
        io,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
            mpsc,
        },
    };

    pub struct CoreAudioCapture {
        _stream: Stream,
        incoming: mpsc::Receiver<Vec<f32>>,
        pending: VecDeque<f32>,
        callback_error: Arc<Mutex<Option<String>>>,
        dropped_blocks: Arc<AtomicU64>,
        channel_peaks: Arc<Mutex<Vec<f32>>>,
    }

    impl CoreAudioCapture {
        pub fn open(device_name: &str, rate: u32, channels: [usize; 2]) -> io::Result<Self> {
            let host = cpal::default_host();
            let device = host
                .input_devices()
                .map_err(other)?
                .find(|device| device.to_string() == device_name)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("CoreAudio input device {device_name:?} was not found"),
                    )
                })?;

            let supported = device
                .supported_input_configs()
                .map_err(other)?
                .find(|config| {
                    config.sample_format() == SampleFormat::F32
                        && config.min_sample_rate() <= rate
                        && config.max_sample_rate() >= rate
                        && usize::from(config.channels()) > channels[0].max(channels[1])
                })
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        format!(
                            "{device_name:?} has no f32 input format at {rate}Hz containing channels {} and {}",
                            channels[0] + 1,
                            channels[1] + 1,
                        ),
                    )
                })?;
            let source_channels = usize::from(supported.channels());
            let config = StreamConfig {
                channels: supported.channels(),
                sample_rate: rate,
                buffer_size: BufferSize::Default,
            };
            let (blocks, incoming) = mpsc::sync_channel(64);
            let dropped_blocks = Arc::new(AtomicU64::new(0));
            let callback_error = Arc::new(Mutex::new(None));
            let channel_peaks = Arc::new(Mutex::new(vec![0.0_f32; source_channels]));
            let callback_drops = Arc::clone(&dropped_blocks);
            let stream_error = Arc::clone(&callback_error);
            let callback_peaks = Arc::clone(&channel_peaks);
            let stream = device
                .build_input_stream(
                    config,
                    move |input: &[f32], _| {
                        let mut stereo = Vec::with_capacity(input.len() / source_channels * 2);
                        let mut peaks = callback_peaks
                            .lock()
                            .expect("CoreAudio peak mutex poisoned");
                        for frame in input.chunks_exact(source_channels) {
                            for (peak, sample) in peaks.iter_mut().zip(frame) {
                                *peak = (*peak).max(sample.abs());
                            }
                            stereo.push(frame[channels[0]]);
                            stereo.push(frame[channels[1]]);
                        }
                        if blocks.try_send(stereo).is_err() {
                            callback_drops.fetch_add(1, Ordering::Relaxed);
                        }
                    },
                    move |error| {
                        *stream_error.lock().expect("CoreAudio error mutex poisoned") =
                            Some(error.to_string());
                    },
                    None,
                )
                .map_err(other)?;
            stream.play().map_err(other)?;
            Ok(Self {
                _stream: stream,
                incoming,
                pending: VecDeque::new(),
                callback_error,
                dropped_blocks,
                channel_peaks,
            })
        }

        #[must_use]
        pub fn dropped_blocks(&self) -> u64 {
            self.dropped_blocks.load(Ordering::Relaxed)
        }

        pub fn take_channel_peaks(&self) -> io::Result<Vec<f32>> {
            let mut peaks = self
                .channel_peaks
                .lock()
                .map_err(|_| io::Error::other("CoreAudio peak mutex poisoned"))?;
            Ok(peaks.iter_mut().map(|peak| std::mem::take(peak)).collect())
        }
    }

    impl Capture for CoreAudioCapture {
        fn read_period(&mut self, interleaved: &mut [f32]) -> io::Result<()> {
            while self.pending.len() < interleaved.len() {
                if let Some(error) = self
                    .callback_error
                    .lock()
                    .map_err(|_| io::Error::other("CoreAudio error mutex poisoned"))?
                    .take()
                {
                    return Err(io::Error::other(format!(
                        "CoreAudio input callback failed: {error}"
                    )));
                }
                self.pending.extend(self.incoming.recv().map_err(|_| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "CoreAudio input stream stopped")
                })?);
            }
            for sample in interleaved {
                *sample = self.pending.pop_front().expect("length checked above");
            }
            Ok(())
        }
    }

    pub struct CoreAudioPlayback {
        _stream: Stream,
        outgoing: mpsc::SyncSender<Vec<f32>>,
        callback_error: Arc<Mutex<Option<String>>>,
    }

    impl CoreAudioPlayback {
        pub fn open(
            device_name: &str,
            rate: u32,
            channels: usize,
            period_frames: usize,
        ) -> io::Result<Self> {
            eprintln!("isochrone: opening CoreAudio output {device_name:?}");
            let host = cpal::default_host();
            let device = host
                .output_devices()
                .map_err(other)?
                .find(|device| device.to_string() == device_name)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("CoreAudio output device {device_name:?} was not found"),
                    )
                })?;
            let supported = device
                .supported_output_configs()
                .map_err(other)?
                .find(|config| {
                    config.sample_format() == SampleFormat::F32
                        && config.min_sample_rate() <= rate
                        && config.max_sample_rate() >= rate
                        && usize::from(config.channels()) == channels
                })
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        format!("{device_name:?} has no {channels}-channel f32 output at {rate}Hz"),
                    )
                })?;
            eprintln!("isochrone: configuring CoreAudio output {device_name:?}");
            let config = StreamConfig {
                channels: supported.channels(),
                sample_rate: rate,
                buffer_size: BufferSize::Fixed(period_frames.try_into().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "CoreAudio period is too large")
                })?),
            };
            // Keep callback decoupling bounded; a deep queue is hidden output
            // latency and defeats the receiver's explicit jitter target.
            let (outgoing, blocks) = mpsc::sync_channel::<Vec<f32>>(8);
            let callback_error = Arc::new(Mutex::new(None));
            let stream_error = Arc::clone(&callback_error);
            let mut pending = VecDeque::new();
            let stream = device
                .build_output_stream(
                    config,
                    move |output: &mut [f32], _| {
                        while pending.len() < output.len() {
                            match blocks.try_recv() {
                                Ok(block) => pending.extend(block),
                                Err(_) => break,
                            }
                        }
                        for sample in output {
                            *sample = pending.pop_front().unwrap_or(0.0);
                        }
                    },
                    move |error| {
                        *stream_error.lock().expect("CoreAudio error mutex poisoned") =
                            Some(error.to_string());
                    },
                    None,
                )
                .map_err(other)?;
            eprintln!("isochrone: starting CoreAudio output {device_name:?}");
            stream.play().map_err(other)?;
            eprintln!("isochrone: CoreAudio output {device_name:?} ready");
            Ok(Self {
                _stream: stream,
                outgoing,
                callback_error,
            })
        }
    }

    impl Playback for CoreAudioPlayback {
        fn write_period(&mut self, interleaved: &[f32]) -> io::Result<()> {
            if let Some(error) = self
                .callback_error
                .lock()
                .map_err(|_| io::Error::other("CoreAudio error mutex poisoned"))?
                .take()
            {
                return Err(io::Error::other(format!(
                    "CoreAudio output callback failed: {error}"
                )));
            }
            self.outgoing.send(interleaved.to_vec()).map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "CoreAudio output stream stopped")
            })
        }
    }

    fn other(error: impl std::fmt::Display) -> io::Error {
        io::Error::other(error.to_string())
    }
}

#[cfg(target_os = "linux")]
pub mod alsa {
    use super::{Capture, Playback, s32_to_sample, sample_to_s32};
    use alsa::{
        Direction, ValueOr,
        pcm::{Access, Format, HwParams, PCM},
    };
    use std::io;

    fn configure(
        device: &str,
        direction: Direction,
        rate: u32,
        channels: u32,
        period_frames: usize,
    ) -> io::Result<PCM> {
        let pcm = PCM::new(device, direction, false).map_err(io::Error::other)?;
        let (actual_period, actual_buffer);
        {
            let params = HwParams::any(&pcm).map_err(io::Error::other)?;
            params
                .set_access(Access::RWInterleaved)
                .map_err(io::Error::other)?;
            // Both Scarlett endpoints expose 24 significant bits in an
            // S32_LE ALSA container, and the Pi's shared dmix contract is
            // S32_LE. Keep floats inside isochrone and convert only here.
            params.set_format(Format::s32()).map_err(io::Error::other)?;
            params
                .set_rate(rate, ValueOr::Nearest)
                .map_err(io::Error::other)?;
            params.set_channels(channels).map_err(io::Error::other)?;
            params
                .set_period_size(period_frames as i64, ValueOr::Nearest)
                .map_err(io::Error::other)?;
            params
                // Keep RTP at 1ms without requiring an ordinary userspace
                // thread to wake with real-time precision every millisecond.
                // The device buffer is scheduling headroom, distinct from
                // the network playout target governed by Receiver.
                .set_buffer_size(period_frames.saturating_mul(4).max(768) as i64)
                .map_err(io::Error::other)?;
            pcm.hw_params(&params).map_err(io::Error::other)?;
            actual_period = params.get_period_size().map_err(io::Error::other)?;
            actual_buffer = params.get_buffer_size().map_err(io::Error::other)?;
        }
        if direction == Direction::Playback {
            let params = pcm.sw_params_current().map_err(io::Error::other)?;
            params
                .set_avail_min(actual_period)
                .map_err(io::Error::other)?;
            params
                .set_start_threshold(actual_buffer.saturating_sub(actual_period))
                .map_err(io::Error::other)?;
            pcm.sw_params(&params).map_err(io::Error::other)?;
        }
        pcm.prepare().map_err(io::Error::other)?;
        Ok(pcm)
    }

    pub struct AlsaCapture {
        pcm: PCM,
        channels: usize,
        recoveries: u64,
        scratch: Vec<i32>,
    }

    impl AlsaCapture {
        pub fn open(
            device: &str,
            rate: u32,
            channels: usize,
            period_frames: usize,
        ) -> io::Result<Self> {
            Ok(Self {
                pcm: configure(
                    device,
                    Direction::Capture,
                    rate,
                    channels as u32,
                    period_frames,
                )?,
                channels,
                recoveries: 0,
                scratch: Vec::new(),
            })
        }

        #[must_use]
        pub fn recoveries(&self) -> u64 {
            self.recoveries
        }
    }

    impl Capture for AlsaCapture {
        fn read_period(&mut self, interleaved: &mut [f32]) -> io::Result<()> {
            let wanted = interleaved.len() / self.channels;
            self.scratch.resize(interleaved.len(), 0);
            let io = self.pcm.io_i32().map_err(io::Error::other)?;
            let mut frames = 0;
            while frames < wanted {
                match io.readi(&mut self.scratch[frames * self.channels..]) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "ALSA capture returned no frames",
                        ));
                    }
                    Ok(read) => frames += read,
                    Err(error) => {
                        self.pcm
                            .try_recover(error, false)
                            .map_err(io::Error::other)?;
                        self.recoveries += 1;
                    }
                }
            }
            for (destination, source) in interleaved.iter_mut().zip(&self.scratch) {
                *destination = s32_to_sample(*source);
            }
            Ok(())
        }
    }

    pub struct AlsaPlayback {
        pcm: PCM,
        channels: usize,
        recoveries: u64,
        scratch: Vec<i32>,
    }

    impl AlsaPlayback {
        pub fn open(
            device: &str,
            rate: u32,
            channels: usize,
            period_frames: usize,
        ) -> io::Result<Self> {
            Ok(Self {
                pcm: configure(
                    device,
                    Direction::Playback,
                    rate,
                    channels as u32,
                    period_frames,
                )?,
                channels,
                recoveries: 0,
                scratch: Vec::new(),
            })
        }

        #[must_use]
        pub fn recoveries(&self) -> u64 {
            self.recoveries
        }
    }

    impl Playback for AlsaPlayback {
        fn write_period(&mut self, interleaved: &[f32]) -> io::Result<()> {
            let wanted = interleaved.len() / self.channels;
            self.scratch.clear();
            self.scratch
                .extend(interleaved.iter().copied().map(sample_to_s32));
            let io = self.pcm.io_i32().map_err(io::Error::other)?;
            let mut frames = 0;
            while frames < wanted {
                match io.writei(&self.scratch[frames * self.channels..]) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "ALSA playback accepted no frames",
                        ));
                    }
                    Ok(written) => frames += written,
                    Err(error) => {
                        self.pcm
                            .try_recover(error, false)
                            .map_err(io::Error::other)?;
                        self.recoveries += 1;
                    }
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{s32_to_sample, sample_to_s32};

    #[test]
    fn alsa_s32_boundary_preserves_sign_and_clamps() {
        for sample in [-1.0_f32, -0.5, 0.0, 0.5, 1.0] {
            let back = s32_to_sample(sample_to_s32(sample));
            assert!((sample - back).abs() < 1e-6, "{sample} became {back}");
        }
        assert_eq!(sample_to_s32(2.0), i32::MAX);
        assert!(sample_to_s32(-2.0) < -2_147_483_000);
    }
}
