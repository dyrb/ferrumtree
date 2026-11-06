//! Application state and the data side of the UI (no painting here).
//!
//! The state lives in a thread-local `RefCell`. [`with`] borrows it for computation only:
//! callers release it before any Win32 call that can synchronously send a message back
//! (the list views ask for text through `WM_NOTIFY`), so re-entrancy never panics.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use ft_index::types::NOT_FILE;
use ft_index::{Index, NONE, nflags};
use ft_search::{Query, SearchIndex};
use ft_treemap::{Options, Style, Treemap, type_color};
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{HBRUSH, HFONT};
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows_sys::Win32::UI::Shell::{
    SHFILEINFOW, SHGFI_SMALLICON, SHGFI_SYSICONINDEX, SHGFI_TYPENAME, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::HACCEL;

use crate::engine::{self, How, Patch, Snapshot, Target};
use crate::lang::{self, S, t};
use crate::model::{Key, Kind, Row, Sort, Tree, node_vals, sort_nodes};
use crate::settings::Settings;
use crate::theme::Palette;
use crate::util::{filetime, wide};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Tree,
    Files,
    Search,
    Dupes,
}

/// Columns of the main list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Col {
    Name,
    Percent,
    Size,
    Alloc,
    Items,
    Files,
    Folders,
    Modified,
    Folder,
    Group,
}

impl Col {
    pub fn title(self) -> S {
        match self {
            Col::Name => S::ColName,
            Col::Percent => S::ColPercent,
            Col::Size => S::ColSize,
            Col::Alloc => S::ColAlloc,
            Col::Items => S::ColItems,
            Col::Files => S::ColFiles,
            Col::Folders => S::ColFolders,
            Col::Modified => S::ColModified,
            Col::Folder => S::ColFolder,
            Col::Group => S::ColGroup,
        }
    }
    /// Width at 96 dpi and right alignment.
    pub fn layout(self) -> (i32, bool) {
        match self {
            Col::Name => (330, false),
            Col::Percent => (120, false),
            Col::Size | Col::Alloc => (92, true),
            Col::Items | Col::Files => (88, true),
            Col::Folders => (76, true),
            Col::Modified => (124, false),
            Col::Folder => (420, false),
            Col::Group => (70, true),
        }
    }
    pub fn key(self, map_alloc: bool) -> Option<Key> {
        Some(match self {
            Col::Name => Key::Name,
            Col::Percent => {
                if map_alloc {
                    Key::Alloc
                } else {
                    Key::Size
                }
            }
            Col::Size => Key::Size,
            Col::Alloc => Key::Alloc,
            Col::Items => Key::Items,
            Col::Files => Key::Files,
            Col::Folders => Key::Folders,
            Col::Modified => Key::Modified,
            Col::Folder => Key::Folder,
            Col::Group => return None,
        })
    }
}

pub fn columns(mode: Mode) -> &'static [Col] {
    match mode {
        Mode::Tree => &[Col::Name, Col::Percent, Col::Size, Col::Alloc, Col::Items, Col::Files, Col::Folders, Col::Modified],
        Mode::Files | Mode::Search => &[Col::Name, Col::Size, Col::Alloc, Col::Modified, Col::Folder],
        Mode::Dupes => &[Col::Name, Col::Group, Col::Size, Col::Alloc, Col::Modified, Col::Folder],
    }
}

/// Columns of the file-types list.
pub const TYPE_COLS: [(S, i32, bool); 6] = [
    (S::ColExt, 110, false),
    (S::ColType, 170, false),
    (S::ColPercent, 96, false),
    (S::ColSize, 86, true),
    (S::ColAlloc, 86, true),
    (S::ColFiles, 80, true),
];

#[derive(Default, Clone, Copy)]
pub struct Wnd {
    pub main: HWND,
    pub combo: HWND,
    pub scan: HWND,
    pub search: HWND,
    pub clear: HWND,
    pub list: HWND,
    pub types: HWND,
    pub map: HWND,
}

/// Rectangles of the painted chrome, for drawing and hit testing.
#[derive(Default, Clone)]
pub struct Layout {
    pub menus: Vec<(RECT, usize)>,
    pub top: RECT,
    pub status_line: RECT,
    pub progress: RECT,
    pub info: RECT,
    pub brand: RECT,
    pub search_row: RECT,
    pub tabs: Vec<(RECT, Mode)>,
    pub tab_row: RECT,
    pub list: RECT,
    pub split_v: RECT,
    pub types: RECT,
    pub split_h: RECT,
    pub crumb: RECT,
    pub map: RECT,
    pub statusbar: RECT,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Drag {
    Vertical,
    Horizontal,
}

/// Weight of the treemap root (what the map's areas add up to).
fn map_weight(ix: &Index, alloc: bool, root: u32) -> u64 {
    let w = if alloc { &ix.alloc } else { &ix.size };
    w.get(root as usize).copied().unwrap_or(0)
}

/// What a change of the tree in place calls for.
pub struct Changed {
    /// Row of the selection, when the rows were rebuilt.
    pub row: Option<usize>,
    /// The treemap differs visibly.
    pub relayout: bool,
    /// Rows were rebuilt (nodes added or moved): the whole list is repainted.
    pub structural: bool,
    /// The file-type rows changed order.
    pub types_reordered: bool,
}

pub struct App {
    pub w: Wnd,
    pub dpi: u32,
    pub font: HFONT,
    pub bold: HFONT,
    pub small: HFONT,
    pub pal: Palette,
    pub brush_input: HBRUSH,
    pub settings: Settings,
    pub accel: HACCEL,
    pub targets: Vec<(Target, String)>,
    pub snap: Option<Arc<Snapshot>>,
    /// Changes not applied yet: the snapshot was shared with a worker (export, duplicates).
    pending: Vec<ft_index::patch::Change>,
    /// What the visible rows showed when last checked: after a live change only rows whose
    /// text differs are repainted.
    pub shown_rows: HashMap<usize, u64>,
    pub shown_types: HashMap<usize, u64>,
    /// Same for the summary panel and the status bar.
    pub shown_chrome: u64,
    /// File type totals within the shown folder (`None`: the whole volume).
    scoped_types: Option<Vec<(u64, u64, u64)>>,
    /// Built on the first search; keyed by snapshot and index revision.
    search_index: Option<((usize, u64), SearchIndex)>,
    pub tree: Tree,
    pub sort: Sort,
    pub mode: Mode,
    pub flat: Vec<u32>,
    pub flat_sort: Sort,
    pub dupes: Option<Vec<(u32, String)>>,
    pub dupe_group: Vec<u32>,
    pub list_msg: Option<String>,
    pub selected: Option<(u32, Kind)>,
    pub view_root: u32,
    pub tm_root: u32,
    pub type_rows: Vec<u16>,
    pub type_sort: (usize, bool),
    pub type_sel: Option<u16>,
    pub type_names: HashMap<String, String>,
    icons: HashMap<Vec<u8>, i32>,
    pub tm: Treemap,
    pub tm_px: Vec<u32>,
    pub tm_owner: Vec<u32>,
    pub hover: u32,
    pub busy: bool,
    pub scan_started: Instant,
    pub status: String,
    pub hover_text: String,
    pub drag: Option<Drag>,
    pub menu_hot: Option<usize>,
    pub layout: Layout,
    pub disk: Option<(u64, u64)>,
    pub cols: &'static [Col],
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

pub fn install(app: App) {
    APP.with(|a| *a.borrow_mut() = Some(app));
}

/// Run `f` on the app state; `None` if it does not exist or is already borrowed.
pub fn with<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

pub fn take() -> Option<App> {
    APP.with(|a| a.borrow_mut().take())
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        w: Wnd,
        dpi: u32,
        fonts: (HFONT, HFONT, HFONT),
        pal: Palette,
        brush_input: HBRUSH,
        settings: Settings,
        accel: HACCEL,
        targets: Vec<(Target, String)>,
    ) -> App {
        App {
            w,
            dpi,
            font: fonts.0,
            bold: fonts.1,
            small: fonts.2,
            pal,
            brush_input,
            settings,
            accel,
            targets,
            snap: None,
            pending: Vec::new(),
            shown_rows: HashMap::new(),
            shown_types: HashMap::new(),
            shown_chrome: 0,
            scoped_types: None,
            search_index: None,
            tree: Tree::default(),
            sort: Sort::default(),
            mode: Mode::Tree,
            flat: Vec::new(),
            flat_sort: Sort::default(),
            dupes: None,
            dupe_group: Vec::new(),
            list_msg: None,
            selected: None,
            view_root: 0,
            tm_root: 0,
            type_rows: Vec::new(),
            type_sort: (3, true),
            type_sel: None,
            type_names: HashMap::new(),
            icons: HashMap::new(),
            tm: Treemap::default(),
            tm_px: Vec::new(),
            tm_owner: Vec::new(),
            hover: NONE,
            busy: false,
            scan_started: Instant::now(),
            status: String::new(),
            hover_text: String::new(),
            drag: None,
            menu_hot: None,
            layout: Layout::default(),
            disk: None,
            cols: columns(Mode::Tree),
        }
    }

    pub fn scale(&self, v: i32) -> i32 {
        v * self.dpi as i32 / 96
    }

    pub fn ix(&self) -> Option<&Index> {
        self.snap.as_deref().map(|s| &s.ix)
    }

    pub fn size(&self, n: u64) -> String {
        lang::size(n, self.settings.units)
    }

    // ---------------------------------------------------------------- list rows

    pub fn row_count(&self) -> usize {
        match (self.snap.is_some(), self.mode) {
            (false, _) => 0,
            (true, Mode::Tree) => self.tree.rows.len(),
            (true, _) => self.flat.len(),
        }
    }

    /// Node and kind shown in a list row.
    pub fn row(&self, row: usize) -> Option<(u32, Kind, u16)> {
        match self.mode {
            Mode::Tree => self.tree.rows.get(row).map(|r| (r.node, r.kind, r.depth)),
            _ => self.flat.get(row).map(|&n| (n, Kind::Node, 0)),
        }
    }

    pub fn tree_row(&self, row: usize) -> Option<Row> {
        (self.mode == Mode::Tree).then(|| self.tree.rows.get(row).copied()).flatten()
    }

    pub fn cell(&self, row: usize, col: Col) -> String {
        let (Some(ix), Some((n, kind, _))) = (self.ix(), self.row(row)) else { return String::new() };
        let i = n as usize;
        let v = match (self.mode, kind) {
            (Mode::Tree, Kind::Files) => self.tree.vals(ix, &self.tree.rows[row]),
            _ => node_vals(ix, n),
        };
        let has_counts = kind == Kind::Files || ix.is_dir(n);
        match col {
            Col::Name => match (self.mode, kind) {
                (Mode::Tree, Kind::Files) => {
                    let glyph = if self.tree.rows[row].expanded { "▾ " } else { "▸ " };
                    format!("{glyph}[{}]", lang::files(v.files))
                }
                (Mode::Tree, Kind::Node) => {
                    let r = &self.tree.rows[row];
                    let glyph = if Tree::expandable(ix, r) { if r.expanded { "▾ " } else { "▸ " } } else { "" };
                    let name = if row == 0 { ix.path(n) } else { ix.name(n).to_string() };
                    format!("{glyph}{name}")
                }
                _ => ix.name(n).to_string(),
            },
            Col::Percent => self.percent(row).map(lang::percent).unwrap_or_default(),
            Col::Size => self.size(v.size),
            Col::Alloc => self.size(v.alloc),
            Col::Items if has_counts => lang::count(v.items),
            Col::Files if has_counts => lang::count(v.files),
            Col::Folders if has_counts => lang::count(v.folders),
            Col::Modified => filetime(v.mtime),
            Col::Folder => {
                let p = ix.parent[i];
                if p == NONE { String::new() } else { ix.path(p) }
            }
            Col::Group => self.dupe_group.get(row).map(|g| (g + 1).to_string()).unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// Share of the parent directory (tree mode), by the treemap metric.
    pub fn percent(&self, row: usize) -> Option<f64> {
        let ix = self.ix()?;
        let r = self.tree_row(row)?;
        let alloc = self.settings.map_alloc;
        let v = self.tree.vals(ix, &r);
        let mine = if alloc { v.alloc } else { v.size };
        if row == 0 {
            return Some(1.0);
        }
        if r.kind == Kind::Node && ix.is_deleted(r.node) {
            return None;
        }
        let p = Tree::parent_dir(ix, &r);
        if p == NONE {
            return None;
        }
        let total = if alloc { ix.alloc[p as usize] } else { ix.size[p as usize] };
        Some(if total == 0 { 0.0 } else { mine as f64 / total as f64 })
    }

    /// Removed from the volume since the scan (drawn struck through).
    pub fn deleted(&self, row: usize) -> bool {
        let (Some(ix), Some((n, kind, _))) = (self.ix(), self.row(row)) else { return false };
        kind == Kind::Node && ix.is_deleted(n)
    }

    pub fn dimmed(&self, row: usize) -> bool {
        let (Some(ix), Some((n, kind, _))) = (self.ix(), self.row(row)) else { return false };
        kind == Kind::Node && n != self.view_root && ix.flags[n as usize] & (nflags::HIDDEN | nflags::SYSTEM | nflags::DUP_LINK) != 0
    }

    pub fn icon(&mut self, row: usize) -> i32 {
        let Some((n, kind, _)) = self.row(row) else { return -1 };
        let Some(ix) = self.ix() else { return -1 };
        if kind == Kind::Files {
            return self.shell_icon("*files", FILE_ATTRIBUTE_NORMAL, "file");
        }
        if ix.is_dir(n) {
            return self.shell_icon("/", FILE_ATTRIBUTE_DIRECTORY, "folder");
        }
        let name = ix.name(n);
        let ext = name.rfind('.').filter(|&p| p > 0).map(|p| name[p + 1..].to_ascii_lowercase()).unwrap_or_default();
        self.ext_icon(&ext)
    }

    /// System image list index of the icon for files with extension `ext` (lower case,
    /// no dot; empty for none), as Explorer shows it. Cached per extension.
    pub fn ext_icon(&mut self, ext: &str) -> i32 {
        let key = format!(".{ext}");
        if matches!(ext, "exe" | "ico" | "lnk" | "url" | "scr") {
            // Per-file icons would need the file itself; use the class icon.
            self.shell_icon(&key, FILE_ATTRIBUTE_NORMAL, &format!("x{key}"))
        } else if ext.is_empty() {
            self.shell_icon(&key, FILE_ATTRIBUTE_NORMAL, "file")
        } else {
            self.shell_icon(&key, FILE_ATTRIBUTE_NORMAL, &format!("file{key}"))
        }
    }

    fn shell_icon(&mut self, key: &str, attrs: u32, probe: &str) -> i32 {
        if let Some(&i) = self.icons.get(key.as_bytes()) {
            return i;
        }
        let w = wide(probe);
        // SAFETY: plain out-parameter; SHGFI_USEFILEATTRIBUTES never touches the disk.
        let i = unsafe {
            let mut info: SHFILEINFOW = std::mem::zeroed();
            SHGetFileInfoW(
                w.as_ptr(),
                attrs,
                &mut info,
                size_of::<SHFILEINFOW>() as u32,
                SHGFI_SYSICONINDEX | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES,
            );
            info.iIcon
        };
        self.icons.insert(key.as_bytes().to_vec(), i);
        i
    }

    /// Icon of a file-type row (none for files without an extension).
    pub fn type_icon(&mut self, id: u16) -> i32 {
        let Some(s) = self.snap.clone() else { return -1 };
        let ext = &s.types.list[id as usize].ext;
        if ext.is_empty() { -1 } else { self.ext_icon(ext) }
    }

    pub fn describe(&self, n: u32) -> String {
        let Some(ix) = self.ix() else { return String::new() };
        let i = n as usize;
        let mut s =
            format!("{}    {}: {}    {}: {}", ix.path(n), t(S::ColSize), self.size(ix.size[i]), t(S::ColAlloc), self.size(ix.alloc[i]));
        if ix.is_dir(n) {
            s += &format!("    {}, {}", lang::files(ix.files[i] as u64), lang::folders(ix.dirs[i] as u64));
        }
        s
    }

    // ---------------------------------------------------------------- modes

    /// Rebuild the rows of the current mode from the current snapshot.
    pub fn refresh_rows(&mut self) {
        let Some(snap) = self.snap.clone() else { return };
        let ix = &snap.ix;
        match self.mode {
            Mode::Tree => {}
            Mode::Files => {
                let mut v = Vec::with_capacity(ix.files[self.view_root as usize] as usize);
                let mut stack = vec![self.view_root];
                while let Some(n) = stack.pop() {
                    for c in ix.children(n) {
                        if ix.is_dir(c) {
                            stack.push(c);
                        } else if ix.flags[c as usize] & nflags::DUP_LINK == 0 {
                            v.push(c);
                        }
                    }
                }
                sort_nodes(ix, &mut v, self.flat_sort);
                self.flat = v;
            }
            Mode::Search => {}
            Mode::Dupes => {
                let (mut rows, mut groups) = (Vec::new(), Vec::new());
                for (g, p) in self.dupes.iter().flatten() {
                    if let Some(n) = ix.find(p) {
                        rows.push(n);
                        groups.push(*g);
                    }
                }
                self.flat = rows;
                self.dupe_group = groups;
            }
        }
    }

    /// Run a name search; returns the status message.
    pub fn search(&mut self, text: &str) -> Option<String> {
        let snap = self.snap.clone()?;
        let text = text.trim();
        if text.is_empty() {
            if self.mode == Mode::Search {
                self.mode = Mode::Tree;
            }
            self.list_msg = None;
            return None;
        }
        // The search index is built on first use and kept until the tree changes.
        let key = (Arc::as_ptr(&snap) as usize, snap.ix.revision);
        if self.search_index.as_ref().is_none_or(|(k, _)| *k != key) {
            self.search_index = Some((key, SearchIndex::build(&snap.ix)));
        }
        let si = &self.search_index.as_ref()?.1;
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
        let mut hits = si.search(&Query::parse(text), threads);
        let root = self.view_root;
        if root != 0 {
            hits.retain(|&n| is_under(&snap.ix, n, root));
        }
        hits.retain(|&n| !snap.ix.is_deleted(n));
        sort_nodes(&snap.ix, &mut hits, self.flat_sort);
        let msg = format!("{}: {}", t(S::Found), lang::count(hits.len() as u64));
        self.flat = hits;
        self.mode = Mode::Search;
        self.dupes = None;
        self.list_msg = Some(msg.clone());
        Some(msg)
    }

    /// Sort the current list by `col` (toggling direction when it is the same key).
    pub fn sort_by(&mut self, col: Col) {
        let Some(key) = col.key(self.settings.map_alloc) else { return };
        let Some(snap) = self.snap.clone() else { return };
        let cur = if self.mode == Mode::Tree { self.sort } else { self.flat_sort };
        let desc = if cur.key == key { !cur.desc } else { !matches!(key, Key::Name | Key::Folder) };
        let s = Sort { key, desc };
        if self.mode == Mode::Tree {
            self.sort = s;
            let state = self.tree.expanded_state(&snap.ix);
            self.tree = Tree::restore(&snap.ix, s, self.view_root, &state);
        } else {
            self.flat_sort = s;
            if self.mode == Mode::Dupes {
                return;
            }
            sort_nodes(&snap.ix, &mut self.flat, s);
        }
    }

    pub fn current_sort(&self) -> Sort {
        if self.mode == Mode::Tree { self.sort } else { self.flat_sort }
    }

    // ---------------------------------------------------------------- snapshot

    /// Install a new snapshot, keeping expansions, selection and zoom when it is a live
    /// update of the same target.
    pub fn install(&mut self, snap: Arc<Snapshot>) -> Option<usize> {
        let keep = snap.live && self.snap.as_ref().is_some_and(|s| s.target == snap.target);
        let old = self.snap.clone();
        let state = old.as_ref().filter(|_| keep).map(|s| self.tree.expanded_state(&s.ix)).unwrap_or_default();
        let sel = old.as_ref().filter(|_| keep).and_then(|s| self.selected.map(|(n, k)| (s.ix.path(n), k)));
        let zoom = old.as_ref().filter(|_| keep && self.tm_root != self.view_root).map(|s| s.ix.path(self.tm_root));
        let type_sel = old.as_ref().filter(|_| keep).and_then(|s| self.type_sel.map(|i| s.types.list[i as usize].ext.clone()));
        let ix = &snap.ix;
        self.view_root = snap.view_root.as_deref().and_then(|p| ix.find(p)).filter(|&n| ix.is_dir(n)).unwrap_or(0);
        self.tree = if keep { Tree::restore(ix, self.sort, self.view_root, &state) } else { Tree::new(ix, self.sort, self.view_root) };
        self.selected = sel.and_then(|(p, k)| ix.find(&p).map(|n| (n, k)));
        self.tm_root = zoom.and_then(|p| ix.find(&p)).filter(|&n| ix.is_dir(n)).unwrap_or(self.view_root);
        self.type_sel = type_sel.and_then(|e| snap.types.list.iter().position(|s| s.ext == e).map(|i| i as u16));
        self.disk = snap.drive.and_then(disk_space);
        self.shown_rows.clear();
        self.shown_types.clear();
        self.snap = Some(snap.clone());
        if !keep {
            self.mode = Mode::Tree;
            self.dupes = None;
            self.list_msg = None;
        }
        self.refresh_rows();
        self.build_type_rows();
        self.status = self.summary();
        self.busy = false;
        self.selected.and_then(|(n, _)| self.tree.row_of(n)).filter(|_| self.mode == Mode::Tree)
    }

    /// Apply changed records from the live engine to the tree in place. `None` if nothing
    /// was applied (yet); see [`Changed`].
    pub fn apply_patch(&mut self, p: Option<Patch>) -> Option<Changed> {
        if let Some(p) = p {
            self.pending.extend(p.changes);
        }
        if self.pending.is_empty() {
            return None;
        }
        let (alloc, root) = (self.settings.map_alloc, self.tm_root);
        // Workers hold the snapshot only for a moment; retried when they finish.
        let s = Arc::get_mut(self.snap.as_mut()?)?;
        let changes = std::mem::take(&mut self.pending);
        let before = map_weight(&s.ix, alloc, root);
        let (structural, renamed) = match s.ix.patch(&changes, &mut s.types) {
            Ok(st) => {
                if st.unresolved != 0 {
                    engine::request_rebuild();
                }
                (st.created + st.moved != 0, st.renamed + st.moved != 0)
            }
            // Hard links: the engine sends a fresh tree.
            Err(_) => {
                engine::request_rebuild();
                (false, false)
            }
        };
        Some(self.after_change(structural, before, renamed))
    }

    /// Mark `node` as deleted right away (the shell deleted it); the journal confirms later.
    pub fn mark_deleted(&mut self, node: u32) -> Option<Changed> {
        let (alloc, root) = (self.settings.map_alloc, self.tm_root);
        let s = Arc::get_mut(self.snap.as_mut()?)?;
        let before = map_weight(&s.ix, alloc, root);
        s.ix.delete_node(node, &mut s.types);
        Some(self.after_change(false, before, false))
    }

    /// Views over a tree changed in place. Rows hold node numbers and read their values
    /// live, so most changes need only a repaint; the rows are rebuilt when nodes were
    /// added or moved (bounded, so a fully expanded tree does not pay for it on every
    /// change). The treemap is laid out again only for a visible change.
    fn after_change(&mut self, structural: bool, before: u64, renamed: bool) -> Changed {
        let Some(snap) = self.snap.clone() else {
            return Changed { row: None, relayout: false, structural: false, types_reordered: false };
        };
        let ix = &snap.ix;
        let structural = structural && self.mode == Mode::Tree && self.tree.rows.len() <= 20_000;
        if structural {
            let expanded = self.tree.expanded_nodes();
            self.tree = Tree::restore_nodes(ix, self.sort, self.view_root, &expanded);
        }
        let mut relayout = renamed;
        if ix.is_deleted(self.tm_root) {
            self.tm_root = self.view_root;
            relayout = true;
        }
        let after = map_weight(ix, self.settings.map_alloc, self.tm_root);
        relayout |= before.abs_diff(after) as u128 * 2000 > before.max(1) as u128;
        let old_types = std::mem::take(&mut self.type_rows);
        self.build_type_rows();
        let types_reordered = old_types != self.type_rows;
        self.status = self.summary();
        let row = self.selected.and_then(|(n, _)| self.tree.row_of(n)).filter(|_| structural);
        Changed { row, relayout, structural, types_reordered }
    }

    /// Fingerprint of what row `row` of the main list shows.
    pub fn row_sig(&self, row: usize) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for &c in self.cols {
            self.cell(row, c).hash(&mut h);
        }
        self.deleted(row).hash(&mut h);
        self.percent(row).map(|p| (p * 1000.0) as i64).hash(&mut h);
        h.finish()
    }

    /// Fingerprint of what row `row` of the file-type list shows.
    pub fn type_sig(&mut self, row: usize) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for c in 0..TYPE_COLS.len() {
            self.type_cell(row, c).hash(&mut h);
        }
        ((self.type_fraction(row) * 1000.0) as i64).hash(&mut h);
        h.finish()
    }

    pub fn summary(&self) -> String {
        let Some(s) = &self.snap else { return String::new() };
        let how = match s.how {
            How::Volume => String::new(),
            How::Walk { errors } => {
                if errors > 0 {
                    format!(", {}: {}", t(S::Inaccessible), lang::count(errors))
                } else {
                    String::new()
                }
            }
        };
        format!("{} {}{how}", t(S::Done), lang::seconds(s.elapsed))
    }

    // ---------------------------------------------------------------- file types

    /// Totals of file type `id` (size, on disk, files) within the shown folder.
    pub fn type_stat(&self, id: u16) -> (u64, u64, u64) {
        if let Some(v) = &self.scoped_types {
            return v.get(id as usize).copied().unwrap_or_default();
        }
        self.snap.as_ref().and_then(|s| s.types.list.get(id as usize)).map_or((0, 0, 0), |e| (e.size, e.alloc, e.files))
    }

    /// What the type percentages refer to: the shown folder.
    fn type_total(&self) -> u64 {
        let Some(ix) = self.ix() else { return 0 };
        let r = self.view_root as usize;
        if self.settings.map_alloc { ix.alloc[r] } else { ix.size[r] }
    }

    /// The file types of the shown folder: the volume's totals, or (for a folder) totals
    /// of the nodes below it.
    fn scope_types(&mut self) {
        self.scoped_types = None;
        let Some(s) = &self.snap else { return };
        if self.view_root == 0 {
            return;
        }
        let (ix, types) = (&s.ix, &s.types);
        let mut acc = vec![(0u64, 0u64, 0u64); types.list.len()];
        let mut stack = vec![self.view_root];
        while let Some(n) = stack.pop() {
            for c in ix.children(n) {
                let f = ix.flags[c as usize];
                if f & nflags::DIR != 0 {
                    stack.push(c);
                } else if f & nflags::DUP_LINK == 0
                    && let Some(a) = types.of_node.get(c as usize).and_then(|&id| acc.get_mut(id as usize))
                {
                    a.0 += ix.size[c as usize];
                    a.1 += ix.alloc[c as usize];
                    a.2 += 1;
                }
            }
        }
        self.scoped_types = Some(acc);
    }

    pub fn build_type_rows(&mut self) {
        self.scope_types();
        let Some(s) = self.snap.clone() else { return };
        let list = &s.types.list;
        let mut rows: Vec<u16> = (0..list.len() as u16).filter(|&i| self.type_stat(i).2 > 0).collect();
        let (col, desc) = self.type_sort;
        rows.sort_by(|&a, &b| {
            let (x, y) = (&list[a as usize], &list[b as usize]);
            let (sx, sy) = (self.type_stat(a), self.type_stat(b));
            let o = match col {
                0 => x.ext.cmp(&y.ext),
                1 => self.type_names.get(&x.ext).cmp(&self.type_names.get(&y.ext)),
                4 => sx.1.cmp(&sy.1),
                5 => sx.2.cmp(&sy.2),
                _ => sx.0.cmp(&sy.0),
            };
            if desc { o.reverse() } else { o }
        });
        self.type_rows = rows;
    }

    pub fn type_name(&mut self, ext: &str) -> String {
        if ext.is_empty() {
            return t(S::NoExt).to_string();
        }
        if let Some(n) = self.type_names.get(ext) {
            return n.clone();
        }
        let w = wide(&format!("x.{ext}"));
        // SAFETY: plain out-parameter; never touches the disk.
        let name = unsafe {
            let mut info: SHFILEINFOW = std::mem::zeroed();
            SHGetFileInfoW(
                w.as_ptr(),
                FILE_ATTRIBUTE_NORMAL,
                &mut info,
                size_of::<SHFILEINFOW>() as u32,
                SHGFI_TYPENAME | SHGFI_USEFILEATTRIBUTES,
            );
            let n = info.szTypeName.iter().position(|&c| c == 0).unwrap_or(0);
            String::from_utf16_lossy(&info.szTypeName[..n])
        };
        self.type_names.insert(ext.to_string(), name.clone());
        name
    }

    pub fn type_cell(&mut self, row: usize, col: usize) -> String {
        let Some(&id) = self.type_rows.get(row) else { return String::new() };
        let Some(s) = self.snap.clone() else { return String::new() };
        let e = &s.types.list[id as usize];
        let (size, alloc, files) = self.type_stat(id);
        let total = self.type_total();
        match col {
            0 => {
                if e.ext.is_empty() {
                    String::new()
                } else {
                    format!(".{}", e.ext)
                }
            }
            1 => self.type_name(&e.ext.clone()),
            2 => {
                let m = if self.settings.map_alloc { alloc } else { size };
                lang::percent(if total == 0 { 0.0 } else { m as f64 / total as f64 })
            }
            3 => self.size(size),
            4 => self.size(alloc),
            5 => lang::count(files),
            _ => String::new(),
        }
    }

    pub fn type_fraction(&self, row: usize) -> f64 {
        let Some(&id) = self.type_rows.get(row) else { return 0.0 };
        let (size, alloc, _) = self.type_stat(id);
        let m = if self.settings.map_alloc { alloc } else { size };
        let total = self.type_total();
        if total == 0 { 0.0 } else { m as f64 / total as f64 }
    }

    pub fn type_color(&self, id: u16) -> [u8; 3] {
        let no_ext = self.snap.as_ref().is_some_and(|s| s.types.list[id as usize].ext.is_empty());
        type_color(id, no_ext)
    }

    // ---------------------------------------------------------------- treemap

    pub fn rebuild_treemap(&mut self, w: i32, h: i32) {
        let Some(s) = self.snap.clone() else {
            self.tm = Treemap::default();
            return;
        };
        let (w, h) = (w.max(0) as u32, h.max(0) as u32);
        let ix = &s.ix;
        if self.tm_root as usize >= ix.len() || !ix.is_dir(self.tm_root) || ix.is_deleted(self.tm_root) {
            self.tm_root = self.view_root;
        }
        let weight = if self.settings.map_alloc { &ix.alloc } else { &ix.size };
        let header = if self.settings.map_labels { self.scale(15) as f32 } else { 0.0 };
        self.tm = Treemap::build(ix, weight, self.tm_root, w, h, Options { header, ..Default::default() });
        let n = (w * h) as usize;
        self.tm_px.resize(n, 0);
        self.tm_owner.resize(n, NONE);
        let types = &s.types;
        let color = |node: u32| -> [u8; 3] {
            match types.of_node[node as usize] {
                NOT_FILE => [0x70, 0x70, 0x70],
                id => type_color(id, types.list[id as usize].ext.is_empty()),
            }
        };
        let sel = self.type_sel;
        let hl = move |node: u32| Some(types.of_node[node as usize]) == sel;
        let p = &self.pal;
        let style = Style { background: p.map_bg, border: p.map_border, header: p.map_header };
        let highlight: Option<&(dyn Fn(u32) -> bool + Sync)> = if sel.is_some() { Some(&hl) } else { None };
        self.tm.render(&color, highlight, &style, &mut self.tm_px, &mut self.tm_owner);
    }

    /// Node under a treemap client point (window may be stretched while resizing).
    pub fn map_hit(&self, x: i32, y: i32, cw: i32, ch: i32) -> Option<u32> {
        let (tw, th) = (self.tm.width as i32, self.tm.height as i32);
        if tw == 0 || th == 0 || cw <= 0 || ch <= 0 {
            return None;
        }
        let tx = (x * tw / cw).clamp(0, tw - 1);
        let ty = (y * th / ch).clamp(0, th - 1);
        self.tm_owner.get((ty * tw + tx) as usize).copied().filter(|&n| n != NONE)
    }
}

pub fn is_under(ix: &Index, mut n: u32, root: u32) -> bool {
    loop {
        if n == root {
            return true;
        }
        n = ix.parent[n as usize];
        if n == NONE {
            return false;
        }
    }
}

/// (total, free) bytes of a drive.
pub fn disk_space(letter: char) -> Option<(u64, u64)> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let root = wide(&format!("{letter}:\\"));
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: valid path and out-parameters.
    let ok = unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut avail, &mut total, &mut free) };
    (ok != 0).then_some((total, free))
}
