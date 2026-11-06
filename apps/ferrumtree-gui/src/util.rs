//! Small Win32 helpers: UTF-16 strings, GDI drawing without per-call objects, dates.

use windows_sys::Win32::Foundation::{FILETIME, HWND, RECT, SIZE, SYSTEMTIME};
use windows_sys::Win32::Graphics::Gdi::{
    DC_BRUSH, DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DrawTextW, FillRect, GetStockObject, GetTextExtentPoint32W, HBRUSH,
    HDC, SetBkMode, SetDCBrushColor, SetTextColor, TRANSPARENT,
};
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetClientRect, SendMessageW};

use crate::lang::{Lang, lang};

/// NUL-terminated UTF-16.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// # Safety
/// `h` must be a window; `l` must be valid for `msg`.
pub unsafe fn send(h: HWND, msg: u32, w: usize, l: isize) -> isize {
    unsafe { SendMessageW(h, msg, w, l) }
}

pub fn rect(l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT { left: l, top: t, right: r, bottom: b }
}

pub fn client(h: HWND) -> RECT {
    let mut r = rect(0, 0, 0, 0);
    // SAFETY: plain out-parameter.
    unsafe { GetClientRect(h, &mut r) };
    r
}

pub fn contains(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// Fill with a solid colour through the DC brush (no GDI object created).
pub fn fill(dc: HDC, r: &RECT, color: u32) {
    // SAFETY: DC_BRUSH is a stock object; its colour is per-DC state.
    unsafe {
        SetDCBrushColor(dc, color);
        FillRect(dc, r, GetStockObject(DC_BRUSH) as HBRUSH);
    }
}

/// 1 px outline.
pub fn frame(dc: HDC, r: &RECT, color: u32) {
    fill(dc, &rect(r.left, r.top, r.right, r.top + 1), color);
    fill(dc, &rect(r.left, r.bottom - 1, r.right, r.bottom), color);
    fill(dc, &rect(r.left, r.top, r.left + 1, r.bottom), color);
    fill(dc, &rect(r.right - 1, r.top, r.right, r.bottom), color);
}

/// Single-line text, vertically centred, ellipsis when too long. `extra` adds DT_* flags
/// (e.g. DT_RIGHT, DT_CENTER).
pub fn text(dc: HDC, s: &str, r: &RECT, color: u32, extra: u32) {
    // Nothing to draw. (An empty Vec has a dangling pointer, and DrawTextW with the
    // ellipsis flags reads the string even when the count is 0.)
    if s.is_empty() || r.right <= r.left || r.bottom <= r.top {
        return;
    }
    // NUL-terminated, with room for the ellipsis variants' internal copy.
    let w: Vec<u16> = s.encode_utf16().chain([0]).collect();
    let mut rc = *r;
    // SAFETY: the buffer and rectangle outlive the call.
    unsafe {
        SetBkMode(dc, TRANSPARENT as i32);
        SetTextColor(dc, color);
        DrawTextW(dc, w.as_ptr(), (w.len() - 1) as i32, &mut rc, DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS | extra);
    }
}

pub fn text_width(dc: HDC, s: &str) -> i32 {
    let w: Vec<u16> = s.encode_utf16().collect();
    let mut sz = SIZE { cx: 0, cy: 0 };
    // SAFETY: plain out-parameter.
    unsafe { GetTextExtentPoint32W(dc, w.as_ptr(), w.len() as i32, &mut sz) };
    sz.cx
}

/// FILETIME (UTC, 100 ns since 1601) -> local date and time.
pub fn filetime(ft: u64) -> String {
    if ft == 0 {
        return String::new();
    }
    let f = FILETIME { dwLowDateTime: ft as u32, dwHighDateTime: (ft >> 32) as u32 };
    // SAFETY: plain out-parameters.
    unsafe {
        let mut utc: SYSTEMTIME = std::mem::zeroed();
        let mut l: SYSTEMTIME = std::mem::zeroed();
        if FileTimeToSystemTime(&f, &mut utc) == 0 || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut l) == 0 {
            return String::new();
        }
        match lang() {
            Lang::Ru => format!("{:02}.{:02}.{:04} {:02}:{:02}", l.wDay, l.wMonth, l.wYear, l.wHour, l.wMinute),
            Lang::En => format!("{:04}-{:02}-{:02} {:02}:{:02}", l.wYear, l.wMonth, l.wDay, l.wHour, l.wMinute),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(filetime(0), "");
        assert!(filetime(133_000_000_000_000_000).contains("2022"));
    }
}
