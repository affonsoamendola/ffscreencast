use std::ptr;
use std::sync::mpsc::{self, Sender, Receiver};
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::{anyhow, Result};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication, IDXGIDevice, IDXGIResource,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTPUT_DESC,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::core::Interface;

pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug)]
pub enum Target {
    Monitor(usize),
    Window(String),
    Combined,
}

enum GrabRequest {
    Grab(Sender<Result<Option<Frame>>>),
    SetTarget(Target),
}

struct CaptureInner {
    request_tx: Sender<GrabRequest>,
    current_target: Target,
}

#[derive(Clone)]
pub struct Capture {
    inner: Arc<Mutex<CaptureInner>>,
}

impl Capture {
    pub fn new(target: Target) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<GrabRequest>();

        let thread_target = target.clone();
        std::thread::Builder::new()
            .name("ffscreencast-capture".into())
            .spawn(move || capture_thread_loop(thread_target, request_rx))
            .expect("failed to spawn capture thread");

        Self {
            inner: Arc::new(Mutex::new(CaptureInner { request_tx, current_target: target })),
        }
    }

    pub fn set_target(&self, target: Target) {
        logln!("[capture] set_target: {target:?}");
        self.inner.lock().unwrap().current_target = target.clone();
        let _ = self.inner.lock().unwrap().request_tx.send(GrabRequest::SetTarget(target));
    }

    pub fn target(&self) -> Target {
        self.inner.lock().unwrap().current_target.clone()
    }

    pub fn grab(&self) -> Result<Option<Frame>> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inner
            .lock()
            .unwrap()
            .request_tx
            .send(GrabRequest::Grab(reply_tx))
            .map_err(|_| anyhow!("capture thread dropped"))?;
        reply_rx
            .recv()
            .map_err(|_| anyhow!("capture thread dropped"))?
    }

    pub fn list_monitors() -> Result<Vec<MonitorInfo>> {
        enum_monitors_win32()
    }

    pub fn list_windows() -> Result<Vec<WindowInfo>> {
        enum_windows_win32()
    }

    pub fn capture_monitor_screen(index: usize) -> Option<(Vec<u8>, u32, u32)> {
        capture_monitor_gdi(index)
    }
}

fn create_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    let feature_levels = [D3D_FEATURE_LEVEL_11_0];
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE(std::ptr::null_mut()),
            Default::default(),
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .map_err(|e| anyhow!("D3D11CreateDevice failed: {e}"))?;
    }
    let device = device.ok_or_else(|| anyhow!("D3D11CreateDevice returned null device"))?;
    let context = context.ok_or_else(|| anyhow!("D3D11CreateDevice returned null context"))?;
    Ok((device, context))
}

fn enumerate_outputs() -> Result<Vec<(IDXGIOutput, DXGI_OUTPUT_DESC)>> {
    let (device, _context) = create_d3d11_device()?;
    let dxgi_device: IDXGIDevice = device.cast()
        .map_err(|e| anyhow!("cast to IDXGIDevice failed: {e}"))?;
    let adapter: IDXGIAdapter = unsafe { dxgi_device.GetParent() }
        .map_err(|e| anyhow!("GetParent(IDXGIAdapter) failed: {e}"))?;

    let mut results = Vec::new();
    let mut i = 0;
    loop {
        match unsafe { adapter.EnumOutputs(i) } {
            Ok(output) => {
                match unsafe { output.GetDesc() } {
                    Ok(desc) => {
                        if desc.AttachedToDesktop.as_bool() {
                            results.push((output, desc));
                        }
                    }
                    Err(e) => logln!("[capture] GetDesc failed for output {i}: {e}"),
                }
            }
            Err(_) => break,
        }
        i += 1;
    }
    Ok(results)
}

fn find_output_for_monitor_index(target_idx: usize) -> Result<(IDXGIOutput, u32, u32)> {
    let outputs = enumerate_outputs()?;

    let (output, desc) = outputs
        .into_iter()
        .nth(target_idx)
        .ok_or_else(|| anyhow!("monitor index {target_idx} not found"))?;

    let r = desc.DesktopCoordinates;
    let w = (r.right - r.left) as u32;
    let h = (r.bottom - r.top) as u32;
    Ok((output, w, h))
}

fn find_output_for_hmonitor(hmon: *mut core::ffi::c_void) -> Result<(IDXGIOutput, u32, u32)> {
    let outputs = enumerate_outputs()?;
    for (output, desc) in outputs {
        if desc.Monitor.0 == hmon {
            let r = desc.DesktopCoordinates;
            let w = (r.right - r.left) as u32;
            let h = (r.bottom - r.top) as u32;
            return Ok((output, w, h));
        }
    }
    Err(anyhow!("output for monitor not found"))
}

fn find_monitor_for_window(title: &str) -> Result<(IDXGIOutput, u32, u32, [i32; 4])> {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Graphics::Gdi::MonitorFromWindow;
    use windows_sys::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    let hwnd = unsafe {
        struct Ctx { found: Option<isize>, search: String }
        extern "system" fn callback(hwnd: HWND, lparam: isize) -> i32 {
            unsafe {
                let ctx = &mut *(lparam as *mut Ctx);
                if IsWindowVisible(hwnd) == 0 { return 1; }
                let len = GetWindowTextLengthW(hwnd);
                if len == 0 { return 1; }
                let mut buf = vec![0u16; (len + 1) as usize];
                GetWindowTextW(hwnd, buf.as_mut_ptr(), len + 1);
                let t = String::from_utf16_lossy(
                    &buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())],
                );
                if t == ctx.search {
                    ctx.found = Some(hwnd as isize);
                    return 0;
                }
            }
            1
        }

        let mut ctx = Ctx { found: None, search: title.to_string() };
        EnumWindows(Some(callback), &mut ctx as *mut Ctx as isize);
        ctx.found.ok_or_else(|| anyhow!("window not found: {title}"))?
    };

    let hmon = unsafe {
        MonitorFromWindow(hwnd as HWND, MONITOR_DEFAULTTONEAREST)
    };

    let mut wr: windows_sys::Win32::Foundation::RECT = unsafe { std::mem::zeroed() };
    unsafe {
        GetWindowRect(hwnd as HWND, &mut wr);
    }
    let window_rect = [wr.left, wr.top, wr.right, wr.bottom];

    let (output, mon_w, mon_h) = find_output_for_hmonitor(hmon)?;
    Ok((output, mon_w, mon_h, window_rect))
}

struct DxgiCapturer {
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging_texture: ID3D11Texture2D,
    width: u32,
    height: u32,
    crop: Option<[i32; 4]>,
}

impl DxgiCapturer {
    fn new_monitor(target_idx: usize) -> Result<Self> {
        let (output, w, h) = find_output_for_monitor_index(target_idx)?;
        Self::from_output(output, w, h, None)
    }

    fn new_window(title: &str) -> Result<Self> {
        let (output, mon_w, mon_h, win_rect) = find_monitor_for_window(title)?;
        Self::from_output(output, mon_w, mon_h, Some(win_rect))
    }

    fn new_combined() -> Result<Self> {
        Self::new_monitor(0)
    }

    fn from_output(
        output: IDXGIOutput,
        width: u32,
        height: u32,
        crop: Option<[i32; 4]>,
    ) -> Result<Self> {
        let output1: IDXGIOutput1 = output.cast()
            .map_err(|e| anyhow!("cast to IDXGIOutput1 failed: {e}"))?;

        let (device, context) = create_d3d11_device()?;
        let duplication: IDXGIOutputDuplication = unsafe { output1.DuplicateOutput(&device) }
            .map_err(|e| anyhow!("DuplicateOutput failed: {e}"))?;

        let staging_texture = create_staging_texture(&device, width, height)?;

        Ok(Self { _device: device, context, duplication, staging_texture, width, height, crop })
    }

    fn grab(&mut self) -> Result<Option<Frame>> {
        let mut frame_info: DXGI_OUTDUPL_FRAME_INFO = unsafe { std::mem::zeroed() };
        let mut resource: Option<IDXGIResource> = None;

        match unsafe {
            self.duplication
                .AcquireNextFrame(100, &mut frame_info, &mut resource)
        } {
            Ok(()) => {}
            Err(e) => {
                let code = e.code().0 as u32;
                match code {
                    0x887A0027 => return Ok(None),
                    0x887A0001 => return Err(anyhow!("AcquireNextFrame: access lost, capturer must be recreated")),
                    _ => return Err(anyhow!("AcquireNextFrame failed: {e}")),
                }
            }
        }

        let resource = match resource {
            Some(r) => r,
            None => {
                let _ = unsafe { self.duplication.ReleaseFrame() };
                return Ok(None);
            }
        };

        let desktop_texture: ID3D11Texture2D = resource.cast()
            .map_err(|e| {
                let _ = unsafe { self.duplication.ReleaseFrame() };
                anyhow!("cast to ID3D11Texture2D failed: {e}")
            })?;

        unsafe {
            self.context.CopyResource(&self.staging_texture, &desktop_texture);
        }

        let mut mapped: D3D11_MAPPED_SUBRESOURCE = unsafe { std::mem::zeroed() };
        unsafe {
            self.context.Map(
                &self.staging_texture,
                0,
                D3D11_MAP_READ,
                0,
                Some(&mut mapped),
            ).map_err(|e| {
                let _ = self.duplication.ReleaseFrame();
                anyhow!("Map staging texture failed: {e}")
            })?;
        }

        let result = self.read_mapped_frame(&mapped);

        unsafe {
            self.context.Unmap(&self.staging_texture, 0);
            let _ = self.duplication.ReleaseFrame();
        }

        result.map(Some)
    }

    fn read_mapped_frame(&self, mapped: &D3D11_MAPPED_SUBRESOURCE) -> Result<Frame> {
        let src_pitch = mapped.RowPitch as usize;
        let pixel_width = self.width as usize * 4;

        match self.crop {
            Some([wl, wt, wr, wb]) => {
                let crop_x = wl.max(0) as u32;
                let crop_y = wt.max(0) as u32;
                let crop_w = ((wr - wl).max(0)) as u32;
                let crop_h = ((wb - wt).max(0)) as u32;
                if crop_w == 0 || crop_h == 0 {
                    return Err(anyhow!("window crop has zero size"));
                }

                let dst_pitch = crop_w as usize * 4;
                let mut data = vec![0u8; dst_pitch * crop_h as usize];

                for row in 0..crop_h as usize {
                    let src_offset = (crop_y as usize + row) * src_pitch + crop_x as usize * 4;
                    let dst_offset = row * dst_pitch;
                    let src_ptr = unsafe { mapped.pData.add(src_offset) } as *const u8;
                    let src_slice = unsafe {
                        std::slice::from_raw_parts(src_ptr, dst_pitch)
                    };
                    data[dst_offset..dst_offset + dst_pitch].copy_from_slice(src_slice);
                }

                for px in data.chunks_exact_mut(4) {
                    px.swap(0, 2);
                }

                Ok(Frame { width: crop_w, height: crop_h, data })
            }
            None => {
                let h = self.height as usize;
                let total = pixel_width * h;
                let mut data = vec![0u8; total];

                if src_pitch == pixel_width {
                    let src = unsafe {
                        std::slice::from_raw_parts(mapped.pData as *const u8, total)
                    };
                    data[..total].copy_from_slice(src);
                } else {
                    for row in 0..h {
                        let src = unsafe {
                            std::slice::from_raw_parts(
                                (mapped.pData as *const u8).add(row * src_pitch),
                                pixel_width,
                            )
                        };
                        let dst_start = row * pixel_width;
                        data[dst_start..dst_start + pixel_width].copy_from_slice(src);
                    }
                }

                for px in data.chunks_exact_mut(4) {
                    px.swap(0, 2);
                }

                Ok(Frame { width: self.width, height: self.height, data })
            }
        }
    }
}

fn create_staging_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe {
        device.CreateTexture2D(&desc, None, Some(&mut texture))
            .map_err(|e| anyhow!("CreateTexture2D (staging) failed: {e}"))?;
    }
    texture.ok_or_else(|| anyhow!("CreateTexture2D returned null"))
}

#[cfg(target_os = "windows")]
fn enum_monitors_win32() -> Result<Vec<MonitorInfo>> {
    use windows_sys::Win32::Graphics::Gdi::*;

    unsafe {
        struct Ctx { monitors: Vec<MonitorInfo> }
        extern "system" fn callback(
            hmon: HMONITOR,
            _hdc: HDC,
            _lprect: *mut windows_sys::Win32::Foundation::RECT,
            lparam: isize,
        ) -> i32 {
            unsafe {
                let ctx = &mut *(lparam as *mut Ctx);
                let mut mi: MONITORINFOEXW = std::mem::zeroed();
                mi.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
                if GetMonitorInfoW(hmon, &mut mi as *mut MONITORINFOEXW as *mut MONITORINFO) == 0 {
                    return 1;
                }
                let r = mi.monitorInfo.rcMonitor;
                let name = String::from_utf16_lossy(
                    &mi.szDevice[..mi.szDevice.iter().position(|&c| c == 0).unwrap_or(mi.szDevice.len())]
                );
                let is_primary = r.left == 0 && r.top == 0;
                let count = ctx.monitors.len();
                ctx.monitors.push(MonitorInfo {
                    index: count,
                    name,
                    width: (r.right - r.left) as u32,
                    height: (r.bottom - r.top) as u32,
                    x: r.left,
                    y: r.top,
                    is_primary,
                });
            }
            1
        }

        let mut ctx = Ctx { monitors: Vec::new() };
        EnumDisplayMonitors(
            ptr::null_mut(),
            ptr::null(),
            Some(callback),
            &mut ctx as *mut Ctx as isize,
        );
        ctx.monitors.sort_by_key(|m| (m.x, m.y));
        for (i, m) in ctx.monitors.iter_mut().enumerate() {
            m.index = i;
        }
        Ok(ctx.monitors)
    }
}

#[cfg(not(target_os = "windows"))]
fn enum_monitors_win32() -> Result<Vec<MonitorInfo>> {
    Ok(Vec::new())
}

#[cfg(target_os = "windows")]
fn enum_windows_win32() -> Result<Vec<WindowInfo>> {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    unsafe {
        struct Ctx { windows: Vec<WindowInfo> }
        extern "system" fn callback(hwnd: HWND, lparam: isize) -> i32 {
            unsafe {
                let ctx = &mut *(lparam as *mut Ctx);
                if IsWindowVisible(hwnd) == 0 { return 1; }
                let len = GetWindowTextLengthW(hwnd);
                if len == 0 { return 1; }
                let mut buf = vec![0u16; (len + 1) as usize];
                GetWindowTextW(hwnd, buf.as_mut_ptr(), len + 1);
                let title = String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())]);
                if title.is_empty() { return 1; }
                let mut rc: windows_sys::Win32::Foundation::RECT = std::mem::zeroed();
                GetWindowRect(hwnd, &mut rc);
                let minimized = IsIconic(hwnd) != 0;
                ctx.windows.push(WindowInfo {
                    title,
                    width: (rc.right - rc.left) as u32,
                    height: (rc.bottom - rc.top) as u32,
                    minimized,
                });
            }
            1
        }

        let mut ctx = Ctx { windows: Vec::new() };
        EnumWindows(Some(callback), &mut ctx as *mut Ctx as isize);
        ctx.windows.sort_by(|a, b| a.title.cmp(&b.title));
        Ok(ctx.windows)
    }
}

#[cfg(not(target_os = "windows"))]
fn enum_windows_win32() -> Result<Vec<WindowInfo>> {
    Ok(Vec::new())
}

#[cfg(target_os = "windows")]
fn capture_monitor_gdi(index: usize) -> Option<(Vec<u8>, u32, u32)> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::*;

    unsafe {
        struct Ctx { monitors: Vec<(HMONITOR, RECT)> }
        extern "system" fn callback(
            hmon: HMONITOR,
            _hdc: HDC,
            lprect: *mut RECT,
            lparam: isize,
        ) -> i32 {
            unsafe {
                let ctx = &mut *(lparam as *mut Ctx);
                ctx.monitors.push((hmon, *lprect));
            }
            1
        }

        let mut ctx = Ctx { monitors: Vec::new() };
        EnumDisplayMonitors(
            ptr::null_mut(),
            ptr::null(),
            Some(callback),
            &mut ctx as *mut Ctx as isize,
        );
        ctx.monitors.sort_by_key(|m| (m.1.left, m.1.top));

        let (_hmon, rc) = ctx.monitors.into_iter().nth(index)?;
        let w = (rc.right - rc.left) as i32;
        let h = (rc.bottom - rc.top) as i32;
        if w <= 0 || h <= 0 { return None; }

        let screen_dc = GetDC(ptr::null_mut());
        let mem_dc = CreateCompatibleDC(screen_dc);

        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = w;
        bmi.bmiHeader.biHeight = -h;
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;

        let mut bits: *mut u8 = ptr::null_mut();
        let h_bmp = CreateDIBSection(
            mem_dc, &bmi, DIB_RGB_COLORS,
            &mut bits as *mut _ as *mut _,
            ptr::null_mut(), 0,
        );
        if h_bmp.is_null() || bits.is_null() {
            DeleteDC(mem_dc);
            ReleaseDC(ptr::null_mut(), screen_dc);
            return None;
        }

        let old_bmp = SelectObject(mem_dc, h_bmp as _);
        BitBlt(mem_dc, 0, 0, w, h, screen_dc, rc.left, rc.top, SRCCOPY);
        SelectObject(mem_dc, old_bmp);

        let pixel_bytes = (w * h * 4) as usize;
        let mut data = vec![0u8; pixel_bytes];
        ptr::copy_nonoverlapping(bits, data.as_mut_ptr(), pixel_bytes);

        DeleteObject(h_bmp as _);
        DeleteDC(mem_dc);
        ReleaseDC(ptr::null_mut(), screen_dc);

        for px in data.chunks_exact_mut(4) {
            px.swap(0, 2);
        }

        Some((data, w as u32, h as u32))
    }
}

#[cfg(not(target_os = "windows"))]
fn capture_monitor_gdi(_index: usize) -> Option<(Vec<u8>, u32, u32)> {
    None
}

fn capture_thread_loop(initial_target: Target, rx: Receiver<GrabRequest>) {
    use std::time::Duration;

    let mut target = initial_target;
    logln!("[capture] thread started, initial target: {target:?}");

    let mut capturer = create_capturer(&target);

    while let Ok(req) = rx.recv() {
        match req {
            GrabRequest::SetTarget(new_target) => {
                logln!("[capture] target changed: {target:?} -> {new_target:?}");
                target = new_target;
                drop(capturer.take());
                capturer = create_capturer(&target);
            }
            GrabRequest::Grab(reply_tx) => {
                let result = match &mut capturer {
                    Some(c) => c.grab(),
                    None => {
                        drop(capturer.take());
                        std::thread::sleep(Duration::from_millis(500));
                        capturer = create_capturer(&target);
                        match &mut capturer {
                            Some(c) => c.grab(),
                            None => Err(anyhow!("DXGI capturer unavailable")),
                        }
                    }
                };
                if result.is_err() {
                    logln!("[capture] grab error, recreating capturer");
                    drop(capturer.take());
                    std::thread::sleep(Duration::from_millis(200));
                    capturer = create_capturer(&target);
                }
                let _ = reply_tx.send(result);
            }
        }
    }
    logln!("[capture] thread exiting (channel closed)");
}

fn create_capturer(target: &Target) -> Option<DxgiCapturer> {
    let result = match target {
        Target::Monitor(idx) => DxgiCapturer::new_monitor(*idx),
        Target::Window(title) => DxgiCapturer::new_window(title),
        Target::Combined => DxgiCapturer::new_combined(),
    };
    match result {
        Ok(c) => {
            logln!("[capture] DXGI capturer created for {target:?}");
            Some(c)
        }
        Err(e) => {
            logln!("[capture] failed to create DXGI capturer: {e}");
            None
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct MonitorInfo {
    pub index: usize,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
    pub is_primary: bool,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct WindowInfo {
    pub title: String,
    #[allow(dead_code)]
    pub width: u32,
    #[allow(dead_code)]
    pub height: u32,
    #[allow(dead_code)]
    pub minimized: bool,
}
