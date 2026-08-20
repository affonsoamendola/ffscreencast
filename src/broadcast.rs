use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use rtc::media::Sample;
use tokio::sync::broadcast;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;

use crate::audio::AudioSettings;
use crate::capture::Capture;
use crate::stats::{FrameTimer, StreamStats};

#[derive(Clone)]
pub struct VideoFrame {
    pub sample_data: Bytes,
    pub duration: Duration,
}

#[derive(Clone)]
pub struct AudioPacket {
    pub data: Vec<u8>,
    pub duration: Duration,
}

#[derive(Clone)]
pub struct Broadcaster {
    video_tx: broadcast::Sender<VideoFrame>,
    audio_tx: broadcast::Sender<AudioPacket>,
    running: Arc<AtomicBool>,
    subscriber_count: Arc<AtomicU32>,
    needs_keyframe: Arc<AtomicBool>,
}

impl Broadcaster {
    pub fn new() -> Self {
        let (video_tx, _) = broadcast::channel(4);
        let (audio_tx, _) = broadcast::channel(64);
        Self {
            video_tx,
            audio_tx,
            running: Arc::new(AtomicBool::new(false)),
            subscriber_count: Arc::new(AtomicU32::new(0)),
            needs_keyframe: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn ensure_running(
        &self,
        capture: Capture,
        fps: u32,
        stats: Arc<StreamStats>,
        audio_settings: AudioSettings,
    ) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let video_tx = self.video_tx.clone();
        let audio_tx = self.audio_tx.clone();
        let running = self.running.clone();
        let subscriber_count = self.subscriber_count.clone();
        let needs_keyframe = self.needs_keyframe.clone();

        tokio::spawn(async move {
            logln!("[broadcast] starting capture+encode loop");
            let r = run_broadcast(
                capture, fps, stats, audio_settings,
                video_tx, audio_tx, subscriber_count, needs_keyframe,
            ).await;
            if let Err(e) = r {
                logln!("[broadcast] loop error: {e}");
            }
            running.store(false, Ordering::SeqCst);
        });
    }

    pub fn add_subscriber(&self) {
        self.subscriber_count.fetch_add(1, Ordering::SeqCst);
        self.needs_keyframe.store(true, Ordering::SeqCst);
    }

    pub fn remove_subscriber(&self) {
        self.subscriber_count.fetch_sub(1, Ordering::SeqCst);
    }

    pub fn subscribe_video(&self) -> broadcast::Receiver<VideoFrame> {
        self.video_tx.subscribe()
    }

    pub fn subscribe_audio(&self) -> broadcast::Receiver<AudioPacket> {
        self.audio_tx.subscribe()
    }

    pub fn request_keyframe(&self) {
        self.needs_keyframe.store(true, Ordering::SeqCst);
    }
}

fn nals_to_sample_data(nals: &[Bytes]) -> Bytes {
    let mut data = Vec::new();
    for nal in nals {
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(nal);
    }
    Bytes::from(data)
}

async fn run_broadcast(
    capture: Capture,
    fps: u32,
    stats: Arc<StreamStats>,
    audio_settings: AudioSettings,
    video_tx: broadcast::Sender<VideoFrame>,
    audio_tx: broadcast::Sender<AudioPacket>,
    subscriber_count: Arc<AtomicU32>,
    needs_keyframe: Arc<AtomicBool>,
) -> Result<()> {
    let frame_duration = Duration::from_millis(1000 / fps as u64);
    let keyframe_interval = fps * 2;
    let mut timer = FrameTimer::new(stats);

    let t0 = Instant::now();
    let first_frame = loop {
        match tokio::task::spawn_blocking({
            let capture = capture.clone();
            move || capture.grab()
        })
        .await
        .map_err(|e| anyhow!("spawn error: {e}"))? {
            Ok(Some(f)) => break f,
            Ok(None) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            Err(e) => return Err(anyhow!("capture error: {e}")),
        }
    };
    let capture_dur = t0.elapsed();
    logln!("[broadcast] first frame: {}x{}, {} bytes, {:.1}ms",
        first_frame.width, first_frame.height, first_frame.data.len(),
        capture_dur.as_secs_f64() * 1000.0);

    let mut encoder = crate::nvenc_encoder::NvencH264Encoder::new(
        first_frame.width, first_frame.height, fps,
    )
    .map_err(|e| anyhow!("NVENC init: {e}"))?;
    logln!("[broadcast] encoder ready");

    let t1 = Instant::now();
    let nals = encoder
        .encode_keyframe(&first_frame)
        .map_err(|e| anyhow!("encode keyframe: {e}"))?;
    let encode_dur = t1.elapsed();
    let sample_data = nals_to_sample_data(&nals);
    logln!("[broadcast] first keyframe: {} NALs, {} bytes, {:.1}ms",
        nals.len(), sample_data.len(), encode_dur.as_secs_f64() * 1000.0);
    let _ = video_tx.send(VideoFrame {
        sample_data,
        duration: frame_duration,
    });

    let mut audio_capture = match crate::audio::AudioCapture::start(&audio_settings) {
        Ok(ac) => {
            logln!("[broadcast] audio capture started");
            Some(ac)
        }
        Err(e) => {
            logln!("[broadcast] audio capture failed: {e}");
            None
        }
    };

    let mut frame_count: u64 = 1;
    let mut next_frame_time = Instant::now();

    loop {
        if subscriber_count.load(Ordering::SeqCst) == 0 {
            logln!("[broadcast] no subscribers, stopping");
            break Ok(());
        }

        let frame = {
            let capture = capture.clone();
            let t_cap = Instant::now();
            let result = tokio::task::spawn_blocking(move || capture.grab()).await;
            let cap_dur = t_cap.elapsed();
            result.map(|r| r.map(|f| (f, cap_dur)))
        };

        match frame {
            Ok(Ok((Some(f), cap_dur))) => {
                frame_count += 1;
                let forced_keyframe = needs_keyframe.swap(false, Ordering::SeqCst);
                let is_keyframe = forced_keyframe || frame_count % keyframe_interval as u64 == 0;

                if f.width != encoder.width() || f.height != encoder.height() {
                    logln!("[broadcast] resolution changed {}x{} -> {}x{}, reinit",
                        encoder.width(), encoder.height(), f.width, f.height);
                    encoder = crate::nvenc_encoder::NvencH264Encoder::new(
                        f.width, f.height, fps,
                    )
                    .map_err(|e| anyhow!("NVENC reinit: {e}"))?;
                }

                let t_enc = Instant::now();
                let encode_result = if is_keyframe {
                    encoder.encode_keyframe(&f)
                } else {
                    encoder.encode(&f)
                };
                let enc_dur = t_enc.elapsed();

                match encode_result {
                    Ok(nals) => {
                        let sample_data = nals_to_sample_data(&nals);
                        let t_wr = Instant::now();
                        let _ = video_tx.send(VideoFrame {
                            sample_data: sample_data.clone(),
                            duration: frame_duration,
                        });
                        let wr_dur = t_wr.elapsed();
                        timer.record_frame(cap_dur, enc_dur, wr_dur, sample_data.len());
                        if frame_count % (fps as u64 * 5) == 0 {
                            logln!("[broadcast] frame {frame_count}: {}x{}, cap={:.1}ms enc={:.1}ms send={:.1}ms {}B {}rx{}",
                                f.width, f.height,
                                cap_dur.as_secs_f64() * 1000.0,
                                enc_dur.as_secs_f64() * 1000.0,
                                wr_dur.as_secs_f64() * 1000.0,
                                sample_data.len(),
                                video_tx.receiver_count(),
                                if forced_keyframe { " KEYFRAME" } else { "" });
                        }
                    }
                    Err(e) => {
                        logln!("[broadcast] encode error frame {frame_count}: {e}");
                    }
                }
            }
            Ok(Ok((None, _))) => {}
            Ok(Err(e)) => {
                logln!("[broadcast] capture error frame {frame_count}: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => {
                logln!("[broadcast] spawn error: {e}");
            }
        }

        if let Some(ref mut ac) = audio_capture {
            let rx = ac.rx();
            let rx_clone = rx.clone();
            match tokio::task::spawn_blocking(move || {
                rx_clone.lock().expect("audio mutex poisoned").try_recv()
            })
            .await
            {
                Ok(Ok(data)) => {
                    let _ = audio_tx.send(AudioPacket {
                        data,
                        duration: Duration::from_millis(20),
                    });
                }
                Ok(Err(_)) => {}
                Err(e) => {
                    logln!("[broadcast] audio task error: {e}");
                }
            }
        }

        next_frame_time += frame_duration;
        let now = Instant::now();
        if next_frame_time > now {
            tokio::time::sleep(next_frame_time - now).await;
        } else if now - next_frame_time > frame_duration {
            next_frame_time = now;
        }
    }
}

pub async fn forward_video(
    mut rx: broadcast::Receiver<VideoFrame>,
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    payload_type: u8,
    mut disconnect: tokio::sync::broadcast::Receiver<()>,
) {
    loop {
        tokio::select! {
            biased;
            _ = disconnect.recv() => {
                logln!("[broadcast] video forwarder: peer disconnected");
                break;
            }
            result = rx.recv() => {
                match result {
                    Ok(frame) => {
                        let sample = Sample {
                            data: frame.sample_data,
                            duration: frame.duration,
                            ..Default::default()
                        };
                        let writer = track.sample_writer(ssrc, payload_type);
                        if let Err(e) = writer.write_sample(&sample).await {
                            logln!("[broadcast] video write error: {e}");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        logln!("[broadcast] video lagged {n} frames");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

pub async fn forward_audio(
    mut rx: broadcast::Receiver<AudioPacket>,
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    payload_type: u8,
) {
    loop {
        match rx.recv().await {
            Ok(packet) => {
                let sample = Sample {
                    data: packet.data.into(),
                    duration: packet.duration,
                    ..Default::default()
                };
                let writer = track.sample_writer(ssrc, payload_type);
                if let Err(e) = writer.write_sample(&sample).await {
                    logln!("[broadcast] audio write error: {e}");
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}
