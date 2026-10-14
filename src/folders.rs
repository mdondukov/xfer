//! Folders whose entire content exists somewhere else.
//!
//! Folder A is redundant if every non-empty file inside it has a copy outside A.
//! This is checked in a single pass. For each group of identical files we find
//! their lowest common ancestor (LCA). A file f supports redundancy only for
//! those of its ancestors that are deeper than its group's LCA: at the LCA and
//! above, all copies are inside. A unique file has no copies, so it blocks all of
//! its ancestors. Result: A is redundant if its depth is greater than the maximum
//! LCA depth among the files in its subtree.
//!
//! The property is inherited downwards: a subfolder of a redundant folder is
//! redundant too. So the report lists only the topmost redundant folders, and for
//! each one we look for where its content lives.

use crate::hashing::DupGroup;
use crate::scan::FileEntry;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FolderKind {
    /// There is a folder with exactly the same content.
    Exact,
    /// All content is inside a single folder that also holds other files.
    Contained,
    /// Copies are spread across several places.
    Scattered,
}

impl FolderKind {
    pub fn title(self) -> &'static str {
        match self {
            FolderKind::Exact => "exact copy",
            FolderKind::Contained => "fully contained",
            FolderKind::Scattered => "copies scattered",
        }
    }
}

pub struct RedundantFolder {
    pub path: PathBuf,
    pub bytes: u64,
    pub files: u64,
    pub kind: FolderKind,
    /// Where the copies are: folder and how many bytes of content were found there, descending.
    pub holders: Vec<(PathBuf, u64)>,
    /// Contains a copy that the file-level choice decided to keep.
    pub has_keeper: bool,
}

#[derive(Default)]
struct DirInfo {
    max_block: usize,
    bytes: u64,
    files: u64,
}

fn depth(p: &Path) -> usize {
    p.components().count()
}

pub fn analyze(files: &[FileEntry], groups: &[DupGroup], roots: &[PathBuf]) -> Vec<RedundantFolder> {
    let mut group_of: Vec<Option<usize>> = vec![None; files.len()];
    let mut lca_depth = Vec::with_capacity(groups.len());
    for (g, grp) in groups.iter().enumerate() {
        let mut lca = files[grp.files[0]].path.parent();
        for &i in &grp.files[1..] {
            while let Some(p) = lca {
                if files[i].path.starts_with(p) {
                    break;
                }
                lca = p.parent();
            }
        }
        lca_depth.push(lca.map_or(0, depth));
        for &i in &grp.files {
            group_of[i] = Some(g);
        }
    }

    let mut dirs: HashMap<PathBuf, DirInfo> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        if f.size == 0 {
            continue;
        }
        let block = match group_of[i] {
            Some(g) => lca_depth[g],
            None => depth(&f.path),
        };
        let root = &roots[f.root];
        for dir in f.path.ancestors().skip(1) {
            if !dir.starts_with(root) {
                break;
            }
            if !dirs.contains_key(dir) {
                dirs.insert(dir.to_path_buf(), DirInfo::default());
            }
            let info = dirs.get_mut(dir).expect("just inserted");
            info.max_block = info.max_block.max(block);
            info.bytes += f.size;
            info.files += 1;
            if dir == root.as_path() {
                break;
            }
        }
    }

    let redundant = |p: &Path, info: &DirInfo| info.bytes > 0 && depth(p) > info.max_block;
    let mut top: HashSet<PathBuf> = HashSet::new();
    for (p, info) in &dirs {
        if !redundant(p, info) {
            continue;
        }
        let parent_redundant = p
            .parent()
            .and_then(|pp| dirs.get(pp).map(|pi| redundant(pp, pi)))
            .unwrap_or(false);
        if !parent_redundant {
            top.insert(p.clone());
        }
    }

    // Where the copies are. If a copy sits at the same relative path
    // (A\x\y.jpg -> B\x\y.jpg), the holder is B; otherwise the copy's own folder.
    let mut holders: HashMap<PathBuf, HashMap<PathBuf, u64>> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        let Some(g) = group_of[i] else { continue };
        let Some(folder) = f.path.ancestors().skip(1).find(|a| top.contains(*a)) else {
            continue;
        };
        let rel = f.path.strip_prefix(folder).expect("folder is an ancestor of the file");
        let rel_len = rel.components().count();
        let mut seen: HashSet<&Path> = HashSet::new();
        for &j in &groups[g].files {
            let copy = &files[j].path;
            if copy.starts_with(folder) {
                continue;
            }
            // A holder can't be an ancestor of the folder itself: "Backup\Photos" and
            // "Photos" are siblings, and "contained in the common root" is useless.
            let holder = if copy.ends_with(rel) {
                (1..=rel_len)
                    .rev()
                    .filter_map(|n| copy.ancestors().nth(n))
                    .find(|h| !folder.starts_with(h))
            } else {
                copy.parent().filter(|h| !folder.starts_with(h))
            };
            if let Some(h) = holder
                && seen.insert(h)
            {
                *holders
                    .entry(folder.to_path_buf())
                    .or_default()
                    .entry(h.to_path_buf())
                    .or_default() += f.size;
            }
        }
    }

    let mut out: Vec<RedundantFolder> = top
        .into_iter()
        .map(|path| {
            let info = &dirs[&path];
            let mut hs: Vec<(PathBuf, u64)> =
                holders.remove(&path).unwrap_or_default().into_iter().collect();
            hs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let kind = match hs.first() {
                Some((b, covered)) if *covered >= info.bytes => {
                    if dirs.get(b).is_some_and(|bi| bi.bytes == info.bytes) {
                        FolderKind::Exact
                    } else {
                        FolderKind::Contained
                    }
                }
                _ => FolderKind::Scattered,
            };
            RedundantFolder {
                bytes: info.bytes,
                files: info.files,
                kind,
                has_keeper: false,
                holders: hs,
                path,
            }
        })
        .collect();
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    out
}

/// For each file: whether it lies inside a redundant folder.
pub fn inside_redundant(files: &[FileEntry], folders: &[RedundantFolder]) -> Vec<bool> {
    let set: HashSet<&Path> = folders.iter().map(|f| f.path.as_path()).collect();
    files
        .iter()
        .map(|f| f.path.ancestors().skip(1).any(|a| set.contains(a)))
        .collect()
}

/// Once the kept copies are chosen: flag redundant folders that contain them,
/// and among holders with equal coverage put the ones containing kept copies first.
pub fn attach_keepers(
    folders: &mut [RedundantFolder],
    files: &[FileEntry],
    keepers: &[usize],
    roots: &[PathBuf],
) {
    let mut keeper_dirs: HashSet<&Path> = HashSet::new();
    for &k in keepers {
        let root = &roots[files[k].root];
        for dir in files[k].path.ancestors().skip(1) {
            if !dir.starts_with(root) || !keeper_dirs.insert(dir) {
                break;
            }
        }
    }
    for f in folders.iter_mut() {
        f.has_keeper = keeper_dirs.contains(f.path.as_path());
        f.holders.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| {
                    let (ka, kb) = (keeper_dirs.contains(a.0.as_path()), keeper_dirs.contains(b.0.as_path()));
                    kb.cmp(&ka)
                })
                .then_with(|| a.0.cmp(&b.0))
        });
        f.holders.truncate(5);
    }
}

/// Suggest deleting a folder only if it holds none of the kept copies:
/// this keeps folder hints consistent with file hints and with --prefer.
pub fn suggest_delete(f: &RedundantFolder) -> bool {
    !f.has_keeper
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashing::Hash;
    use std::time::UNIX_EPOCH;

    fn entry(path: &str, size: u64) -> FileEntry {
        FileEntry {
            path: PathBuf::from(path),
            root: 0,
            size,
            mtime: UNIX_EPOCH,
            category: None,
            pos: 0,
        }
    }

    fn run(files: &[FileEntry], groups: &[DupGroup]) -> Vec<RedundantFolder> {
        let roots = [PathBuf::from("/d")];
        let mut folders = analyze(files, groups, &roots);
        let inside = inside_redundant(files, &folders);
        let keepers: Vec<usize> = groups
            .iter()
            .map(|g| crate::hashing::pick_keeper(g, files, &[], &inside))
            .collect();
        attach_keepers(&mut folders, files, &keepers, &roots);
        folders
    }

    /// Groups are built from the content id passed instead of a hash.
    fn setup(spec: &[(&str, u8)]) -> (Vec<FileEntry>, Vec<DupGroup>) {
        let files: Vec<FileEntry> = spec.iter().map(|(p, _)| entry(p, 10)).collect();
        let mut by: HashMap<u8, Vec<usize>> = HashMap::new();
        for (i, (_, c)) in spec.iter().enumerate() {
            by.entry(*c).or_default().push(i);
        }
        let groups = by
            .into_iter()
            .filter(|(_, v)| v.len() > 1)
            .map(|(c, files)| DupGroup { hash: [c; 32] as Hash, size: 10, files })
            .collect();
        (files, groups)
    }

    #[test]
    fn nested_backup_is_contained() {
        // The 2019 backup is entirely inside the 2021 backup, which also has new photos.
        let (files, groups) = setup(&[
            ("/d/backup2019/photos/a.jpg", 1),
            ("/d/backup2019/photos/b.jpg", 2),
            ("/d/backup2021/photos/a.jpg", 1),
            ("/d/backup2021/photos/b.jpg", 2),
            ("/d/backup2021/photos/c.jpg", 3),
        ]);
        let res = run(&files, &groups);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].path, PathBuf::from("/d/backup2019"));
        assert_eq!(res[0].kind, FolderKind::Contained);
        assert_eq!(res[0].holders[0].0, PathBuf::from("/d/backup2021"));
        assert!(suggest_delete(&res[0]));
    }

    #[test]
    fn holder_is_never_an_ancestor() {
        let (files, groups) = setup(&[
            ("/d/Фото/2019/a.jpg", 1),
            ("/d/Backup/Фото/2019/a.jpg", 1),
        ]);
        let res = run(&files, &groups);
        let backup = res.iter().find(|f| f.path == Path::new("/d/Backup")).unwrap();
        assert_eq!(backup.holders[0].0, PathBuf::from("/d/Фото"));
    }

    #[test]
    fn exact_pair_suggests_only_one() {
        let (files, groups) = setup(&[
            ("/d/Фото/x/a.jpg", 1),
            ("/d/Фото/x/b.jpg", 2),
            ("/d/От коллеги/копия/a.jpg", 1),
            ("/d/От коллеги/копия/b.jpg", 2),
        ]);
        let res = run(&files, &groups);
        assert_eq!(res.len(), 2);
        assert!(res.iter().all(|f| f.kind == FolderKind::Exact));
        assert_eq!(res.iter().filter(|f| suggest_delete(f)).count(), 1);
    }

    #[test]
    fn unique_file_blocks_folder() {
        let (files, groups) = setup(&[
            ("/d/a/1.jpg", 1),
            ("/d/a/only_here.jpg", 9),
            ("/d/b/1.jpg", 1),
        ]);
        let res = run(&files, &groups);
        // a is not redundant: it has a unique file. b is redundant: its file is in a.
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].path, PathBuf::from("/d/b"));
    }

    #[test]
    fn duplicates_inside_one_folder_do_not_make_it_redundant() {
        let (files, groups) = setup(&[("/d/a/1.jpg", 1), ("/d/a/1 (copy).jpg", 1)]);
        assert!(run(&files, &groups).is_empty());
    }

    #[test]
    fn renamed_files_are_scattered_or_contained() {
        // A colleague renamed the files: the holder is the copy's own folder.
        let (files, groups) = setup(&[
            ("/d/orig/IMG_1.jpg", 1),
            ("/d/orig/IMG_2.jpg", 2),
            ("/d/sent/photo1.jpg", 1),
            ("/d/sent/photo2.jpg", 2),
        ]);
        let res = run(&files, &groups);
        assert_eq!(res.len(), 2);
        assert!(res.iter().all(|f| f.kind == FolderKind::Exact));
    }
}
