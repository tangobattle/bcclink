//! Audio output: an SDL3 playback stream pulling from the mgba thread.
//!
//! This is also what paces emulation — the mgba thread free-runs until the
//! sync high-water blocks it, and the high-water is advanced from this
//! stream's callbacks, so the game runs exactly as fast as the audio device
//! consumes samples. The resampling and high-water math follow mGBA's own
//! SDL frontend (via Tango's audio core, which this is a trimmed copy of).

use sdl3::audio::{AudioCallback, AudioFormat, AudioSpec, AudioStream, AudioStreamWithCallback};

pub const NUM_CHANNELS: usize = 2;
pub const SAMPLE_RATE: i32 = 48000;
pub const SAMPLES: usize = 512;

pub struct MgbaStream {
    handle: mgba::thread::Handle,
    resampler: mgba::audio::AudioResampler,
    dest_buffer: mgba::audio::AudioBuffer,
    dest_capacity: usize,
}

impl MgbaStream {
    pub fn new(handle: mgba::thread::Handle) -> Self {
        let dest_capacity = SAMPLES * 2;
        Self {
            handle,
            resampler: mgba::audio::AudioResampler::new(),
            dest_buffer: mgba::audio::AudioBuffer::new(dest_capacity, NUM_CHANNELS as u32),
            dest_capacity,
        }
    }

    fn fill(&mut self, buf: &mut [[i16; NUM_CHANNELS]]) -> usize {
        let frame_count = buf.len();
        let linear_buf: &mut [i16] = bytemuck::cast_slice_mut(buf);

        let mut audio_guard = self.handle.lock_audio();
        let mut fps_target = audio_guard.sync().fps_target();
        if fps_target <= 0.0 {
            fps_target = 1.0;
        }

        let (core_rate, faux_clock) = {
            let core = audio_guard.core_mut();
            (
                core.as_ref().audio_sample_rate() as f64,
                core.as_ref().calculate_framerate_ratio(fps_target as f64),
            )
        };

        let dest_rate = SAMPLE_RATE as f64 * faux_clock;
        let high_water = (frame_count as f64 + 16.0 + frame_count as f64 / 64.0) * core_rate / dest_rate;
        audio_guard.sync_mut().set_audio_high_water(high_water as u32);

        let needed = frame_count.saturating_mul(2);
        if needed > self.dest_capacity {
            let new_capacity = needed.next_power_of_two().max(SAMPLES * 2);
            self.dest_buffer = mgba::audio::AudioBuffer::new(new_capacity, NUM_CHANNELS as u32);
            self.dest_capacity = new_capacity;
        }

        let mut core = audio_guard.core_mut();
        let mut core_buffer = core.audio_buffer();
        self.resampler.set_source(&mut core_buffer, core_rate, true);
        self.resampler.set_destination(&mut self.dest_buffer, dest_rate);
        self.resampler.process();

        let available = self.dest_buffer.available().min(frame_count);
        self.dest_buffer
            .read(&mut linear_buf[..available * NUM_CHANNELS], available);
        available
    }
}

struct CallbackImpl {
    stream: MgbaStream,
    buf: Vec<[i16; NUM_CHANNELS]>,
}

impl AudioCallback<i16> for CallbackImpl {
    fn callback(&mut self, stream: &mut AudioStream, requested: i32) {
        let requested = requested.max(0) as usize;
        let frames = requested / NUM_CHANNELS;
        if frames == 0 {
            return;
        }
        if self.buf.len() < frames {
            self.buf.resize(frames, [0, 0]);
        }
        let filled = self.stream.fill(&mut self.buf[..frames]);
        for v in &mut self.buf[filled..frames] {
            *v = [0, 0];
        }
        let linear: &[i16] = bytemuck::cast_slice(&self.buf[..frames]);
        if let Err(e) = stream.put_data_i16(linear) {
            log::error!("sdl audio put_data: {e}");
        }
    }
}

pub struct Backend {
    _stream: AudioStreamWithCallback<CallbackImpl>,
}

impl Backend {
    pub fn new(sdl: &sdl3::Sdl, handle: mgba::thread::Handle) -> anyhow::Result<Self> {
        let spec = AudioSpec {
            freq: Some(SAMPLE_RATE),
            channels: Some(NUM_CHANNELS as i32),
            format: Some(AudioFormat::s16_sys()),
        };
        let callback = CallbackImpl {
            stream: MgbaStream::new(handle),
            buf: Vec::new(),
        };
        let audio = sdl.audio().map_err(|e| anyhow::anyhow!("sdl audio subsystem: {e}"))?;
        let stream = audio
            .open_playback_stream(&spec, callback)
            .map_err(|e| anyhow::anyhow!("sdl open_playback_stream: {e}"))?;
        stream.resume().map_err(|e| anyhow::anyhow!("sdl resume: {e}"))?;
        Ok(Self { _stream: stream })
    }
}
