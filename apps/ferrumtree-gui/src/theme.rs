//! Light/dark palettes and Windows dark-mode plumbing.
//!
//! * The system preference comes from `HKCU\...\Themes\Personalize\AppsUseLightTheme`.
//! * Title bar: `DwmSetWindowAttribute(DWMWA_USE_IMMERSIVE_DARK_MODE)`.
//! * Menus, scroll bars, list views, combo boxes: the uxtheme exports used by Explorer and
//!   Notepad++ (`SetPreferredAppMode` #135, `AllowDarkModeForWindow` #133,
//!   `FlushMenuThemes` #136) plus the `DarkMode_Explorer` / `DarkMode_CFD` visual styles.
//!   They are resolved at run time; when missing, everything stays light and still works.

use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows_sys::Win32::UI::Controls::SetWindowTheme;
use windows_sys::Win32::UI::WindowsAndMessaging::{SendMessageW, WM_THEMECHANGED};

use crate::util::wide;

/// Colours as COLORREF (`0x00BBGGRR`) for GDI.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub dark: bool,
    pub bg: u32,
    pub panel: u32,
    pub input: u32,
    pub list: u32,
    pub text: u32,
    pub dim: u32,
    pub border: u32,
    pub accent: u32,
    pub hover: u32,
    pub bar: u32,
    pub bar_bg: u32,
    pub sel: u32,
    pub sel_text: u32,
    /// Treemap colours, `0x00RRGGBB` (DIB order).
    pub map_bg: u32,
    pub map_border: u32,
    pub map_header: [u32; 3],
    pub map_text: u32,
}

pub const LIGHT: Palette = Palette {
    dark: false,
    bg: 0xF3F3F3,
    panel: 0xFBFBFB,
    input: 0xFFFFFF,
    list: 0xFFFFFF,
    text: 0x1A1A1A,
    dim: 0x7A7A7A,
    border: 0xD5D5D5,
    accent: 0xD47800,
    hover: 0xE8E8E8,
    bar: 0xB8B8B8,
    bar_bg: 0xEDEDED,
    sel: 0xF7E4CC,
    sel_text: 0x1A1A1A,
    map_bg: 0xF0F0F0,
    map_border: 0x9A9A9A,
    map_header: [0xDCDCDC, 0xD2D2D2, 0xC8C8C8],
    map_text: 0x1A1A1A,
};

pub const DARK: Palette = Palette {
    dark: true,
    bg: 0x202020,
    panel: 0x2B2B2B,
    input: 0x1C1C1C,
    list: 0x191919,
    text: 0xE6E6E6,
    dim: 0x8C8C8C,
    border: 0x3F3F3F,
    accent: 0xD47800,
    hover: 0x383838,
    bar: 0x7A7A7A,
    bar_bg: 0x2E2E2E,
    sel: 0x5C3A17,
    sel_text: 0xFFFFFF,
    map_bg: 0x101010,
    map_border: 0x0C0C0C,
    map_header: [0x3C3C3C, 0x343434, 0x2C2C2C],
    map_text: 0xE6E6E6,
};

/// Does Windows ask apps for the dark theme?
pub fn system_dark() -> bool {
    let key = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let val = wide("AppsUseLightTheme");
    let mut data = 1u32;
    let mut len = 4u32;
    // SAFETY: DWORD out-parameter of the stated size.
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut data as *mut u32).cast(),
            &mut len,
        )
    };
    r == 0 && data == 0
}

type SetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
type AllowDarkModeForWindow = unsafe extern "system" fn(HWND, i32) -> i32;
type FlushMenuThemes = unsafe extern "system" fn();

struct UxTheme {
    set_mode: Option<SetPreferredAppMode>,
    allow: Option<AllowDarkModeForWindow>,
    flush: Option<FlushMenuThemes>,
}

fn ux() -> &'static UxTheme {
    static U: OnceLock<UxTheme> = OnceLock::new();
    U.get_or_init(|| {
        let name = wide("uxtheme.dll");
        // SAFETY: uxtheme is a system DLL; ordinals are looked up, and each pointer is
        // only used with the signature those exports have had since Windows 10 1903.
        unsafe {
            let h = LoadLibraryW(name.as_ptr());
            if h.is_null() {
                return UxTheme { set_mode: None, allow: None, flush: None };
            }
            let get = |ord: usize| GetProcAddress(h, ord as *const u8);
            UxTheme {
                set_mode: get(135).map(|f| std::mem::transmute::<_, SetPreferredAppMode>(f)),
                allow: get(133).map(|f| std::mem::transmute::<_, AllowDarkModeForWindow>(f)),
                flush: get(136).map(|f| std::mem::transmute::<_, FlushMenuThemes>(f)),
            }
        }
    })
}

/// Process-wide: popup menus follow `dark`.
pub fn set_app_dark(dark: bool) {
    let u = ux();
    // SAFETY: see `ux`. 2 = ForceDark, 3 = ForceLight.
    unsafe {
        if let Some(f) = u.set_mode {
            f(if dark { 2 } else { 3 });
        }
        if let Some(f) = u.flush {
            f();
        }
    }
}

/// Title bar colour of a top-level window, and the theme of the popup menus it owns
/// (ours and the shell's context menus are dark only for a window allowed dark mode).
pub fn set_title_dark(hwnd: HWND, dark: bool) {
    let v: i32 = dark as i32;
    // SAFETY: BOOL-sized attribute value. 20 = DWMWA_USE_IMMERSIVE_DARK_MODE; builds before
    // 20H1 used 19.
    unsafe {
        if let Some(f) = ux().allow {
            f(hwnd, dark as i32);
        }
        if DwmSetWindowAttribute(hwnd, 20, (&v as *const i32).cast(), 4) != 0 {
            DwmSetWindowAttribute(hwnd, 19, (&v as *const i32).cast(), 4);
        }
        // Windows 10 repaints the caption colour only on an activation change.
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, WM_NCACTIVATE};
        let active = GetForegroundWindow() == hwnd;
        SendMessageW(hwnd, WM_NCACTIVATE, (!active) as usize, 0);
        SendMessageW(hwnd, WM_NCACTIVATE, active as usize, 0);
    }
}

/// Visual style class for a control kind.
#[derive(Clone, Copy)]
pub enum Kind {
    /// List views, tree-like views, scroll bars.
    Explorer,
    /// Combo boxes and edits.
    Cfd,
    /// List view headers.
    ItemsView,
}

pub fn style_control(hwnd: HWND, dark: bool, kind: Kind) {
    let name = match (dark, kind) {
        (true, Kind::Explorer) => "DarkMode_Explorer",
        (true, Kind::Cfd) => "DarkMode_CFD",
        (true, Kind::ItemsView) => "DarkMode_ItemsView",
        (false, Kind::Explorer | Kind::ItemsView) => "Explorer",
        (false, Kind::Cfd) => "CFD",
    };
    let w = wide(name);
    // SAFETY: plain calls on our own child windows.
    unsafe {
        if let Some(f) = ux().allow {
            f(hwnd, dark as i32);
        }
        SetWindowTheme(hwnd, w.as_ptr(), std::ptr::null());
        SendMessageW(hwnd, WM_THEMECHANGED, 0 as WPARAM, 0 as LPARAM);
    }
}
