//! FerrumTree - native Win32 disk space analyzer.
//!
//! Modules: `engine` (background scanning), `app` (state), `model` (tree rows), `list`
//! (virtual list views), `map` (treemap window), `ui` (layout and painted chrome), `cmds`
//! (commands, menus, file operations), `theme`, `lang`, `settings`, `png`, `util`.

#![windows_subsystem = "windows"]

mod app;
mod cmds;
mod dialog;
mod engine;
mod lang;
mod list;
mod map;
mod model;
mod png;
mod settings;
mod shell;
mod theme;
mod ui;
mod util;

use std::cell::RefCell;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Ole::OleInitialize;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_ESCAPE, VK_F1, VK_F10, VK_F24,
};
use windows_sys::Win32::UI::Shell::{SHFILEINFOW, SHGFI_SMALLICON, SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW};
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use windows_sys::core::PCWSTR;

use app::{App, Drag, Wnd, with};
use engine::{Target, WM_ENGINE};
use settings::Settings;
use util::{send, wide};

const ID_COMBO: i32 = 101;
const ID_SCAN: i32 = 102;
const ID_SEARCH: i32 = 103;
const ID_CLEAR: i32 = 104;
const MAIN_CLASS: &str = "FerrumTreeMain";

thread_local! {
    static PENDING: RefCell<Option<Settings>> = const { RefCell::new(None) };
}

fn loword(v: usize) -> u32 {
    (v & 0xFFFF) as u32
}
fn hiword(v: usize) -> u32 {
    ((v >> 16) & 0xFFFF) as u32
}
fn x_of(l: LPARAM) -> i32 {
    (l & 0xFFFF) as i16 as i32
}
fn y_of(l: LPARAM) -> i32 {
    ((l >> 16) & 0xFFFF) as i16 as i32
}

/// Drives (no empty optical drives) as scan targets, labelled like "[C:] System".
fn drive_targets() -> Vec<(Target, String)> {
    let mut out = Vec::new();
    // SAFETY: plain Win32 queries with local buffers.
    unsafe {
        let mask = GetLogicalDrives();
        for i in 0..26u32 {
            if mask >> i & 1 == 0 {
                continue;
            }
            let letter = (b'A' + i as u8) as char;
            let root = wide(&format!("{letter}:\\"));
            let kind = GetDriveTypeW(root.as_ptr());
            if kind == 1 || kind == 5 {
                continue; // no root dir, CD-ROM
            }
            let mut label = [0u16; 64];
            if GetVolumeInformationW(root.as_ptr(), label.as_mut_ptr(), 64, null_mut(), null_mut(), null_mut(), null_mut(), 0) == 0 {
                continue;
            }
            let n = label.iter().position(|&c| c == 0).unwrap_or(0);
            let l = String::from_utf16_lossy(&label[..n]);
            out.push((Target::Drive(letter), format!("[{letter}:] {}", if l.is_empty() { "Local Disk".into() } else { l })));
        }
    }
    out
}

unsafe fn child(class: PCWSTR, text: &str, style: u32, id: i32, parent: HWND, ex: u32) -> HWND {
    let t = wide(text);
    unsafe {
        CreateWindowExW(
            ex,
            class,
            t.as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | style,
            0,
            0,
            10,
            10,
            parent,
            id as isize as HMENU,
            GetModuleHandleW(null()),
            null(),
        )
    }
}

unsafe fn create(hwnd: HWND) {
    unsafe {
        let settings = PENDING.with(|p| p.borrow_mut().take()).unwrap_or_default();
        let dpi = GetDpiForWindow(hwnd).max(96);
        let fonts = ui::make_fonts(dpi);
        let pal = ui::palette_for(settings.theme);
        let combo = child(
            WC_COMBOBOXW,
            "",
            CBS_DROPDOWNLIST as u32 | CBS_OWNERDRAWFIXED as u32 | CBS_HASSTRINGS as u32 | WS_VSCROLL | WS_TABSTOP,
            ID_COMBO,
            hwnd,
            0,
        );
        let scan = child(WC_BUTTONW, "", BS_OWNERDRAW as u32 | WS_TABSTOP, ID_SCAN, hwnd, 0);
        let search = child(WC_EDITW, "", ES_AUTOHSCROLL as u32 | WS_TABSTOP, ID_SEARCH, hwnd, 0);
        let clear = child(WC_BUTTONW, "", BS_OWNERDRAW as u32 | WS_TABSTOP, ID_CLEAR, hwnd, 0);
        let lv_style = LVS_REPORT | LVS_OWNERDATA | LVS_SINGLESEL | LVS_SHOWSELALWAYS | LVS_SHAREIMAGELISTS | WS_TABSTOP;
        let lst = child(WC_LISTVIEWW, "", lv_style, list::ID_LIST as i32, hwnd, 0);
        let types = child(WC_LISTVIEWW, "", lv_style, list::ID_TYPES as i32, hwnd, 0);
        let mc = wide(map::CLASS);
        let mapw = child(mc.as_ptr(), "", WS_TABSTOP, 0, hwnd, 0);
        let cue = wide(lang::t(lang::S::SearchCue));
        send(search, EM_SETCUEBANNER, 1, cue.as_ptr() as isize);
        for c in [combo, scan, search, clear, lst, types] {
            send(c, WM_SETFONT, fonts.0 as usize, 0);
        }
        // The shared system image list: per-type icons, and indentation by depth.
        let probe = wide("folder");
        let mut info: SHFILEINFOW = std::mem::zeroed();
        let sys = SHGetFileInfoW(
            probe.as_ptr(),
            0x10,
            &mut info,
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES,
        );
        list::setup(lst, types, sys as isize);
        list::set_columns(lst, app::columns(app::Mode::Tree), dpi);
        list::set_type_columns(types, dpi);

        let mut targets = drive_targets();
        let last = settings.last_target.clone();
        if let Some(t) = last.as_deref().and_then(Target::parse)
            && !targets.iter().any(|x| x.0 == t)
        {
            targets.push((t.clone(), t.label()));
        }
        let pick = last
            .as_deref()
            .and_then(Target::parse)
            .and_then(|t| targets.iter().position(|x| x.0 == t))
            .or_else(|| targets.iter().position(|x| x.0 == Target::Drive('C')))
            .unwrap_or(0);
        let w = Wnd { main: hwnd, combo, scan, search, clear, list: lst, types, map: mapw };
        let brush = CreateSolidBrush(pal.input);
        app::install(App::new(w, dpi, fonts, pal, brush, settings, cmds::accelerators(), targets));
        cmds::fill_combo(pick);
        let ih = (22 * dpi / 96) as isize;
        send(combo, CB_SETITEMHEIGHT, usize::MAX, ih);
        send(combo, CB_SETITEMHEIGHT, 0, ih);
        ui::apply_theme();
        ui::layout(hwnd);
        // Scan once the window is up.
        PostMessageW(hwnd, WM_COMMAND, cmds::CMD_SCAN, 0);
    }
}

fn drag_to(x: i32, y: i32) {
    let main = with(|a| {
        let l = a.layout.clone();
        match a.drag {
            Some(Drag::Vertical) => {
                let span = (l.types.right - l.list.left - a.scale(6)).max(1);
                a.settings.split_types = ((x - l.list.left) as f32 / span as f32).clamp(0.2, 0.85);
            }
            Some(Drag::Horizontal) => {
                let span = (l.map.bottom - l.list.top - a.scale(28)).max(1);
                a.settings.split_map = ((y - l.list.top) as f32 / span as f32).clamp(0.15, 0.85);
            }
            None => {}
        }
        a.w.main
    });
    if let Some(m) = main {
        ui::layout(m);
    }
}

fn save_placement(hwnd: HWND) {
    // SAFETY: plain struct out-parameter.
    unsafe {
        let mut wp: WINDOWPLACEMENT = std::mem::zeroed();
        wp.length = size_of::<WINDOWPLACEMENT>() as u32;
        if GetWindowPlacement(hwnd, &mut wp) != 0 {
            let r = wp.rcNormalPosition;
            let max = wp.showCmd == SW_SHOWMAXIMIZED as u32;
            with(|a| {
                a.settings.window = Some((r.left, r.top, r.right - r.left, r.bottom - r.top, max));
                a.settings.save();
            });
        }
    }
}

unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if let Some(r) = shell::menu_message(msg, w, l) {
        return r;
    }
    unsafe {
        match msg {
            WM_CREATE => {
                create(hwnd);
                0
            }
            WM_SIZE => {
                ui::layout(hwnd);
                0
            }
            WM_GETMINMAXINFO => {
                let mm = &mut *(l as *mut MINMAXINFO);
                mm.ptMinTrackSize = POINT { x: 900, y: 560 };
                0
            }
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                ui::paint(hwnd);
                0
            }
            WM_DRAWITEM => ui::draw_item(&*(l as *const DRAWITEMSTRUCT)) as isize,
            WM_MEASUREITEM => {
                let mi = &mut *(l as *mut MEASUREITEMSTRUCT);
                mi.itemHeight = 22 * GetDpiForWindow(hwnd).max(96) / 96;
                1
            }
            WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
                let dc = w as HDC;
                match with(|a| (a.pal, a.brush_input)) {
                    Some((p, b)) => {
                        SetTextColor(dc, p.text);
                        SetBkColor(dc, p.input);
                        b as isize
                    }
                    None => DefWindowProcW(hwnd, msg, w, l),
                }
            }
            WM_COMMAND => {
                let (id, code) = (loword(w) as i32, hiword(w));
                match (id, code) {
                    (ID_SCAN, BN_CLICKED) => cmds::start_scan(false),
                    (ID_CLEAR, BN_CLICKED) => cmds::clear_search(),
                    (ID_COMBO, CBN_SELCHANGE) => {
                        let auto = with(|a| a.settings.auto_scan).unwrap_or(true);
                        match cmds::combo_choice() {
                            Some(None) => cmds::pick_folder(),
                            Some(Some(_)) if auto => cmds::start_scan(false),
                            _ => {}
                        }
                    }
                    (ID_SEARCH, EN_CHANGE) => {
                        SetTimer(hwnd, cmds::TIMER_SEARCH, 140, None);
                    }
                    _ if id as usize >= cmds::CMD_SCAN && id < 1000 => cmds::run(id as usize),
                    _ => {}
                }
                0
            }
            WM_TIMER => {
                match w {
                    cmds::TIMER_SEARCH => {
                        KillTimer(hwnd, cmds::TIMER_SEARCH);
                        cmds::search_changed();
                    }
                    cmds::TIMER_PROGRESS => {
                        if let Some((busy, top)) = with(|a| (a.busy, a.layout.top)) {
                            if busy {
                                InvalidateRect(hwnd, &top, 0);
                            } else {
                                KillTimer(hwnd, cmds::TIMER_PROGRESS);
                            }
                        }
                    }
                    _ => {}
                }
                0
            }
            WM_ACTIVATEAPP => {
                engine::set_active(w != 0);
                0
            }
            WM_NOTIFY => list::on_notify(l).unwrap_or(0),
            WM_ENGINE => {
                cmds::on_engine(engine::take(l));
                0
            }
            cmds::WM_DUPES => {
                cmds::on_dupes_msg(l);
                0
            }
            cmds::WM_DONE => {
                cmds::on_done(*Box::from_raw(l as *mut String));
                0
            }
            WM_LBUTTONDOWN => {
                match ui::hit(x_of(l), y_of(l)) {
                    ui::Hit::Menu(i) => cmds::show_menu(i),
                    ui::Hit::Tab(m) => cmds::set_mode(m),
                    ui::Hit::Split(d) => {
                        with(|a| a.drag = Some(d));
                        SetCapture(hwnd);
                    }
                    ui::Hit::Crumb => cmds::zoom_out(),
                    ui::Hit::None => {}
                }
                0
            }
            WM_MOUSEMOVE => {
                if with(|a| a.drag.is_some()).unwrap_or(false) {
                    drag_to(x_of(l), y_of(l));
                } else {
                    let hot = match ui::hit(x_of(l), y_of(l)) {
                        ui::Hit::Menu(i) => Some(i),
                        _ => None,
                    };
                    let changed = with(|a| std::mem::replace(&mut a.menu_hot, hot) != hot).unwrap_or(false);
                    if changed {
                        InvalidateRect(hwnd, null(), 0);
                    }
                    if hot.is_some() {
                        let mut tme = TRACKMOUSEEVENT {
                            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE,
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        };
                        TrackMouseEvent(&mut tme);
                    }
                }
                0
            }
            WM_MOUSELEAVE => {
                if with(|a| a.menu_hot.take().is_some()).unwrap_or(false) {
                    InvalidateRect(hwnd, null(), 0);
                }
                0
            }
            WM_LBUTTONUP => {
                if with(|a| a.drag.take().is_some()).unwrap_or(false) {
                    ReleaseCapture();
                    with(|a| a.settings.save());
                }
                0
            }
            WM_SETCURSOR => {
                let mut pt = POINT { x: 0, y: 0 };
                GetCursorPos(&mut pt);
                ScreenToClient(hwnd, &mut pt);
                if w as HWND == hwnd {
                    let cur = match ui::hit(pt.x, pt.y) {
                        ui::Hit::Split(Drag::Vertical) => Some(IDC_SIZEWE),
                        ui::Hit::Split(Drag::Horizontal) => Some(IDC_SIZENS),
                        ui::Hit::Menu(_) | ui::Hit::Tab(_) | ui::Hit::Crumb => Some(IDC_HAND),
                        ui::Hit::None => None,
                    };
                    if let Some(c) = cur {
                        SetCursor(LoadCursorW(null_mut(), c));
                        return 1;
                    }
                }
                DefWindowProcW(hwnd, msg, w, l)
            }
            WM_SYSKEYDOWN => {
                let menu = match w as u16 {
                    0x46 => Some(ui::MENU_FILE), // F
                    0x56 => Some(ui::MENU_VIEW), // V
                    0x48 => Some(ui::MENU_HELP), // H
                    _ => None,
                };
                if let Some(m) = menu {
                    cmds::show_menu(m);
                    return 0;
                }
                if w as u16 == VK_F10 {
                    cmds::run(cmds::CMD_ZOOM_IN);
                    return 0;
                }
                DefWindowProcW(hwnd, msg, w, l)
            }
            WM_SYSCHAR if char::from_u32(w as u32).is_some_and(|c| c.is_alphabetic()) => 0,
            WM_SETTINGCHANGE => {
                if l != 0 {
                    let p = l as *const u16;
                    let n = (0..64).take_while(|&i| *p.add(i) != 0).count();
                    let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
                    let follow = with(|a| a.settings.theme == settings::ThemePref::System).unwrap_or(false);
                    if s == "ImmersiveColorSet" && follow {
                        with(|a| a.pal = ui::palette_for(a.settings.theme));
                        ui::apply_theme();
                    }
                }
                DefWindowProcW(hwnd, msg, w, l)
            }
            WM_DPICHANGED => {
                let dpi = hiword(w);
                let fonts = ui::make_fonts(dpi);
                let old = with(|a| {
                    a.dpi = dpi;
                    let old = (a.font, a.bold, a.small);
                    (a.font, a.bold, a.small) = fonts;
                    (old, a.w, a.cols)
                });
                let r = &*(l as *const RECT);
                SetWindowPos(hwnd, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                if let Some((old, wnd, cols)) = old {
                    for c in [wnd.combo, wnd.scan, wnd.search, wnd.clear, wnd.list, wnd.types] {
                        send(c, WM_SETFONT, fonts.0 as usize, 1);
                    }
                    list::set_columns(wnd.list, cols, dpi);
                    ui::delete_fonts(old);
                }
                ui::layout(hwnd);
                0
            }
            WM_DESTROY => {
                save_placement(hwnd);
                engine::shutdown(std::time::Duration::from_secs(3));
                if let Some(a) = app::take() {
                    ui::delete_fonts((a.font, a.bold, a.small));
                    DeleteObject(a.brush_input);
                    DestroyAcceleratorTable(a.accel);
                }
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

/// A GUI has no console: show a panic before the process ends (nothing is written to
/// disk).
fn install_panic_box() {
    std::panic::set_hook(Box::new(|info| {
        let (text, caption) = (wide(&format!("{info}")), wide("FerrumTree"));
        // SAFETY: plain message box with NUL-terminated strings.
        unsafe { MessageBoxW(null_mut(), text.as_ptr(), caption.as_ptr(), MB_ICONERROR) };
    }));
}

fn main() {
    install_panic_box();
    // SAFETY: standard Win32 application bootstrap.
    unsafe {
        // STA + OLE: shell context menus use the OLE clipboard and drag-and-drop.
        OleInitialize(null());
        let inst = GetModuleHandleW(null());
        let icc = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_BAR_CLASSES | ICC_STANDARD_CLASSES,
        };
        InitCommonControlsEx(&icc);
        let settings = Settings::load();
        lang::set_lang(settings.lang);
        engine::set_live(settings.live);
        theme::set_app_dark(ui::palette_for(settings.theme).dark);
        let placement = settings.window;
        PENDING.with(|p| *p.borrow_mut() = Some(settings));

        let main_class = wide(MAIN_CLASS);
        let map_class = wide(map::CLASS);
        let mut wc: WNDCLASSEXW = std::mem::zeroed();
        wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
        wc.lpfnWndProc = Some(main_proc);
        wc.hInstance = inst;
        wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        // Icon group 1 from the embedded resources (build.rs).
        let icon = 1 as windows_sys::core::PCWSTR;
        wc.hIcon = LoadIconW(inst, icon);
        wc.hIconSm = LoadImageW(inst, icon, IMAGE_ICON, GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON), 0) as _;
        wc.lpszClassName = main_class.as_ptr();
        RegisterClassExW(&wc);
        wc.lpfnWndProc = Some(map::proc);
        wc.style = CS_DBLCLKS;
        wc.hIcon = null_mut();
        wc.hIconSm = null_mut();
        wc.lpszClassName = map_class.as_ptr();
        RegisterClassExW(&wc);

        let title = wide("FerrumTree");
        let (x, y, w, h) = placement.map_or((CW_USEDEFAULT, CW_USEDEFAULT, 1360, 900), |p| (p.0, p.1, p.2, p.3));
        let hwnd = CreateWindowExW(
            0,
            main_class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            x,
            y,
            w,
            h,
            null_mut(),
            null_mut(),
            inst,
            null(),
        );
        if hwnd.is_null() {
            return;
        }
        if placement.is_none() {
            let dpi = GetDpiForWindow(hwnd).max(96) as i32;
            SetWindowPos(hwnd, null_mut(), 0, 0, 1360 * dpi / 96, 900 * dpi / 96, SWP_NOMOVE | SWP_NOZORDER);
        }
        ShowWindow(hwnd, if placement.is_some_and(|p| p.4) { SW_SHOWMAXIMIZED } else { SW_SHOW });
        // Started behind another window: no WM_ACTIVATEAPP arrives until it is activated.
        engine::set_active(GetForegroundWindow() == hwnd);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            let key = matches!(msg.message, WM_KEYDOWN | WM_SYSKEYDOWN);
            let (accel, search) = with(|a| (a.accel, a.w.search)).unwrap_or((null_mut(), null_mut()));
            let in_edit = !search.is_null() && GetFocus() == search;
            if key && in_edit && msg.wParam as u16 == VK_ESCAPE {
                cmds::clear_search();
                continue;
            }
            // In the search box only function keys are shortcuts (Ctrl+C, Del... edit text).
            let fkey = (VK_F1..=VK_F24).contains(&(msg.wParam as u16));
            if !accel.is_null() && !(key && in_edit && !fkey) && TranslateAcceleratorW(hwnd, accel, &msg) != 0 {
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
