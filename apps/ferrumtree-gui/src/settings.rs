//! Settings: `FerrumTree.ini` next to the program (portable: nothing elsewhere), one
//! `key=value` per line. Unknown or malformed lines are ignored. A program folder that
//! cannot be written to just means the settings are not kept.

use std::path::PathBuf;

use crate::lang::{Lang, Units};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemePref {
    System,
    Light,
    Dark,
}

/// When the tree follows changes on the volume (NTFS journal).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Live {
    /// A snapshot: F5 refreshes (instant with the cache).
    Off,
    /// While the window is active; changes made meanwhile are applied on return.
    Active,
    Always,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub units: Units,
    pub theme: ThemePref,
    pub show_types: bool,
    pub show_map: bool,
    pub map_labels: bool,
    pub map_alloc: bool,
    pub auto_scan: bool,
    /// Share of the body (list + types) above the treemap.
    pub split_map: f32,
    /// Share of the body width taken by the list (rest: file types).
    pub split_types: f32,
    /// Normal window rectangle and maximized state.
    pub window: Option<(i32, i32, i32, i32, bool)>,
    /// Last scanned drive letter or folder.
    pub last_target: Option<String>,
    /// Keep a catalog file per volume next to the program: repeat scans take a fraction
    /// of a second instead of a full MFT read.
    pub cache: bool,
    pub live: Live,
    /// `None`: as in Windows.
    pub lang: Option<Lang>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            units: Units::Auto,
            theme: ThemePref::System,
            show_types: true,
            show_map: true,
            map_labels: true,
            map_alloc: false,
            auto_scan: true,
            split_map: 0.52,
            split_types: 0.66,
            window: None,
            last_target: None,
            cache: true,
            live: Live::Active,
            lang: None,
        }
    }
}

fn path() -> Option<PathBuf> {
    Some(ft_usn::program_dir()?.join("FerrumTree.ini"))
}

fn units_name(u: Units) -> &'static str {
    match u {
        Units::Auto => "auto",
        Units::B => "b",
        Units::KB => "kb",
        Units::MB => "mb",
        Units::GB => "gb",
        Units::TB => "tb",
    }
}

impl Settings {
    pub fn load() -> Settings {
        let mut s = Settings::default();
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else { return s };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            let flag = v == "1";
            let frac = v.parse::<f32>().ok().filter(|f| (0.1..=0.9).contains(f));
            match k {
                "units" => {
                    s.units = match v {
                        "b" => Units::B,
                        "kb" => Units::KB,
                        "mb" => Units::MB,
                        "gb" => Units::GB,
                        "tb" => Units::TB,
                        _ => Units::Auto,
                    }
                }
                "theme" => {
                    s.theme = match v {
                        "light" => ThemePref::Light,
                        "dark" => ThemePref::Dark,
                        _ => ThemePref::System,
                    }
                }
                "show_types" => s.show_types = flag,
                "show_map" => s.show_map = flag,
                "map_labels" => s.map_labels = flag,
                "map_alloc" => s.map_alloc = flag,
                "auto_scan" => s.auto_scan = flag,
                "split_map" => s.split_map = frac.unwrap_or(s.split_map),
                "split_types" => s.split_types = frac.unwrap_or(s.split_types),
                "window" => {
                    let p: Vec<i32> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
                    if p.len() == 5 && p[2] > 200 && p[3] > 150 {
                        s.window = Some((p[0], p[1], p[2], p[3], p[4] != 0));
                    }
                }
                "last_target" if !v.is_empty() => s.last_target = Some(v.to_string()),
                "cache" => s.cache = flag,
                "language" => {
                    s.lang = match v {
                        "en" => Some(Lang::En),
                        "ru" => Some(Lang::Ru),
                        _ => None,
                    }
                }
                "live" => {
                    s.live = match v {
                        "off" => Live::Off,
                        "always" => Live::Always,
                        _ => Live::Active,
                    }
                }
                _ => {}
            }
        }
        s
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        let b = |v: bool| if v { 1 } else { 0 };
        let mut out = format!(
            "language={}\nunits={}\ntheme={}\nshow_types={}\nshow_map={}\nmap_labels={}\nmap_alloc={}\nauto_scan={}\nsplit_map={:.3}\nsplit_types={:.3}\ncache={}\nlive={}\n",
            match self.lang {
                None => "system",
                Some(Lang::En) => "en",
                Some(Lang::Ru) => "ru",
            },
            units_name(self.units),
            match self.theme {
                ThemePref::System => "system",
                ThemePref::Light => "light",
                ThemePref::Dark => "dark",
            },
            b(self.show_types),
            b(self.show_map),
            b(self.map_labels),
            b(self.map_alloc),
            b(self.auto_scan),
            self.split_map,
            self.split_types,
            b(self.cache),
            match self.live {
                Live::Off => "off",
                Live::Active => "active",
                Live::Always => "always",
            },
        );
        if let Some((x, y, w, h, m)) = self.window {
            out += &format!("window={x},{y},{w},{h},{}\n", b(m));
        }
        if let Some(t) = &self.last_target {
            out += &format!("last_target={t}\n");
        }
        // Unchanged settings are not rewritten.
        if std::fs::read_to_string(&p).is_ok_and(|old| old == out) {
            return;
        }
        let _ = std::fs::write(&p, out);
    }
}
