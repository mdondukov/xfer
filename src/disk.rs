//! Which physical disk a path lives on, and whether it is a hard drive.
//!
//! A hard drive has one head: parallel reads or writes on the same disk make it
//! seek back and forth and get slower than a single sequential stream. Different
//! disks, on the other hand, don't get in each other's way. So xfer reads and
//! hashes each HDD sequentially, and different disks in parallel.
//!
//! On Windows the media type comes from the "seek penalty" storage property (the
//! same one the defragmenter uses), queried without admin rights. If it can't be
//! determined, as often happens behind USB bridges, the disk is treated as an HDD:
//! sequential access costs an SSD little, while parallel access hurts an HDD a lot.

use crate::scan::FileEntry;
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::thread;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Media {
    Ssd,
    // Only detected on Windows.
    #[cfg_attr(not(windows), allow(dead_code))]
    Hdd,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct Disk {
    pub media: Media,
    /// Physical disk number; volumes on the same number share one device.
    pub number: Option<u32>,
    /// Volume root such as `D:\`, for display and as a fallback grouping key.
    pub volume: String,
}

impl Disk {
    /// Read and write this disk with a single sequential stream.
    pub fn sequential(&self) -> bool {
        self.media != Media::Ssd
    }

    /// Groups paths that share one physical device.
    pub fn key(&self) -> String {
        match self.number {
            Some(n) => format!("disk{n}"),
            None => self.volume.to_lowercase(),
        }
    }

    pub fn describe(&self) -> String {
        let media = match self.media {
            Media::Ssd => "SSD",
            Media::Hdd => "HDD",
            Media::Unknown => "unknown type, treated as HDD",
        };
        match self.number {
            Some(n) => format!("{media}, physical disk {n}"),
            None => media.to_string(),
        }
    }
}

/// Probes the disk under `path`. The path itself doesn't have to exist yet
/// (a target folder about to be created); its nearest existing ancestor is used.
pub fn probe(path: &Path) -> Disk {
    let existing = path.ancestors().find(|p| p.exists()).unwrap_or(path);
    #[cfg(windows)]
    if let Some(d) = windows::probe(&crate::paths::display(existing)) {
        return d;
    }
    let _ = existing;
    Disk { media: Media::Unknown, number: None, volume: String::new() }
}

/// Opens a file for one front-to-back read. On Windows this asks the cache
/// manager for aggressive read-ahead (`FILE_FLAG_SEQUENTIAL_SCAN`).
pub fn open_sequential(path: &Path) -> io::Result<File> {
    let mut o = OpenOptions::new();
    o.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
        o.custom_flags(FILE_FLAG_SEQUENTIAL_SCAN);
    }
    o.open(path)
}

/// Where the file's data starts on the volume (first logical cluster), used to
/// read files on a hard drive in one sweep of the head instead of jumping between
/// folders. Files small enough to live inside the MFT have no clusters and get 0.
/// `None` when unknown (non-Windows, no access).
pub fn first_cluster(path: &Path) -> Option<u64> {
    #[cfg(windows)]
    return windows::first_cluster(path);
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

/// Runs `f` over files grouped by physical disk: disks in parallel, each hard
/// drive sequentially in on-disk order (`FileEntry::pos`, then scan order),
/// each SSD on the rayon pool (`--jobs`).
pub fn per_disk<T, F>(idx: &[usize], files: &[FileEntry], disks: &[Disk], f: F) -> Vec<(usize, T)>
where
    T: Send,
    F: Fn(usize) -> T + Sync,
{
    let mut groups: BTreeMap<String, (bool, Vec<usize>)> = BTreeMap::new();
    for &i in idx {
        let d = &disks[files[i].root];
        groups.entry(d.key()).or_insert((d.sequential(), Vec::new())).1.push(i);
    }
    let f = &f;
    thread::scope(|s| {
        let handles: Vec<_> = groups
            .into_values()
            .map(|(sequential, mut group)| {
                s.spawn(move || {
                    group.sort_unstable_by_key(|&i| (files[i].pos, i));
                    if sequential {
                        group.iter().map(|&i| (i, f(i))).collect::<Vec<_>>()
                    } else {
                        group.par_iter().map(|&i| (i, f(i))).collect()
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("per-disk thread"))
            .collect()
    })
}

/// Fills `FileEntry::pos` for files on hard drives. One open per file, which on
/// an HDD is far cheaper than the seeks it saves later.
pub fn locate(files: &mut [FileEntry], disks: &[Disk]) {
    let idx: Vec<usize> = (0..files.len())
        .filter(|&i| disks[files[i].root].sequential())
        .collect();
    if idx.is_empty() {
        return;
    }
    let stage = crate::progress::Stage::count("Locating", idx.len() as u64);
    let found = per_disk(&idx, files, disks, |i| {
        let pos = first_cluster(&files[i].path);
        stage.bar().inc(1);
        pos
    });
    let known = found.iter().filter(|(_, p)| p.is_some()).count();
    for (i, pos) in found {
        // Unknown positions sort last, in scan order.
        files[i].pos = pos.unwrap_or(u64::MAX);
    }
    stage.finish(format!(
        "{} of {} files on hard drives placed in on-disk order",
        crate::report::num(known as u64),
        crate::report::num(idx.len() as u64)
    ));
}

#[cfg(windows)]
mod windows {
    use super::{Disk, Media};
    use std::mem::{size_of, zeroed};
    use std::ptr::{null, null_mut};
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_HANDLE_EOF, ERROR_MORE_DATA, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        DEVICE_SEEK_PENALTY_DESCRIPTOR, FSCTL_GET_RETRIEVAL_POINTERS,
        IOCTL_STORAGE_GET_DEVICE_NUMBER, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
        RETRIEVAL_POINTERS_BUFFER, STARTING_VCN_INPUT_BUFFER, STORAGE_DEVICE_NUMBER,
        STORAGE_PROPERTY_QUERY, StorageDeviceSeekPenaltyProperty,
    };

    /// First extent of the file via FSCTL_GET_RETRIEVAL_POINTERS. The output
    /// buffer holds one extent; ERROR_MORE_DATA just means there are more.
    pub fn first_cluster(path: &Path) -> Option<u64> {
        let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: the name is NUL-terminated; the handle is closed below.
        let h = unsafe {
            CreateFileW(
                name.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return None;
        }
        let input = STARTING_VCN_INPUT_BUFFER { StartingVcn: 0 };
        // SAFETY: plain-old-data struct, all-zero is a valid value.
        let mut out: RETRIEVAL_POINTERS_BUFFER = unsafe { zeroed() };
        let mut returned = 0u32;
        // SAFETY: in/out pointers and sizes describe live local structs.
        let ok = unsafe {
            DeviceIoControl(
                h,
                FSCTL_GET_RETRIEVAL_POINTERS,
                (&input as *const STARTING_VCN_INPUT_BUFFER).cast(),
                size_of::<STARTING_VCN_INPUT_BUFFER>() as u32,
                (&mut out as *mut RETRIEVAL_POINTERS_BUFFER).cast(),
                size_of::<RETRIEVAL_POINTERS_BUFFER>() as u32,
                &mut returned,
                null_mut(),
            )
        };
        // SAFETY: reads the calling thread's last error; no preconditions.
        let err = unsafe { GetLastError() };
        // SAFETY: `h` is a valid handle we own.
        unsafe { CloseHandle(h) };
        if ok != 0 || err == ERROR_MORE_DATA {
            if out.ExtentCount == 0 {
                return Some(0);
            }
            // Lcn -1 marks a sparse or compressed run: no fixed place on disk.
            let lcn = out.Extents[0].Lcn;
            return Some(if lcn < 0 { u64::MAX } else { lcn as u64 });
        }
        // Resident in the MFT: no clusters of its own.
        (err == ERROR_HANDLE_EOF).then_some(0)
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn from_wide(buf: &[u16]) -> String {
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..n])
    }

    pub fn probe(path: &str) -> Option<Disk> {
        let mut root = [0u16; 1024];
        // SAFETY: both buffers are valid for the lengths passed.
        if unsafe { GetVolumePathNameW(wide(path).as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
            return None;
        }
        let volume = from_wide(&root);

        // `\\?\Volume{GUID}` works for drive letters and mount points alike;
        // CreateFile wants it without the trailing backslash.
        let mut guid = [0u16; 128];
        // SAFETY: `root` is NUL-terminated, `guid` is valid for its length.
        let device = if unsafe {
            GetVolumeNameForVolumeMountPointW(root.as_ptr(), guid.as_mut_ptr(), guid.len() as u32)
        } != 0
        {
            from_wide(&guid).trim_end_matches('\\').to_string()
        } else {
            format!(r"\\.\{}", volume.trim_end_matches('\\'))
        };

        // Access 0: querying device properties needs no read rights and no admin.
        // SAFETY: the name is NUL-terminated; the handle is closed below.
        let h = unsafe {
            CreateFileW(
                wide(&device).as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                0,
                null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Some(Disk { media: Media::Unknown, number: None, volume });
        }
        let media = seek_penalty(h);
        let number = device_number(h);
        // SAFETY: `h` is a valid handle we own.
        unsafe { CloseHandle(h) };
        Some(Disk { media, number, volume })
    }

    fn seek_penalty(h: HANDLE) -> Media {
        let query = STORAGE_PROPERTY_QUERY {
            PropertyId: StorageDeviceSeekPenaltyProperty,
            QueryType: PropertyStandardQuery,
            AdditionalParameters: [0],
        };
        // SAFETY: plain-old-data struct, all-zero is a valid value.
        let mut out: DEVICE_SEEK_PENALTY_DESCRIPTOR = unsafe { zeroed() };
        let mut returned = 0u32;
        // SAFETY: in/out pointers and sizes describe live local structs.
        let ok = unsafe {
            DeviceIoControl(
                h,
                IOCTL_STORAGE_QUERY_PROPERTY,
                (&query as *const STORAGE_PROPERTY_QUERY).cast(),
                size_of::<STORAGE_PROPERTY_QUERY>() as u32,
                (&mut out as *mut DEVICE_SEEK_PENALTY_DESCRIPTOR).cast(),
                size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
                &mut returned,
                null_mut(),
            )
        };
        if ok == 0 || (returned as usize) < size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() {
            return Media::Unknown;
        }
        if out.IncursSeekPenalty { Media::Hdd } else { Media::Ssd }
    }

    /// Fails for volumes spanning several disks; those fall back to the volume key.
    fn device_number(h: HANDLE) -> Option<u32> {
        // SAFETY: plain-old-data struct, all-zero is a valid value.
        let mut out: STORAGE_DEVICE_NUMBER = unsafe { zeroed() };
        let mut returned = 0u32;
        // SAFETY: the out pointer and size describe a live local struct.
        let ok = unsafe {
            DeviceIoControl(
                h,
                IOCTL_STORAGE_GET_DEVICE_NUMBER,
                null(),
                0,
                (&mut out as *mut STORAGE_DEVICE_NUMBER).cast(),
                size_of::<STORAGE_DEVICE_NUMBER>() as u32,
                &mut returned,
                null_mut(),
            )
        };
        (ok != 0).then_some(out.DeviceNumber)
    }

    #[test]
    fn finds_first_cluster() {
        let exe = std::env::current_exe().unwrap();
        let lcn = first_cluster(&exe);
        assert!(lcn.is_some(), "no position for {}", exe.display());
        println!("{} starts at cluster {lcn:?}", exe.display());
    }

    #[test]
    fn probes_system_drive() {
        let d = probe(r"C:\Windows").expect("C: has a volume");
        assert_eq!(d.volume, r"C:\");
        println!("{d:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hard_drive_is_read_in_on_disk_order() {
        use std::sync::Mutex;
        let files: Vec<FileEntry> = [30u64, 10, 20]
            .iter()
            .map(|&pos| FileEntry {
                path: format!("/d/{pos}").into(),
                root: 0,
                size: 1,
                mtime: std::time::UNIX_EPOCH,
                category: None,
                pos,
            })
            .collect();
        let hdd = [Disk { media: Media::Hdd, number: Some(1), volume: r"D:\".into() }];
        let order = Mutex::new(Vec::new());
        per_disk(&[0, 1, 2], &files, &hdd, |i| order.lock().unwrap().push(files[i].pos));
        assert_eq!(order.into_inner().unwrap(), vec![10, 20, 30]);
    }

    #[test]
    fn unknown_is_treated_as_hdd() {
        let d = Disk { media: Media::Unknown, number: None, volume: r"F:\".into() };
        assert!(d.sequential());
        assert_eq!(d.key(), r"f:\");
        let ssd = Disk { media: Media::Ssd, number: Some(0), volume: r"C:\".into() };
        assert!(!ssd.sequential());
        assert_eq!(ssd.key(), "disk0");
    }
}
