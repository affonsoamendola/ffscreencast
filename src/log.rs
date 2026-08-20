use std::fs::{File, OpenOptions};
use std::io::{stderr, Write};
use std::path::PathBuf;
use std::sync::Mutex;

static LOG_FILE: Mutex<Option<File>> = Mutex::new(None);

pub fn init() {
    let exe_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ffscreencast"));
    let dir = exe_path
        .parent()
        .unwrap_or(std::path::Path::new("."));

    let log_path = dir.join("ffscreencast.log");
    let max_backups: u32 = 5;

    // Rotate existing logs: .log4 -> delete, .log3 -> .log4, ..., .log -> .log1
    let _ = std::fs::remove_file(dir.join(format!("ffscreencast.log{max_backups}")));
    for i in (1..max_backups).rev() {
        let src = dir.join(format!("ffscreencast.log{i}"));
        let dst = dir.join(format!("ffscreencast.log{}", i + 1));
        let _ = std::fs::rename(&src, &dst);
    }
    if log_path.exists() {
        let _ = std::fs::rename(&log_path, dir.join("ffscreencast.log1"));
    }

    match OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(f) => {
            *LOG_FILE.lock().unwrap() = Some(f);
            let _ = writeln!(stderr(), "[log] logging to {}", log_path.display());
        }
        Err(e) => {
            let _ = writeln!(stderr(), "[log] failed to open {}: {e}", log_path.display());
        }
    }
}

pub fn write_log(msg: &str) {
    let _ = writeln!(stderr(), "{msg}");
    if let Ok(mut guard) = LOG_FILE.lock() {
        if let Some(ref mut f) = *guard {
            let _ = writeln!(f, "{msg}");
            let _ = f.flush();
        }
    }
}

macro_rules! logln {
    ($($arg:tt)*) => {{
        let _msg = format!($($arg)*);
        $crate::log::write_log(&_msg);
    }};
}
