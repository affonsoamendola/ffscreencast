//! Auto-update system: checks GitHub Releases for new versions,
//! downloads the update, replaces the binary, and restarts.
//!
//! Update flow:
//! 1. App downloads new exe as `ffscreencast.new.exe` next to itself
//! 2. App spawns `ffscreencast.new.exe` as a detached process
//! 3. App exits
//! 4. `ffscreencast.new.exe` sees its name ends with `.new.exe`, enters update mode
//! 5. It waits for the old process to exit, deletes the old exe, renames itself, restarts

use anyhow::{Context, Result};
use semver::Version;
use std::os::windows::process::CommandExt;

const GITHUB_REPO: &str = "affonsoamendola/ffscreencast";
const EXE_NAME: &str = "ffscreencast.exe";
const EXE_NEW: &str = "ffscreencast.new.exe";

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Called at startup. If we are `ffscreencast.new.exe`, perform the update and restart.
pub fn handle_update_mode() -> bool {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return false,
    };
    let name = exe.file_name().unwrap_or_default().to_string_lossy();

    if !name.eq_ignore_ascii_case(EXE_NEW) {
        return false;
    }

    logln!("[update] running as {}, entering update mode", EXE_NEW);

    let dir = exe.parent().unwrap_or(std::path::Path::new("."));
    let target = dir.join(EXE_NAME);

    // Wait for the old process to exit
    loop {
        let is_running = std::process::Command::new("tasklist")
            .args(["/FI", &format!("IMAGENAME eq {}", EXE_NAME)])
            .output()
            .map(|o| {
                let out = String::from_utf8_lossy(&o.stdout);
                out.lines()
                    .any(|l| l.contains(EXE_NAME) && !l.contains("ffscreencast.new"))
            })
            .unwrap_or(false);

        if !is_running {
            break;
        }

        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    // Small delay for OS to release file handles
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Delete the old exe
    let _ = std::fs::remove_file(&target);

    // Rename ourselves to the target name
    match std::fs::rename(&exe, &target) {
        Ok(_) => {
            logln!("[update] renamed self to {}", target.display());
            // Launch the real app
            let _ = std::process::Command::new(&target).spawn();
        }
        Err(e) => {
            logln!("[update] rename failed: {e}");
        }
    }

    true
}

struct Release {
    exe_url: String,
}

fn check_github() -> Result<Option<Release>> {
    let url = format!(
        "https://api.github.com/repos/{}/releases/latest",
        GITHUB_REPO
    );

    let client = reqwest::blocking::Client::builder()
        .user_agent("ffscreencast-updater")
        .build()
        .context("failed to build HTTP client")?;

    let resp = client
        .get(&url)
        .send()
        .context("failed to check for updates")?;

    if !resp.status().is_success() {
        anyhow::bail!("GitHub API returned {}", resp.status());
    }

    let json: serde_json::Value = resp.json().context("failed to parse release info")?;

    let tag = json["tag_name"]
        .as_str()
        .context("missing tag_name in release")?
        .to_string();

    let exe_url = json["assets"]
        .as_array()
        .context("missing assets")?
        .iter()
        .find(|a| a["name"].as_str() == Some(EXE_NAME))
        .and_then(|a| a["browser_download_url"].as_str())
        .context(format!("{} not found in release assets", EXE_NAME))?
        .to_string();

    let current = Version::parse(current_version())
        .context("failed to parse current version")?;
    let latest = Version::parse(tag.trim_start_matches('v'))
        .context(format!("failed to parse version from tag '{}'", tag))?;

    if latest <= current {
        logln!("[update] up to date (current={}, latest={})", current, latest);
        return Ok(None);
    }

    logln!("[update] new version available: {} -> {}", current, latest);
    Ok(Some(Release { exe_url }))
}

fn download_file(url: &str, dest: &std::path::Path) -> Result<()> {
    logln!("[update] downloading {}...", url);

    let client = reqwest::blocking::Client::builder()
        .user_agent("ffscreencast-updater")
        .build()
        .context("failed to build HTTP client")?;

    let mut resp = client.get(url).send().context("failed to start download")?;

    if !resp.status().is_success() {
        anyhow::bail!("download failed: HTTP {}", resp.status());
    }

    let mut file = std::fs::File::create(dest).context("failed to create temp file")?;
    std::io::copy(&mut resp, &mut file).context("failed to write download")?;

    logln!("[update] download complete: {}", dest.display());
    Ok(())
}

fn spawn_new_and_exit(new_exe: &std::path::Path) {
    logln!("[update] spawning new version and exiting");

    let _ = std::process::Command::new(new_exe)
        .creation_flags(0x00000008) // DETACHED_PROCESS
        .spawn();

    std::process::exit(0);
}

pub fn check_and_update() {
    // Clean up leftover .new.exe from a previous update
    if let Ok(exe) = std::env::current_exe() {
        let dir = exe.parent().unwrap_or(std::path::Path::new("."));
        let _ = std::fs::remove_file(dir.join(EXE_NEW));
    }

    match check_github() {
        Ok(Some(release)) => {
            let current_exe = std::env::current_exe().unwrap_or_else(|_| {
                std::path::PathBuf::from(EXE_NAME)
            });
            let dir = current_exe.parent().unwrap_or(std::path::Path::new("."));
            let new_exe = dir.join(EXE_NEW);

            if let Err(e) = download_file(&release.exe_url, &new_exe) {
                logln!("[update] download failed: {e}");
                return;
            }

            spawn_new_and_exit(&new_exe);
        }
        Ok(None) => {}
        Err(e) => {
            logln!("[update] check failed: {e}");
        }
    }
}
