//! The dupes command: find duplicates and prepare a report for marking.

use crate::DupesArgs;
use crate::cache::HashCache;
use crate::filter::Filter;
use crate::folders::{self, RedundantFolder};
use crate::hashing::{self, DupGroup, to_hex};
use crate::paths;
use crate::progress::{self, Stage};
use crate::report::{self, Text, human, num};
use crate::scan::{self, FileEntry};
use anyhow::Result;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

pub struct Dedup {
    pub groups: Vec<DupGroup>,
    /// For each group, the index of the file that is kept.
    pub keepers: Vec<usize>,
    pub folders: Vec<RedundantFolder>,
    pub errors: Vec<String>,
}

pub fn analyze(
    files: &[FileEntry],
    roots: &[PathBuf],
    prefer: &[PathBuf],
    no_cache: bool,
) -> Result<Dedup> {
    let prefer = paths::canonical_list(prefer)?;
    let cache = HashCache::open(no_cache);
    let (groups, errors) = hashing::find_duplicates(files, &cache);
    let stage = Stage::spinner("Folders");
    let mut folders = folders::analyze(files, &groups, roots);
    let inside = folders::inside_redundant(files, &folders);
    let keepers: Vec<usize> = groups
        .iter()
        .map(|g| hashing::pick_keeper(g, files, &prefer, &inside))
        .collect();
    folders::attach_keepers(&mut folders, files, &keepers, roots);
    stage.finish(format!(
        "{} duplicate groups, {} duplicate folders",
        report::num(groups.len() as u64),
        report::num(folders.len() as u64)
    ));
    Ok(Dedup { groups, keepers, folders, errors })
}

pub fn run(args: &DupesArgs, no_cache: bool) -> Result<()> {
    let roots = paths::prepare_roots(&args.paths)?;
    progress::set_stages(4);
    let filter = Filter::new(&args.filter);
    let scan = scan::scan(&roots, &filter, &[]);
    let files = &scan.files;
    let d = analyze(files, &roots, &args.prefer, no_cache)?;

    let mut text = Text::default();
    report::sources(&mut text, &roots);
    report::scan_section(&mut text, &scan, filter.only_useful);
    report::dup_section(&mut text, files, &d.groups, &d.keepers, &d.folders, scan.total().bytes);
    print!("{}", text.0);

    let out = match &args.out {
        Some(p) => {
            fs::create_dir_all(p)?;
            p.clone()
        }
        None => report::unique_dir(Path::new(&format!("xfer-dupes-{}", report::timestamp())))?,
    };
    write_files_csv(&out.join("files.csv"), files, &roots, &d)?;
    write_folders_csv(&out.join("folders.csv"), &d.folders)?;
    fs::write(out.join("report.html"), html(&text, files, &d))?;
    fs::write(out.join("summary.txt"), &text.0)?;
    report::write_filtered(&out, &scan)?;
    let mut errors = scan.errors.clone();
    errors.extend(d.errors.iter().cloned());
    report::write_errors(&out, &errors)?;

    let out_abs = fs::canonicalize(&out)?;
    println!();
    println!("Report: {}", paths::display(&out_abs));
    println!("  report.html - overview to browse");
    println!("  folders.csv - duplicate folders, files.csv - duplicate files.");
    println!("  Put x in the delete column next to what should be removed.");
    println!("Then: xfer apply \"{}\"", paths::display(&out_abs));
    Ok(())
}

fn write_files_csv(path: &Path, files: &[FileEntry], roots: &[PathBuf], d: &Dedup) -> Result<()> {
    let mut w = report::excel_csv(path)?;
    w.write_record([
        "delete", "group", "suggest", "size", "size_text", "path", "modified", "hash", "root",
    ])?;
    for (n, (g, &keeper)) in d.groups.iter().zip(&d.keepers).enumerate() {
        let order = std::iter::once(keeper).chain(g.files.iter().copied().filter(|&i| i != keeper));
        for i in order {
            let f = &files[i];
            w.write_record([
                String::new(),
                (n + 1).to_string(),
                if i == keeper { "keep" } else { "duplicate" }.to_string(),
                f.size.to_string(),
                human(f.size),
                paths::display(&f.path),
                report::mtime_text(f.mtime),
                to_hex(&g.hash),
                paths::display(&roots[f.root]),
            ])?;
        }
    }
    w.flush()?;
    Ok(())
}

fn write_folders_csv(path: &Path, folders: &[RedundantFolder]) -> Result<()> {
    let mut w = report::excel_csv(path)?;
    w.write_record(["delete", "suggest", "kind", "size", "size_text", "files", "folder", "copies_in"])?;
    for f in folders {
        w.write_record([
            String::new(),
            if folders::suggest_delete(f) { "delete" } else { "keep" }.to_string(),
            f.kind.title().to_string(),
            f.bytes.to_string(),
            human(f.bytes),
            f.files.to_string(),
            paths::display(&f.path),
            report::holders_text(f),
        ])?;
    }
    w.flush()?;
    Ok(())
}

const HTML_GROUPS: usize = 500;
const HTML_FOLDERS: usize = 300;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn html(summary: &Text, files: &[FileEntry], d: &Dedup) -> String {
    let mut h = String::new();
    h.push_str(HTML_HEAD);
    let _ = writeln!(h, "<h1>Duplicates</h1><pre class=\"summary\">{}</pre>", esc(&summary.0));

    let _ = writeln!(
        h,
        "<h2>Folders whose entire content exists elsewhere ({})</h2>",
        num(d.folders.len() as u64)
    );
    if d.folders.is_empty() {
        h.push_str("<p>No such folders.</p>");
    } else {
        h.push_str("<table><tr><th>Size</th><th>Files</th><th>Folder</th><th>Copies in</th><th>Suggestion</th></tr>");
        for f in d.folders.iter().take(HTML_FOLDERS) {
            let suggest = if folders::suggest_delete(f) { "delete" } else { "keep" };
            let _ = writeln!(
                h,
                "<tr><td class=\"n\">{}</td><td class=\"n\">{}</td><td class=\"p\">{}</td><td class=\"p\">{}</td><td>{}</td></tr>",
                human(f.bytes),
                num(f.files),
                esc(&paths::display(&f.path)),
                esc(&report::holders_text(f)),
                suggest
            );
        }
        h.push_str("</table>");
        if d.folders.len() > HTML_FOLDERS {
            let _ = writeln!(h, "<p>Showing the first {HTML_FOLDERS}; the full list is in folders.csv.</p>");
        }
    }

    let _ = writeln!(
        h,
        "<h2>Groups of identical files ({})</h2><p>The copy suggested to keep is in bold.</p>",
        num(d.groups.len() as u64)
    );
    for (n, (g, &k)) in d.groups.iter().zip(&d.keepers).enumerate().take(HTML_GROUPS) {
        let _ = writeln!(
            h,
            "<details><summary>#{} - {} copies x {} ({} wasted) - {}</summary><ul>",
            n + 1,
            g.files.len(),
            human(g.size),
            human(g.wasted()),
            esc(&files[k].path.file_name().unwrap_or_default().to_string_lossy())
        );
        for &i in &g.files {
            let p = esc(&paths::display(&files[i].path));
            if i == k {
                let _ = writeln!(h, "<li><b>{p}</b></li>");
            } else {
                let _ = writeln!(h, "<li>{p}</li>");
            }
        }
        h.push_str("</ul></details>");
    }
    if d.groups.len() > HTML_GROUPS {
        let _ = writeln!(h, "<p>Showing the first {HTML_GROUPS} groups; the full list is in files.csv.</p>");
    }
    h.push_str("</body></html>\n");
    h
}

const HTML_HEAD: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Duplicate report</title>
<style>
:root { --bg: #fff; --fg: #1d1d1f; --muted: #6e6e73; --line: #e3e3e8; --card: #f6f6f8; }
@media (prefers-color-scheme: dark) { :root { --bg: #161618; --fg: #ececf0; --muted: #9a9aa2; --line: #2e2e33; --card: #202024; } }
body { background: var(--bg); color: var(--fg); font: 14px/1.5 system-ui, "Segoe UI", sans-serif; margin: 24px auto; max-width: 1200px; padding: 0 16px; }
h1 { font-size: 22px; } h2 { font-size: 17px; margin-top: 32px; }
pre.summary { background: var(--card); padding: 12px 16px; border-radius: 8px; overflow-x: auto; font-size: 13px; }
table { border-collapse: collapse; width: 100%; font-size: 13px; }
th, td { border-bottom: 1px solid var(--line); padding: 6px 8px; text-align: left; vertical-align: top; }
th { color: var(--muted); font-weight: 600; }
td.n { white-space: nowrap; text-align: right; font-variant-numeric: tabular-nums; }
td.p, li { word-break: break-all; }
details { border-bottom: 1px solid var(--line); padding: 6px 0; }
summary { cursor: pointer; }
ul { margin: 6px 0 6px 8px; }
</style></head><body>
"#;
