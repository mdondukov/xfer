//! Paths: Windows long paths, display form, layout inside the target folder.

use anyhow::{Result, bail};
use std::path::{Component, Path, PathBuf, Prefix};

/// Service folder inside the target: manifests and summaries of every run.
pub const SERVICE_DIR: &str = "_xfer";
/// Quarantine for removed duplicates, created at the root of each scanned folder.
pub const TRASH_DIR: &str = "_dupes_trash";

/// Fixes common cmd input pitfalls: `"D:\Photos\"` arrives in the program
/// as `D:\Photos"`, and `D:` means the drive's current folder, not its root.
pub fn fix_arg(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    let s = s.trim_end_matches('"');
    if s.len() == 2 && s.ends_with(':') {
        return PathBuf::from(format!("{s}\\"));
    }
    PathBuf::from(s)
}

/// Canonicalizes sources. On Windows this yields `\\?\C:\...` paths, which are
/// not subject to the 260-character limit.
/// Nested sources are dropped, otherwise their files would be walked twice.
pub fn prepare_roots(args: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    for a in args {
        let p = fix_arg(a);
        let c = std::fs::canonicalize(&p).map_err(|e| open_error(&p, e))?;
        if !c.is_dir() {
            bail!("{} is not a folder or a drive", display(&c));
        }
        roots.push(c);
    }
    // After sorting, a parent always comes before its subfolders.
    roots.sort();
    roots.dedup();
    let mut out: Vec<PathBuf> = Vec::new();
    for r in roots {
        if let Some(parent) = out.iter().find(|o| r.starts_with(o)) {
            eprintln!(
                "{} is already inside {}, not scanning it separately",
                display(&r),
                display(parent)
            );
            continue;
        }
        out.push(r);
    }
    Ok(out)
}

pub fn canonical_list(args: &[PathBuf]) -> Result<Vec<PathBuf>> {
    args.iter()
        .map(|a| {
            let p = fix_arg(a);
            std::fs::canonicalize(&p).map_err(|e| open_error(&p, e))
        })
        .collect()
}

/// Explorer shows some folders under translated names that don't exist on disk
/// ("Пользователи" is really "Users"), a common reason for "not found".
const LOCALIZED_HINT: &str = "Explorer shows some folders under translated names: for example \
\"Пользователи\" is really \"Users\" and \"Документы\" is \"Documents\". Click Explorer's \
address bar or use Shift+right-click > \"Copy as path\" to get the real path.";

fn open_error(p: &Path, e: std::io::Error) -> anyhow::Error {
    let mut msg = format!("cannot open {}: {e}", p.display());
    if cfg!(windows) && e.kind() == std::io::ErrorKind::NotFound {
        msg.push('\n');
        msg.push_str(LOCALIZED_HINT);
    }
    anyhow::anyhow!(msg)
}

/// Path for humans and CSV: without the `\\?\` prefix.
pub fn display(p: &Path) -> String {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return rest.to_string();
    }
    s.into_owned()
}

/// Inverse of `display`: turns a path from CSV back into long form.
pub fn to_long(s: &str) -> PathBuf {
    if cfg!(windows) && !s.starts_with(r"\\?\") {
        if let Some(rest) = s.strip_prefix(r"\\") {
            return PathBuf::from(format!(r"\\?\UNC\{rest}"));
        }
        if Path::new(s).is_absolute() {
            return PathBuf::from(format!(r"\\?\{s}"));
        }
    }
    PathBuf::from(s)
}

/// Mirrors the full source path inside the target folder:
/// `C:\Users\a.txt` -> `<target>\C\Users\a.txt`.
pub fn mirror_path(target: &Path, src: &Path) -> PathBuf {
    let mut out = target.to_path_buf();
    for c in src.components() {
        match c {
            Component::Prefix(p) => match p.kind() {
                Prefix::Disk(d) | Prefix::VerbatimDisk(d) => {
                    out.push((d as char).to_ascii_uppercase().to_string())
                }
                Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                    out.push("UNC");
                    out.push(server);
                    out.push(share);
                }
                _ => out.push("OTHER"),
            },
            Component::Normal(n) => out.push(n),
            Component::RootDir | Component::CurDir | Component::ParentDir => {}
        }
    }
    out
}

/// Path inside the target without the drive prefix: `<target>\<path from source root>`.
pub fn relative_path(target: &Path, root: &Path, src: &Path) -> PathBuf {
    match src.strip_prefix(root) {
        Ok(rel) => target.join(rel),
        Err(_) => mirror_path(target, src),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixes_cmd_quirks() {
        assert_eq!(fix_arg(Path::new("D:")), PathBuf::from("D:\\"));
        assert_eq!(fix_arg(Path::new("D:\\Фото\"")), PathBuf::from("D:\\Фото"));
    }

    #[cfg(unix)]
    #[test]
    fn mirror_on_unix_keeps_full_path() {
        let t = Path::new("/mnt/transfer");
        assert_eq!(
            mirror_path(t, Path::new("/home/max/Фото/a.jpg")),
            PathBuf::from("/mnt/transfer/home/max/Фото/a.jpg")
        );
    }

    #[cfg(windows)]
    #[test]
    fn mirror_on_windows_uses_drive_letter() {
        let t = Path::new(r"\\?\E:\transfer");
        assert_eq!(
            mirror_path(t, Path::new(r"\\?\c:\Users\a.txt")),
            PathBuf::from(r"\\?\E:\transfer\C\Users\a.txt")
        );
        assert_eq!(display(Path::new(r"\\?\C:\x")), r"C:\x");
        assert_eq!(to_long(r"C:\x"), PathBuf::from(r"\\?\C:\x"));
    }
}
