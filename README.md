# ffscreencast

Streams a monitor from this Windows machine to a web browser over WebRTC.
Media travels over UDP (SRTP) with ICE/STUN hole punching; only the short
signaling handshake uses HTTP.

## Setup

```powershell
py -3.14 -m venv .venv
.\.venv\Scripts\python.exe -m pip install -r requirements.txt
```

## Run from source

```powershell
.\.venv\Scripts\python.exe server.py
```

Open <http://127.0.0.1:8080> and click **Connect**.

## Build a standalone exe

```powershell
.\.venv\Scripts\python.exe -m pip install pyinstaller
.\build.ps1
```

Output: `dist\ffscreencast.exe` — one file, no Python required on the target
machine. First launch takes ~1–2s while it extracts to a temp folder.

```powershell
.\dist\ffscreencast.exe
# or with options
.\dist\ffscreencast.exe --monitor 1 --scale 0.5 --fps 30
```

Open <http://127.0.0.1:8080> and click **Connect**.

## Sign the exe

SmartScreen shows a warning for unsigned binaries. A standard (non-EV) code
signing certificate removes "Unknown Publisher" and lets reputation accrue as
people run the file — but the warning still appears until enough unique
Windows users have executed that specific build. Reputation is per file hash,
so each new release starts at zero.

EV certificates grant immediate reputation but cost more ($300–700/yr).
Non-EV certificates are $30–200/yr and are the right choice while you're
growing the user base.

### Get a certificate

A few affordable non-EV options:

- **Certum Open Source** — cheapest, restricted to OSS maintainers with a
  public project (your GitHub qualifies). ~$30/yr.
- **SSL.com Code Signing** — ~$75/yr.
- **Sectigo Code Signing** — ~$100–180/yr.

When the CA issues your cert, export it to a `.pfx` file with a password you
remember.

### Install signtool

`signtool.exe` ships with the Windows SDK. If you don't have it:

```powershell
winget install Microsoft.VisualStudio.2022.BuildTools --override "--add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
```

Or download the standalone Windows SDK from
<https://developer.microsoft.com/en-us/windows/downloads/windows-sdk/>.

### Build and sign in one step

```powershell
$env:FFSTREAM_PFX_PASSWORD = 'your-pfx-password'
.\build-and-sign.ps1
```

Or sign an already-built exe:

```powershell
$env:FFSTREAM_PFX_PASSWORD = 'your-pfx-password'
.\sign.ps1 -PfxPath .\cert.pfx
```

The script timestamps the signature at `timestamp.sectigo.com` so it stays
valid after the certificate expires.

### What distribution looks like

- First-time users will see the SmartScreen prompt with your name and org
  ("More info → Run anyway").
- Each build is a new file hash, so each new release resets the count.
- After enough unique Windows machines have run a given build (Microsoft
  hasn't published a threshold; think tens of thousands), the prompt stops
  appearing for that exact file.

### Options

| Flag | Default | Notes |
| --- | --- | --- |
| `--host` | `0.0.0.0` | Bind address |
| `--port` | `8080` | Bind port |
| `--monitor` | `1` | `0` = both screens joined, `1` = primary, `2` = second |
| `--fps` | `60` | Target frame rate |
| `--scale` | `1.0` | `0.5` halves resolution — big bandwidth/CPU win |
| `--stun` | Google STUN | Pass `""` to disable |
| `--turn` | none | `turn:user:pass@host:3478` |
| `--verbose` | off | Debug logging (very chatty ICE output) |

Detected on this machine: monitor `1` and `2` are 1920x1080; monitor `0` is the
joined 3840x1080 desktop.

Half-resolution primary screen at 30fps:

```powershell
.\.venv\Scripts\python.exe server.py --monitor 1 --scale 0.5 --fps 30
```

## Viewing from another device

**Same network** — the banner prints a LAN URL. Allow Python through the
Windows Firewall on first run, or add the rule explicitly:

```powershell
New-NetFirewallRule -DisplayName "ffscreencast" -Direction Inbound -Action Allow -Protocol TCP -LocalPort 8080
```

**Over the internet, no port forwarding.** Two pieces are involved, and this is
the part worth understanding:

1. *Signaling* (the `/offer` HTTP request) must be reachable. Expose it with a
   tunnel, which needs no router changes:

   ```powershell
   cloudflared tunnel --url http://localhost:8080
   ```

   Browsers block WebRTC on plain HTTP from non-localhost origins, so a tunnel
   giving you HTTPS is required for remote viewing regardless.

2. *Media* is negotiated peer-to-peer via STUN. This works for most home
   connections. If either side is behind symmetric/carrier-grade NAT, STUN
   alone fails and you need a TURN relay:

   ```powershell
   .\.venv\Scripts\python.exe server.py --turn "turn:user:pass@your-turn-host:3478"
   ```

   TURN relays the video through a third-party server, so it costs bandwidth.
   Free STUN is enough to try first; add TURN only if the status stays stuck on
   `connecting` or flips to `failed`.

A VPN mesh such as Tailscale sidesteps both concerns — connect both devices and
use the private address directly.

## Files

- `server.py` — CLI, aiohttp signaling, peer connection setup
- `screen_track.py` — `mss` capture on a dedicated thread, paced to target fps
- `viewer.html` — browser client
- `smoke_test.py` — headless client that verifies frames actually flow

```powershell
.\.venv\Scripts\python.exe server.py --port 8080
# in another terminal
.\.venv\Scripts\python.exe smoke_test.py http://127.0.0.1:8080
```

## Notes

- Captures the screen only, no audio.
- `mss` grabs the desktop, so UAC prompts and the secure desktop appear black.
- H.264 needs even dimensions; odd sizes are cropped by a pixel.
- Each viewer gets an independent capture track, so CPU scales with viewers.
