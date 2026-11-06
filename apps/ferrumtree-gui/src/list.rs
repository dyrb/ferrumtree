//! The two virtual list views: the main list (tree / files / search / duplicates) and the
//! file types. Text comes on demand (`LVN_GETDISPINFO`); percentage bars and type colour
//! swatches are custom-drawn; header text is recoloured through a subclass of the list
//! view, which is where the header's own notifications go.

use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateFontIndirectW, DeleteObject, GetObjectW, HDC, HFONT, InvalidateRect, LOGFONTW, SelectObject, SetTextColor,
};
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{VK_BACK, VK_LEFT, VK_RIGHT};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};

use crate::app::{Col, Mode, TYPE_COLS, with};
use crate::cmds;
use crate::lang::{S, t};
use crate::model::{Key, Tree};
use crate::theme::{self, Kind as ThemeKind, Palette};
use crate::util::{fill, rect, send, text, wide};

pub const ID_LIST: usize = 106;
pub const ID_TYPES: usize = 107;

/// Subclass proc: header text colour follows the theme.
unsafe extern "system" fn lv_subclass(h: HWND, msg: u32, w: WPARAM, l: LPARAM, _id: usize, _data: usize) -> LRESULT {
    unsafe {
        if msg == windows_sys::Win32::UI::WindowsAndMessaging::WM_NOTIFY {
            let hdr = &*(l as *const NMHDR);
            if hdr.code == NM_CUSTOMDRAW {
                let cd = &*(l as *const NMCUSTOMDRAW);
                if cd.dwDrawStage == CDDS_PREPAINT {
                    return CDRF_NOTIFYITEMDRAW as isize;
                }
                if cd.dwDrawStage == CDDS_ITEMPREPAINT {
                    let c = with(|a| a.pal.text).unwrap_or(0);
                    SetTextColor(cd.hdc, c);
                    return CDRF_DODEFAULT as isize;
                }
            }
        }
        DefSubclassProc(h, msg, w, l)
    }
}

pub fn setup(list: HWND, types: HWND, sys_images: isize) {
    // SAFETY: plain configuration of our own controls.
    unsafe {
        let ex = LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_HEADERDRAGDROP;
        for lv in [list, types] {
            send(lv, LVM_SETEXTENDEDLISTVIEWSTYLE, ex as usize, ex as isize);
            SetWindowSubclass(lv, Some(lv_subclass), 1, 0);
        }
        // Types draw their icons themselves; the image list also sizes their rows.
        for lv in [list, types] {
            send(lv, LVM_SETIMAGELIST, LVSIL_SMALL as usize, sys_images);
        }
    }
}

pub fn set_columns(list: HWND, cols: &[Col], dpi: u32) {
    // SAFETY: plain LVM calls; strings outlive each call.
    unsafe {
        while send(list, LVM_DELETECOLUMN, 0, 0) != 0 {}
        for (i, c) in cols.iter().enumerate() {
            let (w, right) = c.layout();
            insert_column(list, i, t(c.title()), w * dpi as i32 / 96, right);
        }
    }
}

pub fn set_type_columns(types: HWND, dpi: u32) {
    for (i, &(s, w, right)) in TYPE_COLS.iter().enumerate() {
        // SAFETY: as above.
        unsafe { insert_column(types, i, t(s), w * dpi as i32 / 96, right) };
    }
}

unsafe fn insert_column(lv: HWND, i: usize, title: &str, width: i32, right: bool) {
    let w = wide(title);
    unsafe {
        let mut c: LVCOLUMNW = std::mem::zeroed();
        c.mask = LVCF_TEXT | LVCF_WIDTH | LVCF_FMT;
        c.fmt = if right { LVCFMT_RIGHT } else { LVCFMT_LEFT };
        c.cx = width;
        c.pszText = w.as_ptr() as *mut u16;
        send(lv, LVM_INSERTCOLUMNW, i, &c as *const _ as isize);
    }
}

/// Show the sort direction on header item `col` (and clear the others).
pub fn set_sort_arrow(lv: HWND, col: Option<usize>, desc: bool) {
    // SAFETY: header items are queried and updated in place.
    unsafe {
        let hdr = send(lv, LVM_GETHEADER, 0, 0) as HWND;
        let n = send(hdr, HDM_GETITEMCOUNT, 0, 0);
        for i in 0..n.max(0) as usize {
            let mut it: HDITEMW = std::mem::zeroed();
            it.mask = HDI_FORMAT;
            send(hdr, HDM_GETITEMW, i, &mut it as *mut _ as isize);
            it.fmt &= !(HDF_SORTUP | HDF_SORTDOWN);
            if Some(i) == col {
                it.fmt |= if desc { HDF_SORTDOWN } else { HDF_SORTUP };
            }
            send(hdr, HDM_SETITEMW, i, &it as *const _ as isize);
        }
    }
}

pub fn apply_theme(lv: HWND, p: &Palette) {
    // SAFETY: plain configuration of our own controls.
    unsafe {
        theme::style_control(lv, p.dark, ThemeKind::Explorer);
        let hdr = send(lv, LVM_GETHEADER, 0, 0) as HWND;
        if !hdr.is_null() {
            theme::style_control(hdr, p.dark, ThemeKind::ItemsView);
        }
        send(lv, LVM_SETBKCOLOR, 0, p.list as isize);
        send(lv, LVM_SETTEXTBKCOLOR, 0, p.list as isize);
        send(lv, LVM_SETTEXTCOLOR, 0, p.text as isize);
        InvalidateRect(lv, null(), 1);
    }
}

/// Set the item count; without `keep` the selection is dropped (rows mean new items).
pub fn refresh(lv: HWND, n: usize, keep: bool) {
    // SAFETY: owner-data list; text arrives through LVN_GETDISPINFO.
    unsafe {
        if !keep {
            let mut it: LVITEMW = std::mem::zeroed();
            it.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
            send(lv, LVM_SETITEMSTATE, usize::MAX, &it as *const _ as isize);
        }
        send(lv, LVM_SETITEMCOUNT, n, if keep { LVSICF_NOSCROLL as isize } else { 0 });
        InvalidateRect(lv, null(), 0);
    }
}

/// Rows currently on screen.
pub fn visible_rows(lv: HWND) -> std::ops::Range<usize> {
    // SAFETY: plain queries.
    unsafe {
        let top = send(lv, LVM_GETTOPINDEX, 0, 0).max(0) as usize;
        let per = send(lv, LVM_GETCOUNTPERPAGE, 0, 0).max(0) as usize + 1;
        let n = send(lv, LVM_GETITEMCOUNT, 0, 0).max(0) as usize;
        top.min(n)..(top + per).min(n)
    }
}

/// Repaint just these rows (ascending), in runs.
pub fn redraw_rows(lv: HWND, rows: &[usize]) {
    let mut i = 0;
    while i < rows.len() {
        let mut j = i;
        while j + 1 < rows.len() && rows[j + 1] == rows[j] + 1 {
            j += 1;
        }
        // SAFETY: plain invalidation of item rectangles.
        unsafe { send(lv, LVM_REDRAWITEMS, rows[i], rows[j] as isize) };
        i = j + 1;
    }
}

pub fn select(lv: HWND, row: usize) {
    // SAFETY: LVITEMW lives across the synchronous calls.
    unsafe {
        let mut it: LVITEMW = std::mem::zeroed();
        it.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
        send(lv, LVM_SETITEMSTATE, usize::MAX, &it as *const _ as isize);
        it.state = LVIS_SELECTED | LVIS_FOCUSED;
        send(lv, LVM_SETITEMSTATE, row, &it as *const _ as isize);
        send(lv, LVM_ENSUREVISIBLE, row, 0);
    }
}

pub fn selected(lv: HWND) -> Option<usize> {
    // SAFETY: plain query.
    let i = unsafe { send(lv, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize) };
    (i >= 0).then_some(i as usize)
}

fn is_selected(lv: HWND, row: usize) -> bool {
    // SAFETY: plain query.
    unsafe { send(lv, LVM_GETITEMSTATE, row, LVIS_SELECTED as isize) as u32 & LVIS_SELECTED != 0 }
}

fn subitem_rect(lv: HWND, row: usize, sub: i32) -> RECT {
    let mut r = rect(LVIR_BOUNDS as i32, sub, 0, 0);
    // SAFETY: RECT in/out parameter.
    unsafe { send(lv, LVM_GETSUBITEMRECT, row, &mut r as *mut RECT as isize) };
    r
}

/// Percentage cell: a bar and the number on top.
fn draw_bar(dc: HDC, r: &RECT, frac: f64, label: &str, p: &Palette, sel: bool) {
    fill(dc, r, if sel { p.sel } else { p.list });
    let inner = rect(r.left + 3, r.top + 2, r.right - 4, r.bottom - 2);
    fill(dc, &inner, p.bar_bg);
    let w = ((inner.right - inner.left) as f64 * frac.clamp(0.0, 1.0)) as i32;
    fill(dc, &rect(inner.left, inner.top, inner.left + w, inner.bottom), p.bar);
    text(dc, label, &inner, if sel { p.sel_text } else { p.text }, windows_sys::Win32::Graphics::Gdi::DT_CENTER);
}

fn custom_draw(l: LPARAM, types: bool) -> LRESULT {
    // SAFETY: lParam is an NMLVCUSTOMDRAW for list-view custom draw notifications.
    unsafe {
        let cd = &mut *(l as *mut NMLVCUSTOMDRAW);
        let stage = cd.nmcd.dwDrawStage;
        if stage == CDDS_PREPAINT {
            return CDRF_NOTIFYITEMDRAW as isize;
        }
        let row = cd.nmcd.dwItemSpec;
        let lv = cd.nmcd.hdr.hwndFrom;
        let dc = cd.nmcd.hdc;
        if stage == CDDS_ITEMPREPAINT {
            if !types {
                let (dim, del, p, font) =
                    with(|a| (a.dimmed(row), a.deleted(row), a.pal, a.font)).unwrap_or((false, false, crate::theme::LIGHT, null_mut()));
                cd.clrText = if del {
                    deleted_color(&p)
                } else if dim {
                    p.dim
                } else {
                    p.text
                };
                if del {
                    SelectObject(dc, strike_font(font));
                    return (CDRF_NOTIFYSUBITEMDRAW | CDRF_NEWFONT) as isize;
                }
            }
            return CDRF_NOTIFYSUBITEMDRAW as isize;
        }
        if stage != (CDDS_ITEMPREPAINT | CDDS_SUBITEM) {
            return CDRF_DODEFAULT as isize;
        }
        let sub = cd.iSubItem;
        if !types {
            // Deleted since the scan: struck through in red.
            if let Some((p, font)) = with(|a| a.deleted(row).then_some((a.pal, a.font))).flatten() {
                cd.clrText = deleted_color(&p);
                SelectObject(dc, strike_font(font));
                if !a_col_is_percent(sub) {
                    return CDRF_NEWFONT as isize;
                }
            }
            let info = with(|a| {
                let col = *a.cols.get(sub as usize)?;
                (col == Col::Percent).then(|| (a.percent(row), a.cell(row, col), a.pal, a.font))
            })
            .flatten();
            if let Some((frac, label, p, font)) = info {
                let old = SelectObject(dc, font);
                draw_bar(dc, &subitem_rect(lv, row, sub), frac.unwrap_or(0.0), &label, &p, is_selected(lv, row));
                SelectObject(dc, old);
                return CDRF_SKIPDEFAULT as isize;
            }
            return CDRF_DODEFAULT as isize;
        }
        // Types: colour swatch in column 0, bar in column 2.
        let info = with(|a| {
            let id = *a.type_rows.get(row)?;
            let icon = if sub == 0 { a.type_icon(id) } else { -1 };
            Some((a.type_color(id), a.type_cell(row, sub as usize), a.type_fraction(row), a.pal, a.font, a.scale(12), icon))
        })
        .flatten();
        let Some((rgb, label, frac, p, font, sw, icon)) = info else { return CDRF_DODEFAULT as isize };
        let sel = is_selected(lv, row);
        let old = SelectObject(dc, font);
        match sub {
            0 => {
                let mut r = subitem_rect(lv, row, 0);
                r.right = r.left + send(lv, LVM_GETCOLUMNWIDTH, 0, 0) as i32;
                fill(dc, &r, if sel { p.sel } else { p.list });
                let cy = (r.top + r.bottom) / 2;
                let swatch = rect(r.left + 4, cy - sw / 2, r.left + 4 + sw, cy - sw / 2 + sw);
                fill(dc, &swatch, (rgb[2] as u32) << 16 | (rgb[1] as u32) << 8 | rgb[0] as u32);
                // Icon slot (empty for "no extension"), so the extensions line up.
                let images: HIMAGELIST = send(lv, LVM_GETIMAGELIST, LVSIL_SMALL as usize, 0);
                let (mut cx, mut cy) = (0, 0);
                if images != 0 {
                    ImageList_GetIconSize(images, &mut cx, &mut cy);
                }
                let x = swatch.right + 5;
                if icon >= 0 && cx > 0 {
                    ImageList_Draw(images, icon, dc, x, cy_top(r.top, r.bottom, cy), ILD_TRANSPARENT);
                }
                let tx = if cx > 0 { x + cx + 4 } else { swatch.right + 6 };
                text(dc, &label, &rect(tx, r.top, r.right - 2, r.bottom), if sel { p.sel_text } else { p.text }, 0);
            }
            2 => draw_bar(dc, &subitem_rect(lv, row, 2), frac, &label, &p, sel),
            _ => {
                SelectObject(dc, old);
                return CDRF_DODEFAULT as isize;
            }
        }
        SelectObject(dc, old);
        CDRF_SKIPDEFAULT as isize
    }
}

fn a_col_is_percent(sub: i32) -> bool {
    with(|a| a.cols.get(sub as usize) == Some(&Col::Percent)).unwrap_or(false)
}

fn deleted_color(p: &crate::theme::Palette) -> u32 {
    // COLORREF 0x00BBGGRR
    if p.dark { 0x006060FF } else { 0x000000C8 }
}

thread_local! {
    /// (base font, its struck-through twin)
    static STRIKE: std::cell::Cell<(HFONT, HFONT)> = const { std::cell::Cell::new((null_mut(), null_mut())) };
}

/// `base` with a strike-out line, created once per base font.
fn strike_font(base: HFONT) -> HFONT {
    let (b, s) = STRIKE.get();
    if b == base && !s.is_null() {
        return s;
    }
    // SAFETY: reading the LOGFONT of a live font and creating its twin; the previous twin
    // is no longer selected anywhere (fonts are selected only during custom draw).
    unsafe {
        let mut lf: LOGFONTW = std::mem::zeroed();
        GetObjectW(base, size_of::<LOGFONTW>() as i32, (&mut lf as *mut LOGFONTW).cast());
        lf.lfStrikeOut = 1;
        let f = CreateFontIndirectW(&lf);
        if !s.is_null() {
            DeleteObject(s);
        }
        STRIKE.set((base, f));
        f
    }
}

/// Top of something `h` tall, centred between `top` and `bottom`.
fn cy_top(top: i32, bottom: i32, h: i32) -> i32 {
    top + (bottom - top - h) / 2
}

fn dispinfo(l: LPARAM, types: bool) {
    // SAFETY: lParam is an NMLVDISPINFOW; we write into the buffer the control provides.
    unsafe {
        let di = &mut *(l as *mut NMLVDISPINFOW);
        let (row, sub) = (di.item.iItem as usize, di.item.iSubItem as usize);
        if di.item.mask & LVIF_TEXT != 0 && !di.item.pszText.is_null() && di.item.cchTextMax > 0 {
            let s = with(|a| if types { a.type_cell(row, sub) } else { a.cols.get(sub).map(|&c| a.cell(row, c)).unwrap_or_default() })
                .unwrap_or_default();
            let w: Vec<u16> = s.encode_utf16().take(di.item.cchTextMax as usize - 1).collect();
            std::ptr::copy_nonoverlapping(w.as_ptr(), di.item.pszText, w.len());
            *di.item.pszText.add(w.len()) = 0;
        }
        if !types && sub == 0 {
            let (icon, depth) = with(|a| (a.icon(row), a.row(row).map_or(0, |r| r.2))).unwrap_or((-1, 0));
            if di.item.mask & LVIF_IMAGE != 0 {
                di.item.iImage = icon;
            }
            if di.item.mask & LVIF_INDENT != 0 {
                di.item.iIndent = depth as i32;
            }
        }
    }
}

/// WM_NOTIFY from one of the two lists.
pub fn on_notify(l: LPARAM) -> Option<LRESULT> {
    // SAFETY: lParam of WM_NOTIFY points at an NMHDR-prefixed structure for its code.
    unsafe {
        let hdr = &*(l as *const NMHDR);
        let types = match hdr.idFrom {
            ID_LIST => false,
            ID_TYPES => true,
            _ => return None,
        };
        match hdr.code {
            LVN_GETDISPINFOW => dispinfo(l, types),
            NM_CUSTOMDRAW => return Some(custom_draw(l, types)),
            LVN_ITEMCHANGED => {
                let nm = &*(l as *const NMLISTVIEW);
                let became = nm.uNewState & LVIS_SELECTED != 0 && nm.uOldState & LVIS_SELECTED == 0;
                let lost = nm.uNewState & LVIS_SELECTED == 0 && nm.uOldState & LVIS_SELECTED != 0;
                if types {
                    if became && nm.iItem >= 0 {
                        cmds::select_type(with(|a| a.type_rows.get(nm.iItem as usize).copied()).flatten());
                    } else if lost && selected(hdr.hwndFrom).is_none() {
                        cmds::select_type(None);
                    }
                } else if became && nm.iItem >= 0 {
                    cmds::on_row_selected(nm.iItem as usize);
                }
            }
            NM_DBLCLK | NM_RETURN if !types => {
                if let Some(r) = selected(hdr.hwndFrom) {
                    cmds::activate(r);
                }
            }
            LVN_KEYDOWN if !types => {
                let vk = (*(l as *const NMLVKEYDOWN)).wVKey;
                if let Some(r) = selected(hdr.hwndFrom) {
                    on_key(r, vk);
                }
            }
            NM_RCLICK => {
                let nm = &*(l as *const NMITEMACTIVATE);
                if !types && nm.iItem >= 0 {
                    select(hdr.hwndFrom, nm.iItem as usize);
                    cmds::context_menu(None);
                }
            }
            LVN_COLUMNCLICK => {
                let nm = &*(l as *const NMLISTVIEW);
                if types {
                    cmds::sort_types(nm.iSubItem as usize);
                } else {
                    cmds::sort_list(nm.iSubItem as usize);
                }
            }
            _ => {}
        }
        Some(0)
    }
}

fn on_key(row: usize, vk: u16) {
    let action = with(|a| {
        let r = a.tree_row(row)?;
        let ix = a.ix()?;
        let can = Tree::expandable(ix, &r);
        match vk {
            VK_RIGHT if can && !r.expanded => Some((row, true)),
            VK_LEFT if r.expanded => Some((row, true)),
            VK_LEFT | VK_BACK => a.tree.parent_row(row).map(|p| (p, false)),
            _ => None,
        }
    })
    .flatten();
    match action {
        Some((r, true)) => cmds::toggle(r),
        Some((r, false)) => cmds::select_row(r),
        None => {}
    }
}

/// Header index of the column sorted by `key` in the current mode.
pub fn sort_column(cols: &[Col], key: Key, map_alloc: bool) -> Option<usize> {
    cols.iter().position(|c| c.key(map_alloc) == Some(key) && *c != Col::Percent)
}

pub fn mode_title(m: Mode) -> &'static str {
    t(match m {
        Mode::Tree => S::TabTree,
        Mode::Files => S::TabFiles,
        Mode::Search => S::TabSearch,
        Mode::Dupes => S::TabDupes,
    })
}
