//! The treemap window: double-buffered painting of the rendered pixels, folder labels in
//! the header bands, labels on large files, and the selection frame.

use std::ptr::null;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_BACK, VK_ESCAPE};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::app::with;
use crate::cmds;
use crate::util::{client, fill, frame, rect, text};

pub const CLASS: &str = "FerrumTreemap";
const TIMER_LAYOUT: usize = 2;

fn x_of(l: LPARAM) -> i32 {
    (l & 0xFFFF) as i16 as i32
}
fn y_of(l: LPARAM) -> i32 {
    ((l >> 16) & 0xFFFF) as i16 as i32
}

/// Lay the treemap out again shortly (live changes come in bursts).
pub fn rebuild_later(hwnd: HWND) {
    // SAFETY: timer on our own window.
    unsafe { SetTimer(hwnd, TIMER_LAYOUT, 250, None) };
}

pub fn rebuild(hwnd: HWND) {
    let r = client(hwnd);
    with(|a| a.rebuild_treemap(r.right, r.bottom));
    // SAFETY: plain invalidation.
    unsafe { InvalidateRect(hwnd, null(), 0) };
}

fn paint(hwnd: HWND) {
    // SAFETY: standard WM_PAINT; every GDI object created here is released here.
    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let dc = BeginPaint(hwnd, &mut ps);
        let c = client(hwnd);
        let (cw, ch) = (c.right, c.bottom);
        let mem = CreateCompatibleDC(dc);
        let bmp = CreateCompatibleBitmap(dc, cw.max(1), ch.max(1));
        let old_bmp = SelectObject(mem, bmp);
        with(|a| {
            let (tw, th) = (a.tm.width as i32, a.tm.height as i32);
            if tw == 0 || th == 0 || a.tm_px.len() < (tw * th) as usize {
                fill(mem, &c, a.pal.map_bg);
                return;
            }
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = tw;
            bmi.bmiHeader.biHeight = -th;
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB;
            StretchDIBits(mem, 0, 0, cw, ch, 0, 0, tw, th, a.tm_px.as_ptr().cast(), &bmi, DIB_RGB_COLORS, SRCCOPY);
            let (sx, sy) = (cw as f32 / tw as f32, ch as f32 / th as f32);
            let to_rc = |r: [f32; 4]| rect((r[0] * sx) as i32, (r[1] * sy) as i32, (r[2] * sx) as i32, (r[3] * sy) as i32);
            let Some(snap) = a.snap.as_ref() else { return };
            let ix = &snap.ix;
            if a.settings.map_labels && (sx - 1.0).abs() < 0.01 && (sy - 1.0).abs() < 0.01 {
                let old_font = SelectObject(mem, a.small);
                let hb = a.tm.header as i32;
                let rgb = |c: u32| (c & 0xFF) << 16 | (c & 0xFF00) | (c >> 16 & 0xFF);
                let label_color = rgb(a.pal.map_text);
                for f in a.tm.headers() {
                    let r = to_rc(f.rect);
                    if r.right - r.left < a.scale(36) {
                        continue;
                    }
                    let name = if f.node == a.tm_root { ix.path(f.node) } else { ix.name(f.node).to_string() };
                    let sep = if name.ends_with('\\') { "" } else { "\\" };
                    let label = format!(
                        "{name}{sep}  ({})",
                        a.size(if a.settings.map_alloc { ix.alloc[f.node as usize] } else { ix.size[f.node as usize] })
                    );
                    text(mem, &label, &rect(r.left + 4, r.top, r.right - 3, r.top + hb), label_color, 0);
                }
                // Large files get their name in the corner, with a shadow for contrast.
                let (min_w, min_h) = (a.scale(90), a.scale(30));
                for tile in a.tm.tiles.iter().filter(|t| !ix.is_dir(t.node)) {
                    let r = to_rc(tile.rect);
                    if r.right - r.left < min_w || r.bottom - r.top < min_h {
                        continue;
                    }
                    let i = tile.node as usize;
                    let label = format!("{} ({})", ix.name(tile.node), a.size(if a.settings.map_alloc { ix.alloc[i] } else { ix.size[i] }));
                    let lr = rect(r.left + 5, r.top + 3, r.right - 4, r.top + 3 + a.scale(16));
                    text(mem, &label, &rect(lr.left + 1, lr.top + 1, lr.right + 1, lr.bottom + 1), 0x000000, 0);
                    text(mem, &label, &lr, 0xFFFFFF, 0);
                }
                SelectObject(mem, old_font);
            }
            if let Some(r) = a.selected.and_then(|(n, _)| a.tm.rect_of(n)) {
                let mut rc = to_rc(r);
                rc.right = rc.right.max(rc.left + 3);
                rc.bottom = rc.bottom.max(rc.top + 3);
                let s = a.scale(1).max(1);
                for k in 0..s {
                    frame(mem, &rect(rc.left - k, rc.top - k, rc.right + k, rc.bottom + k), 0x000000);
                }
                for k in s..s * 3 {
                    frame(mem, &rect(rc.left - k, rc.top - k, rc.right + k, rc.bottom + k), 0xFFFFFF);
                }
            }
        });
        BitBlt(dc, 0, 0, cw, ch, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old_bmp);
        DeleteObject(bmp);
        DeleteDC(mem);
        EndPaint(hwnd, &ps);
    }
}

fn hit(hwnd: HWND, l: LPARAM) -> Option<u32> {
    let c = client(hwnd);
    with(|a| a.map_hit(x_of(l), y_of(l), c.right, c.bottom)).flatten()
}

pub unsafe extern "system" fn proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                paint(hwnd);
                0
            }
            WM_ERASEBKGND => 1,
            WM_SIZE => {
                // Re-layout once resizing settles; meanwhile the old picture is stretched.
                SetTimer(hwnd, TIMER_LAYOUT, 90, None);
                InvalidateRect(hwnd, null(), 0);
                0
            }
            WM_TIMER if w == TIMER_LAYOUT => {
                KillTimer(hwnd, TIMER_LAYOUT);
                rebuild(hwnd);
                0
            }
            WM_MOUSEMOVE => {
                if let Some(n) = hit(hwnd, l) {
                    cmds::hover(n);
                }
                0
            }
            WM_LBUTTONDOWN => {
                SetFocus(hwnd);
                if let Some(n) = hit(hwnd, l) {
                    cmds::reveal(n);
                }
                0
            }
            WM_LBUTTONDBLCLK => {
                if let Some(n) = hit(hwnd, l) {
                    cmds::zoom_towards(n);
                }
                0
            }
            WM_RBUTTONUP => {
                if let Some(n) = hit(hwnd, l) {
                    cmds::reveal(n);
                    cmds::context_menu(Some(n));
                }
                0
            }
            WM_KEYDOWN if w as u16 == VK_BACK || w as u16 == VK_ESCAPE => {
                cmds::zoom_out();
                0
            }
            WM_GETDLGCODE => DLGC_WANTALLKEYS as isize,
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}
