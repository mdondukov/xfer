//! Progress bars, number formatting and text summaries.

use crate::folders::{FolderKind, RedundantFolder};
use crate::hashing::DupGroup;
use crate::paths;
use crate::scan::{FileEntry, ScanResult, Stat};
use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(120));
    pb
}

pub fn count_bar(len: u64, msg: &str) -> ProgressBar {
    let pb = ProgressBar::new(len);
    pb.set_style(
        ProgressStyle::with_template("{msg} [{bar:40}] {pos}/{len} ({eta})")
            .expect("valid template")
            .progress_chars("=> "),
    );
    pb.set_message(msg.to_string());
    pb
}

pub fn bytes_bar(len: u64, msg: &str) -> ProgressBar {
    let pb = ProgressBar::new(len);
    pb.set_style(
        ProgressStyle::with_template(
            "{msg} [{bar:40}] {bytes}/{total_bytes} {bytes_per_sec} ({eta} left)",
        )
        .expect("valid template")
        .progress_chars("=> "),
    );
    pb.set_message(msg.to_string());
    pb
}

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// 1234567 -> "1,234,567"
pub fn num(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

pub fn stat(s: Stat) -> String {
    format!("{} files, {}", num(s.count), human(s.bytes))
}

pub fn percent(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "0%".into();
    }
    format!("{:.0}%", part as f64 * 100.0 / whole as f64)
}

pub fn timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string()
}

/// Creates a folder; if it already exists (two runs in the same second), appends -2, -3...
pub fn unique_dir(base: &Path) -> Result<PathBuf> {
    let mut path = base.to_path_buf();
    let mut n = 1;
    while path.exists() {
        n += 1;
        let mut name = base.as_os_str().to_owned();
        name.push(format!("-{n}"));
        path = PathBuf::from(name);
    }
    std::fs::create_dir_all(&path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    Ok(path)
}

pub fn mtime_text(t: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Local>::from(t)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

/// CSV that Excel opens on double-click in locales using `;` as list separator: UTF-8 with BOM.
pub fn excel_csv(path: &Path) -> Result<csv::Writer<BufWriter<File>>> {
    let mut f = BufWriter::new(
        File::create(path).with_context(|| format!("cannot create {}", path.display()))?,
    );
    f.write_all(b"\xEF\xBB\xBF")?;
    Ok(csv::WriterBuilder::new().delimiter(b';').from_writer(f))
}

/// Summary text: printed to the console and saved next to the results.
#[derive(Default)]
pub struct Text(pub String);

impl Text {
    pub fn line(&mut self, s: impl AsRef<str>) {
        self.0.push_str(s.as_ref());
        self.0.push('\n');
    }
}

pub fn sources(out: &mut Text, roots: &[PathBuf]) {
    out.line("Sources:");
    for r in roots {
        out.line(format!("  {}", paths::display(r)));
    }
}

pub fn scan_section(out: &mut Text, scan: &ScanResult, only_useful: bool) {
    let total = scan.total();
    let filtered = scan.filtered_total();
    out.line(format!("Found: {}", stat(total)));
    if filtered.count > 0 {
        let why = if only_useful { "not whitelisted" } else { "excluded extensions" };
        out.line(format!("Filtered out ({why}): {}", stat(filtered)));
        let mut by_ext: Vec<(&String, &Stat)> = scan.filtered_by_ext.iter().collect();
        by_ext.sort_by_key(|e| std::cmp::Reverse(e.1.bytes));
        for (ext, s) in by_ext.iter().take(15) {
            let ext = if ext.is_empty() { "(no extension)".to_string() } else { format!(".{ext}") };
            out.line(format!("  {ext:<18} {:>10} files  {:>10}", num(s.count), human(s.bytes)));
        }
        if by_ext.len() > 15 {
            out.line(format!("  ... and {} more extensions", by_ext.len() - 15));
        }
    }
    if !scan.skipped_dirs.is_empty() {
        out.line(format!("Skipped system folders: {}", num(scan.skipped_dirs.len() as u64)));
        for d in scan.skipped_dirs.iter().take(10) {
            out.line(format!("  {}", paths::display(d)));
        }
        if scan.skipped_dirs.len() > 10 {
            out.line("  ...");
        }
    }
    if scan.cloud_only.count > 0 {
        out.line(format!(
            "Skipped OneDrive cloud-only files: {}",
            stat(scan.cloud_only)
        ));
    }
    if !scan.errors.is_empty() {
        out.line(format!("Unreadable: {} (see errors.txt)", num(scan.errors.len() as u64)));
    }
}

pub fn categories_section(out: &mut Text, files: &[FileEntry]) {
    let mut by_cat: BTreeMap<&str, Stat> = BTreeMap::new();
    for f in files {
        let title = f.category.map_or("Other", |c| c.title());
        by_cat.entry(title).or_default().add(f.size);
    }
    out.line("By category:");
    let mut rows: Vec<_> = by_cat.into_iter().collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.1.bytes));
    for (title, s) in rows {
        out.line(format!("  {title:<20} {:>10} files  {:>10}", num(s.count), human(s.bytes)));
    }
}

pub fn dup_section(
    out: &mut Text,
    files: &[FileEntry],
    groups: &[DupGroup],
    keepers: &[usize],
    folders: &[RedundantFolder],
    total_bytes: u64,
) {
    let wasted: u64 = groups.iter().map(|g| g.wasted()).sum();
    let extra: u64 = groups.iter().map(|g| g.files.len() as u64 - 1).sum();
    out.line("Duplicates:");
    out.line(format!("  Groups of identical files: {}", num(groups.len() as u64)));
    out.line(format!("  Extra copies: {} files, {}", num(extra), human(wasted)));
    out.line(format!(
        "  Space saved: {} ({} of total)",
        human(wasted),
        percent(wasted, total_bytes)
    ));
    if !groups.is_empty() {
        out.line("  Largest duplicates:");
        for (g, &k) in groups.iter().zip(keepers).take(10) {
            out.line(format!(
                "    {} copies x {:>9}  {}",
                g.files.len(),
                human(g.size),
                paths::display(&files[k].path)
            ));
        }
    }
    if !folders.is_empty() {
        out.line(format!(
            "  Folders whose entire content exists elsewhere: {}",
            num(folders.len() as u64)
        ));
        for f in folders.iter().take(10) {
            out.line(format!("    {:>9}  {}", human(f.bytes), paths::display(&f.path)));
            out.line(format!("               -> {}", holders_text(f)));
        }
    }
}

/// "fully contained: D:\Photos" or "copies scattered: D:\a (60%), E:\b (40%)".
pub fn holders_text(f: &RedundantFolder) -> String {
    let list: Vec<String> = match f.kind {
        FolderKind::Scattered => f
            .holders
            .iter()
            .take(3)
            .map(|(h, b)| format!("{} ({})", paths::display(h), percent(*b, f.bytes)))
            .collect(),
        _ => f.holders.iter().take(1).map(|(h, _)| paths::display(h)).collect(),
    };
    format!("{}: {}", f.kind.title(), list.join(", "))
}

pub fn write_errors(dir: &Path, errors: &[String]) -> Result<()> {
    if errors.is_empty() {
        return Ok(());
    }
    let mut text = String::new();
    for e in errors {
        let _ = writeln!(text, "{e}");
    }
    std::fs::write(dir.join("errors.txt"), text)?;
    Ok(())
}

pub fn write_filtered(dir: &Path, scan: &ScanResult) -> Result<()> {
    if scan.filtered.is_empty() {
        return Ok(());
    }
    let mut w = excel_csv(&dir.join("filtered.csv"))?;
    w.write_record(["path", "ext", "size"])?;
    for (p, size) in &scan.filtered {
        w.write_record([paths::display(p), crate::filter::ext_of(p), size.to_string()])?;
    }
    w.flush()?;
    Ok(())
}
