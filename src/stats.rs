use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

pub static STATS_DIRTY: AtomicBool = AtomicBool::new(false);

pub struct StreamStats {
    pub fps: AtomicU32,
    pub capture_us: AtomicU32,
    pub encode_us: AtomicU32,
    pub write_us: AtomicU32,
    pub frame_bytes: AtomicU32,
    pub total_frames: AtomicU64,
}

impl StreamStats {
    pub fn new() -> Self {
        Self {
            fps: AtomicU32::new(0),
            capture_us: AtomicU32::new(0),
            encode_us: AtomicU32::new(0),
            write_us: AtomicU32::new(0),
            frame_bytes: AtomicU32::new(0),
            total_frames: AtomicU64::new(0),
        }
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            fps: self.fps.load(Ordering::Relaxed),
            capture_us: self.capture_us.load(Ordering::Relaxed),
            encode_us: self.encode_us.load(Ordering::Relaxed),
            write_us: self.write_us.load(Ordering::Relaxed),
            frame_bytes: self.frame_bytes.load(Ordering::Relaxed),
            total_frames: self.total_frames.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy)]
pub struct StatsSnapshot {
    pub fps: u32,
    pub capture_us: u32,
    pub encode_us: u32,
    pub write_us: u32,
    pub frame_bytes: u32,
    pub total_frames: u64,
}

pub struct FrameTimer {
    frame_count: u64,
    window_start: Instant,
    stats: Arc<StreamStats>,
}

impl FrameTimer {
    pub fn new(stats: Arc<StreamStats>) -> Self {
        Self {
            frame_count: 0,
            window_start: Instant::now(),
            stats,
        }
    }

    pub fn record_frame(
        &mut self,
        capture_dur: std::time::Duration,
        encode_dur: std::time::Duration,
        write_dur: std::time::Duration,
        frame_bytes: usize,
    ) {
        self.frame_count += 1;
        self.stats.capture_us.store(capture_dur.as_micros() as u32, Ordering::Relaxed);
        self.stats.encode_us.store(encode_dur.as_micros() as u32, Ordering::Relaxed);
        self.stats.write_us.store(write_dur.as_micros() as u32, Ordering::Relaxed);
        self.stats.frame_bytes.store(frame_bytes as u32, Ordering::Relaxed);
        self.stats.total_frames.fetch_add(1, Ordering::Relaxed);

        let elapsed = self.window_start.elapsed();
        if elapsed.as_secs() >= 1 {
            let fps = (self.frame_count as f64 / elapsed.as_secs_f64()) as u32;
            self.stats.fps.store(fps, Ordering::Relaxed);
            self.frame_count = 0;
            self.window_start = Instant::now();
            STATS_DIRTY.store(true, Ordering::Relaxed);
        }
    }
}
