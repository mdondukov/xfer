//! Whitelist of useful files and folders not worth descending into.

use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Category {
    Documents,
    Books,
    Photos,
    Video,
    Audio,
    Mail,
    Design,
    Archives,
    Custom,
}

impl Category {
    pub fn title(self) -> &'static str {
        match self {
            Category::Documents => "Documents",
            Category::Books => "Books",
            Category::Photos => "Photos",
            Category::Video => "Video",
            Category::Audio => "Audio",
            Category::Mail => "Mail & contacts",
            Category::Design => "Design & CAD",
            Category::Archives => "Archives",
            Category::Custom => "Custom",
        }
    }
}

const DOCUMENTS: &[&str] = &[
    // text and Word
    "pdf", "doc", "docx", "docm", "dot", "dotx", "dotm", "rtf", "odt", "ott", "txt", "md",
    "wps", "wpd", "pages", "tex",
    // spreadsheets
    "xls", "xlsx", "xlsm", "xlsb", "xlt", "xltx", "xltm", "ods", "ots", "csv", "numbers",
    // presentations
    "ppt", "pptx", "pptm", "pps", "ppsx", "ppsm", "pot", "potx", "potm", "odp", "otp", "key",
    // other office formats
    "odg", "vsd", "vsdx", "pub", "xps", "oxps", "one", "enex",
    // Word backup copies
    "wbk",
    // personal databases, BI reports and project plans
    "accdb", "mdb", "pbix", "twb", "twbx", "mpp",
    // mind maps
    "mm", "xmind",
    // saved web pages
    "htm", "html", "mht", "mhtml",
    // detached digital signatures
    "sig", "p7s", "p7m",
];

const BOOKS: &[&str] = &[
    "djvu", "djv", "epub", "fb2", "mobi", "azw", "azw3", "chm", "cbr", "cbz",
];

const PHOTOS: &[&str] = &[
    "jpg", "jpeg", "jpe", "jfif", "png", "gif", "bmp", "tif", "tiff", "webp", "heic", "heif",
    "avif", "jxl", "svg",
    // camera RAW: Canon, Nikon, Sony, Olympus, Panasonic, Fuji, Pentax, Samsung, Sigma, etc.
    "dng", "cr2", "cr3", "crw", "nef", "nrw", "arw", "srf", "sr2", "orf", "rw2", "raf", "pef",
    "srw", "x3f", "3fr", "erf", "kdc", "mrw", "rwl", "iiq",
    // Lightroom and other editors' sidecar metadata
    "xmp",
];

const VIDEO: &[&str] = &[
    "mp4", "m4v", "mov", "avi", "mkv", "wmv", "flv", "f4v", "webm", "mpg", "mpeg", "mpe", "m2v",
    "ts", "mts", "m2ts", "vob", "3gp", "3g2", "ogv", "asf", "rm", "rmvb", "divx", "dv", "mxf",
    "insv",
    // MPEG-2 from JVC/Panasonic camcorders, MPEG-2 program streams
    "mod", "tod", "m2p",
    // Zoom recordings that were never converted to mp4
    "zoom",
    // subtitles
    "srt", "ass", "ssa", "sub", "vtt",
];

const AUDIO: &[&str] = &[
    "mp3", "m4a", "m4b", "aac", "wav", "flac", "ogg", "oga", "opus", "wma", "aif", "aiff", "amr",
    "ape", "wv", "mid", "midi", "3ga",
];

const MAIL: &[&str] = &["pst", "eml", "msg", "mbox", "vcf", "ics", "contact"];

const DESIGN: &[&str] = &[
    "psd", "psb", "ai", "eps", "indd", "idml", "cdr", "xcf", "kra", "afphoto", "afdesign",
    "afpub", "sketch", "fig", "dwg", "dxf", "dwf", "skp",
];

const ARCHIVES: &[&str] = &["zip", "rar", "7z", "tar", "gz", "tgz", "bz2", "xz"];

const CATEGORIES: &[(Category, &[&str])] = &[
    (Category::Documents, DOCUMENTS),
    (Category::Books, BOOKS),
    (Category::Photos, PHOTOS),
    (Category::Video, VIDEO),
    (Category::Audio, AUDIO),
    (Category::Mail, MAIL),
    (Category::Design, DESIGN),
    (Category::Archives, ARCHIVES),
];

/// Never descend into: recycle bin, restore points, our own quarantine.
const ALWAYS_SKIP_DIRS: &[&str] = &[
    "$recycle.bin",
    "recycler",
    "system volume information",
    crate::paths::TRASH_DIR,
];

/// With --only-useful, skipped anywhere: application and development data.
const JUNK_DIRS_ANYWHERE: &[&str] = &[
    "appdata",
    "node_modules",
    ".git",
    ".svn",
    "__pycache__",
    ".venv",
    "$windows.~bt",
    "$windows.~ws",
];

/// With --only-useful, skipped only at a volume root or inside Windows.old.
/// Only there: a "Windows" folder inside documents may well be someone's course notes.
const JUNK_DIRS_AT_ROOT: &[&str] = &[
    "windows",
    "program files",
    "program files (x86)",
    "programdata",
    "$sysreset",
    "$winreagent",
    "$getcurrent",
    "recovery",
    "perflogs",
    "msocache",
    "config.msi",
    "onedrivetemp",
    "boot",
    "drivers",
    "swsetup",
    "intel",
    "amd",
    "nvidia",
];

/// System files at a volume root that can't be read.
const ALWAYS_SKIP_FILES: &[&str] = &[
    "pagefile.sys",
    "hiberfil.sys",
    "swapfile.sys",
    "dumpstack.log.tmp",
];

/// Leftover of an interrupted copy.
pub const PARTIAL_SUFFIX: &str = ".xfer-partial";

pub enum Decision {
    Keep(Option<Category>),
    Drop { ext: String },
}

pub struct Filter {
    pub only_useful: bool,
    by_ext: HashMap<String, Category>,
    excluded: HashSet<String>,
}

impl Filter {
    pub fn new(args: &crate::FilterArgs) -> Self {
        let mut by_ext = HashMap::new();
        for (cat, exts) in CATEGORIES {
            for e in *exts {
                by_ext.insert((*e).to_string(), *cat);
            }
        }
        for e in &args.include_ext {
            by_ext.entry(norm_ext(e)).or_insert(Category::Custom);
        }
        Filter {
            only_useful: args.only_useful,
            by_ext,
            excluded: args.exclude_ext.iter().map(|e| norm_ext(e)).collect(),
        }
    }

    pub fn decide(&self, path: &Path) -> Decision {
        let ext = ext_of(path);
        if self.excluded.contains(&ext) {
            return Decision::Drop { ext };
        }
        let cat = self.by_ext.get(&ext).copied();
        if self.only_useful && cat.is_none() {
            return Decision::Drop { ext };
        }
        Decision::Keep(cat)
    }

    pub fn skip_dir(&self, path: &Path) -> bool {
        let Some(name) = path.file_name() else {
            return false;
        };
        let name = name.to_string_lossy().to_lowercase();
        if ALWAYS_SKIP_DIRS.contains(&name.as_str()) {
            return true;
        }
        if !self.only_useful {
            return false;
        }
        if JUNK_DIRS_ANYWHERE.contains(&name.as_str()) {
            return true;
        }
        JUNK_DIRS_AT_ROOT.contains(&name.as_str()) && at_volume_root(path)
    }
}

pub fn skip_file(name: &str) -> bool {
    let lower = name.to_lowercase();
    ALWAYS_SKIP_FILES.contains(&lower.as_str()) || lower.ends_with(PARTIAL_SUFFIX)
}

fn at_volume_root(path: &Path) -> bool {
    match path.parent() {
        None => false,
        Some(parent) => {
            parent.parent().is_none()
                || parent
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("windows.old"))
        }
    }
}

fn norm_ext(e: &str) -> String {
    e.trim().trim_start_matches('.').to_lowercase()
}

pub fn ext_of(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FilterArgs;

    fn filter(only_useful: bool) -> Filter {
        Filter::new(&FilterArgs {
            only_useful,
            include_ext: vec![".1CD".into()],
            exclude_ext: vec!["iso".into()],
        })
    }

    #[test]
    fn whitelist() {
        let f = filter(true);
        assert!(matches!(f.decide(Path::new("a/Фото.JPG")), Decision::Keep(Some(Category::Photos))));
        assert!(matches!(f.decide(Path::new("a/base.1cd")), Decision::Keep(Some(Category::Custom))));
        assert!(matches!(f.decide(Path::new("a/CAYEN.accdb")), Decision::Keep(Some(Category::Documents))));
        assert!(matches!(f.decide(Path::new("a/Anna.contact")), Decision::Keep(Some(Category::Mail))));
        assert!(matches!(f.decide(Path::new("a/MOV001.MOD")), Decision::Keep(Some(Category::Video))));
        assert!(matches!(f.decide(Path::new("a/double_click_to_convert_01.zoom")), Decision::Keep(Some(Category::Video))));
        assert!(matches!(f.decide(Path::new("a/setup.exe")), Decision::Drop { .. }));
        assert!(matches!(f.decide(Path::new("a/Makefile")), Decision::Drop { .. }));
    }

    #[test]
    fn as_is_mode_keeps_everything_but_excluded() {
        let f = filter(false);
        assert!(matches!(f.decide(Path::new("a/setup.exe")), Decision::Keep(None)));
        assert!(matches!(f.decide(Path::new("a/win.iso")), Decision::Drop { .. }));
    }

    #[test]
    fn system_dirs_only_at_root() {
        let f = filter(true);
        assert!(f.skip_dir(Path::new("/Windows")));
        assert!(!f.skip_dir(Path::new("/Users/max/Курсы/Windows")));
        assert!(f.skip_dir(Path::new("/Users/max/AppData")));
        assert!(f.skip_dir(Path::new("/x/$Recycle.Bin")));
        assert!(!filter(false).skip_dir(Path::new("/Users/max/AppData")));
    }
}
