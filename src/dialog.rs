//! Native Win32 settings dialog shown at startup.

use std::mem::size_of;
use std::ptr;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BeginPaint, CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush,
    DeleteDC, DeleteObject, DIB_RGB_COLORS, DrawTextW, EndPaint, FillRect, FrameRect, GetSysColor,
    GetSysColorBrush, HDC, HFONT, InvalidateRect, PAINTSTRUCT, SelectObject, SetBkColor,
    SetBkMode, SetTextColor, StretchBlt, UpdateWindow, BITMAPINFO, BITMAPINFOHEADER, SRCCOPY,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::IsDlgButtonChecked;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::audio::{AudioDevice, AudioSettings, AudioSourceType};
use crate::capture::{Capture, MonitorInfo, Target, WindowInfo};

const BS_GROUPBOX: u32 = 0x00000007;
const BS_AUTORADIOBUTTON: u32 = 0x00000008;
const BS_DEFPUSHBUTTON: u32 = 0x00000001;
const BST_CHECKED: u32 = 0x0001;
const CBS_DROPDOWNLIST: u32 = 0x00000003;
const CBS_HASSTRINGS: u32 = 0x00000040;
const CB_ADDSTRING: u32 = 0x0143;
const CB_SETCURSEL: u32 = 0x014E;
const CB_GETCURSEL: u32 = 0x0147;
const CB_RESETCONTENT: u32 = 0x014B;
const CBN_SELCHANGE: u16 = 1;
const ES_NUMBER: u32 = 0x2000;
const ES_AUTOHSCROLL: u32 = 0x0080;
const SS_PATHELLIPSIS: u32 = 0x00004000;
const GWLP_USERDATA: i32 = -21;
const SM_CXSCREEN: i32 = 0;
const SM_CYSCREEN: i32 = 1;
const DT_CENTER: u32 = 0x00000001;
const DT_VCENTER: u32 = 0x00000004;
const DT_SINGLELINE: u32 = 0x00000020;
const TRANSPARENT: i32 = 1;

const CLASS_NAME: &str = "FFStreamSettings\0";

fn generate_default_password() -> String {
    const CHARS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghjkmnpqrstuvwxyz23456789";
    let mut pw = String::with_capacity(16);
    for _ in 0..16 {
        let idx = rand::random::<u8>() as usize % CHARS.len();
        pw.push(CHARS[idx] as char);
    }
    pw
}

const ID_RADIO_COMBINED: i32 = 1998;
const ID_RADIO_WINDOW: i32 = 1999;
const ID_COMBO_WINDOW: i32 = 2000;
const ID_EDIT_FPS: i32 = 3000;
const ID_EDIT_PASSWORD: i32 = 3100;
const ID_BTN_COPY: i32 = 4000;
const ID_BTN_START: i32 = 4001;
const ID_BTN_QUIT: i32 = 4002;
const ID_STATIC_URL: i32 = 5000;
const ID_RADIO_AUDIO_ENABLE: i32 = 6000;
const ID_RADIO_AUDIO_DISABLE: i32 = 6001;
const ID_RADIO_AUDIO_SYSTEM: i32 = 6002;
const ID_RADIO_AUDIO_MIC: i32 = 6003;
const ID_COMBO_AUDIO_DEVICE: i32 = 6004;

const COLOR_WINDOW: i32 = 5;
const COLOR_WINDOWTEXT: i32 = 8;
const COLOR_HIGHLIGHT: i32 = 13;
const COLOR_BTNFACE: i32 = 15;
const COLOR_BTNTEXT: i32 = 18;

const DLG_CX: i32 = 460;
const MARGIN: i32 = 16;
const PREVIEW_HEIGHT: i32 = 220;
const THUMB_HEIGHT: u32 = 200;

#[derive(Clone, Copy, PartialEq)]
enum CaptureMode {
    Monitor(usize),
    Combined,
    Window,
}

pub struct SettingsResult {
    pub target: Target,
    pub fps: u32,
    pub host: String,
    pub port: u16,
    pub password: String,
    pub audio: AudioSettings,
}

struct DialogState {
    monitors: Vec<MonitorInfo>,
    windows: Vec<WindowInfo>,
    combo_window: HWND,
    combined_radio: HWND,
    window_radio: HWND,
    edit_fps: HWND,
    edit_password: HWND,
    static_url: HWND,
    preview_rect: RECT,
    hfont: HFONT,
    monitor_captures: Vec<Option<(Vec<u8>, u32, u32)>>,
    capture_mode: CaptureMode,
    radio_audio_enable: HWND,
    radio_audio_disable: HWND,
    radio_audio_system: HWND,
    radio_audio_mic: HWND,
    combo_audio_device: HWND,
    render_devices: Vec<AudioDevice>,
    capture_devices: Vec<AudioDevice>,
    result: Option<SettingsResult>,
}

unsafe fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn create_control(
    parent: HWND,
    class: &str,
    text: &str,
    style: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: i32,
    hfont: *mut core::ffi::c_void,
) -> HWND {
    let wc = to_wide(class);
    let wt = to_wide(text);
    let hwnd = CreateWindowExW(
        0,
        wc.as_ptr(),
        wt.as_ptr(),
        style | WS_CHILD | WS_VISIBLE,
        x,
        y,
        w,
        h,
        parent,
        id as HMENU,
        ptr::null_mut(),
        ptr::null_mut(),
    );
    if !hfont.is_null() {
        SendMessageW(hwnd, WM_SETFONT, hfont as usize, 1);
    }
    hwnd
}

unsafe fn get_state(hwnd: HWND) -> *mut DialogState {
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut DialogState
}

unsafe fn local_ip_guess() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            Ok(s.local_addr()?.ip().to_string())
        })
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

unsafe fn populate_audio_devices(combo: HWND, devices: &[AudioDevice]) {
    SendMessageW(combo, CB_RESETCONTENT, 0, 0);
    for d in devices {
        let wide = to_wide(&d.name);
        SendMessageW(combo, CB_ADDSTRING, 0, wide.as_ptr() as LPARAM);
    }
    if !devices.is_empty() {
        SendMessageW(combo, CB_SETCURSEL, 0, 0);
    }
}

fn scale_rgba(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return Vec::new();
    }
    let mut dst = vec![0u8; (dst_w * dst_h * 4) as usize];
    for y in 0..dst_h {
        let sy = ((y as u64 * src_h as u64) / dst_h as u64).min(src_h as u64 - 1) as u32;
        for x in 0..dst_w {
            let sx = ((x as u64 * src_w as u64) / dst_w as u64).min(src_w as u64 - 1) as u32;
            let si = ((sy * src_w + sx) * 4) as usize;
            let di = ((y * dst_w + x) * 4) as usize;
            dst[di..di + 4].copy_from_slice(&src[si..si + 4]);
        }
    }
    dst
}

struct PreviewLayout {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

fn compute_preview_layout(monitors: &[MonitorInfo], area_w: i32, area_h: i32) -> Vec<PreviewLayout> {
    if monitors.is_empty() {
        return Vec::new();
    }
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    for m in monitors {
        min_x = min_x.min(m.x);
        min_y = min_y.min(m.y);
        max_x = max_x.max(m.x + m.width as i32);
        max_y = max_y.max(m.y + m.height as i32);
    }
    let total_w = (max_x - min_x) as f32;
    let total_h = (max_y - min_y) as f32;
    if total_w <= 0.0 || total_h <= 0.0 {
        return Vec::new();
    }
    let padding = 12.0_f32;
    let avail_w = area_w as f32 - 2.0 * padding;
    let avail_h = area_h as f32 - 2.0 * padding;
    let scale = (avail_w / total_w).min(avail_h / total_h);
    let ox = padding + (avail_w - total_w * scale) / 2.0;
    let oy = padding + (avail_h - total_h * scale) / 2.0;

    monitors
        .iter()
        .map(|m| PreviewLayout {
            x: (ox + (m.x - min_x) as f32 * scale) as i32,
            y: (oy + (m.y - min_y) as f32 * scale) as i32,
            w: (m.width as f32 * scale) as i32,
            h: (m.height as f32 * scale) as i32,
        })
        .collect()
}

unsafe fn draw_bitmap_from_rgba(
    hdc: HDC,
    rgba: &[u8],
    src_w: u32,
    src_h: u32,
    dst_x: i32,
    dst_y: i32,
    dst_w: i32,
    dst_h: i32,
) {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return;
    }
    let mut bmi: BITMAPINFO = std::mem::zeroed();
    bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = src_w as i32;
    bmi.bmiHeader.biHeight = -(src_h as i32);
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB;

    let mut bits: *mut core::ffi::c_void = ptr::null_mut();
    let hbm = CreateDIBSection(hdc, &bmi, DIB_RGB_COLORS, &mut bits, ptr::null_mut(), 0);
    if hbm.is_null() || bits.is_null() {
        return;
    }

    let pixel_count = (src_w * src_h) as usize;
    let src_u32 = std::slice::from_raw_parts(rgba.as_ptr() as *const u32, pixel_count);
    let dst_u32 = std::slice::from_raw_parts_mut(bits as *mut u32, pixel_count);
    for i in 0..pixel_count {
        let p = src_u32[i];
        let r = p & 0xFF;
        let g = (p >> 8) & 0xFF;
        let b = (p >> 16) & 0xFF;
        let a = (p >> 24) & 0xFF;
        dst_u32[i] = (a << 24) | (r << 16) | (g << 8) | b;
    }

    let mem_dc = CreateCompatibleDC(hdc);
    let old_bmp = SelectObject(mem_dc, hbm);
    StretchBlt(hdc, dst_x, dst_y, dst_w, dst_h, mem_dc, 0, 0, src_w as i32, src_h as i32, SRCCOPY);
    SelectObject(mem_dc, old_bmp);
    DeleteDC(mem_dc);
    DeleteObject(hbm as _);
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            let cs = &*(lparam as *const CREATESTRUCTW);
            let state_ptr = cs.lpCreateParams as *mut DialogState;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
            let state = &mut *state_ptr;

            let hfont = CreateFontW(
                -14, 0, 0, 0, 400, 0, 0, 0, 0, 0, 0, 5, 0,
                to_wide("Segoe UI").as_ptr(),
            );
            state.hfont = hfont;

            let content_w = DLG_CX - 2 * MARGIN;
            let mut y = MARGIN;

            // ── Capture Source ──
            let group_h = 24 + PREVIEW_HEIGHT + 8 + 28 + 36 + 8;
            create_control(
                hwnd, "BUTTON", "Capture Source",
                WS_GROUP | BS_GROUPBOX, MARGIN, y, content_w, group_h, -1, hfont,
            );
            y += 24;

            state.preview_rect = RECT {
                left: MARGIN + 8,
                top: y,
                right: MARGIN + 8 + content_w - 16,
                bottom: y + PREVIEW_HEIGHT,
            };
            y += PREVIEW_HEIGHT + 8;

            state.monitor_captures = state
                .monitors
                .iter()
                .map(|m| {
                    Capture::capture_monitor_screen(m.index).map(|(rgba, w, h)| {
                        let thumb_h = THUMB_HEIGHT;
                        let thumb_w =
                            std::cmp::max(1, (w as f32 * thumb_h as f32 / h as f32) as u32);
                        (scale_rgba(&rgba, w, h, thumb_w, thumb_h), thumb_w, thumb_h)
                    })
                })
                .collect();

            state.combined_radio = create_control(
                hwnd, "BUTTON", "Combined (all monitors)",
                WS_GROUP | BS_AUTORADIOBUTTON,
                MARGIN + 12, y, content_w - 24, 22, ID_RADIO_COMBINED, hfont,
            );
            y += 28;

            state.window_radio = create_control(
                hwnd, "BUTTON", "Window:",
                WS_GROUP | BS_AUTORADIOBUTTON,
                MARGIN + 12, y, 72, 22, ID_RADIO_WINDOW, hfont,
            );
            state.combo_window = create_control(
                hwnd, "COMBOBOX", "",
                CBS_DROPDOWNLIST | CBS_HASSTRINGS,
                MARGIN + 88, y - 2, content_w - 100, 200,
                ID_COMBO_WINDOW, hfont,
            );
            for w in &state.windows {
                let title = if w.title.chars().count() > 50 {
                    let truncated: String = w.title.chars().take(47).collect();
                    format!("{truncated}...")
                } else {
                    w.title.clone()
                };
                let wide = to_wide(&title);
                SendMessageW(state.combo_window, CB_ADDSTRING, 0, wide.as_ptr() as LPARAM);
            }
            SendMessageW(state.combo_window, CB_SETCURSEL, 0, 0);
            EnableWindow(state.combo_window, 0);
            y += 36;

            // ── Frame Rate ──
            y += 8;
            create_control(
                hwnd, "BUTTON", "Frame Rate",
                BS_GROUPBOX, MARGIN, y, content_w, 52, -1, hfont,
            );
            y += 22;
            state.edit_fps = create_control(
                hwnd, "EDIT", "60",
                ES_NUMBER | ES_AUTOHSCROLL,
                MARGIN + 12, y + 1, 52, 24, ID_EDIT_FPS, hfont,
            );
            create_control(
                hwnd, "STATIC", "fps  (1 \u{2013} 60)",
                0, MARGIN + 76, y + 4, 100, 20, -1, hfont,
            );
            y += 52;

            // ── Password ──
            y += 8;
            create_control(
                hwnd, "BUTTON", "Viewer Password",
                BS_GROUPBOX, MARGIN, y, content_w, 52, -1, hfont,
            );
            y += 22;
            state.edit_password = create_control(
                hwnd, "EDIT", "",
                ES_AUTOHSCROLL,
                MARGIN + 12, y + 1, content_w - 24, 24, ID_EDIT_PASSWORD, hfont,
            );
            {
                let default_pw = generate_default_password();
                let wide_pw = to_wide(&default_pw);
                SetWindowTextW(state.edit_password, wide_pw.as_ptr());
            }
            y += 52;

            // ── Audio ──
            y += 8;
            let audio_group_h = 130;
            create_control(
                hwnd, "BUTTON", "Audio",
                BS_GROUPBOX, MARGIN, y, content_w, audio_group_h, -1, hfont,
            );
            y += 22;

            state.radio_audio_enable = create_control(
                hwnd, "BUTTON", "Enable audio",
                WS_GROUP | BS_AUTORADIOBUTTON,
                MARGIN + 12, y, content_w - 24, 22, ID_RADIO_AUDIO_ENABLE, hfont,
            );
            SendMessageW(state.radio_audio_enable, BM_SETCHECK, BST_CHECKED as usize, 0);
            y += 24;

            state.radio_audio_disable = create_control(
                hwnd, "BUTTON", "Disable audio",
                BS_AUTORADIOBUTTON,
                MARGIN + 12, y, content_w - 24, 22, ID_RADIO_AUDIO_DISABLE, hfont,
            );
            y += 26;

            state.radio_audio_system = create_control(
                hwnd, "BUTTON", "System audio (loopback)",
                WS_GROUP | BS_AUTORADIOBUTTON,
                MARGIN + 12, y, 180, 22, ID_RADIO_AUDIO_SYSTEM, hfont,
            );
            SendMessageW(state.radio_audio_system, BM_SETCHECK, BST_CHECKED as usize, 0);
            y += 24;

            state.radio_audio_mic = create_control(
                hwnd, "BUTTON", "Microphone",
                BS_AUTORADIOBUTTON,
                MARGIN + 12, y, 180, 22, ID_RADIO_AUDIO_MIC, hfont,
            );
            y += 26;

            create_control(
                hwnd, "STATIC", "Device:",
                0, MARGIN + 12, y + 3, 52, 20, -1, hfont,
            );
            state.combo_audio_device = create_control(
                hwnd, "COMBOBOX", "",
                CBS_DROPDOWNLIST | CBS_HASSTRINGS,
                MARGIN + 68, y - 2, content_w - 80, 200,
                ID_COMBO_AUDIO_DEVICE, hfont,
            );
            populate_audio_devices(state.combo_audio_device, &state.render_devices);
            y += audio_group_h - 22 - 24 - 26 - 24 - 26 - 26;

            // ── Viewer URL ──
            y += 8;
            let ip = local_ip_guess();
            let url = format!("http://{}:8080", ip);
            create_control(
                hwnd, "STATIC", "Viewer URL:",
                0, MARGIN, y + 3, 80, 20, -1, hfont,
            );
            state.static_url = create_control(
                hwnd, "STATIC", &url,
                SS_PATHELLIPSIS,
                MARGIN + 84, y + 3, 260, 20, ID_STATIC_URL, hfont,
            );
            create_control(
                hwnd, "BUTTON", "Copy",
                0, DLG_CX - MARGIN - 72, y - 1, 72, 28, ID_BTN_COPY, hfont,
            );
            y += 40;

            // ── Separator line ──
            create_control(
                hwnd, "STATIC", "",
                0x00000010, // SS_ETCHEDHORZ
                MARGIN, y, content_w, 2, -1, hfont,
            );
            y += 2 + MARGIN;

            // ── Bottom buttons ──
            let btn_w = 90;
            let btn_h = 34;
            create_control(
                hwnd, "BUTTON", "Quit",
                0,
                MARGIN, y, btn_w, btn_h, ID_BTN_QUIT, hfont,
            );
            create_control(
                hwnd, "BUTTON", "Start",
                BS_DEFPUSHBUTTON,
                DLG_CX - MARGIN - btn_w, y, btn_w, btn_h,
                ID_BTN_START, hfont,
            );
            y += btn_h + MARGIN;

            let mut rc = RECT {
                left: 0,
                top: 0,
                right: DLG_CX,
                bottom: y,
            };
            let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
            let exstyle = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
            AdjustWindowRectEx(&mut rc, style, 0, exstyle);
            MoveWindow(hwnd, 0, 0, rc.right - rc.left, rc.bottom - rc.top, 0);

            0
        }

        WM_ERASEBKGND => 1,

        WM_PAINT => {
            let state = &*get_state(hwnd);
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);

            let pr = &state.preview_rect;
            let area_w = pr.right - pr.left;
            let area_h = pr.bottom - pr.top;

            FillRect(hdc, pr, GetSysColorBrush(COLOR_WINDOW));

            let layouts = compute_preview_layout(&state.monitors, area_w, area_h);

            if layouts.is_empty() {
                let msg = to_wide("No monitors detected");
                SetBkMode(hdc, TRANSPARENT);
                SetTextColor(hdc, GetSysColor(COLOR_BTNTEXT) as u32);
                let mut tr = RECT {
                    left: pr.left,
                    top: pr.top,
                    right: pr.right,
                    bottom: pr.bottom,
                };
                DrawTextW(hdc, msg.as_ptr(), -1, &mut tr, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            } else {
                for (i, layout) in layouts.iter().enumerate() {
                    let ax = pr.left + layout.x;
                    let ay = pr.top + layout.y;

                    if let Some((pixels, pw, ph)) =
                        state.monitor_captures.get(i).and_then(|c| c.as_ref())
                    {
                        draw_bitmap_from_rgba(
                            hdc,
                            pixels,
                            *pw,
                            *ph,
                            ax,
                            ay,
                            layout.w,
                            layout.h,
                        );
                    } else {
                        let r = RECT {
                            left: ax,
                            top: ay,
                            right: ax + layout.w,
                            bottom: ay + layout.h,
                        };
                        FillRect(hdc, &r, GetSysColorBrush(COLOR_BTNFACE));
                    }

                    let is_sel = match state.capture_mode {
                        CaptureMode::Monitor(idx) => idx == i,
                        CaptureMode::Combined => true,
                        CaptureMode::Window => false,
                    };

                    let bc = if is_sel { COLOR_HIGHLIGHT } else { COLOR_BTNFACE };
                    let r = RECT {
                        left: ax,
                        top: ay,
                        right: ax + layout.w,
                        bottom: ay + layout.h,
                    };
                    let brush = CreateSolidBrush(GetSysColor(bc) as u32);
                    FrameRect(hdc, &r, brush);
                    DeleteObject(brush as _);

                    if is_sel {
                        let ir = RECT {
                            left: ax + 2,
                            top: ay + 2,
                            right: ax + layout.w - 2,
                            bottom: ay + layout.h - 2,
                        };
                        let ib = CreateSolidBrush(GetSysColor(bc) as u32);
                        FrameRect(hdc, &ir, ib);
                        DeleteObject(ib as _);
                    }

                    let bar_h = 22;
                    let bar_rect = RECT {
                        left: ax,
                        top: ay + layout.h - bar_h,
                        right: ax + layout.w,
                        bottom: ay + layout.h,
                    };
                    let bar_brush = CreateSolidBrush(0x00000000);
                    FillRect(hdc, &bar_rect, bar_brush);
                    DeleteObject(bar_brush as _);

                    let label = format!(
                        "Monitor {} \u{2013} {} \u{00D7} {}",
                        state.monitors[i].index,
                        state.monitors[i].width,
                        state.monitors[i].height,
                    );
                    let label_wide = to_wide(&label);
                    SetBkMode(hdc, TRANSPARENT);
                    SetTextColor(hdc, 0x00FFFFFF);
                    let mut tr = bar_rect;
                    DrawTextW(
                        hdc,
                        label_wide.as_ptr(),
                        -1,
                        &mut tr,
                        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                    );
                }
            }

            EndPaint(hwnd, &ps);
            0
        }

        WM_LBUTTONDOWN => {
            let state = &mut *get_state(hwnd);
            let mx = (lparam & 0xFFFF) as i16 as i32;
            let my = ((lparam >> 16) & 0xFFFF) as i16 as i32;

            let pr = &state.preview_rect;
            if mx >= pr.left && mx < pr.right && my >= pr.top && my < pr.bottom {
                let area_w = pr.right - pr.left;
                let area_h = pr.bottom - pr.top;
                let layouts = compute_preview_layout(&state.monitors, area_w, area_h);

                for (i, layout) in layouts.iter().enumerate() {
                    let ax = pr.left + layout.x;
                    let ay = pr.top + layout.y;
                    if mx >= ax && mx < ax + layout.w && my >= ay && my < ay + layout.h {
                        state.capture_mode = CaptureMode::Monitor(i);
                        SendMessageW(state.combined_radio, BM_SETCHECK, 0, 0);
                        SendMessageW(state.window_radio, BM_SETCHECK, 0, 0);
                        EnableWindow(state.combo_window, 0);
                        InvalidateRect(hwnd, pr, 0);
                        break;
                    }
                }
            }

            0
        }

        WM_COMMAND => {
            let id = (wparam & 0xFFFF) as i32;
            let hi = ((wparam >> 16) & 0xFFFF) as u16;
            let state = &mut *get_state(hwnd);

            if id == ID_RADIO_COMBINED {
                state.capture_mode = CaptureMode::Combined;
                EnableWindow(state.combo_window, 0);
                InvalidateRect(hwnd, &state.preview_rect, 0);
                0
            } else if id == ID_RADIO_WINDOW {
                state.capture_mode = CaptureMode::Window;
                EnableWindow(state.combo_window, 1);
                InvalidateRect(hwnd, &state.preview_rect, 0);
                0
            } else if id == ID_COMBO_WINDOW && hi == CBN_SELCHANGE {
                0
            } else if id == ID_RADIO_AUDIO_ENABLE {
                EnableWindow(state.combo_audio_device, 1);
                EnableWindow(state.radio_audio_system, 1);
                EnableWindow(state.radio_audio_mic, 1);
                0
            } else if id == ID_RADIO_AUDIO_DISABLE {
                EnableWindow(state.combo_audio_device, 0);
                EnableWindow(state.radio_audio_system, 0);
                EnableWindow(state.radio_audio_mic, 0);
                0
            } else if id == ID_RADIO_AUDIO_SYSTEM {
                populate_audio_devices(state.combo_audio_device, &state.render_devices);
                0
            } else if id == ID_RADIO_AUDIO_MIC {
                populate_audio_devices(state.combo_audio_device, &state.capture_devices);
                0
            } else if id == ID_COMBO_AUDIO_DEVICE && hi == CBN_SELCHANGE {
                0
            } else if id == ID_BTN_COPY {
                let mut buf = [0u16; 256];
                GetWindowTextW(state.static_url, buf.as_mut_ptr(), 256);
                let text = String::from_utf16_lossy(
                    &buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())],
                );
                if let Ok(mut ctx) = arboard::Clipboard::new() {
                    let _ = ctx.set_text(&text);
                }
                0
            } else if id == ID_BTN_START {
                let target = match state.capture_mode {
                    CaptureMode::Monitor(idx) => Target::Monitor(idx),
                    CaptureMode::Combined => Target::Combined,
                    CaptureMode::Window => {
                        let idx = SendMessageW(state.combo_window, CB_GETCURSEL, 0, 0);
                        if idx >= 0 && (idx as usize) < state.windows.len() {
                            Target::Window(state.windows[idx as usize].title.clone())
                        } else {
                            Target::Monitor(0)
                        }
                    }
                };

                let mut fps_buf = [0u16; 8];
                GetWindowTextW(state.edit_fps, fps_buf.as_mut_ptr(), 8);
                let fps_str: String = fps_buf
                    .iter()
                    .take_while(|&&c| c != 0)
                    .map(|&c| char::from_u32(c as u32).unwrap_or('0'))
                    .collect();
                let fps = fps_str.parse::<u32>().unwrap_or(60).clamp(1, 60);

                let mut pw_buf = [0u16; 128];
                GetWindowTextW(state.edit_password, pw_buf.as_mut_ptr(), 128);
                let password: String = pw_buf
                    .iter()
                    .take_while(|&&c| c != 0)
                    .map(|&c| char::from_u32(c as u32).unwrap_or('\0'))
                    .collect();

                if password.is_empty() {
                    let msg = to_wide("Password cannot be empty.");
                    let title = to_wide("ffscreencast");
                    MessageBoxW(hwnd, msg.as_ptr(), title.as_ptr(), 0x00000010);
                    return 0;
                }

                let audio_enabled =
                    IsDlgButtonChecked(hwnd, ID_RADIO_AUDIO_ENABLE) as u32 == BST_CHECKED;
                let source_type =
                    if IsDlgButtonChecked(hwnd, ID_RADIO_AUDIO_MIC) as u32 == BST_CHECKED {
                        AudioSourceType::Microphone
                    } else {
                        AudioSourceType::SystemAudio
                    };

                let device_id = if audio_enabled {
                    let idx = SendMessageW(state.combo_audio_device, CB_GETCURSEL, 0, 0);
                    let devices = match source_type {
                        AudioSourceType::SystemAudio => &state.render_devices,
                        AudioSourceType::Microphone => &state.capture_devices,
                    };
                    if idx >= 0 && (idx as usize) < devices.len() {
                        Some(devices[idx as usize].id.clone())
                    } else {
                        None
                    }
                } else {
                    None
                };

                state.result = Some(SettingsResult {
                    target,
                    fps,
                    host: "0.0.0.0".to_string(),
                    port: 8080,
                    password,
                    audio: AudioSettings {
                        enabled: audio_enabled,
                        source_type,
                        device_id,
                    },
                });
                DestroyWindow(hwnd);
                0
            } else if id == ID_BTN_QUIT {
                DestroyWindow(hwnd);
                0
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }

        WM_CTLCOLOREDIT => {
            let hdc = wparam as HDC;
            SetTextColor(hdc, GetSysColor(COLOR_WINDOWTEXT) as u32);
            SetBkColor(hdc, GetSysColor(COLOR_WINDOW) as u32);
            GetSysColorBrush(COLOR_WINDOW) as LRESULT
        }

        WM_CTLCOLORSTATIC => {
            let hdc = wparam as HDC;
            SetTextColor(hdc, GetSysColor(COLOR_BTNTEXT) as u32);
            SetBkColor(hdc, GetSysColor(COLOR_WINDOW) as u32);
            GetSysColorBrush(COLOR_WINDOW) as LRESULT
        }

        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub fn run_settings() -> Option<SettingsResult> {
    let monitors = Capture::list_monitors().unwrap_or_default();
    let windows = Capture::list_windows().unwrap_or_default();
    let default_monitor = monitors.iter().position(|m| m.is_primary).unwrap_or(0);

    let render_devices = crate::audio::list_audio_devices(&wasapi::Direction::Render);
    let capture_devices = crate::audio::list_audio_devices(&wasapi::Direction::Capture);

    logln!(
        "[dialog] found {} render devices, {} capture devices",
        render_devices.len(),
        capture_devices.len()
    );

    let mut state = DialogState {
        monitors,
        windows,
        combo_window: ptr::null_mut(),
        combined_radio: ptr::null_mut(),
        window_radio: ptr::null_mut(),
        edit_fps: ptr::null_mut(),
        edit_password: ptr::null_mut(),
        static_url: ptr::null_mut(),
        preview_rect: RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        },
        hfont: ptr::null_mut(),
        monitor_captures: Vec::new(),
        capture_mode: CaptureMode::Monitor(default_monitor),
        radio_audio_enable: ptr::null_mut(),
        radio_audio_disable: ptr::null_mut(),
        radio_audio_system: ptr::null_mut(),
        radio_audio_mic: ptr::null_mut(),
        combo_audio_device: ptr::null_mut(),
        render_devices,
        capture_devices,
        result: None,
    };

    unsafe {
        let hinst = GetModuleHandleW(ptr::null());

        let class_wide = to_wide(CLASS_NAME);
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: ptr::null_mut(),
            hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
            hbrBackground: ((COLOR_WINDOW + 1) as *mut core::ffi::c_void),
            lpszMenuName: ptr::null(),
            lpszClassName: class_wide.as_ptr(),
            hIconSm: ptr::null_mut(),
        };
        RegisterClassExW(&wc);

        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let screen_h = GetSystemMetrics(SM_CYSCREEN);
        let dlg_cy = 740;
        let x = (screen_w - DLG_CX) / 2;
        let y = (screen_h - dlg_cy) / 2;

        let title_wide = to_wide("ffscreencast - Settings");
        let hwnd = CreateWindowExW(
            0,
            class_wide.as_ptr(),
            title_wide.as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
            x,
            y,
            DLG_CX,
            dlg_cy,
            ptr::null_mut(),
            ptr::null_mut(),
            hinst,
            &mut state as *mut DialogState as *mut _,
        );

        if hwnd == ptr::null_mut() {
            return None;
        }

        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            if IsDialogMessageW(hwnd, &mut msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&mut msg);
            }
        }

        state.result
    }
}
