//! Commands (menus, accelerators, context menus) and the UI actions behind them.
//!
//! Every action borrows the app state only for computing, then talks to Win32 with the
//! borrow released (see `app::with`).

use std::io::Write;
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use ft_index::nflags;
use windows_sys::Win32::Foundation::{HWND, POINT};
use windows_sys::Win32::Graphics::Gdi::{ClientToScreen, InvalidateRect};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::UI::Controls::Dialogs::{GetSaveFileNameW, OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW};
use windows_sys::Win32::UI::Shell::{
    BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW, FO_DELETE, FOF_ALLOWUNDO, FOF_WANTNUKEWARNING, SHBrowseForFolderW,
    SHFILEOPSTRUCTW, SHFileOperationW, SHGetPathFromIDListW, ShellExecuteW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::app::{self, Mode, with};
use crate::engine::{self, Event, How, Target};
use crate::lang::{self, Lang, S, Units, t};
use crate::list;
use crate::map;
use crate::model::{Kind, Tree};
use crate::settings::{Live, ThemePref};
use crate::shell;
use crate::ui;
use crate::util::{send, wide};

pub const CMD_SCAN: usize = 300;
pub const CMD_RESCAN: usize = 301;
pub const CMD_FOLDER: usize = 302;
pub const CMD_COLLAPSE: usize = 303;
pub const CMD_EXPAND: usize = 304;
pub const CMD_PARENT: usize = 305;
pub const CMD_ZOOM_IN: usize = 306;
pub const CMD_ZOOM_OUT: usize = 307;
pub const CMD_PNG: usize = 308;
pub const CMD_EXPLORER: usize = 309;
pub const CMD_CMD: usize = 310;
pub const CMD_COPY_PATH: usize = 311;
pub const CMD_COPY: usize = 312;
pub const CMD_CUT: usize = 313;
pub const CMD_RECYCLE: usize = 314;
pub const CMD_DELETE: usize = 315;
pub const CMD_EXPORT: usize = 316;
pub const CMD_EXPORT_TYPES: usize = 317;
pub const CMD_DUPES: usize = 318;
pub const CMD_FIND: usize = 319;
pub const CMD_EXIT: usize = 320;
pub const CMD_TYPES: usize = 330;
pub const CMD_MAP: usize = 331;
pub const CMD_LABELS: usize = 332;
pub const CMD_MAP_ALLOC: usize = 333;
pub const CMD_UNITS: usize = 340; // ..=345
pub const CMD_THEME: usize = 350; // ..=352
pub const CMD_AUTOSCAN: usize = 355;
pub const CMD_TAB_TREE: usize = 360;
pub const CMD_TAB_FILES: usize = 361;
pub const CMD_BACK: usize = 362;
pub const CMD_ZOOM_HERE: usize = 363;
pub const CMD_ABOUT: usize = 370;
pub const CMD_LIVE: usize = 380; // ..=382
pub const CMD_CACHE: usize = 385;
pub const CMD_CLEAR_CACHE: usize = 386;
pub const CMD_LANG: usize = 390; // ..=392

pub const WM_DUPES: u32 = WM_APP + 2;
pub const WM_DONE: u32 = WM_APP + 3;
pub const TIMER_SEARCH: usize = 11;
pub const TIMER_PROGRESS: usize = 12;

const UNITS: [Units; 6] = [Units::Auto, Units::B, Units::KB, Units::MB, Units::GB, Units::TB];
const THEMES: [ThemePref; 3] = [ThemePref::System, ThemePref::Light, ThemePref::Dark];
const LIVE: [Live; 3] = [Live::Off, Live::Active, Live::Always];
const LANGS: [Option<Lang>; 3] = [None, Some(Lang::En), Some(Lang::Ru)];

fn main_wnd() -> Option<app::Wnd> {
    with(|a| a.w)
}

fn invalidate_main() {
    if let Some(w) = main_wnd() {
        // SAFETY: plain invalidation (children are clipped).
        unsafe { InvalidateRect(w.main, null(), 0) };
    }
}

fn invalidate_map() {
    if let Some(w) = main_wnd() {
        // SAFETY: plain invalidation.
        unsafe { InvalidateRect(w.map, null(), 0) };
    }
}

fn status(msg: String) {
    with(|a| a.status = msg);
    invalidate_main();
}

// ------------------------------------------------------------------------- list

/// Push rows (and columns, when the mode changed them) to the main list.
pub fn refresh_list(keep: bool) {
    let Some((w, n, changed, cols, sort, alloc, dpi)) = with(|a| {
        let cols = app::columns(a.mode);
        let changed = a.cols.as_ptr() != cols.as_ptr() || a.cols.len() != cols.len();
        a.cols = cols;
        (a.w, a.row_count(), changed, cols, a.current_sort(), a.settings.map_alloc, a.dpi)
    }) else {
        return;
    };
    if changed {
        list::set_columns(w.list, cols, dpi);
    }
    list::set_sort_arrow(w.list, list::sort_column(cols, sort.key, alloc), sort.desc);
    list::refresh(w.list, n, keep && !changed);
    invalidate_main();
}

pub fn refresh_types() {
    let Some((w, n, (col, desc))) = with(|a| (a.w, a.type_rows.len(), a.type_sort)) else { return };
    list::set_sort_arrow(w.types, Some(col), desc);
    list::refresh(w.types, n, false);
}

pub fn select_row(row: usize) {
    if let Some(w) = main_wnd() {
        list::select(w.list, row);
    }
}

pub fn on_row_selected(row: usize) {
    with(|a| {
        if let Some((n, k, _)) = a.row(row) {
            a.selected = Some((n, k));
            a.hover_text = a.describe(n);
        }
    });
    invalidate_main();
    invalidate_map();
}

pub fn toggle(row: usize) {
    let ok = with(|a| {
        let snap = a.snap.clone()?;
        (a.mode == Mode::Tree && row < a.tree.rows.len()).then(|| a.tree.toggle(&snap.ix, a.sort, row))
    })
    .flatten()
    .is_some();
    if ok {
        refresh_list(true);
        select_row(row);
    }
}

/// Enter / double click: expand in the tree, jump to the tree from flat lists.
pub fn activate(row: usize) {
    match with(|a| (a.mode, a.row(row))) {
        Some((Mode::Tree, _)) => toggle(row),
        Some((_, Some((n, _, _)))) => {
            with(|a| {
                a.mode = Mode::Tree;
                a.list_msg = None;
            });
            reveal(n);
        }
        _ => {}
    }
}

/// Select `node` in the list (expanding the tree as needed).
pub fn reveal(node: u32) {
    let (row, _) = with(|a| {
        let snap = a.snap.clone()?;
        a.selected = Some((node, Kind::Node));
        a.hover_text = a.describe(node);
        if a.mode != Mode::Tree {
            if let Some(r) = a.flat.iter().position(|&n| n == node) {
                return Some((Some(r), ()));
            }
            a.mode = Mode::Tree;
            a.list_msg = None;
        }
        Some((a.tree.reveal(&snap.ix, a.sort, node), ()))
    })
    .flatten()
    .unwrap_or((None, ()));
    refresh_list(true);
    if let Some(r) = row {
        select_row(r);
    }
    invalidate_map();
}

pub fn hover(n: u32) {
    let changed = with(|a| {
        if a.hover == n {
            return false;
        }
        a.hover = n;
        a.hover_text = a.describe(n);
        true
    })
    .unwrap_or(false);
    if changed && let Some((w, r)) = with(|a| (a.w, a.layout.statusbar)) {
        // SAFETY: plain invalidation.
        unsafe { InvalidateRect(w.main, &r, 0) };
    }
}

pub fn sort_list(col: usize) {
    with(|a| {
        if let Some(&c) = a.cols.get(col) {
            a.sort_by(c);
        }
    });
    refresh_list(true);
}

pub fn sort_types(col: usize) {
    with(|a| {
        a.type_sort = if a.type_sort.0 == col { (col, !a.type_sort.1) } else { (col, col >= 2) };
        a.build_type_rows();
    });
    refresh_types();
}

pub fn select_type(id: Option<u16>) {
    with(|a| a.type_sel = id);
    if let Some(w) = main_wnd() {
        map::rebuild(w.map);
    }
}

pub fn set_mode(m: Mode) {
    let ok = with(|a| {
        if (m == Mode::Dupes && a.dupes.is_none()) || a.snap.is_none() {
            return false;
        }
        a.mode = m;
        if matches!(m, Mode::Tree | Mode::Files) {
            a.list_msg = None;
        }
        a.refresh_rows();
        true
    })
    .unwrap_or(false);
    if ok {
        refresh_list(false);
    }
}

// ------------------------------------------------------------------------- treemap

pub fn zoom_to(dir: u32) {
    let w = with(|a| {
        if a.ix().is_some_and(|ix| ix.is_dir(dir)) {
            a.tm_root = dir;
        }
        a.w
    });
    if let Some(w) = w {
        map::rebuild(w.map);
        invalidate_main();
    }
}

pub fn zoom_out() {
    let up = with(|a| {
        let ix = a.ix()?;
        (a.tm_root != a.view_root).then(|| ix.parent[a.tm_root as usize]).filter(|&p| p != ft_index::NONE)
    })
    .flatten();
    if let Some(p) = up {
        zoom_to(p);
    }
}

/// The child of the treemap root on the way down to `node`.
pub fn zoom_towards(node: u32) {
    let t = with(|a| {
        let ix = a.ix()?;
        let mut n = node;
        while ix.parent[n as usize] != a.tm_root {
            n = ix.parent[n as usize];
            if n == ft_index::NONE {
                return None;
            }
        }
        ix.is_dir(n).then_some(n)
    })
    .flatten();
    if let Some(t) = t {
        zoom_to(t);
    }
}

// ------------------------------------------------------------------------- scanning

pub fn fill_combo(select: usize) {
    let Some((w, labels)) = with(|a| (a.w, a.targets.iter().map(|t| t.1.clone()).collect::<Vec<_>>())) else { return };
    // SAFETY: plain combo box messages; strings outlive each call.
    unsafe {
        send(w.combo, CB_RESETCONTENT, 0, 0);
        for l in labels.iter().map(|s| s.as_str()).chain([t(S::SelectFolderItem)]) {
            let s = wide(l);
            send(w.combo, CB_ADDSTRING, 0, s.as_ptr() as isize);
        }
        send(w.combo, CB_SETCURSEL, select, 0);
    }
}

pub fn combo_choice() -> Option<Option<Target>> {
    let (w, targets) = with(|a| (a.w, a.targets.iter().map(|t| t.0.clone()).collect::<Vec<_>>()))?;
    // SAFETY: plain query.
    let i = unsafe { send(w.combo, CB_GETCURSEL, 0, 0) };
    let i = usize::try_from(i).ok()?;
    Some(targets.get(i).cloned())
}

pub fn start_scan(rescan: bool) {
    let Some(choice) = combo_choice() else { return };
    let Some(target) = choice else {
        pick_folder();
        return;
    };
    let Some((w, cache)) = with(|a| {
        a.busy = true;
        a.scan_started = Instant::now();
        a.status = format!("{} {}", t(S::Reading), target.label());
        a.settings.last_target = Some(target.label());
        a.settings.save();
        (a.w, a.settings.cache)
    }) else {
        return;
    };
    engine::start(w.main as isize, target, rescan, cache);
    // SAFETY: progress repaint timer on our window.
    unsafe { SetTimer(w.main, TIMER_PROGRESS, 100, None) };
    invalidate_main();
}

pub fn pick_folder() {
    let Some(w) = main_wnd() else { return };
    let title = wide(t(S::SelectFolder));
    let mut name = [0u16; 260];
    // SAFETY: standard shell folder dialog; the returned PIDL is freed here.
    let path = unsafe {
        let mut bi: BROWSEINFOW = std::mem::zeroed();
        bi.hwndOwner = w.main;
        bi.pszDisplayName = name.as_mut_ptr();
        bi.lpszTitle = title.as_ptr();
        bi.ulFlags = BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE;
        let pidl = SHBrowseForFolderW(&bi);
        if pidl.is_null() {
            None
        } else {
            let mut buf = vec![0u16; 32768];
            let ok = SHGetPathFromIDListW(pidl, buf.as_mut_ptr()) != 0;
            CoTaskMemFree(pidl as *const _);
            let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
            ok.then(|| String::from_utf16_lossy(&buf[..n]))
        }
    };
    let Some(path) = path else {
        // Cancelled: back to the current target.
        let cur = with(|a| a.snap.as_ref().and_then(|s| a.targets.iter().position(|t| t.0 == s.target))).flatten();
        fill_combo(cur.unwrap_or(0));
        return;
    };
    let Some(target) = Target::parse(&path) else { return };
    let idx = with(|a| match a.targets.iter().position(|t| t.0 == target) {
        Some(i) => i,
        None => {
            a.targets.push((target.clone(), target.label()));
            a.targets.len() - 1
        }
    });
    if let Some(i) = idx {
        fill_combo(i);
        start_scan(false);
    }
}

pub fn on_engine(ev: Event) {
    match ev {
        Event::Status(id, what) if id == engine::generation() => {
            status(format!("{} {what}", t(S::Reading)));
            with(|a| a.busy = true);
        }
        Event::Ready(s) if s.generation == engine::generation() => {
            let live = s.live;
            let row = with(|a| a.install(Arc::from(s))).flatten();
            refresh_list(live);
            refresh_types();
            if let Some(r) = row {
                select_row(r);
            }
            if let Some(w) = main_wnd() {
                map::rebuild(w.map);
                // SAFETY: stop the progress timer.
                unsafe { KillTimer(w.main, TIMER_PROGRESS) };
            }
            invalidate_main();
        }
        Event::Patch(id, p) if id == engine::generation() => {
            if let Some(c) = with(|a| a.apply_patch(Some(*p))).flatten() {
                after_tree_change(c);
            }
        }
        // The journal moved on too far while we were not following it.
        Event::Stale(id) if id == engine::generation() => start_scan(false),
        Event::Failed(id, e) if id == engine::generation() => {
            with(|a| a.busy = false);
            status(format!("{}: {e}", t(S::Error)));
        }
        _ => {}
    }
}

pub fn search_changed() {
    let Some(w) = main_wnd() else { return };
    // SAFETY: buffer sized from the reported length.
    let text = unsafe {
        let n = GetWindowTextLengthW(w.search);
        let mut buf = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(w.search, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..got.max(0) as usize])
    };
    with(|a| a.search(&text));
    refresh_list(false);
}

pub fn clear_search() {
    if let Some(w) = main_wnd() {
        let e = wide("");
        // SAFETY: our edit control; EN_CHANGE re-runs the (empty) search.
        unsafe { send(w.search, WM_SETTEXT, 0, e.as_ptr() as isize) };
    }
}

// ------------------------------------------------------------------------- targets of commands

struct Sel {
    node: u32,
    kind: Kind,
    path: String,
    dir: bool,
    /// Removable from here: a real file or folder, not the scan root or a group row.
    deletable: bool,
}

fn selection() -> Option<Sel> {
    with(|a| {
        let ix = a.ix()?;
        let (node, kind) = a.selected.unwrap_or((a.view_root, Kind::Node));
        let path = ix.path(node);
        let i = node as usize;
        let deletable = kind == Kind::Node
            && node != a.view_root
            && node != 0
            && ix.flags[i] & nflags::SYNTHETIC == 0
            && path.trim_end_matches('\\').len() > 2;
        Some(Sel { node, kind, path, dir: ix.is_dir(node) || kind == Kind::Files, deletable })
    })
    .flatten()
}

fn explorer(path: &str) {
    let args = wide(&format!("/select,\"{path}\""));
    let (exe, verb) = (wide("explorer.exe"), wide("open"));
    // SAFETY: valid strings; explorer starts detached.
    unsafe { ShellExecuteW(null_mut(), verb.as_ptr(), exe.as_ptr(), args.as_ptr(), null(), SW_SHOWNORMAL) };
}

fn cmd_here(dir: &str) {
    let (exe, verb, d) = (wide("cmd.exe"), wide("open"), wide(dir));
    // SAFETY: as above.
    unsafe { ShellExecuteW(null_mut(), verb.as_ptr(), exe.as_ptr(), null(), d.as_ptr(), SW_SHOWNORMAL) };
}

/// Put a global memory block on the clipboard (ownership passes to the system).
unsafe fn clip_put(format: u32, bytes: &[u8]) -> bool {
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len());
        if h.is_null() {
            return false;
        }
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            return false;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        GlobalUnlock(h);
        !SetClipboardData(format, h as _).is_null()
    }
}

fn utf16_bytes(s: &str, nuls: usize) -> Vec<u8> {
    s.encode_utf16().chain(std::iter::repeat_n(0, nuls)).flat_map(|u| u.to_le_bytes()).collect()
}

fn copy_text(owner: HWND, s: &str) {
    // SAFETY: standard clipboard protocol.
    unsafe {
        if OpenClipboard(owner) != 0 {
            EmptyClipboard();
            clip_put(13 /* CF_UNICODETEXT */, &utf16_bytes(s, 1));
            CloseClipboard();
        }
    }
}

/// Files on the clipboard (CF_HDROP), as Explorer's Copy / Cut do.
fn copy_files(owner: HWND, path: &str, cut: bool) {
    // DROPFILES: pFiles, pt.x, pt.y, fNC, fWide - then the double-NUL list.
    let mut data = Vec::with_capacity(20 + path.len() * 2 + 4);
    data.extend_from_slice(&20u32.to_le_bytes());
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(&1u32.to_le_bytes());
    data.extend_from_slice(&utf16_bytes(path, 2));
    // SAFETY: standard clipboard protocol.
    unsafe {
        if OpenClipboard(owner) == 0 {
            return;
        }
        EmptyClipboard();
        clip_put(15 /* CF_HDROP */, &data);
        let name = wide("Preferred DropEffect");
        let fmt = RegisterClipboardFormatW(name.as_ptr());
        let effect: u32 = if cut { 2 } else { 1 }; // DROPEFFECT_MOVE / COPY
        clip_put(fmt, &effect.to_le_bytes());
        CloseClipboard();
    }
}

/// Delete through the shell: its own confirmation, Recycle Bin unless `permanent`.
fn delete(owner: HWND, sel: &Sel, permanent: bool) {
    if !sel.deletable {
        let (m, c) = (wide(t(S::NotAllowed)), wide("FerrumTree"));
        // SAFETY: plain message box.
        unsafe { MessageBoxW(owner, m.as_ptr(), c.as_ptr(), MB_ICONINFORMATION) };
        return;
    }
    let from: Vec<u16> = sel.path.encode_utf16().chain([0, 0]).collect();
    // SAFETY: double-NUL-terminated path; the shell shows its confirmation dialog.
    let (r, aborted) = unsafe {
        let mut op: SHFILEOPSTRUCTW = std::mem::zeroed();
        op.hwnd = owner;
        op.wFunc = FO_DELETE;
        op.pFrom = from.as_ptr();
        op.fFlags = if permanent { 0 } else { (FOF_ALLOWUNDO | FOF_WANTNUKEWARNING) as _ };
        let r = SHFileOperationW(&mut op);
        (r, op.fAnyOperationsAborted != 0)
    };
    if r == 0 && !aborted && std::fs::symlink_metadata(&sel.path).is_err() {
        // Struck through at once; the journal (if followed) confirms it.
        if let Some(c) = with(|a| a.mark_deleted(sel.node)).flatten() {
            after_tree_change(c);
        }
        status(format!("{}: {}", t(S::Deleted), sel.path));
    }
}

fn save_dialog(default_name: &str, filter: &str, ext: &str) -> Option<String> {
    let owner = main_wnd()?.main;
    let mut file: Vec<u16> = default_name.encode_utf16().collect();
    file.resize(32768, 0);
    let filter: Vec<u16> = filter.encode_utf16().chain([0, 0]).map(|c| if c == '|' as u16 { 0 } else { c }).collect();
    let def = wide(ext);
    // SAFETY: OPENFILENAMEW with buffers that outlive the call.
    unsafe {
        let mut o: OPENFILENAMEW = std::mem::zeroed();
        o.lStructSize = size_of::<OPENFILENAMEW>() as u32;
        o.hwndOwner = owner;
        o.lpstrFilter = filter.as_ptr();
        o.lpstrFile = file.as_mut_ptr();
        o.nMaxFile = file.len() as u32;
        o.lpstrDefExt = def.as_ptr();
        o.Flags = OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;
        if GetSaveFileNameW(&mut o) == 0 {
            return None;
        }
    }
    let n = file.iter().position(|&c| c == 0).unwrap_or(0);
    Some(String::from_utf16_lossy(&file[..n]))
}

fn post_done_to(hwnd: isize, msg: String) {
    let p = Box::into_raw(Box::new(msg));
    // SAFETY: reclaimed in on_done, or here if posting fails.
    if unsafe { PostMessageW(hwnd as HWND, WM_DONE, 0, p as isize) } == 0 {
        drop(unsafe { Box::from_raw(p) });
    }
}

pub fn on_done(msg: String) {
    status(msg);
    apply_pending();
}

/// Patches held back while a worker shared the snapshot.
fn apply_pending() {
    if let Some(c) = with(|a| a.apply_patch(None)).flatten() {
        after_tree_change(c);
    }
}

/// Redraw what shows the tree after it changed in place. The treemap is laid out again
/// once changes settle (bursts arrive as several patches), and only for visible ones.
fn after_tree_change(c: app::Changed) {
    let Some(w) = main_wnd() else { return };
    // Rows rebuilt: new count, full repaint. Otherwise only the visible rows whose text
    // changed are repainted (most live changes alter nothing you can see).
    if c.structural {
        let n = with(|a| {
            a.shown_rows.clear();
            a.row_count()
        });
        list::refresh(w.list, n.unwrap_or(0), true);
    } else {
        let rows = list::visible_rows(w.list);
        let changed: Vec<usize> = with(|a| {
            rows.filter(|&r| {
                let s = a.row_sig(r);
                a.shown_rows.insert(r, s) != Some(s)
            })
            .collect()
        })
        .unwrap_or_default();
        list::redraw_rows(w.list, &changed);
    }
    if c.types_reordered {
        let n = with(|a| {
            a.shown_types.clear();
            a.type_rows.len()
        });
        list::refresh(w.types, n.unwrap_or(0), true);
    } else {
        let rows = list::visible_rows(w.types);
        let changed: Vec<usize> = with(|a| {
            rows.filter(|&r| {
                let s = a.type_sig(r);
                a.shown_types.insert(r, s) != Some(s)
            })
            .collect()
        })
        .unwrap_or_default();
        list::redraw_rows(w.types, &changed);
    }
    if let Some(r) = c.row {
        select_row(r);
    }
    if c.relayout {
        map::rebuild_later(w.map);
    }
    // The summary panel and the status bar, if what they show changed.
    if let Some((true, top, bar)) = with(|a| {
        let s = ui::chrome_sig(a);
        (std::mem::replace(&mut a.shown_chrome, s) != s, a.layout.top, a.layout.statusbar)
    }) {
        // SAFETY: plain invalidation.
        unsafe {
            InvalidateRect(w.main, &top, 0);
            InvalidateRect(w.main, &bar, 0);
        }
    }
}

/// Every node below the scan root, written on a background thread.
fn export_csv() {
    let Some((snap, root, hwnd)) = with(|a| a.snap.clone().map(|s| (s, a.view_root, a.w.main as isize))).flatten() else { return };
    let Some(path) = save_dialog("ferrumtree.csv", "CSV (*.csv)|*.csv|", "csv") else { return };
    status(format!("{} {path}", t(S::Exporting)));
    std::thread::spawn(move || {
        let ix = &snap.ix;
        let r = (|| -> std::io::Result<u64> {
            let mut f = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&path)?);
            f.write_all(b"\xEF\xBB\xBFPath,Size,OnDisk,Modified,Files,Folders,Attributes\r\n")?;
            let mut stack = vec![root];
            let mut rows = 0u64;
            while let Some(n) = stack.pop() {
                let i = n as usize;
                let fl = ix.flags[i];
                let dir = ix.is_dir(n);
                let attrs: String =
                    [(dir, 'D'), (fl & nflags::HIDDEN != 0, 'H'), (fl & nflags::SYSTEM != 0, 'S'), (fl & nflags::DUP_LINK != 0, 'L')]
                        .iter()
                        .filter(|x| x.0)
                        .map(|x| x.1)
                        .collect();
                let (files, dirs) = if dir { (ix.files[i].to_string(), ix.dirs[i].to_string()) } else { (String::new(), String::new()) };
                writeln!(
                    f,
                    "\"{}\",{},{},{},{files},{dirs},{attrs}\r",
                    ix.path(n).replace('"', "\"\""),
                    ix.size[i],
                    ix.alloc[i],
                    crate::util::filetime(ix.mtime_filetime(n))
                )?;
                rows += 1;
                let kids: Vec<u32> = ix.children(n).collect();
                stack.extend(kids.into_iter().rev());
            }
            f.flush()?;
            Ok(rows)
        })();
        // Let go of the snapshot before reporting: live patches wait for it.
        drop(snap);
        let msg = match r {
            Ok(n) => format!("{}: {path} ({})", t(S::Exported), lang::rows(n)),
            Err(e) => format!("{}: {e}", t(S::Error)),
        };
        post_done_to(hwnd, msg);
    });
}

/// Stop the engine (it maps the cache file) and delete the cache files next to the
/// program, on a worker: an engine in the middle of a scan finishes it first.
fn clear_cache() {
    let Some(w) = main_wnd() else { return };
    let hwnd = w.main as isize;
    std::thread::spawn(move || {
        engine::stop_and_wait();
        let freed = ft_usn::remove_caches();
        post_done_to(hwnd, format!("{} ({})", t(S::CacheCleared), lang::size(freed, Units::Auto)));
    });
}

fn export_types() {
    let Some(path) = save_dialog("ferrumtree-types.csv", "CSV (*.csv)|*.csv|", "csv") else { return };
    let rows = with(|a| {
        let n = a.type_rows.len();
        (0..n).map(|r| (0..6).map(|c| a.type_cell(r, c)).collect::<Vec<_>>()).collect::<Vec<_>>()
    })
    .unwrap_or_default();
    let raw = with(|a| a.type_rows.iter().map(|&i| a.type_stat(i)).collect::<Vec<_>>()).unwrap_or_default();
    let mut out = String::from("\u{FEFF}Extension,Type,Size,OnDisk,Files\r\n");
    for (cells, (size, alloc, files)) in rows.iter().zip(&raw) {
        out += &format!("\"{}\",\"{}\",{size},{alloc},{files}\r\n", cells[0], cells[1].replace('"', "\"\""));
    }
    let msg = match std::fs::write(&path, out) {
        Ok(()) => format!("{}: {path}", t(S::Exported)),
        Err(e) => format!("{}: {e}", t(S::Error)),
    };
    status(msg);
}

fn save_png() {
    let Some((w, h, px)) = with(|a| (a.tm.width, a.tm.height, a.tm_px.clone())) else { return };
    if w == 0 || h == 0 {
        return;
    }
    let Some(path) = save_dialog("ferrumtree.png", "PNG (*.png)|*.png|", "png") else { return };
    let msg = match std::fs::write(&path, crate::png::encode(w, h, &px)) {
        Ok(()) => format!("{}: {path}", t(S::Saved)),
        Err(e) => format!("{}: {e}", t(S::Error)),
    };
    status(msg);
}

struct DupesDone {
    generation: u64,
    list: Vec<(u32, String)>,
    summary: String,
}

fn find_dupes(dir: u32) {
    let Some((snap, hwnd)) = with(|a| a.snap.clone().map(|s| (s, a.w.main as isize))).flatten() else { return };
    let path = snap.ix.path(dir);
    status(format!("{} {path}", t(S::LookingDupes)));
    let generation = engine::generation();
    std::thread::spawn(move || {
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8);
        let o = ft_dupes::Options { min_size: 1 << 20, threads };
        let rep = ft_dupes::find(&snap.ix, dir, &o, &AtomicBool::new(false));
        let mut list = Vec::new();
        for (g, grp) in rep.groups.iter().enumerate() {
            list.extend(grp.nodes.iter().map(|&n| (g as u32, snap.ix.path(n))));
        }
        let summary = format!(
            "{}: {}, {} {}",
            t(S::DupeGroups),
            lang::count(rep.groups.len() as u64),
            t(S::Reclaimable),
            lang::size(rep.wasted(), Units::Auto)
        );
        drop(snap);
        let p = Box::into_raw(Box::new(DupesDone { generation, list, summary }));
        // SAFETY: reclaimed in on_dupes_msg, or here if posting fails.
        if unsafe { PostMessageW(hwnd as HWND, WM_DUPES, 0, p as isize) } == 0 {
            drop(unsafe { Box::from_raw(p) });
        }
    });
}

/// # Safety
/// `l` must come from a WM_DUPES message posted by `find_dupes`.
pub unsafe fn on_dupes_msg(l: isize) {
    let d = *unsafe { Box::from_raw(l as *mut DupesDone) };
    if d.generation != engine::generation() {
        return;
    }
    with(|a| {
        a.dupes = Some(d.list);
        a.mode = Mode::Dupes;
        a.list_msg = Some(d.summary.clone());
        a.status = d.summary;
        a.refresh_rows();
    });
    refresh_list(false);
    apply_pending();
}

// ------------------------------------------------------------------------- menus

fn item(m: HMENU, id: usize, s: S, accel: &str, checked: bool, enabled: bool) {
    let text = if accel.is_empty() { t(s).to_string() } else { format!("{}\t{accel}", t(s)) };
    let w = wide(&text);
    let flags = MF_STRING | if checked { MF_CHECKED } else { 0 } | if enabled { 0 } else { MF_GRAYED };
    // SAFETY: menu we own; string copied by the call.
    unsafe { AppendMenuW(m, flags, id, w.as_ptr()) };
}

fn item_text(m: HMENU, id: usize, text: &str, checked: bool) {
    let w = wide(text);
    let flags = MF_STRING | if checked { MF_CHECKED } else { 0 };
    // SAFETY: menu we own; string copied by the call.
    unsafe { AppendMenuW(m, flags, id, w.as_ptr()) };
}

/// A submenu titled `title`, filled by `fill`.
fn submenu(m: HMENU, title: S, fill: impl FnOnce(HMENU)) {
    // SAFETY: the submenu is owned by `m` once appended and destroyed with it.
    unsafe {
        let sub = CreatePopupMenu();
        fill(sub);
        let w = wide(t(title));
        AppendMenuW(m, MF_POPUP, sub as usize, w.as_ptr());
    }
}

fn sep(m: HMENU) {
    // SAFETY: as above.
    unsafe { AppendMenuW(m, MF_SEPARATOR, 0, null()) };
}

fn build_menu(which: usize) -> HMENU {
    let Some((set, has, sel, mode)) = with(|a| (a.settings.clone(), a.snap.is_some(), a.selected.is_some(), a.mode)) else {
        // SAFETY: an empty menu.
        return unsafe { CreatePopupMenu() };
    };
    // SAFETY: a fresh popup menu filled below.
    let m = unsafe { CreatePopupMenu() };
    match which {
        ui::MENU_FILE => {
            item(m, CMD_SCAN, S::Scan, "F5", false, true);
            item(m, CMD_RESCAN, S::FullRescan, "Shift+F5", false, true);
            item(m, CMD_FOLDER, S::SelectFolder, "F3", false, true);
            sep(m);
            item(m, CMD_COLLAPSE, S::CollapseAll, "F4", false, has);
            item(m, CMD_EXPAND, S::ExpandAll, "Shift+F4", false, has);
            item(m, CMD_PARENT, S::SelectParent, "F6", false, sel);
            item(m, CMD_ZOOM_IN, S::ZoomIn, "F10", false, sel);
            item(m, CMD_ZOOM_OUT, S::ZoomOut, "F11", false, has);
            item(m, CMD_PNG, S::SavePng, "Ctrl+S", false, has);
            sep(m);
            item(m, CMD_EXPLORER, S::ShowInExplorer, "Ctrl+E", false, has);
            item(m, CMD_CMD, S::OpenCmd, "Ctrl+P", false, has);
            item(m, CMD_COPY_PATH, S::CopyPath, "Ctrl+Alt+C", false, has);
            sep(m);
            item(m, CMD_COPY, S::Copy, "Ctrl+C", false, sel);
            item(m, CMD_CUT, S::Cut, "Ctrl+X", false, sel);
            item(m, CMD_RECYCLE, S::Recycle, "Del", false, sel);
            item(m, CMD_DELETE, S::DeleteForever, "Shift+Del", false, sel);
            sep(m);
            item(m, CMD_EXPORT, S::ExportCsv, "Ctrl+Alt+E", false, has);
            item(m, CMD_EXPORT_TYPES, S::ExportTypes, "Ctrl+Alt+F", false, has);
            item(m, CMD_DUPES, S::FindDupes, "Ctrl+D", false, has);
            sep(m);
            item(m, CMD_EXIT, S::Exit, "Alt+F4", false, true);
        }
        ui::MENU_VIEW => {
            item(m, CMD_TAB_TREE, S::TabTree, "Ctrl+1", mode == Mode::Tree, has);
            item(m, CMD_TAB_FILES, S::TabFiles, "Ctrl+2", mode == Mode::Files, has);
            sep(m);
            let names = [S::UnitsAuto, S::UnitsB, S::UnitsKB, S::UnitsMB, S::UnitsGB, S::UnitsTB];
            for (i, (u, s)) in UNITS.iter().zip(names).enumerate() {
                item(m, CMD_UNITS + i, s, "", set.units == *u, true);
            }
            sep(m);
            item(m, CMD_TYPES, S::ShowTypes, "F8", set.show_types, true);
            item(m, CMD_MAP, S::ShowMap, "F9", set.show_map, true);
            item(m, CMD_LABELS, S::MapLabels, "Ctrl+Shift+N", set.map_labels, true);
            item(m, CMD_MAP_ALLOC, S::MapAlloc, "", set.map_alloc, true);
            sep(m);
            submenu(m, S::ThemeMenu, |sub| {
                for (i, (th, s)) in THEMES.iter().zip([S::ThemeSystem, S::ThemeLight, S::ThemeDark]).enumerate() {
                    item(sub, CMD_THEME + i, s, "", set.theme == *th, true);
                }
            });
            submenu(m, S::LangMenu, |sub| {
                item(sub, CMD_LANG, S::LangSystem, "", set.lang.is_none(), true);
                sep(sub);
                for (i, l) in [Lang::En, Lang::Ru].into_iter().enumerate() {
                    item_text(sub, CMD_LANG + 1 + i, lang::native_name(l), set.lang == Some(l));
                }
            });
            sep(m);
            item(m, CMD_AUTOSCAN, S::AutoScan, "", set.auto_scan, true);
            submenu(m, S::LiveMenu, |sub| {
                for (i, (l, s)) in LIVE.iter().zip([S::LiveOff, S::LiveActive, S::LiveAlways]).enumerate() {
                    item(sub, CMD_LIVE + i, s, "", set.live == *l, true);
                }
            });
            sep(m);
            item(m, CMD_CACHE, S::CacheKeep, "", set.cache, true);
            item(m, CMD_CLEAR_CACHE, S::CacheClear, "", false, true);
        }
        _ => item(m, CMD_ABOUT, S::About, "", false, true),
    }
    m
}

/// Drop-down of a menu-strip item.
pub fn show_menu(which: usize) {
    let Some((w, r)) = with(|a| {
        a.menu_hot = Some(which);
        (a.w, a.layout.menus.iter().find(|m| m.1 == which).map(|m| m.0))
    }) else {
        return;
    };
    let Some(r) = r else { return };
    invalidate_main();
    let m = build_menu(which);
    let mut pt = POINT { x: r.left, y: r.bottom };
    // SAFETY: modal menu loop on our window; the menu is destroyed afterwards.
    let cmd = unsafe {
        ClientToScreen(w.main, &mut pt);
        let c = TrackPopupMenuEx(m, TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN, pt.x, pt.y, w.main, null());
        DestroyMenu(m);
        c
    };
    with(|a| a.menu_hot = None);
    invalidate_main();
    if cmd > 0 {
        run(cmd as usize);
    }
}

/// Right-click menu for the selection (list) or a treemap node: Explorer's own menu for
/// files and folders; ours for what the shell cannot show (the
/// "[N files]" group rows, NTFS metadata, items deleted since the scan).
pub fn context_menu(_map_node: Option<u32>) {
    let Some(sel) = selection() else { return };
    let Some((w, mode, zoomed, synthetic)) = with(|a| {
        let synthetic = a.ix().is_some_and(|ix| ix.flags[sel.node as usize] & nflags::SYNTHETIC != 0);
        (a.w, a.mode, a.tm_root != a.view_root, synthetic)
    }) else {
        return;
    };
    let mut pt = POINT { x: 0, y: 0 };
    // SAFETY: plain query.
    unsafe { GetCursorPos(&mut pt) };
    if sel.kind == Kind::Node && !synthetic {
        // Deletion goes through our own path: it refuses the scan root and reports.
        let shown = shell::context_menu(w.main, &sel.path, pt, |verb| match verb {
            "delete" => {
                delete(w.main, &sel, shell::shift_down());
                true
            }
            _ => false,
        });
        if shown {
            return;
        }
    }
    // SAFETY: a temporary popup menu.
    let m = unsafe { CreatePopupMenu() };
    let file_ops = sel.kind == Kind::Node;
    item(m, CMD_EXPLORER, S::ShowInExplorer, "Ctrl+E", false, true);
    item(m, CMD_CMD, S::OpenCmd, "Ctrl+P", false, true);
    item(m, CMD_COPY_PATH, S::CopyPath, "Ctrl+Alt+C", false, true);
    sep(m);
    item(m, CMD_COPY, S::Copy, "Ctrl+C", false, file_ops);
    item(m, CMD_CUT, S::Cut, "Ctrl+X", false, sel.deletable);
    item(m, CMD_RECYCLE, S::Recycle, "Del", false, sel.deletable);
    item(m, CMD_DELETE, S::DeleteForever, "Shift+Del", false, sel.deletable);
    sep(m);
    if sel.dir {
        item(m, CMD_ZOOM_HERE, S::ZoomIn, "F10", false, file_ops);
        item(m, CMD_DUPES, S::FindDupes, "Ctrl+D", false, file_ops);
    }
    if zoomed {
        item(m, CMD_ZOOM_OUT, S::ZoomOut, "F11", false, true);
    }
    item(m, CMD_PARENT, S::SelectParent, "F6", false, true);
    if mode != Mode::Tree {
        item(m, CMD_BACK, S::TabTree, "Ctrl+1", false, true);
    }
    // SAFETY: modal menu loop; menu destroyed afterwards.
    let cmd = unsafe {
        let c = TrackPopupMenuEx(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, w.main, null());
        DestroyMenu(m);
        c
    };
    if cmd > 0 {
        run(cmd as usize);
    }
}

pub fn accelerators() -> HACCEL {
    const V: u8 = FVIRTKEY;
    const C: u8 = FCONTROL;
    const SH: u8 = FSHIFT;
    const AL: u8 = FALT;
    let keys: [(u8, u16, usize); 25] = [
        (V, 0x74, CMD_SCAN), // F5
        (V | SH, 0x74, CMD_RESCAN),
        (V, 0x72, CMD_FOLDER),   // F3
        (V, 0x73, CMD_COLLAPSE), // F4
        (V | SH, 0x73, CMD_EXPAND),
        (V, 0x75, CMD_PARENT),   // F6
        (V, 0x77, CMD_TYPES),    // F8
        (V, 0x78, CMD_MAP),      // F9
        (V, 0x79, CMD_ZOOM_IN),  // F10
        (V, 0x7A, CMD_ZOOM_OUT), // F11
        (V | C, b'S' as u16, CMD_PNG),
        (V | C, b'E' as u16, CMD_EXPLORER),
        (V | C, b'P' as u16, CMD_CMD),
        (V | C | AL, b'C' as u16, CMD_COPY_PATH),
        (V | C, b'C' as u16, CMD_COPY),
        (V | C, b'X' as u16, CMD_CUT),
        (V, 0x2E, CMD_RECYCLE), // Del
        (V | SH, 0x2E, CMD_DELETE),
        (V | C | AL, b'E' as u16, CMD_EXPORT),
        (V | C | AL, b'F' as u16, CMD_EXPORT_TYPES),
        (V | C, b'D' as u16, CMD_DUPES),
        (V | C, b'F' as u16, CMD_FIND),
        (V | C | SH, b'N' as u16, CMD_LABELS),
        (V | C, b'1' as u16, CMD_TAB_TREE),
        (V | C, b'2' as u16, CMD_TAB_FILES),
    ];
    let table: Vec<ACCEL> = keys.iter().map(|&(f, k, c)| ACCEL { fVirt: f, key: k, cmd: c as u16 }).collect();
    // SAFETY: valid array; the table lives for the process.
    unsafe { CreateAcceleratorTableW(table.as_ptr(), table.len() as i32) }
}

// ------------------------------------------------------------------------- dispatch

/// Every text on screen again, in the language just chosen.
fn apply_language() {
    let Some((w, cols, dpi, busy)) = with(|a| {
        a.settings.save();
        (a.w, a.cols, a.dpi, a.busy)
    }) else {
        return;
    };
    list::set_columns(w.list, cols, dpi);
    list::set_type_columns(w.types, dpi);
    let cue = wide(t(S::SearchCue));
    // SAFETY: plain messages to our own controls.
    let sel = unsafe {
        send(w.search, 0x1501 /* EM_SETCUEBANNER */, 1, cue.as_ptr() as isize);
        send(w.combo, CB_GETCURSEL, 0, 0)
    };
    fill_combo(usize::try_from(sel).unwrap_or(0));
    with(|a| {
        if !busy {
            a.status = a.summary();
        }
        let shown = a.selected.map(|s| s.0).or((a.hover != ft_index::NONE).then_some(a.hover));
        a.hover_text = shown.map(|n| a.describe(n)).unwrap_or_default();
        a.type_names.clear();
    });
    refresh_list(true);
    refresh_types();
    map::rebuild(w.map);
    // SAFETY: repaint everything else (buttons, panels).
    unsafe {
        InvalidateRect(w.scan, null(), 0);
        InvalidateRect(w.clear, null(), 0);
    }
    invalidate_main();
}

fn apply_settings(relayout: bool, rebuild_map: bool) {
    let w = with(|a| {
        a.settings.save();
        a.w
    });
    let Some(w) = w else { return };
    if relayout {
        ui::layout(w.main);
    }
    if rebuild_map {
        map::rebuild(w.map);
    }
    // SAFETY: repaint the lists (units, percentages).
    unsafe {
        InvalidateRect(w.list, null(), 0);
        InvalidateRect(w.types, null(), 0);
    }
    invalidate_main();
}

pub fn run(cmd: usize) {
    let Some(w) = main_wnd() else { return };
    match cmd {
        CMD_SCAN => start_scan(false),
        CMD_RESCAN => start_scan(true),
        CMD_FOLDER => pick_folder(),
        CMD_COLLAPSE => {
            with(|a| {
                if let Some(s) = a.snap.clone() {
                    a.mode = Mode::Tree;
                    a.tree = Tree::new(&s.ix, a.sort, a.view_root);
                }
            });
            refresh_list(false);
        }
        CMD_EXPAND => {
            let row = with(|a| {
                let s = a.snap.clone()?;
                if a.mode != Mode::Tree {
                    return None;
                }
                let r = list::selected(a.w.list).unwrap_or(0);
                a.tree.expand_all(&s.ix, a.sort, r, 3_000_000);
                Some(r)
            })
            .flatten();
            refresh_list(true);
            if let Some(r) = row {
                select_row(r);
            }
        }
        CMD_PARENT => {
            let p = with(|a| {
                let ix = a.ix()?;
                let (n, k) = a.selected?;
                let p = if k == Kind::Files { n } else { ix.parent[n as usize] };
                (p != ft_index::NONE && app::is_under(ix, p, a.view_root)).then_some(p)
            })
            .flatten();
            if let Some(p) = p {
                reveal(p);
            }
        }
        CMD_ZOOM_IN | CMD_ZOOM_HERE => {
            let d = with(|a| {
                let ix = a.ix()?;
                let (n, _) = a.selected?;
                Some(if ix.is_dir(n) { n } else { ix.parent[n as usize] })
            })
            .flatten();
            if let Some(d) = d {
                zoom_to(d);
            }
        }
        CMD_ZOOM_OUT => zoom_out(),
        CMD_PNG => save_png(),
        CMD_EXPLORER | CMD_CMD | CMD_COPY_PATH | CMD_COPY | CMD_CUT | CMD_RECYCLE | CMD_DELETE | CMD_DUPES => {
            let Some(sel) = selection() else { return };
            match cmd {
                CMD_EXPLORER => explorer(&sel.path),
                CMD_CMD => {
                    let dir = if sel.dir {
                        sel.path.clone()
                    } else {
                        std::path::Path::new(&sel.path).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
                    };
                    cmd_here(&dir);
                }
                CMD_COPY_PATH => copy_text(w.main, &sel.path),
                CMD_COPY if sel.kind == Kind::Node => copy_files(w.main, &sel.path, false),
                CMD_CUT if sel.deletable => copy_files(w.main, &sel.path, true),
                CMD_RECYCLE => delete(w.main, &sel, false),
                CMD_DELETE => delete(w.main, &sel, true),
                CMD_DUPES => find_dupes(if sel.dir && sel.kind == Kind::Node { sel.node } else { with(|a| a.view_root).unwrap_or(0) }),
                _ => {}
            }
        }
        CMD_EXPORT => export_csv(),
        CMD_EXPORT_TYPES => export_types(),
        CMD_FIND => {
            // SAFETY: focus and select the search text.
            unsafe {
                windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus(w.search);
                send(w.search, 0x00B1 /* EM_SETSEL */, 0, -1);
            }
        }
        CMD_EXIT => {
            // SAFETY: normal close path.
            unsafe { PostMessageW(w.main, WM_CLOSE, 0, 0) };
        }
        CMD_TYPES => {
            with(|a| a.settings.show_types = !a.settings.show_types);
            apply_settings(true, false);
        }
        CMD_MAP => {
            with(|a| a.settings.show_map = !a.settings.show_map);
            apply_settings(true, false);
        }
        CMD_LABELS => {
            with(|a| a.settings.map_labels = !a.settings.map_labels);
            apply_settings(false, true);
        }
        CMD_MAP_ALLOC => {
            with(|a| a.settings.map_alloc = !a.settings.map_alloc);
            apply_settings(false, true);
        }
        c if (CMD_UNITS..CMD_UNITS + UNITS.len()).contains(&c) => {
            with(|a| a.settings.units = UNITS[c - CMD_UNITS]);
            apply_settings(false, false);
        }
        c if (CMD_LANG..CMD_LANG + LANGS.len()).contains(&c) => {
            let l = LANGS[c - CMD_LANG];
            with(|a| a.settings.lang = l);
            lang::set_lang(l);
            apply_language();
        }
        c if (CMD_THEME..CMD_THEME + THEMES.len()).contains(&c) => {
            with(|a| {
                a.settings.theme = THEMES[c - CMD_THEME];
                a.pal = ui::palette_for(a.settings.theme);
            });
            apply_settings(false, false);
            ui::apply_theme();
        }
        CMD_AUTOSCAN => {
            with(|a| a.settings.auto_scan = !a.settings.auto_scan);
            apply_settings(false, false);
        }
        c if (CMD_LIVE..CMD_LIVE + LIVE.len()).contains(&c) => {
            let l = LIVE[c - CMD_LIVE];
            let follow = with(|a| {
                a.settings.live = l;
                a.settings.save();
                // A snapshot of an NTFS volume taken with live updates off: start following.
                a.snap.as_ref().is_some_and(|s| !matches!(s.how, How::Walk { .. }))
            })
            .unwrap_or(false);
            engine::set_live(l);
            if l != Live::Off && follow && !engine::is_live() {
                start_scan(false);
            }
        }
        CMD_CACHE => {
            let on = with(|a| {
                a.settings.cache = !a.settings.cache;
                a.settings.save();
                a.settings.cache
            })
            .unwrap_or(true);
            if !on {
                clear_cache();
            }
        }
        CMD_CLEAR_CACHE => clear_cache(),
        CMD_TAB_TREE | CMD_BACK => set_mode(Mode::Tree),
        CMD_TAB_FILES => set_mode(Mode::Files),
        CMD_ABOUT => {
            let text = format!(
                "FerrumTree
{} {}

{}
{}",
                t(S::Version),
                env!("CARGO_PKG_VERSION"),
                t(S::AboutText),
                t(S::License)
            );
            let (m, c) = (wide(&text), wide(t(S::About)));
            // SAFETY: plain message box.
            unsafe { MessageBoxW(w.main, m.as_ptr(), c.as_ptr(), MB_ICONINFORMATION) };
        }
        _ => {}
    }
}
