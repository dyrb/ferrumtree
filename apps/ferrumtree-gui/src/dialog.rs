//! Windows Common Item Dialogs. Called on the UI thread after OLE initialization,
//! without holding a borrow of application state across the modal message loop.

use std::ffi::{OsString, c_void};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{E_POINTER, ERROR_CANCELLED, HWND};
use windows_sys::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows_sys::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows_sys::Win32::UI::Shell::{
    FOS_DONTADDTORECENT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR, FOS_NOTESTFILECREATE, FOS_OVERWRITEPROMPT,
    FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog, FileSaveDialog, SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
};
use windows_sys::core::{GUID, HRESULT};

const IID_IFILEDIALOG: GUID = GUID::from_u128(0x42F85136_DB7E_439C_85F1_E4075D135FC8);
const IID_ISHELLITEM: GUID = GUID::from_u128(0x43826D1E_E718_42EE_BC55_A1E261C37BFE);
const CANCELLED: HRESULT = (0x80070000 | ERROR_CANCELLED) as HRESULT;

#[derive(Clone, Copy, Default)]
pub enum Kind {
    #[default]
    Open,
    Folder,
    Save {
        overwrite: bool,
    },
}

#[derive(Default)]
pub struct Options<'a> {
    pub kind: Kind,
    pub title: &'a str,
    pub initial: Option<&'a Path>,
    pub file_name: &'a str,
    pub extension: &'a str,
    pub filters: &'a [(&'a str, &'a str)],
}

#[repr(C)]
struct UnknownVtbl {
    _query: usize,
    _add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
}

#[repr(C)]
struct FileDialogVtbl {
    unknown: UnknownVtbl,
    show: unsafe extern "system" fn(*mut c_void, HWND) -> HRESULT,
    set_file_types: unsafe extern "system" fn(*mut c_void, u32, *const COMDLG_FILTERSPEC) -> HRESULT,
    set_file_type_index: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    get_file_type_index: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
    _advise: [usize; 2],
    set_options: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    get_options: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
    _set_default_folder: usize,
    set_folder: unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
    _get_folder_and_selection: [usize; 2],
    set_file_name: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
    get_file_name: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HRESULT,
    set_title: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
    _labels: [usize; 2],
    get_result: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    _add_place: usize,
    set_default_extension: unsafe extern "system" fn(*mut c_void, *const u16) -> HRESULT,
}

#[repr(C)]
struct ShellItemVtbl {
    unknown: UnknownVtbl,
    _bind: usize,
    get_parent: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    get_display_name: unsafe extern "system" fn(*mut c_void, i32, *mut *mut u16) -> HRESULT,
    get_attributes: unsafe extern "system" fn(*mut c_void, u32, *mut u32) -> HRESULT,
}

struct Com(*mut c_void);

impl Com {
    fn owned(hr: HRESULT, raw: *mut c_void) -> Result<Self, HRESULT> {
        check(hr)?;
        if raw.is_null() { Err(E_POINTER) } else { Ok(Self(raw)) }
    }

    /// # Safety
    /// This interface pointer must expose the vtable prefix `T`.
    unsafe fn vt<T>(&self) -> &T {
        unsafe { &**(self.0 as *const *const T) }
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        // SAFETY: every owned interface starts with IUnknown.
        unsafe { (self.vt::<UnknownVtbl>().release)(self.0) };
    }
}

struct TaskString(*mut u16);

impl TaskString {
    fn path(&self) -> Result<PathBuf, HRESULT> {
        if self.0.is_null() {
            return Err(E_POINTER);
        }
        // SAFETY: successful shell string results are NUL-terminated task allocations.
        unsafe {
            let mut len = 0;
            while *self.0.add(len) != 0 {
                len += 1;
            }
            Ok(PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(self.0, len))))
        }
    }
}

impl Drop for TaskString {
    fn drop(&mut self) {
        // SAFETY: the shell allocated this string; null is also accepted.
        unsafe { CoTaskMemFree(self.0.cast()) };
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn check(hr: HRESULT) -> Result<(), HRESULT> {
    if hr < 0 { Err(hr) } else { Ok(()) }
}

struct Filters {
    _strings: Vec<(Vec<u16>, Vec<u16>)>,
    specs: Vec<COMDLG_FILTERSPEC>,
}

impl Filters {
    fn new(filters: &[(&str, &str)]) -> Self {
        let strings: Vec<_> = filters.iter().map(|(name, pattern)| (wide(name), wide(pattern))).collect();
        let specs = strings.iter().map(|(name, pattern)| COMDLG_FILTERSPEC { pszName: name.as_ptr(), pszSpec: pattern.as_ptr() }).collect();
        Self { _strings: strings, specs }
    }
}

fn create(options: &Options<'_>, filters: &Filters) -> Result<Com, HRESULT> {
    // SAFETY: the caller runs in an initialized STA. Both classes implement IFileDialog;
    // its inherited vtable layout matches the Windows SDK. All references are owned.
    unsafe {
        let class = if matches!(options.kind, Kind::Save { .. }) { &FileSaveDialog } else { &FileOpenDialog };
        let mut raw = null_mut();
        let hr = CoCreateInstance(class, null_mut(), CLSCTX_INPROC_SERVER, &IID_IFILEDIALOG, &mut raw);
        let dialog = Com::owned(hr, raw)?;
        let vt = dialog.vt::<FileDialogVtbl>();
        let mut flags = 0;
        check((vt.get_options)(dialog.0, &mut flags))?;
        flags &= !(FOS_PICKFOLDERS | FOS_FILEMUSTEXIST | FOS_OVERWRITEPROMPT);
        flags |= FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST | FOS_NOCHANGEDIR | FOS_DONTADDTORECENT;
        flags |= match options.kind {
            Kind::Folder => FOS_PICKFOLDERS,
            Kind::Open => FOS_FILEMUSTEXIST,
            Kind::Save { overwrite } => FOS_NOTESTFILECREATE | if overwrite { FOS_OVERWRITEPROMPT } else { 0 },
        };
        check((vt.set_options)(dialog.0, flags))?;
        if !matches!(options.kind, Kind::Folder) && !filters.specs.is_empty() {
            check((vt.set_file_types)(dialog.0, filters.specs.len() as u32, filters.specs.as_ptr()))?;
            check((vt.set_file_type_index)(dialog.0, 1))?;
        }
        for (value, set) in
            [(options.title, vt.set_title), (options.file_name, vt.set_file_name), (options.extension, vt.set_default_extension)]
        {
            if !value.is_empty() {
                check(set(dialog.0, wide(value).as_ptr()))?;
            }
        }
        // An unavailable initial path must not prevent browsing. Existing files seed
        // their parent directory; neither lookup nor selection changes the working dir.
        if let Some(initial) = options.initial.filter(|p| !p.as_os_str().is_empty()) {
            let path = std::path::absolute(initial).unwrap_or_else(|_| initial.to_path_buf());
            let path: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
            let mut raw = null_mut();
            let hr = SHCreateItemFromParsingName(path.as_ptr(), null_mut(), &IID_ISHELLITEM, &mut raw);
            if let Ok(item) = Com::owned(hr, raw) {
                let ivt = item.vt::<ShellItemVtbl>();
                let mut attrs = 0;
                const FOLDER: u32 = 0x20000000;
                if (ivt.get_attributes)(item.0, FOLDER, &mut attrs) >= 0 {
                    if attrs & FOLDER != 0 {
                        (vt.set_folder)(dialog.0, item.0);
                    } else {
                        let mut raw = null_mut();
                        let hr = (ivt.get_parent)(item.0, &mut raw);
                        if let Ok(parent) = Com::owned(hr, raw) {
                            (vt.set_folder)(dialog.0, parent.0);
                        }
                    }
                }
            }
        }
        Ok(dialog)
    }
}

/// Return the full filesystem path and the one-based file type index. Cancel is not
/// an error. Paths retain their UTF-16 data and are not limited to a fixed-size buffer.
pub fn show(owner: HWND, options: Options<'_>) -> Result<Option<(PathBuf, u32)>, HRESULT> {
    let filters = Filters::new(options.filters);
    let dialog = create(&options, &filters)?;
    // SAFETY: create returned IFileDialog. Its strings, filters and interfaces outlive Show.
    unsafe {
        let vt = dialog.vt::<FileDialogVtbl>();
        let hr = (vt.show)(dialog.0, owner);
        if hr == CANCELLED {
            return Ok(None);
        }
        check(hr)?;
        let mut raw = null_mut();
        let hr = (vt.get_result)(dialog.0, &mut raw);
        let item = Com::owned(hr, raw)?;
        let mut name = null_mut();
        let hr = (item.vt::<ShellItemVtbl>().get_display_name)(item.0, SIGDN_FILESYSPATH, &mut name);
        let name = TaskString(name);
        check(hr)?;
        let path = name.path()?;
        let mut index = 1;
        if !matches!(options.kind, Kind::Folder) && !filters.specs.is_empty() {
            check((vt.get_file_type_index)(dialog.0, &mut index))?;
        }
        Ok(Some((path, index)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};

    #[test]
    fn system_dialog_interfaces_preserve_modes_filters_and_names_without_showing() {
        struct Apartment;
        impl Drop for Apartment {
            fn drop(&mut self) {
                unsafe { CoUninitialize() };
            }
        }
        assert!(unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) } >= 0);
        let _apartment = Apartment;
        for kind in [Kind::Open, Kind::Folder, Kind::Save { overwrite: false }, Kind::Save { overwrite: true }] {
            let options = Options {
                kind,
                file_name: "отчёт",
                extension: "csv",
                filters: &[("CSV (*.csv)", "*.csv"), ("JSON (*.json)", "*.json")],
                ..Default::default()
            };
            let filters = Filters::new(options.filters);
            let dialog = create(&options, &filters).expect("create and configure system dialog");
            unsafe {
                let vt = dialog.vt::<FileDialogVtbl>();
                let mut flags = 0;
                check((vt.get_options)(dialog.0, &mut flags)).unwrap();
                assert_ne!(flags & FOS_NOCHANGEDIR, 0);
                assert_ne!(flags & FOS_FORCEFILESYSTEM, 0);
                assert_eq!(flags & FOS_PICKFOLDERS != 0, matches!(kind, Kind::Folder));
                assert_eq!(flags & FOS_FILEMUSTEXIST != 0, matches!(kind, Kind::Open | Kind::Folder));
                assert_eq!(flags & FOS_OVERWRITEPROMPT != 0, matches!(kind, Kind::Save { overwrite: true }));
                if matches!(kind, Kind::Save { .. }) {
                    assert_ne!(flags & FOS_NOTESTFILECREATE, 0);
                }
                if !matches!(kind, Kind::Folder) {
                    check((vt.set_file_type_index)(dialog.0, 2)).unwrap();
                    let mut index = 0;
                    check((vt.get_file_type_index)(dialog.0, &mut index)).unwrap();
                    assert_eq!(index, 2);
                }
                let mut name = null_mut();
                let hr = (vt.get_file_name)(dialog.0, &mut name);
                let name = TaskString(name);
                check(hr).unwrap();
                assert_eq!(name.path().unwrap(), PathBuf::from("отчёт"));
            }
        }
    }
}
