//! Auto-update system: checks GitHub Releases for new versions,
//! downloads the update, replaces the binary, and restarts.

use anyhow::{Context, Result};
use semver::Version;
use std::os::windows::process::CommandExt;

const GITHUB_REPO: &str = "affonsoamendola/ffscreencast";
const EXE_NAME: &str = "ffscreencast.exe";

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
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

fn spawn_updater_and_exit(current_exe: &std::path::Path, new_exe: &std::path::Path) {
    let pid = std::process::id();
    let current = current_exe.to_string_lossy().to_string();
    let new = new_exe.to_string_lossy().to_string();

    // Write a .ps1 script file — avoids all quoting issues with inline commands
    let ps1 = std::env::temp_dir().join("ffscreencast_update.ps1");
    let script = format!(
        "while (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ Start-Sleep -Milliseconds 500 }}\n\
         Start-Sleep -Milliseconds 500\n\
         Rename-Item '{cur}' '{cur}.old' -Force -ErrorAction SilentlyContinue\n\
         Move-Item -Path '{new}' -Destination '{cur}' -Force\n\
         Remove-Item '{cur}.old' -Force -ErrorAction SilentlyContinue\n\
         Remove-Item '{ps1}' -Force -ErrorAction SilentlyContinue\n\
         Start-Process '{cur}'",
        pid = pid,
        cur = current,
        new = new,
        ps1 = ps1.display(),
    );

    let _ = std::fs::write(&ps1, script);

    logln!("[update] spawning updater and exiting");

    let _ = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            &ps1.to_string_lossy(),
        ])
        .creation_flags(0x00000008) // DETACHED_PROCESS
        .spawn();

    std::process::exit(0);
}

pub fn check_and_update() {
    // Clean up leftover .old file from a previous update
    if let Ok(exe) = std::env::current_exe() {
        let old = exe.with_extension("exe.old");
        let _ = std::fs::remove_file(old);
    }

    match check_github() {
        Ok(Some(release)) => {
            let temp_dir = std::env::temp_dir();
            let new_exe = temp_dir.join(EXE_NAME);

            if let Err(e) = download_file(&release.exe_url, &new_exe) {
                logln!("[update] download failed: {e}");
                return;
            }

            let current_exe = std::env::current_exe().unwrap_or_else(|_| {
                std::path::PathBuf::from(EXE_NAME)
            });

            spawn_updater_and_exit(&current_exe, &new_exe);
        }
        Ok(None) => {}
        Err(e) => {
            logln!("[update] check failed: {e}");
        }
    }
}
