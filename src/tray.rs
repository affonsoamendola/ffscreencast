//! System tray icon with monitor/window selection and audio settings.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::audio::{AudioDevice, AudioSettings, AudioSourceType};
use crate::capture::{Capture, Target};
use crate::stats::StreamStats;

const ID_REFRESH: &str = "refresh";
const ID_COPY_IP: &str = "copy_ip";
const ID_QUIT: &str = "quit";
const ID_CHECK_UPDATE: &str = "check_update";
const ID_MON_BASE: &str = "mon_";
const ID_WIN_BASE: &str = "win_";
const ID_COMBINED: &str = "combined";
const ID_AUDIO_TOGGLE: &str = "audio_toggle";
const ID_AUDIO_SRC_BASE: &str = "audio_src_";
const ID_AUDIO_DEV_BASE: &str = "audio_dev_";

fn make_icon() -> Icon {
    let mut rgba = vec![0u8; 16 * 16 * 4];
    let cx = 7.5f64;
    let cy = 7.5f64;
    let r = 7.0f64;
    for y in 0..16 {
        for x in 0..16 {
            let dx = x as f64 - cx;
            let dy = y as f64 - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            let i = (y * 16 + x) * 4;
            if dist <= r {
                let alpha = if dist > r - 1.0 {
                    ((r - dist) * 255.0) as u8
                } else {
                    255
                };
                rgba[i] = 220;
                rgba[i + 1] = 40;
                rgba[i + 2] = 40;
                rgba[i + 3] = alpha;
            } else {
                rgba[i + 3] = 0;
            }
        }
    }
    Icon::from_rgba(rgba, 16, 16).unwrap()
}

pub struct TrayCtx {
    pub tray: TrayIcon,
    pub capture: Capture,
    pub port: u16,
    pub stats: Arc<StreamStats>,
    pub audio_settings: Arc<Mutex<AudioSettings>>,
    pub render_devices: Vec<AudioDevice>,
    pub capture_devices: Vec<AudioDevice>,
}

impl TrayCtx {
    pub fn new(
        capture: Capture,
        port: u16,
        stats: Arc<StreamStats>,
        audio_settings: AudioSettings,
    ) -> Result<Self> {
        let render_devices = crate::audio::list_audio_devices(&wasapi::Direction::Render);
        let capture_devices = crate::audio::list_audio_devices(&wasapi::Direction::Capture);

        let audio_settings = Arc::new(Mutex::new(audio_settings));

        let tray = TrayIconBuilder::new()
            .with_tooltip(&tooltip_text(&stats))
            .with_icon(make_icon())
            .with_menu(Box::new(build_menu(
                &capture,
                &stats,
                &audio_settings,
                &render_devices,
                &capture_devices,
            )?))
            .build()?;

        Ok(Self {
            tray,
            capture,
            port,
            stats,
            audio_settings,
            render_devices,
            capture_devices,
        })
    }

    pub fn rebuild_menu(&self) -> Result<()> {
        self.tray.set_menu(Some(Box::new(build_menu(
            &self.capture,
            &self.stats,
            &self.audio_settings,
            &self.render_devices,
            &self.capture_devices,
        )?)));
        self.tray.set_tooltip(Some(&tooltip_text(&self.stats)))?;
        Ok(())
    }
}

fn build_menu(
    capture: &Capture,
    stats: &Arc<StreamStats>,
    audio_settings: &Arc<Mutex<AudioSettings>>,
    render_devices: &[AudioDevice],
    capture_devices: &[AudioDevice],
) -> Result<Menu> {
    let menu = Menu::new();
    let current = capture.target();
    let audio = audio_settings.lock().unwrap().clone();

    // Stats submenu
    let stats_sub = Submenu::new("Stats", true);
    {
        let s = stats.snapshot();
        let items = [
            format!("{} fps", s.fps),
            format!(
                "capture {}.{:01}ms",
                s.capture_us / 1000,
                (s.capture_us % 1000) / 100
            ),
            format!(
                "encode {}.{:01}ms",
                s.encode_us / 1000,
                (s.encode_us % 1000) / 100
            ),
            format!(
                "write  {}.{:01}ms",
                s.write_us / 1000,
                (s.write_us % 1000) / 100
            ),
            format!("frame  {} KB", s.frame_bytes / 1024),
        ];
        for label in &items {
            stats_sub.append(&MenuItem::new(label.as_str(), false, None))?;
        }
        if s.total_frames > 0 {
            stats_sub.append(&PredefinedMenuItem::separator())?;
            stats_sub.append(&MenuItem::new(
                format!("total {} frames", s.total_frames).as_str(),
                false,
                None,
            ))?;
        }
    }
    menu.append(&stats_sub)?;
    menu.append(&PredefinedMenuItem::separator())?;

    // Audio submenu
    let audio_sub = Submenu::new("Audio", true);
    {
        let toggle_label = format!(
            "{}{}",
            if audio.enabled { "Disable audio" } else { "Enable audio" },
            if audio.enabled { " \u{2713}" } else { "" }
        );
        audio_sub.append(&MenuItem::with_id(ID_AUDIO_TOGGLE, &toggle_label, true, None))?;
        audio_sub.append(&PredefinedMenuItem::separator())?;

        for src in &[AudioSourceType::SystemAudio, AudioSourceType::Microphone] {
            let check = if audio.source_type == *src { " \u{2713}" } else { "" };
            let label = format!("{}{}", src.label(), check);
            let id = format!("{ID_AUDIO_SRC_BASE}{:?}", src);
            audio_sub.append(&MenuItem::with_id(id, &label, true, None))?;
        }
        audio_sub.append(&PredefinedMenuItem::separator())?;

        let devices = match audio.source_type {
            AudioSourceType::SystemAudio => render_devices,
            AudioSourceType::Microphone => capture_devices,
        };
        if devices.is_empty() {
            audio_sub.append(&MenuItem::new("No devices found", false, None))?;
        } else {
            for d in devices {
                let check = if audio.device_id.as_deref() == Some(d.id.as_str()) {
                    " \u{2713}"
                } else {
                    ""
                };
                let label = format!("{}{}", truncate(&d.name, 48), check);
                let id = format!("{ID_AUDIO_DEV_BASE}{}", d.id);
                audio_sub.append(&MenuItem::with_id(id, &label, true, None))?;
            }
        }
    }
    menu.append(&audio_sub)?;

    // Monitors
    let mon_sub = Submenu::new("Monitors", true);
    {
        let label = format!(
            "Combined (all monitors){}",
            if matches!(&current, Target::Combined) {
                " \u{2713}"
            } else {
                ""
            }
        );
        mon_sub.append(&MenuItem::with_id(ID_COMBINED, label, true, None))?;
        mon_sub.append(&PredefinedMenuItem::separator())?;
    }
    match Capture::list_monitors() {
        Ok(mons) => {
            if mons.is_empty() {
                mon_sub.append(&MenuItem::new("No monitors found", false, None))?;
            }
            for m in &mons {
                let label = format!(
                    "{} {}x{}{}",
                    monitor_label(m),
                    m.width,
                    m.height,
                    if matches!(&current, Target::Monitor(i) if *i == m.index) {
                        " \u{2713}"
                    } else {
                        ""
                    }
                );
                let id = format!("{ID_MON_BASE}{}", m.index);
                mon_sub.append(&MenuItem::with_id(id, label, true, None))?;
            }
        }
        Err(_) => {
            mon_sub.append(&MenuItem::new("Failed to list monitors", false, None))?;
        }
    }
    menu.append(&mon_sub)?;

    // Windows
    let win_sub = Submenu::new("Windows", true);
    match Capture::list_windows() {
        Ok(wins) => {
            if wins.is_empty() {
                win_sub.append(&MenuItem::new("No windows found", false, None))?;
            }
            for w in &wins {
                let label = format!(
                    "{}{}",
                    truncate(&w.title, 48),
                    if matches!(&current, Target::Window(t) if *t == w.title) {
                        " \u{2713}"
                    } else {
                        ""
                    }
                );
                let id = format!("{ID_WIN_BASE}{}", w.title);
                win_sub.append(&MenuItem::with_id(id, label, true, None))?;
            }
        }
        Err(_) => {
            win_sub.append(&MenuItem::new("Failed to list windows", false, None))?;
        }
    }
    menu.append(&win_sub)?;

    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&MenuItem::with_id(ID_COPY_IP, "Copy IP", true, None))?;
    menu.append(&MenuItem::with_id(ID_REFRESH, "Refresh", true, None))?;
    menu.append(&MenuItem::with_id(
        ID_CHECK_UPDATE,
        "Check for updates",
        true,
        None,
    ))?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&MenuItem::with_id(ID_QUIT, "Quit", true, None))?;

    Ok(menu)
}

fn monitor_label(m: &crate::capture::MonitorInfo) -> String {
    if m.is_primary {
        format!("Monitor {} (primary)", m.index)
    } else if m.name.is_empty() {
        format!("Monitor {}", m.index)
    } else {
        format!("{} ({})", m.index, m.name)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let end = s.floor_char_boundary(max - 3);
        format!("{}...", &s[..end])
    }
}

fn local_ip_guess() -> String {
    use std::net::UdpSocket;
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            Ok(s.local_addr()?.ip().to_string())
        })
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

fn tooltip_text(stats: &StreamStats) -> String {
    let s = stats.snapshot();
    format!(
        "ffscreencast | {} fps | cap {:.1}ms | enc {:.1}ms",
        s.fps,
        s.capture_us as f64 / 1000.0,
        s.encode_us as f64 / 1000.0,
    )
}

pub fn handle_menu_event(event: MenuEvent, tray: &TrayCtx) {
    let id = event.id().0.as_str();

    if id == ID_QUIT {
        std::process::exit(0);
    } else if id == ID_REFRESH {
        let _ = tray.rebuild_menu();
    } else if id == ID_CHECK_UPDATE {
        logln!("[tray] manual update check triggered");
        std::thread::spawn(crate::update::check_and_update);
    } else if id == ID_COPY_IP {
        let addr = format!("{}:{}", local_ip_guess(), tray.port);
        if let Ok(mut ctx) = arboard::Clipboard::new() {
            let _ = ctx.set_text(&addr);
        }
    } else if id == ID_AUDIO_TOGGLE {
        let mut audio = tray.audio_settings.lock().unwrap();
        audio.enabled = !audio.enabled;
        drop(audio);
        let _ = tray.rebuild_menu();
    } else if let Some(src_name) = id.strip_prefix(ID_AUDIO_SRC_BASE) {
        let new_source = match src_name {
            "SystemAudio" => AudioSourceType::SystemAudio,
            "Microphone" => AudioSourceType::Microphone,
            _ => return,
        };
        let mut audio = tray.audio_settings.lock().unwrap();
        if audio.source_type != new_source {
            audio.source_type = new_source;
            audio.device_id = None;
        }
        drop(audio);
        let _ = tray.rebuild_menu();
    } else if let Some(device_id) = id.strip_prefix(ID_AUDIO_DEV_BASE) {
        let mut audio = tray.audio_settings.lock().unwrap();
        audio.device_id = Some(device_id.to_string());
        drop(audio);
        let _ = tray.rebuild_menu();
    } else if id == ID_COMBINED {
        tray.capture.set_target(Target::Combined);
        let _ = tray.rebuild_menu();
    } else if let Some(idx) = id.strip_prefix(ID_MON_BASE) {
        if let Ok(idx) = idx.parse::<usize>() {
            tray.capture.set_target(Target::Monitor(idx));
            let _ = tray.rebuild_menu();
        }
    } else if let Some(title) = id.strip_prefix(ID_WIN_BASE) {
        tray.capture.set_target(Target::Window(title.to_string()));
        let _ = tray.rebuild_menu();
    }
}
