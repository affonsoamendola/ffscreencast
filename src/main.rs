//! ffscreencast - screen streaming over WebRTC.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[macro_use]
mod log;
mod audio;
mod broadcast;
mod capture;
mod dialog;
mod nvenc_encoder;
mod signaling;
mod stats;
mod track;
mod tray;
mod update;

use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::capture::Capture;
use crate::signaling::{build_router, AppState};
use crate::stats::{StreamStats, STATS_DIRTY};
use crate::tray::TrayCtx;

fn main() -> Result<()> {
    log::init();

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>");
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "Box<dyn Any>".to_string()
        };
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        logln!("[PANIC] thread '{thread_name}' panicked at {location}: {payload}");
        default_hook(info);
    }));

    logln!("[main] ffscreencast v{} starting...", update::current_version());

    // Check for updates in the background
    std::thread::Builder::new()
        .name("update-check".into())
        .spawn(|| update::check_and_update())
        .ok();

    unsafe {
        windows_sys::Win32::System::Com::CoInitializeEx(
            std::ptr::null_mut(),
            windows_sys::Win32::System::Com::COINIT_MULTITHREADED as u32,
        );
    }
    let _ = wasapi::initialize_mta();

    let settings = match dialog::run_settings() {
        Some(s) => s,
        None => return Ok(()),
    };

    logln!(
        "[main] settings: target={:?}, fps={}, host={}, port={}, audio={:?}",
        settings.target,
        settings.fps,
        settings.host,
        settings.port,
        settings.audio,
    );

    let capture = Capture::new(settings.target);
    let stun_urls = vec!["stun:stun.l.google.com:19302".to_string()];
    let stats = Arc::new(StreamStats::new());

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let state = AppState::new(
        capture.clone(),
        stun_urls,
        settings.fps,
        stats.clone(),
        settings.password,
        shutdown_tx,
        settings.audio.clone(),
    );

    let runtime = tokio::runtime::Runtime::new()?;
    logln!("[main] tokio runtime created");
    let host = settings.host.clone();
    let port = settings.port;
    std::thread::Builder::new()
        .name("ffscreencast-runtime".into())
        .spawn(move || {
            runtime.block_on(async move {
                let app = build_router(state);
                let addr = format!("{}:{}", host, port);
                logln!("ffscreencast listening on http://{addr}");
                let listener = match tokio::net::TcpListener::bind(&addr).await {
                    Ok(l) => l,
                    Err(e) => {
                        logln!("failed to bind: {e}");
                        return;
                    }
                };
                tokio::select! {
                    result = axum::serve(listener, app) => {
                        if let Err(e) = result {
                            logln!("server error: {e}");
                        }
                    }
                    _ = shutdown_rx.changed() => {
                        logln!("[main] brute force shutdown triggered");
                    }
                }
            });
        })?;

    let tray_ctx = TrayCtx::new(capture, settings.port, stats.clone(), settings.audio)?;
    logln!("[main] tray icon created, entering message loop");

    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            loop {
                if STATS_DIRTY.swap(false, Ordering::Relaxed) {
                    let _ = tray_ctx.rebuild_menu();
                }
                if let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
                    crate::tray::handle_menu_event(event, &tray_ctx);
                }
                if PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) > 0 {
                    if msg.message == WM_QUIT {
                        break;
                    }
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        loop {
            if STATS_DIRTY.swap(false, Ordering::Relaxed) {
                let _ = tray_ctx.rebuild_menu();
            }
            if let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
                crate::tray::handle_menu_event(event, &tray_ctx);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    Ok(())
}
