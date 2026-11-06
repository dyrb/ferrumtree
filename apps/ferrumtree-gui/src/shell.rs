//! Explorer's own context menu for a file or folder: the item's `IContextMenu`, taken
//! from its parent shell folder. windows-sys has no COM interfaces,
//! so the few vtable slots used here are declared by hand.
//!
//! While the menu is open, owner-drawn and submenu messages ("Send to", "Open with",
//! "New") reach the main window, which hands them back here ([`menu_message`]).

use std::cell::Cell;
use std::ffi::c_void;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Controls::ODT_MENU;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT};
use windows_sys::Win32::UI::Shell::Common::ITEMIDLIST;
use windows_sys::Win32::UI::Shell::{
    CMF_EXTENDEDVERBS, CMF_NORMAL, CMIC_MASK_CONTROL_DOWN, CMIC_MASK_PTINVOKE, CMIC_MASK_SHIFT_DOWN, CMINVOKECOMMANDINFOEX, GCS_VERBW,
    SHBindToParent, SHParseDisplayName,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, HMENU, SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx, WM_DRAWITEM, WM_INITMENUPOPUP,
    WM_MEASUREITEM, WM_MENUCHAR,
};
use windows_sys::core::{GUID, HRESULT};

use crate::util::wide;

const IID_ISHELLFOLDER: GUID = GUID::from_u128(0x000214E6_0000_0000_C000_000000000046);
const IID_ICONTEXTMENU: GUID = GUID::from_u128(0x000214E4_0000_0000_C000_000000000046);
const IID_ICONTEXTMENU2: GUID = GUID::from_u128(0x000214F4_0000_0000_C000_000000000046);
const IID_ICONTEXTMENU3: GUID = GUID::from_u128(0xBCFCE0A0_EC17_11D0_8D10_00A0C90F2719);

/// `CMIC_MASK_UNICODE` (missing from windows-sys): `lpVerbW` and friends are valid.
const CMIC_MASK_UNICODE: u32 = 0x4000;
/// Menu ids handed to the shell: 1..=LAST (0 means "nothing chosen").
const FIRST: u32 = 1;
const LAST: u32 = 0x7FFF;

type Fn0 = unsafe extern "system" fn(*mut c_void) -> u32;

#[repr(C)]
struct UnknownVtbl {
    query_interface: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: Fn0,
    release: Fn0,
}

#[repr(C)]
struct ShellFolderVtbl {
    unknown: UnknownVtbl,
    /// ParseDisplayName .. GetAttributesOf (slots 3..=9), unused.
    _other: [usize; 7],
    get_ui_object_of:
        unsafe extern "system" fn(*mut c_void, HWND, u32, *const *const ITEMIDLIST, *const GUID, *mut u32, *mut *mut c_void) -> HRESULT,
}

/// IContextMenu, extended by IContextMenu2 (`handle_menu_msg`) and IContextMenu3
/// (`handle_menu_msg2`): those slots exist only on pointers obtained for those IIDs.
#[repr(C)]
struct ContextMenuVtbl {
    unknown: UnknownVtbl,
    query_context_menu: unsafe extern "system" fn(*mut c_void, HMENU, u32, u32, u32, u32) -> HRESULT,
    invoke_command: unsafe extern "system" fn(*mut c_void, *const CMINVOKECOMMANDINFOEX) -> HRESULT,
    get_command_string: unsafe extern "system" fn(*mut c_void, usize, u32, *mut u32, *mut u8, u32) -> HRESULT,
    handle_menu_msg: unsafe extern "system" fn(*mut c_void, u32, WPARAM, LPARAM) -> HRESULT,
    handle_menu_msg2: unsafe extern "system" fn(*mut c_void, u32, WPARAM, LPARAM, *mut LRESULT) -> HRESULT,
}

/// An owned COM interface pointer, released on drop.
struct Com(*mut c_void);

impl Com {
    /// # Safety
    /// The object's vtable must start with `T`'s layout.
    unsafe fn vt<T>(&self) -> &T {
        unsafe { &**(self.0 as *const *const T) }
    }

    fn query(&self, iid: &GUID) -> Option<Com> {
        let mut p = null_mut();
        // SAFETY: every COM vtable starts with IUnknown.
        let hr = unsafe { (self.vt::<UnknownVtbl>().query_interface)(self.0, iid, &mut p) };
        (hr >= 0 && !p.is_null()).then_some(Com(p))
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: we own one reference.
        unsafe { (self.vt::<UnknownVtbl>().release)(self.0) };
    }
}

/// A PIDL from the shell allocator.
struct Pidl(*mut ITEMIDLIST);

impl Drop for Pidl {
    fn drop(&mut self) {
        // SAFETY: allocated by SHParseDisplayName.
        unsafe { CoTaskMemFree(self.0.cast()) };
    }
}

thread_local! {
    /// The open menu's IContextMenu2/3 and its level (2 or 3), while it is tracked.
    static ACTIVE: Cell<(*mut c_void, u8)> = const { Cell::new((null_mut(), 0)) };
}

/// Messages of the open shell menu, which arrive at its owner window. `Some` when the
/// menu handled the message (the window procedure returns that result).
pub fn menu_message(msg: u32, w: WPARAM, l: LPARAM) -> Option<LRESULT> {
    let (p, level) = ACTIVE.get();
    if p.is_null() || !matches!(msg, WM_INITMENUPOPUP | WM_MENUCHAR | WM_DRAWITEM | WM_MEASUREITEM) {
        return None;
    }
    // Our own owner-drawn controls also send these two; both structs start with CtlType.
    // SAFETY: lParam of WM_DRAWITEM / WM_MEASUREITEM points to such a struct.
    if matches!(msg, WM_DRAWITEM | WM_MEASUREITEM) && unsafe { *(l as *const u32) } != ODT_MENU {
        return None;
    }
    // SAFETY: `p` is a live IContextMenu2/3 for the duration of the menu loop.
    unsafe {
        let vt = &**(p as *const *const ContextMenuVtbl);
        if level == 3 {
            let mut r: LRESULT = 0;
            ((vt.handle_menu_msg2)(p, msg, w, l, &mut r) >= 0).then_some(r)
        } else if msg != WM_MENUCHAR {
            ((vt.handle_menu_msg)(p, msg, w, l) >= 0).then_some(if msg == WM_INITMENUPOPUP { 0 } else { 1 })
        } else {
            None
        }
    }
}

fn key_down(vk: u16) -> bool {
    // SAFETY: plain query.
    unsafe { GetKeyState(vk as i32) < 0 }
}

pub fn shift_down() -> bool {
    key_down(VK_SHIFT)
}

/// Show the shell's context menu for `path` at screen point `pt`. `intercept` sees the
/// chosen command's canonical verb (such as `delete`, `copy`, `properties`) first and
/// returns `true` if it carried the command out itself.
///
/// `false` if the shell has no menu for `path` (it no longer exists, or is not
/// something the shell can show); the caller then offers its own.
pub fn context_menu(owner: HWND, path: &str, pt: POINT, intercept: impl FnOnce(&str) -> bool) -> bool {
    // "C:" names the current directory on C:, "C:\" the drive.
    let path = if path.len() == 2 && path.ends_with(':') { format!("{path}\\") } else { path.to_string() };
    let wpath = wide(&path);
    // SAFETY: shell calls with valid out-pointers; every object is released by its owner
    // (Com / Pidl), `last` points into `pidl`, which outlives its use.
    unsafe {
        let mut raw = null_mut();
        if SHParseDisplayName(wpath.as_ptr(), null_mut(), &mut raw, 0, null_mut()) < 0 || raw.is_null() {
            return false;
        }
        let pidl = Pidl(raw);
        let (mut folder, mut last) = (null_mut(), null_mut());
        if SHBindToParent(pidl.0, &IID_ISHELLFOLDER, &mut folder, &mut last) < 0 || folder.is_null() {
            return false;
        }
        let folder = Com(folder);
        let mut cm = null_mut();
        let items = [last.cast_const()];
        let hr =
            (folder.vt::<ShellFolderVtbl>().get_ui_object_of)(folder.0, owner, 1, items.as_ptr(), &IID_ICONTEXTMENU, null_mut(), &mut cm);
        if hr < 0 || cm.is_null() {
            return false;
        }
        let cm = Com(cm);
        let (ext, level) = match cm.query(&IID_ICONTEXTMENU3) {
            Some(c) => (Some(c), 3),
            None => (cm.query(&IID_ICONTEXTMENU2), 2),
        };

        let menu = CreatePopupMenu();
        if menu.is_null() {
            return false;
        }
        let flags = CMF_NORMAL | if shift_down() { CMF_EXTENDEDVERBS } else { 0 };
        if (cm.vt::<ContextMenuVtbl>().query_context_menu)(cm.0, menu, 0, FIRST, LAST, flags) < 0 {
            DestroyMenu(menu);
            return false;
        }
        ACTIVE.set(ext.as_ref().map_or((null_mut(), 0), |c| (c.0, level)));
        let cmd = TrackPopupMenuEx(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, owner, null());
        ACTIVE.set((null_mut(), 0));
        DestroyMenu(menu);
        if cmd < FIRST as i32 {
            return true;
        }
        let id = cmd as u32 - FIRST;

        let mut verb = [0u16; 64];
        let hr = (cm.vt::<ContextMenuVtbl>().get_command_string)(
            cm.0,
            id as usize,
            GCS_VERBW,
            null_mut(),
            verb.as_mut_ptr().cast(),
            verb.len() as u32,
        );
        if hr >= 0 {
            let n = verb.iter().position(|&c| c == 0).unwrap_or(0);
            if intercept(&String::from_utf16_lossy(&verb[..n]).to_ascii_lowercase()) {
                return true;
            }
        }

        let mut mask = CMIC_MASK_UNICODE | CMIC_MASK_PTINVOKE;
        if shift_down() {
            mask |= CMIC_MASK_SHIFT_DOWN;
        }
        if key_down(VK_CONTROL) {
            mask |= CMIC_MASK_CONTROL_DOWN;
        }
        let info = CMINVOKECOMMANDINFOEX {
            cbSize: size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            fMask: mask,
            hwnd: owner,
            // MAKEINTRESOURCE: the command offset, not a verb string.
            lpVerb: id as usize as *const u8,
            lpVerbW: id as usize as *const u16,
            nShow: SW_SHOWNORMAL,
            ptInvoke: pt,
            ..Default::default()
        };
        (cm.vt::<ContextMenuVtbl>().invoke_command)(cm.0, &info);
        true
    }
}
