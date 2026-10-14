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

use std::path::Path;

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

#[cfg(windows)]
mod windows {
    use super::{Disk, Media};
    use std::mem::{size_of, zeroed};
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, GetVolumeNameForVolumeMountPointW,
        GetVolumePathNameW, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        DEVICE_SEEK_PENALTY_DESCRIPTOR, IOCTL_STORAGE_GET_DEVICE_NUMBER,
        IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_DEVICE_NUMBER,
        STORAGE_PROPERTY_QUERY, StorageDeviceSeekPenaltyProperty,
    };

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
    fn unknown_is_treated_as_hdd() {
        let d = Disk { media: Media::Unknown, number: None, volume: r"F:\".into() };
        assert!(d.sequential());
        assert_eq!(d.key(), r"f:\");
        let ssd = Disk { media: Media::Ssd, number: Some(0), volume: r"C:\".into() };
        assert!(!ssd.sequential());
        assert_eq!(ssd.key(), "disk0");
    }
}
