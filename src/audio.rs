use std::collections::VecDeque;
use std::sync::{mpsc, Arc, Mutex};

use anyhow::Result;
use opus_rs::{Application, OpusEncoder};
use wasapi::{DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

const SAMPLE_RATE: i32 = 48000;
const CHANNELS: usize = 2;
const FRAME_SIZE: usize = 960;
const OPUS_MAX_PACKET: usize = 4000;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AudioDevice {
    pub name: String,
    pub id: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum AudioSourceType {
    SystemAudio,
    Microphone,
}

impl AudioSourceType {
    pub fn label(&self) -> &'static str {
        match self {
            AudioSourceType::SystemAudio => "System audio (loopback)",
            AudioSourceType::Microphone => "Microphone",
        }
    }

    pub fn direction(&self) -> Direction {
        match self {
            AudioSourceType::SystemAudio => Direction::Render,
            AudioSourceType::Microphone => Direction::Capture,
        }
    }
}

impl Default for AudioSourceType {
    fn default() -> Self {
        AudioSourceType::SystemAudio
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AudioSettings {
    pub enabled: bool,
    pub source_type: AudioSourceType,
    pub device_id: Option<String>,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            source_type: AudioSourceType::default(),
            device_id: None,
        }
    }
}

pub fn list_audio_devices(direction: &Direction) -> Vec<AudioDevice> {
    let enumerator = match DeviceEnumerator::new() {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let collection = match enumerator.get_device_collection(direction) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut devices = Vec::new();
    let count = match collection.get_nbr_devices() {
        Ok(n) => n,
        Err(_) => return Vec::new(),
    };
    for i in 0..count {
        if let Ok(device) = collection.get_device_at_index(i) {
            let name = device.get_friendlyname().unwrap_or_default();
            let id = device.get_id().unwrap_or_default();
            if !name.is_empty() {
                devices.push(AudioDevice { name, id });
            }
        }
    }
    devices
}

pub struct AudioCapture {
    rx: Arc<Mutex<mpsc::Receiver<Vec<u8>>>>,
    stop_tx: Option<mpsc::Sender<()>>,
    _thread: Option<std::thread::JoinHandle<()>>,
}

impl AudioCapture {
    pub fn start(settings: &AudioSettings) -> Result<Self> {
        logln!("[audio] starting audio capture (enabled={})", settings.enabled);
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(64);
        let (stop_tx, stop_rx) = mpsc::channel::<()>();

        let settings = settings.clone();

        let thread = std::thread::Builder::new()
            .name("ffscreencast-audio".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    capture_loop(tx, stop_rx, &settings)
                }));
                match result {
                    Ok(Ok(())) => {
                        logln!("[audio] capture loop exited normally");
                    }
                    Ok(Err(e)) => {
                        logln!("[audio] capture error: {e}");
                    }
                    Err(panic) => {
                        let msg = if let Some(s) = panic.downcast_ref::<&str>() {
                            s.to_string()
                        } else if let Some(s) = panic.downcast_ref::<String>() {
                            s.clone()
                        } else {
                            "unknown panic".to_string()
                        };
                        logln!("[audio] PANIC in audio thread: {msg}");
                    }
                }
            })?;

        logln!("[audio] audio capture thread started");
        Ok(Self {
            rx: Arc::new(Mutex::new(rx)),
            stop_tx: Some(stop_tx),
            _thread: Some(thread),
        })
    }

    pub fn rx(&self) -> Arc<Mutex<mpsc::Receiver<Vec<u8>>>> {
        self.rx.clone()
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
    }
}

fn capture_loop(
    tx: mpsc::SyncSender<Vec<u8>>,
    stop_rx: mpsc::Receiver<()>,
    settings: &AudioSettings,
) -> Result<()> {
    if !settings.enabled {
        logln!("[audio] audio disabled, capture loop exiting");
        return Ok(());
    }

    logln!(
        "[audio] capture_loop starting (source={:?}, device={:?}), initializing COM...",
        settings.source_type,
        settings.device_id,
    );
    unsafe {
        windows_sys::Win32::System::Com::CoInitializeEx(
            std::ptr::null_mut(),
            windows_sys::Win32::System::Com::COINIT_MULTITHREADED as u32,
        );
    }

    let _ = wasapi::initialize_mta();

    logln!("[audio] COM initialized, getting device enumerator...");
    let enumerator = DeviceEnumerator::new()?;
    let direction = settings.source_type.direction();

    let device = if let Some(ref device_id) = settings.device_id {
        logln!("[audio] using specified device: {device_id}");
        enumerator.get_device(device_id)?
    } else {
        logln!("[audio] using default {:?} device", settings.source_type);
        enumerator.get_default_device(&direction)?
    };

    logln!("[audio] got device, getting audio client...");
    let mut audio_client = device.get_iaudioclient()?;

    let desired_format =
        WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE as usize, CHANNELS, None);

    let (_, min_time) = audio_client.get_device_period()?;
    logln!("[audio] device period: min={} hns", min_time);

    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: min_time,
    };
    logln!("[audio] initializing audio client (direction={:?})...", direction);
    audio_client.initialize_client(&desired_format, &Direction::Capture, &mode)?;

    logln!("[audio] audio client initialized, setting up event handle...");
    let h_event = audio_client.set_get_eventhandle()?;
    let render_client = audio_client.get_audiocaptureclient()?;

    let mut encoder = OpusEncoder::new(SAMPLE_RATE, CHANNELS, Application::Audio)
        .map_err(|e| anyhow::anyhow!("opus encoder init: {e}"))?;

    let bytes_per_sample = 4;
    let channels = CHANNELS;
    let frame_bytes = FRAME_SIZE * channels * bytes_per_sample;

    let mut sample_buf: VecDeque<u8> = VecDeque::with_capacity(frame_bytes * 4);
    let mut opus_buf = vec![0u8; OPUS_MAX_PACKET];

    audio_client.start_stream()?;
    logln!("[audio] WASAPI capture stream started ({}Hz, {}ch, {:?}), entering capture loop", SAMPLE_RATE, CHANNELS, settings.source_type);

    let mut iteration: u64 = 0;
    loop {
        if stop_rx.try_recv().is_ok() {
            logln!("[audio] stop signal received, breaking");
            break;
        }

        iteration += 1;
        match render_client.read_from_device_to_deque(&mut sample_buf) {
            Ok(_) => {}
            Err(e) => {
                logln!("[audio] read_from_device_to_deque error on iter {iteration}: {e}");
                return Err(e.into());
            }
        }

        if iteration % 100 == 0 {
            logln!("[audio] iter {iteration}: sample_buf len={} bytes", sample_buf.len());
        }

        while sample_buf.len() >= frame_bytes {
            let raw: Vec<u8> = sample_buf.drain(..frame_bytes).collect();

            let floats: Vec<f32> = raw
                .chunks_exact(4)
                .map(|b| f32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
                .collect();

            match encoder.encode(&floats, FRAME_SIZE, &mut opus_buf) {
                Ok(encoded_len) => {
                    let packet = opus_buf[..encoded_len].to_vec();
                    if tx.try_send(packet).is_err() {
                        logln!("audio: channel full, dropping frame");
                    }
                }
                Err(e) => {
                    logln!("opus encode error: {e}");
                }
            }
        }

        if h_event.wait_for_event(1000).is_err() {
            logln!("audio: wait_for_event error, stopping");
            break;
        }
    }
    logln!("[audio] capture loop ended after {iteration} iterations");

    audio_client.stop_stream()?;
    unsafe {
        windows_sys::Win32::System::Com::CoUninitialize();
    }
    Ok(())
}
