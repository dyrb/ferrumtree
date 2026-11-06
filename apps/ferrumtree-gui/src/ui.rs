//! Layout and painting of the window chrome (menu strip, top panel, tabs, borders,
//! splitters, breadcrumb, status bar) and of the owner-drawn buttons and combo items.
//! Painting goes to a memory DC and is copied with one BitBlt (no flicker).

use std::ptr::null;

use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::Controls::{DRAWITEMSTRUCT, ODS_COMBOBOXEDIT, ODS_SELECTED, ODT_BUTTON, ODT_COMBOBOX};
use windows_sys::Win32::UI::HiDpi::SystemParametersInfoForDpi;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::app::{App, Layout, Mode, with};
use crate::engine;
use crate::lang::{self, S, t};
use crate::list;
use crate::theme::{self, Kind, Palette};
use crate::util::{client, fill, frame, rect, text, text_width};

pub const MENU_FILE: usize = 0;
pub const MENU_VIEW: usize = 1;
pub const MENU_HELP: usize = 2;

/// Message font for the DPI, plus bold and small variants.
pub fn make_fonts(dpi: u32) -> (HFONT, HFONT, HFONT) {
    // SAFETY: plain struct out-parameter.
    unsafe {
        let mut m: NONCLIENTMETRICSW = std::mem::zeroed();
        m.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
        SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS, m.cbSize, (&mut m as *mut NONCLIENTMETRICSW).cast(), 0, dpi);
        let mut lf = m.lfMessageFont;
        let normal = CreateFontIndirectW(&lf);
        lf.lfWeight = 600;
        let bold = CreateFontIndirectW(&lf);
        lf.lfWeight = 400;
        lf.lfHeight = lf.lfHeight * 9 / 10;
        let small = CreateFontIndirectW(&lf);
        (normal, bold, small)
    }
}

pub fn delete_fonts(f: (HFONT, HFONT, HFONT)) {
    // SAFETY: fonts we created and no longer select anywhere.
    unsafe {
        DeleteObject(f.0);
        DeleteObject(f.1);
        DeleteObject(f.2);
    }
}

/// Compute all rectangles for the client size.
pub fn compute(a: &App, w: i32, h: i32, menu_widths: &[i32; 3]) -> Layout {
    let s = |v: i32| a.scale(v);
    let mut l = Layout::default();
    let menu_h = s(26);
    let mut x = s(6);
    for (i, mw) in menu_widths.iter().enumerate() {
        l.menus.push((rect(x, 0, x + mw + s(20), menu_h), i));
        x += mw + s(20);
    }
    let top = menu_h;
    l.top = rect(0, top, w, top + s(88));
    let left_w = s(470).min(w - s(24));
    l.status_line = rect(s(12), top + s(46), s(12) + left_w, top + s(66));
    l.progress = rect(s(12), top + s(70), s(12) + left_w, top + s(78));
    l.info = rect(s(12) + left_w + s(28), top + s(8), (w - s(270)).max(s(12) + left_w + s(320)), l.top.bottom - s(8));
    l.brand = rect(w - s(260), top + s(10), w - s(14), l.top.bottom - s(10));
    l.search_row = rect(s(12), l.top.bottom + s(8), w - s(12), l.top.bottom + s(8) + s(28));
    l.tab_row = rect(s(8), l.search_row.bottom + s(8), w - s(8), l.search_row.bottom + s(8) + s(28));
    let status_h = s(24);
    l.statusbar = rect(0, h - status_h, w, h);
    let body_top = l.tab_row.bottom;
    let body_bottom = l.statusbar.top - s(6);
    let total = (body_bottom - body_top).max(0);
    let (list_bottom, map_top) = if a.settings.show_map {
        let lb = body_top + ((total - s(6) - s(22)) as f32 * a.settings.split_map) as i32;
        l.split_h = rect(s(8), lb, w - s(8), lb + s(6));
        l.crumb = rect(s(8), lb + s(6), w - s(8), lb + s(6) + s(22));
        (lb, l.crumb.bottom)
    } else {
        (body_bottom, body_bottom)
    };
    if a.settings.show_map {
        l.map = rect(s(8), map_top, w - s(8), body_bottom);
    }
    let (lx0, lx1) = (s(8), w - s(8));
    if a.settings.show_types {
        let split = lx0 + ((lx1 - lx0 - s(6)) as f32 * a.settings.split_types) as i32;
        l.list = rect(lx0, body_top, split, list_bottom);
        l.split_v = rect(split, body_top, split + s(6), list_bottom);
        l.types = rect(split + s(6), body_top, lx1, list_bottom);
    } else {
        l.list = rect(lx0, body_top, lx1, list_bottom);
    }
    l
}

fn inset(r: &RECT, d: i32) -> RECT {
    rect(r.left + d, r.top + d, r.right - d, r.bottom - d)
}

/// Lay out the child windows for the current client size.
pub fn layout(hwnd: HWND) {
    let c = client(hwnd);
    // SAFETY: GetDC/ReleaseDC pair around text measuring.
    let widths = unsafe {
        let dc = GetDC(hwnd);
        let Some(font) = with(|a| a.font) else {
            ReleaseDC(hwnd, dc);
            return;
        };
        let old = SelectObject(dc, font);
        let wd = [text_width(dc, t(S::MenuFile)), text_width(dc, t(S::MenuView)), text_width(dc, t(S::MenuHelp))];
        SelectObject(dc, old);
        ReleaseDC(hwnd, dc);
        wd
    };
    let Some((l, w, show_types, show_map, s)) = with(|a| {
        a.layout = compute(a, c.right, c.bottom, &widths);
        (a.layout.clone(), a.w, a.settings.show_types, a.settings.show_map, a.dpi)
    }) else {
        return;
    };
    let sc = |v: i32| v * s as i32 / 96;
    let top = l.top.top;
    // SAFETY: moving our own children.
    unsafe {
        let x_combo = sc(12) + sc(84);
        MoveWindow(w.combo, x_combo, top + sc(10), sc(262), sc(300), 1);
        MoveWindow(w.scan, x_combo + sc(262) + sc(10), top + sc(9), sc(110), sc(30), 1);
        let sr = &l.search_row;
        let edit_w = (sr.right - sr.left - sc(120)).min(sc(640));
        MoveWindow(w.search, sr.left + sc(8), sr.top + sc(6), edit_w - sc(10), sr.bottom - sr.top - sc(10), 1);
        MoveWindow(w.clear, sr.left + edit_w + sc(10), sr.top, sc(110), sr.bottom - sr.top, 1);
        let li = inset(&l.list, 1);
        MoveWindow(w.list, li.left, li.top, li.right - li.left, li.bottom - li.top, 1);
        let ti = inset(&l.types, 1);
        MoveWindow(w.types, ti.left, ti.top, (ti.right - ti.left).max(0), (ti.bottom - ti.top).max(0), 1);
        ShowWindow(w.types, if show_types { SW_SHOW } else { SW_HIDE });
        let m = inset(&l.map, 1);
        MoveWindow(w.map, m.left, m.top, (m.right - m.left).max(0), (m.bottom - m.top).max(0), 1);
        ShowWindow(w.map, if show_map { SW_SHOW } else { SW_HIDE });
        InvalidateRect(hwnd, null(), 0);
    }
}

fn mode_tabs(a: &App) -> Vec<Mode> {
    let mut v = vec![Mode::Tree, Mode::Files];
    if a.mode == Mode::Search {
        v.push(Mode::Search);
    }
    if a.mode == Mode::Dupes || a.dupes.is_some() {
        v.push(Mode::Dupes);
    }
    v
}

fn paint_progress(dc: HDC, a: &App, p: &Palette) {
    let r = a.layout.progress;
    fill(dc, &r, p.bar_bg);
    let wdt = r.right - r.left;
    if a.busy {
        match engine::progress().fraction {
            Some(f) if f > 0.0 => fill(dc, &rect(r.left, r.top, r.left + (wdt as f64 * f) as i32, r.bottom), p.accent),
            _ => {
                // Unknown length (walks, cache load): a moving segment.
                let seg = wdt / 5;
                let ph = (a.scan_started.elapsed().as_millis() as i32 / 4) % (wdt + seg).max(1) - seg;
                fill(dc, &rect((r.left + ph).max(r.left), r.top, (r.left + ph + seg).min(r.right), r.bottom), p.accent);
            }
        }
    } else if a.snap.is_some() {
        fill(dc, &r, p.accent);
    }
}

/// Fingerprint of what the summary panel and the status bar show.
pub fn chrome_sig(a: &App) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    status_line(a).hash(&mut h);
    a.hover_text.hash(&mut h);
    if let Some(ix) = a.ix() {
        let n = a.selected.map_or(a.view_root, |s| s.0) as usize;
        a.size(ix.size[n]).hash(&mut h);
        a.size(ix.alloc[n]).hash(&mut h);
    }
    h.finish()
}

fn status_line(a: &App) -> String {
    if a.busy {
        // "Reading C: 45.0 % (12,345 folders, 234,567 files)"
        let p = engine::progress();
        let mut s = a.status.clone();
        if let Some(f) = p.fraction.filter(|&f| f > 0.0) {
            s += &format!(" {}", lang::percent(f));
        }
        if p.files + p.dirs > 0 {
            s += &format!(" ({}, {})", lang::folders(p.dirs), lang::files(p.files));
        }
        return s;
    }
    a.status.clone()
}

/// WM_PAINT of the main window.
pub fn paint(hwnd: HWND) {
    // SAFETY: standard WM_PAINT; objects created here are released here.
    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let dc = BeginPaint(hwnd, &mut ps);
        let c = client(hwnd);
        let mem = CreateCompatibleDC(dc);
        let bmp = CreateCompatibleBitmap(dc, c.right.max(1), c.bottom.max(1));
        let old_bmp = SelectObject(mem, bmp);
        with(|a| paint_chrome(mem, a, &c));
        BitBlt(dc, 0, 0, c.right, c.bottom, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old_bmp);
        DeleteObject(bmp);
        DeleteDC(mem);
        EndPaint(hwnd, &ps);
    }
}

fn paint_chrome(dc: HDC, a: &mut App, c: &RECT) {
    let tabs = paint_parts(dc, a, c);
    a.layout.tabs = tabs;
}

/// Paints everything; returns the tab rectangles as drawn (for hit testing).
fn paint_parts(dc: HDC, a: &App, c: &RECT) -> Vec<(RECT, Mode)> {
    let p = a.pal;
    let l = &a.layout;
    let mut tab_rects = Vec::new();
    fill(dc, c, p.bg);
    // SAFETY: font selection on our memory DC.
    let old = unsafe { SelectObject(dc, a.font) };
    // Menu strip.
    let names = [t(S::MenuFile), t(S::MenuView), t(S::MenuHelp)];
    for (r, i) in &l.menus {
        if a.menu_hot == Some(*i) {
            fill(dc, r, p.hover);
        }
        text(dc, names[*i], r, p.text, DT_CENTER);
    }
    // Top panel.
    fill(dc, &l.top, p.panel);
    fill(dc, &rect(l.top.left, l.top.bottom - 1, l.top.right, l.top.bottom), p.border);
    let s = |v: i32| a.scale(v);
    text(dc, t(S::Choose), &rect(s(12), l.top.top + s(10), s(12) + s(80), l.top.top + s(38)), p.dim, 0);
    text(dc, &status_line(a), &l.status_line, p.text, 0);
    paint_progress(dc, a, &p);
    // Volume and selection figures.
    let row_h = (l.info.bottom - l.info.top) / 4;
    let label_w = s(118);
    let mut rows: Vec<(String, String, bool)> = Vec::new();
    if let Some(ix) = a.ix() {
        let n = a.selected.map_or(a.view_root, |s| s.0);
        let sel = format!("{}   {}", ix.path(n), a.size(if a.settings.map_alloc { ix.alloc[n as usize] } else { ix.size[n as usize] }));
        rows.push((t(S::Selected).into(), sel, true));
    }
    if let Some((total, free)) = a.disk {
        let used = total.saturating_sub(free);
        let f = |v: u64| format!("{}   ({})", a.size(v), lang::percent(v as f64 / total.max(1) as f64));
        rows.push((t(S::Total).into(), a.size(total), false));
        rows.push((t(S::Used).into(), f(used), false));
        rows.push((t(S::Free).into(), f(free), false));
    }
    for (i, (k, v, strong)) in rows.iter().enumerate() {
        let y = l.info.top + row_h * i as i32;
        text(dc, k, &rect(l.info.left, y, l.info.left + label_w, y + row_h), p.dim, 0);
        // SAFETY: font switch on our DC.
        unsafe { SelectObject(dc, if *strong { a.bold } else { a.font }) };
        text(dc, v, &rect(l.info.left + label_w, y, l.info.right, y + row_h), p.text, 0);
        unsafe { SelectObject(dc, a.font) };
    }
    // Brand.
    unsafe { SelectObject(dc, a.bold) };
    let b = &l.brand;
    let third = (b.bottom - b.top) / 3;
    text(dc, "FerrumTree", &rect(b.left, b.top, b.right, b.top + third), p.text, DT_RIGHT);
    unsafe { SelectObject(dc, a.small) };
    let version = format!("{} {}", t(S::Version), env!("CARGO_PKG_VERSION"));
    text(dc, &version, &rect(b.left, b.top + third, b.right, b.top + third * 2), p.dim, DT_RIGHT);
    unsafe { SelectObject(dc, a.font) };
    // Search box frame.
    let sr = &l.search_row;
    let edit_w = (sr.right - sr.left - s(120)).min(s(640));
    let er = rect(sr.left, sr.top, sr.left + edit_w, sr.bottom);
    fill(dc, &er, p.input);
    frame(dc, &er, p.border);
    // Tabs.
    let mut x = l.tab_row.left;
    let tabs = mode_tabs(a);
    for m in tabs {
        let title = list::mode_title(m);
        unsafe { SelectObject(dc, if a.mode == m { a.bold } else { a.font }) };
        let w = text_width(dc, title) + s(26);
        let r = rect(x, l.tab_row.top, x + w, l.tab_row.bottom);
        if a.mode == m {
            fill(dc, &r, p.panel);
            fill(dc, &rect(r.left, r.bottom - s(2), r.right, r.bottom), p.accent);
        }
        text(dc, title, &r, if a.mode == m { p.text } else { p.dim }, DT_CENTER);
        tab_rects.push((r, m));
        x += w + s(2);
    }
    unsafe { SelectObject(dc, a.font) };
    if let Some(m) = &a.list_msg {
        text(dc, m, &rect(x + s(16), l.tab_row.top, l.tab_row.right, l.tab_row.bottom), p.dim, 0);
    }
    // Borders around the lists and the map.
    frame(dc, &l.list, p.border);
    if a.settings.show_types {
        frame(dc, &l.types, p.border);
        let g = &l.split_v;
        let cx = (g.left + g.right) / 2;
        fill(dc, &rect(cx, g.top + (g.bottom - g.top) / 2 - s(12), cx + 1, g.top + (g.bottom - g.top) / 2 + s(12)), p.border);
    }
    if a.settings.show_map {
        frame(dc, &l.map, p.border);
        let g = &l.split_h;
        let cy = (g.top + g.bottom) / 2;
        fill(dc, &rect(g.left + (g.right - g.left) / 2 - s(16), cy, g.left + (g.right - g.left) / 2 + s(16), cy + 1), p.border);
        if let Some(ix) = a.ix() {
            let n = a.tm_root;
            let w = if a.settings.map_alloc { ix.alloc[n as usize] } else { ix.size[n as usize] };
            text(dc, &format!("{}   {}", ix.path(n), a.size(w)), &l.crumb, p.text, DT_CENTER);
        }
    }
    // Status bar.
    let sb = &l.statusbar;
    fill(dc, sb, p.panel);
    fill(dc, &rect(sb.left, sb.top, sb.right, sb.top + 1), p.border);
    let mid = sb.left + (sb.right - sb.left) * 2 / 5;
    let left = if a.busy { status_line(a) } else { a.list_msg.clone().unwrap_or_else(|| a.status.clone()) };
    text(dc, &left, &rect(sb.left + s(10), sb.top, mid, sb.bottom), p.dim, 0);
    text(dc, &a.hover_text, &rect(mid + s(10), sb.top, sb.right - s(10), sb.bottom), p.dim, DT_RIGHT | DT_PATH_ELLIPSIS);
    unsafe { SelectObject(dc, old) };
    tab_rects
}

/// WM_DRAWITEM for our owner-drawn buttons and combo box.
pub fn draw_item(di: &DRAWITEMSTRUCT) -> bool {
    let Some((p, font, bold, labels, scan, target_names)) =
        with(|a| (a.pal, a.font, a.bold, (t(S::Scan), t(S::Clear)), a.w.scan, a.targets.iter().map(|t| t.1.clone()).collect::<Vec<_>>()))
    else {
        return false;
    };
    let r = di.rcItem;
    let dc = di.hDC;
    // SAFETY: drawing on the DC handed to us for this item.
    unsafe {
        match di.CtlType {
            ODT_BUTTON => {
                let primary = di.hwndItem == scan;
                let pressed = di.itemState & ODS_SELECTED != 0;
                let (bg, fg) = if primary {
                    (if pressed { 0x9A5A00 } else { p.accent }, 0xFFFFFF)
                } else {
                    (if pressed { p.hover } else { p.input }, p.text)
                };
                fill(dc, &r, p.panel);
                let pen = CreatePen(PS_SOLID, 1, if primary { bg } else { p.border });
                SetDCBrushColor(dc, bg);
                let ob = SelectObject(dc, GetStockObject(DC_BRUSH));
                let op = SelectObject(dc, pen);
                RoundRect(dc, r.left, r.top, r.right, r.bottom, 8, 8);
                SelectObject(dc, op);
                SelectObject(dc, ob);
                DeleteObject(pen);
                let of = SelectObject(dc, if primary { bold } else { font });
                text(dc, if primary { labels.0 } else { labels.1 }, &r, fg, DT_CENTER);
                SelectObject(dc, of);
                true
            }
            ODT_COMBOBOX => {
                let edit = di.itemState & ODS_COMBOBOXEDIT != 0;
                let sel = di.itemState & ODS_SELECTED != 0 && !edit;
                fill(dc, &r, if sel { p.accent } else { p.input });
                let s = target_names.get(di.itemID as usize).cloned().unwrap_or_else(|| t(S::SelectFolderItem).to_string());
                let of = SelectObject(dc, font);
                text(dc, &s, &rect(r.left + 6, r.top, r.right - 4, r.bottom), if sel { 0xFFFFFF } else { p.text }, 0);
                SelectObject(dc, of);
                true
            }
            _ => false,
        }
    }
}

/// Apply the palette to every child control and the frame.
pub fn apply_theme() {
    let Some((w, p)) = with(|a| (a.w, a.pal)) else { return };
    theme::set_app_dark(p.dark);
    theme::set_title_dark(w.main, p.dark);
    list::apply_theme(w.list, &p);
    list::apply_theme(w.types, &p);
    theme::style_control(w.combo, p.dark, Kind::Cfd);
    theme::style_control(w.search, p.dark, Kind::Cfd);
    with(|a| {
        // SAFETY: replacing a brush we own.
        unsafe {
            DeleteObject(a.brush_input);
            a.brush_input = CreateSolidBrush(p.input);
        }
    });
    crate::map::rebuild(w.map);
    // SAFETY: repaint everything.
    unsafe {
        RedrawWindow(w.main, null(), std::ptr::null_mut(), RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_FRAME);
    }
}

pub fn palette_for(pref: crate::settings::ThemePref) -> Palette {
    use crate::settings::ThemePref;
    let dark = match pref {
        ThemePref::System => theme::system_dark(),
        ThemePref::Light => false,
        ThemePref::Dark => true,
    };
    if dark { theme::DARK } else { theme::LIGHT }
}

/// Hit test of the painted parts: menus, tabs, splitters.
pub enum Hit {
    Menu(usize),
    Tab(Mode),
    Split(crate::app::Drag),
    Crumb,
    None,
}

pub fn hit(x: i32, y: i32) -> Hit {
    use crate::util::contains;
    with(|a| {
        let l = &a.layout;
        if let Some((_, i)) = l.menus.iter().find(|(r, _)| contains(r, x, y)) {
            return Hit::Menu(*i);
        }
        if contains(&l.split_v, x, y) && a.settings.show_types {
            return Hit::Split(crate::app::Drag::Vertical);
        }
        if contains(&l.split_h, x, y) && a.settings.show_map {
            return Hit::Split(crate::app::Drag::Horizontal);
        }
        if contains(&l.crumb, x, y) && a.settings.show_map {
            return Hit::Crumb;
        }
        if let Some((_, m)) = l.tabs.iter().find(|(r, _)| contains(r, x, y)) {
            return Hit::Tab(*m);
        }
        Hit::None
    })
    .unwrap_or(Hit::None)
}
