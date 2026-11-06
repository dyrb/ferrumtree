//! UI strings (English and Russian) and number formatting in the chosen units.
//!
//! The language follows Windows unless chosen in the View menu; it can change while the
//! program runs, so every text is looked up when it is drawn.

use std::sync::atomic::{AtomicU8, Ordering};

use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    En,
    Ru,
}

/// 0: as in Windows, 1: English, 2: Russian.
static CHOICE: AtomicU8 = AtomicU8::new(0);

fn system_lang() -> Lang {
    // SAFETY: plain query. Primary language 0x19 is Russian.
    if unsafe { GetUserDefaultUILanguage() } & 0x3FF == 0x19 { Lang::Ru } else { Lang::En }
}

/// The name of a language in that language, as language menus show it.
pub fn native_name(l: Lang) -> &'static str {
    match l {
        Lang::En => "English",
        Lang::Ru => "Русский",
    }
}

/// `None`: as in Windows.
pub fn set_lang(l: Option<Lang>) {
    CHOICE.store(
        match l {
            None => 0,
            Some(Lang::En) => 1,
            Some(Lang::Ru) => 2,
        },
        Ordering::Relaxed,
    );
}

pub fn lang() -> Lang {
    match CHOICE.load(Ordering::Relaxed) {
        1 => Lang::En,
        2 => Lang::Ru,
        _ => system_lang(),
    }
}

macro_rules! strings {
    ($($id:ident = $en:literal, $ru:literal;)*) => {
        #[derive(Clone, Copy, Debug)]
        pub enum S { $($id),* }
        pub fn t(s: S) -> &'static str {
            match (lang(), s) {
                $((Lang::En, S::$id) => $en, (Lang::Ru, S::$id) => $ru,)*
            }
        }
    };
}

strings! {
    MenuFile = "File", "Файл";
    MenuView = "View", "Вид";
    MenuHelp = "Help", "Справка";
    Scan = "Scan", "Анализ";
    FullRescan = "Full scan without cache", "Полное сканирование без кеша";
    SelectFolder = "Select folder...", "Выбрать папку...";
    SelectFolderItem = "<Select folder...>", "<Выбрать папку...>";
    CollapseAll = "Collapse all", "Свернуть все ветви";
    ExpandAll = "Expand all", "Раскрыть все";
    SelectParent = "Select parent", "Выбрать родительский элемент";
    ZoomIn = "Zoom treemap in", "Увеличить графическую карту";
    ZoomOut = "Zoom treemap out", "Уменьшить графическую карту";
    SavePng = "Save treemap as PNG...", "Сохранить карту как PNG...";
    ShowInExplorer = "Show in Explorer", "Показать в Проводнике";
    OpenCmd = "Open command prompt here", "Открыть командную строку в этой папке";
    CopyPath = "Copy path", "Скопировать путь";
    Copy = "Copy", "Скопировать";
    Cut = "Cut", "Вырезать";
    Recycle = "Delete (to Recycle Bin)", "Удалить (в Корзину)";
    DeleteForever = "Delete permanently", "Удалить (окончательно)";
    ExportCsv = "Export to CSV...", "Экспортировать в CSV...";
    ExportTypes = "Export file types to CSV...", "Экспорт типов файлов в CSV...";
    FindDupes = "Find duplicates", "Найти дубликаты";
    Exit = "Exit", "Выход";
    ShowTypes = "Show file types", "Показывать типы файлов";
    ShowMap = "Show treemap", "Показывать графическую карту";
    MapLabels = "Show names on the treemap", "Показывать на карте имена папок и файлов";
    MapAlloc = "Treemap by size on disk", "Карта по размеру на диске";
    UnitsAuto = "Dynamic units", "Динамические значения";
    UnitsB = "Values in bytes", "Значения в байтах";
    UnitsKB = "Values in kilobytes", "Значения в килобайтах";
    UnitsMB = "Values in megabytes", "Значения в мегабайтах";
    UnitsGB = "Values in gigabytes", "Значения в гигабайтах";
    UnitsTB = "Values in terabytes", "Значения в терабайтах";
    ThemeMenu = "Theme", "Тема";
    ThemeSystem = "As in Windows", "Как в Windows";
    ThemeLight = "Light", "Светлая";
    ThemeDark = "Dark", "Тёмная";
    LangMenu = "Language", "Язык";
    LangSystem = "As in Windows", "Как в Windows";
    AutoScan = "Scan automatically on selection", "Сканировать автоматически при выборе";
    LiveMenu = "Track changes on disk", "Отслеживать изменения на диске";
    LiveOff = "Never", "Никогда";
    LiveActive = "While the window is active", "Когда окно активно";
    LiveAlways = "Always", "Всегда";
    CacheKeep = "Keep cache next to the program", "Хранить кеш рядом с программой";
    CacheClear = "Delete cache", "Удалить кеш";
    CacheCleared = "Cache deleted", "Кеш удалён";
    About = "About FerrumTree", "О программе";
    AboutText = "Disk space analyzer for Windows.", "Анализатор занятого места на дисках.";
    Version = "Version", "Версия";
    License = "License: MIT or Apache-2.0", "Лицензия: MIT или Apache-2.0";
    Choose = "Scan:", "Выберите:";
    Selected = "Selection:", "Выбор:";
    Total = "Total:", "Общий объём:";
    Used = "Used:", "Занято:";
    Free = "Free:", "Свободно:";
    SearchCue = "Search by name (Ctrl+F)", "Поиск по имени (Ctrl+F)";
    Clear = "Clear", "Очистить";
    TabTree = "Tree", "Дерево";
    TabFiles = "Files", "Файлы";
    TabSearch = "Search results", "Результаты поиска";
    TabDupes = "Duplicates", "Дубликаты";
    ColName = "Name", "Имя";
    ColFolder = "Folder", "Папка";
    ColPercent = "Percent of parent", "Процент от родителя";
    ColSize = "Size", "Размер";
    ColAlloc = "On disk", "На диске";
    ColItems = "Items", "Элементы";
    ColFiles = "Files", "Файлы";
    ColFolders = "Folders", "Папки";
    ColModified = "Modified", "Изменено";
    ColGroup = "Group", "Группа";
    ColExt = "Extension", "Расширение";
    ColType = "File type", "Тип файлов";
    NoExt = "(no extension)", "(без расширения)";
    Reading = "Reading", "Чтение";
    Done = "Scan completed in", "Сканирование выполнено за";
    Inaccessible = "folders without access", "папок без доступа";
    Found = "Found", "Найдено";
    DupeGroups = "Duplicate groups", "Групп дубликатов";
    Reclaimable = "can be freed", "можно освободить";
    LookingDupes = "Looking for duplicate files of 1 MB and larger in", "Поиск дубликатов файлов от 1 МБ в";
    Deleted = "Deleted", "Удалено";
    Exported = "Exported", "Экспортировано";
    Exporting = "Exporting", "Экспорт";
    Saved = "Saved", "Сохранено";
    Error = "Error", "Ошибка";
    NotAllowed = "This item cannot be deleted from here.", "Этот элемент нельзя удалить отсюда.";
    NeedAdmin = "Access denied. Run FerrumTree as administrator.", "Нет доступа. Запустите FerrumTree от имени администратора.";
}

/// "1 file", "2 файла", "5 файлов": `forms` are (English singular, English plural,
/// Russian for 1, Russian for 2..4, Russian for 5..20).
pub fn plural(n: u64, forms: [&str; 5]) -> String {
    let w = match lang() {
        Lang::En if n == 1 => forms[0],
        Lang::En => forms[1],
        Lang::Ru => match (n % 10, n % 100) {
            (_, 11..=14) => forms[4],
            (1, _) => forms[2],
            (2..=4, _) => forms[3],
            _ => forms[4],
        },
    };
    format!("{} {w}", count(n))
}

pub fn files(n: u64) -> String {
    plural(n, ["file", "files", "файл", "файла", "файлов"])
}

pub fn folders(n: u64) -> String {
    plural(n, ["folder", "folders", "папка", "папки", "папок"])
}

pub fn rows(n: u64) -> String {
    plural(n, ["row", "rows", "строка", "строки", "строк"])
}

/// Size units for the size columns.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Units {
    Auto,
    B,
    KB,
    MB,
    GB,
    TB,
}

fn unit_names() -> [&'static str; 5] {
    match lang() {
        Lang::En => ["KB", "MB", "GB", "TB", "PB"],
        Lang::Ru => ["КБ", "МБ", "ГБ", "ТБ", "ПБ"],
    }
}

fn decimal(v: f64, digits: usize) -> String {
    let s = format!("{v:.digits$}");
    if lang() == Lang::Ru { s.replace('.', ",") } else { s }
}

/// Thousands grouped with a no-break space (Russian) or a comma (English).
pub fn count(n: u64) -> String {
    let sep = if lang() == Lang::Ru { '\u{00A0}' } else { ',' };
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3 * 2);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(sep);
        }
        out.push(c);
    }
    out
}

pub fn size(n: u64, u: Units) -> String {
    let names = unit_names();
    let b = if lang() == Lang::Ru { "Б" } else { "B" };
    let fixed = |p: i32, name: &str| format!("{} {name}", decimal(n as f64 / 1024f64.powi(p), 1));
    match u {
        Units::B => format!("{} {b}", count(n)),
        Units::KB => fixed(1, names[0]),
        Units::MB => fixed(2, names[1]),
        Units::GB => fixed(3, names[2]),
        Units::TB => fixed(4, names[3]),
        Units::Auto => {
            if n < 1024 {
                return format!("{n} {b}");
            }
            let mut v = n as f64 / 1024.0;
            let mut i = 0;
            while v >= 1024.0 && i < names.len() - 1 {
                v /= 1024.0;
                i += 1;
            }
            format!("{} {}", decimal(v, 1), names[i])
        }
    }
}

pub fn percent(f: f64) -> String {
    format!("{} %", decimal(f * 100.0, 1))
}

pub fn seconds(d: std::time::Duration) -> String {
    let s = d.as_secs_f64();
    match lang() {
        Lang::En => format!("{} sec", decimal(s, 2)),
        Lang::Ru => format!("{} сек", decimal(s, 2)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn russian_plurals() {
        set_lang(Some(Lang::Ru));
        let f = |n| files(n);
        assert_eq!(
            [f(1), f(2), f(5), f(11), f(21), f(22), f(112)],
            ["1 файл", "2 файла", "5 файлов", "11 файлов", "21 файл", "22 файла", "112 файлов"]
        );
        assert_eq!(count(1_234_567), "1\u{00A0}234\u{00A0}567");
        set_lang(Some(Lang::En));
        assert_eq!(files(1), "1 file");
        assert_eq!(folders(1_000), "1,000 folders");
        set_lang(None);
    }
}
