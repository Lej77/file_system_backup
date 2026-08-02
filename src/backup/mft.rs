use std::{
    collections::HashMap,
    error::Error,
    ffi::OsString,
    fmt,
    fs::File,
    io::{self, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    range::Range,
};

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use color_eyre::eyre::{Context, eyre};
use indicatif::HumanBytes;
use ntfs::{
    Ntfs, NtfsAttributeType, NtfsError, NtfsFile, NtfsFileFlags,
    attribute_value::NtfsAttributeValue,
    structured_values::{NtfsFileName, NtfsFileNamespace},
};

use super::{get_windows_attributes, sector_reader};
use crate::{
    CancelSignal, Result,
    fs_index::{FsEntryMetadata, FsIndex, FsIndexBuildOptions},
};

pub fn scan_using_mft(
    scan_path: &str,
    retries: u32,
    custom_root: Option<&str>,
    cancel_signal: &CancelSignal,
) -> Result<FsIndex> {
    let mft_path = get_mft_path(scan_path)?;

    for attempt in 0..(retries + 1) {
        cancel_signal.as_error()?;
        let data = File::open(&mft_path).wrap_err_with(|| {
            format!(
                "Failed to open MFT at path \"{}\"",
                mft_path.to_string_lossy()
            )
        })?;

        match parse_mft(
            data,
            custom_root.unwrap_or(scan_path),
            Some(scan_path.as_ref()),
            cancel_signal,
        ) {
            Ok(index) => return Ok(index),
            Err(e) if attempt + 1 == retries => {
                return Err(e).context(format!(
                    "Failed to parse MFT table from data at {mft_path:?}"
                ));
            }
            Err(e) => {
                log::warn!(
                    "Failed MFT parse attempt {}/{}, \
                    perhaps there was a concurrent write to it so retrying after error:\
                    \n\tMFT PATH: {mft_path:?}\n\tError: {e}",
                    attempt + 1,
                    retries,
                )
            }
        }
    }
    unreachable!("loop will return")
}

/// Convert NT timestamp (number of 100-nanosecond intervals since January 1, 1601) to a datetime.
fn nt_time_to_chrono_datetime(nt: u64) -> Option<NaiveDateTime> {
    /// Difference between 1601-01-01 and 1970-01-01 in 100ns units
    ///
    /// - 369 years between 1601 and 1970
    /// - 89 leap days in that period
    /// - 10_000_000 = 100ns ticks per second
    const EPOCH_DIFF: u64 = (369 * 365 + 89) * 24 * 60 * 60 * 10_000_000;

    const EPOCH_DIFF_CHRONO: u64 = {
        // 1970-01-01 (Unix epoch)
        let unix_epoch = NaiveDate::from_ymd_opt(1970, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();

        // 1601-01-01 (NTFS epoch)
        let ntfs_epoch = NaiveDate::from_ymd_opt(1601, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();

        let duration = unix_epoch.signed_duration_since(ntfs_epoch);

        // convert seconds → 100ns ticks
        (duration.num_seconds() as u64) * 10_000_000
    };

    const _: () = assert!(EPOCH_DIFF == EPOCH_DIFF_CHRONO);

    let unix_100ns = nt.checked_sub(EPOCH_DIFF)?;

    let secs = unix_100ns / 10_000_000;
    let nsecs = (unix_100ns % 10_000_000) * 100;

    Some(DateTime::<Utc>::from_timestamp(secs as i64, nsecs as u32)?.naive_utc())
}

/// Cross-platform logic that accepts raw MFT bytes.
///
/// On errors it might make sense to reread the MFT in case there was concurrent
/// writes while it was last read.
pub fn parse_mft<T: Read + Seek>(
    mut volume: T,
    root_path: &str,
    scan_path: Option<&Path>,
    cancel_signal: &CancelSignal,
) -> Result<FsIndex> {
    pub struct MftCache<T: Read + Seek> {
        inner: T,
        cached: Vec<(Range<u64>, Vec<u8>)>,
        cache_read_values: bool,
    }
    impl<T: Read + Seek> MftCache<T> {
        pub fn new(inner: T) -> io::Result<Self> {
            Ok(Self {
                inner,
                cached: Vec::new(),
                cache_read_values: true,
            })
        }
    }
    impl<T: Read + Seek> Read for MftCache<T> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let pos = self.inner.stream_position()?;

            for (range, data) in &self.cached {
                if !range.contains(&pos) {
                    continue;
                }
                let offset = (pos - range.start) as usize;
                let len = buf.len().min(data.len() - offset);
                buf[..len].copy_from_slice(&data[offset..offset + len]);
                self.inner.seek(SeekFrom::Start(pos + len as u64))?;
                log::trace!("Read from {pos}, cached=true,  length: {len}",);
                return Ok(len);
            }
            let len = self.inner.read(buf)?;
            if self.cache_read_values {
                self.cached
                    .push((Range::from(pos..pos + len as u64), buf[..len].to_owned()))
            }

            log::trace!("Read from {pos}, cached=false, length: {len}",);
            Ok(len)
        }
    }
    impl<T: Read + Seek> Seek for MftCache<T> {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    fn find_best_name(
        file: &NtfsFile<'_>,
        volume: &mut MftCache<impl Read + Seek>,
    ) -> Option<Result<NtfsFileName, NtfsError>> {
        file.name(volume, Some(NtfsFileNamespace::Win32), None)
            .or_else(|| file.name(volume, Some(NtfsFileNamespace::Win32AndDos), None))
            .or_else(|| file.name(volume, Some(NtfsFileNamespace::Posix), None))
            .or_else(|| file.name(volume, None, None))
    }

    let ntfs = Ntfs::new(&mut sector_reader::SectorReader::new(&mut volume, 4096)?)?;

    let volume = sector_reader::SectorReader::new(volume, ntfs.file_record_size() as usize)?;
    let volume = BufReader::with_capacity(ntfs.file_record_size() as usize, volume);
    let mut volume = MftCache::new(volume)?;

    // This approach also works but might be less robust:
    /*
    let estimated_total_records = {
        ust ntfs::attribute_value::NtfsAttributeValue;

        // Get the MFT file (record 0).
        let mft = ntfs.file(&mut volume, 0)?;

        // Find the unnamed $DATA attribute.
        let mut attrs = mft.attributes();

        let data_item = std::iter::from_fn(|| attrs.next(&mut volume))
            .find_map(|item| {
                let item = item.ok()?;
                let attr = item.to_attribute().ok()?;

                if attr.ty().ok()? != NtfsAttributeType::Data {
                    return None;
                }

                // unnamed stream
                if attr.name().ok()?.is_empty() {
                    Some(item)
                } else {
                    None
                }
            })
            .ok_or_else(|| eyre!("$MFT has no unnamed $DATA attribute"))?;

        let data_attr = data_item.to_attribute()?;
        let value = data_attr.value(&mut volume)?;

        // Logical size of the $MFT stream.
        let mft_size = match value {
            NtfsAttributeValue::Resident(v) => v.len(),
            NtfsAttributeValue::NonResident(v) => v.len(),
            NtfsAttributeValue::AttributeListNonResident(v) => v.len(),
        };

        let record_size = u64::from(ntfs.file_record_size());
        mft_size / record_size
    };
    */

    let total_records = {
        // Get the MFT file (record 0).
        let mft = ntfs.file(&mut volume, 0)?;

        // Find the unnamed $BITMAP attribute.
        let mut attrs = mft.attributes();

        let bitmap_item = std::iter::from_fn(|| attrs.next(&mut volume))
            .find_map(|item| {
                let item = item.ok()?;
                let attr = item.to_attribute().ok()?;

                // Look specifically for the Bitmap attribute type
                if attr.ty().ok()? != NtfsAttributeType::Bitmap {
                    return None;
                }

                // We want the unnamed stream
                if attr.name().ok()?.is_empty() {
                    Some(item)
                } else {
                    None
                }
            })
            .ok_or_else(|| eyre!("$MFT has no unnamed $BITMAP attribute"))?;

        let bitmap_attr = bitmap_item.to_attribute()?;

        // The size of the bitmap in bytes.
        let bitmap_bytes = bitmap_attr.value_length();

        // Each bit represents one MFT record (allocated or free).
        bitmap_bytes * 8
    };

    volume.cache_read_values = false;
    log::debug!(
        "Cached {} bytes of data in memory for faster MFT parsing",
        HumanBytes(
            volume
                .cached
                .iter()
                .map(|(_, data)| data.len() as u64)
                .sum()
        )
    );

    #[derive(Debug)]
    struct Entry {
        metadata: FsEntryMetadata,
        name: String,
        parent: u64,
        size_success: bool,
        info_success: bool,
    }

    let mut entry_map: HashMap<u64, Entry> = HashMap::new();

    for record in 0u64..total_records {
        cancel_signal.as_error()?;

        let file = match ntfs.file(&mut volume, record) {
            Ok(file) => file,
            Err(NtfsError::VcnOutOfBoundsInIndexAllocation { .. }) => break,
            Err(NtfsError::InvalidFileRecordNumber { .. }) => break, // read too far
            Err(e) => {
                log::warn!("Failed to parse record {record}/{total_records}: {e}");
                continue;
            }
        };

        if !file.flags().contains(NtfsFileFlags::IN_USE) {
            // Ignore unallocated / deleted MFT records
            continue;
        }

        let info = file.info();

        // FIXME: a file can have multiple parents (hard links and so on) so we
        // should handle that better (currently only selects one path per file/folder)
        let Some(Ok(name_attr)) = find_best_name(&file, &mut volume) else {
            if info.is_ok() {
                // Only warn if record also has $STANDARD_INFORMATION:
                log::warn!("No valid name attribute found for MFT record {record}/{total_records}");
            }
            continue;
        };

        let is_dir = file.is_directory();
        let (size, allocated, size_success) = if is_dir {
            (0, 0, true)
        } else {
            // Standard $DATA lookup, traverse all attributes (this seamlessly resolves $ATTRIBUTE_LIST entries)
            if let Some(Ok(data_item)) = file.data(&mut volume, "")
                && let Ok(attr) = data_item.to_attribute()
                && let Ok(value) = attr.value(&mut volume)
            {
                let size = value.len();
                let allocated = match value {
                    NtfsAttributeValue::Resident(_) => size,
                    NtfsAttributeValue::NonResident(non_res) => non_res
                        .data_runs()
                        .filter_map(Result::ok)
                        .map(|run| run.allocated_size())
                        .sum::<u64>(),
                    NtfsAttributeValue::AttributeListNonResident(_) => size,
                };
                (size, allocated, true)
            } else {
                // TIER 2: Fallback to cached size stored inside the $FILE_NAME attribute
                // On Linux this has sometimes allowed getting the correct size
                let cached_size = name_attr.data_size();
                let cached_allocated = name_attr.allocated_size();

                if cached_size > 0 {
                    log::warn!(
                        "Fallback to possible stale cached size information for MFT record {record}/{total_records} with file name \"{}\"",
                        name_attr.name().to_string_lossy()
                    );
                    (cached_size, cached_allocated.max(cached_size), false)
                } else {
                    // Fallback: If no $DATA attribute exists (e.g. 0-byte or sparse placeholder file)
                    log::warn!(
                        "No size information for MFT record {record}/{total_records} with file name \"{}\"",
                        name_attr.name().to_string_lossy()
                    );
                    (0, 0, false)
                }
            }
        };

        let parent = name_attr.parent_directory_reference().file_record_number();
        let name = name_attr.name().to_string_lossy();

        let metadata = FsEntryMetadata {
            size,
            allocated,
            modified: info
                .as_ref()
                .ok()
                .and_then(|info| {
                    nt_time_to_chrono_datetime(info.modification_time().nt_timestamp())
                })
                .unwrap_or_default(),
            attributes: u64::from(
                info.as_ref()
                    .map(|info| info.file_attributes().bits())
                    .unwrap_or(if is_dir {
                        0x10 // FILE_ATTRIBUTE_DIRECTORY (16)
                    } else {
                        0x20 // FILE_ATTRIBUTE_ARCHIVE (32)
                    }),
            ),
            files: 0,
            folders: 0,
            drive_capacity: None,
            free_space: None,
            used_space: None,
            reserved_space: None,
            is_dir,
            children: None,
        };

        let entry_info = Entry {
            metadata,
            name,
            parent,
            size_success,
            info_success: info.is_ok(),
        };
        log::trace!(
            "Gathered info about MFT record {}/{}: {:?}",
            file.file_record_number(),
            total_records,
            entry_info
        );
        let old = entry_map.insert(file.file_record_number(), entry_info);
        if let Some(old) = old {
            log::warn!(
                "Multiple files with same NTFS File Record Number {}, \
                MFT record {record}/{total_records} replaced the previous entry, \
                forgot entry with name \"{}\"",
                file.file_record_number(),
                old.name,
            );
        }
    }

    let mut lookup_children: HashMap<u64, Vec<u64>> = HashMap::new();
    for (&id, entry) in &entry_map {
        lookup_children.entry(entry.parent).or_default().push(id);
    }

    log::debug!(
        "Finished scanning MFT, found {} records of which {} were non-empty folders (i.e. had any children)",
        entry_map.len(),
        lookup_children.len()
    );

    // Fix duplicate records:
    let mut dup_child = HashMap::new();
    for children in lookup_children.values() {
        dup_child.clear();
        for child_id in children {
            let child = entry_map.get_mut(child_id).unwrap();
            if let Some(old_id) = dup_child.insert(child.name.clone(), child_id) {
                let old = entry_map.remove(old_id).unwrap(); // remove duplicate, we will never emit it.
                let child = entry_map.get_mut(child_id).unwrap();
                if old.info_success {
                    child.metadata.attributes = old.metadata.attributes;
                    child.metadata.modified = old.metadata.modified;
                }
                if old.size_success {
                    child.metadata.size += old.metadata.size;
                    child.metadata.allocated += old.metadata.allocated;
                }
                // Lets just recalculate these values if possible:
                child.info_success = false;
                child.size_success = false;
            }
        }
    }

    let root = 5;

    let mut full_path = root_path.to_owned();
    if !full_path.ends_with(['/', '\\']) {
        full_path.push('\\');
    }
    let root_path_len = full_path.len();

    let root_entry = entry_map.remove(&root).unwrap();

    let mut root_record = root_entry.metadata.to_csv_record_without_filename();
    root_record.file_name = full_path.clone();

    let root_children = lookup_children.get(&root).map(Vec::as_slice).unwrap_or(&[]);

    let mut parents = Vec::new();
    parents.push((root, root_entry, root_children, full_path.len()));

    let csv_records = std::iter::from_fn(|| {
        loop {
            if cancel_signal.check() {
                return None;
            }

            let (_parent_id, parent, children, path_len) = parents.last_mut()?;

            full_path.truncate(*path_len);

            let [child_id, rest @ ..] = children else {
                parents.pop();
                continue;
            };

            *children = rest;

            let Some(mut child) = entry_map.remove(child_id) else {
                continue;
            };

            *parent.metadata.children.get_or_insert(0) += 1;

            full_path.push_str(&child.name);

            // Maybe recover information using OS File APIs:
            if let Some(scan_path) = scan_path
                && (!child.size_success || !child.info_success)
            {
                // Attempt to get size from file API
                let file_path = scan_path.join(full_path[root_path_len..].replace("\\", "/"));
                match std::fs::metadata(&file_path) {
                    Ok(meta) => {
                        if !child.size_success {
                            child.metadata.size = meta.len();
                            child.metadata.allocated = child.metadata.size;
                        }
                        if !child.info_success {
                            match meta.modified() {
                                Ok(modified) => {
                                    child.metadata.modified =
                                        DateTime::<Utc>::from(modified).naive_utc()
                                }
                                Err(e) => {
                                    log::warn!(
                                        "Failed to recover last last modification time using OS file API: {e}\
                                        \n\tFile with invalid information: {file_path:?}"
                                    )
                                }
                            }
                            child.metadata.attributes = u64::from(get_windows_attributes(&meta));
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "Failed to recover information using OS file API: {e}\
                            \n\tFile with invalid information: {file_path:?}"
                        );
                    }
                }
            }

            let grandchildren = lookup_children.get(child_id);

            if child.metadata.is_dir || grandchildren.is_some() {
                child.metadata.is_dir = true;
                full_path.push('\\');
            }

            let mut record = child.metadata.to_csv_record_without_filename();
            record.file_name = full_path.clone();

            if let Some(grandchildren) = grandchildren {
                parents.push((*child_id, child, grandchildren.as_slice(), full_path.len()));
            }

            return Some(record);
        }
    });

    let index = FsIndex::from_csv_records_with_root(
        std::iter::once(root_record.clone()).chain(csv_records),
        &root_record,
        FsIndexBuildOptions {
            recount_children: true,
            recalculate_folder_size: true,
            resort: true,
            custom_root: None,
        },
    );

    Ok(index)
}

#[derive(Debug)]
pub enum GetMftError {
    InvalidPath(String),
    NotNtfsVolume(String),
    VolumeResolutionFailed(String),
}
impl fmt::Display for GetMftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GetMftError::InvalidPath(e) => write!(f, "invalid path: {e}"),
            GetMftError::NotNtfsVolume(e) => write!(f, "path was not on a NTFS volume: {e}"),
            GetMftError::VolumeResolutionFailed(e) => {
                write!(f, "failed to resolve mount point for scan path: {e}")
            }
        }
    }
}
impl Error for GetMftError {}

pub fn get_mft_path<P: AsRef<Path>>(path: P) -> Result<OsString, GetMftError> {
    let volume_root = resolve_volume_root(path.as_ref())?;

    #[cfg(windows)]
    {
        let guid = volume_root
            .to_string_lossy()
            .trim_end_matches('\\')
            .replacen(r"\\?\", "", 1);

        Ok(OsString::from(format!(r"\\.\{guid}")))
    }
    #[cfg(target_os = "linux")]
    {
        ensure_ntfs(volume_root.as_ref())?;

        Ok(volume_root)
    }
}

#[cfg(windows)]
fn resolve_volume_root(path: &Path) -> Result<OsString, GetMftError> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW;

    let input = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<u16>>();

    let mut buffer = vec![0u16; 1024];

    let ok = unsafe {
        GetVolumeNameForVolumeMountPointW(input.as_ptr(), buffer.as_mut_ptr(), buffer.len() as u32)
    };

    if ok == 0 {
        return Err(GetMftError::VolumeResolutionFailed(
            "Failed to get volume GUID path".into(),
        ));
    }

    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    let guid_path = OsString::from_wide(&buffer[..len]);

    Ok(guid_path)
}

#[cfg(target_os = "linux")]
fn resolve_volume_root(path: &Path) -> Result<OsString, GetMftError> {
    use std::fs;

    // Canonicalize the target path so symlinks and trailing slashes are resolved
    let canonical_target = path.canonicalize().map_err(|e| {
        GetMftError::VolumeResolutionFailed(format!("Invalid path '{}': {e}", path.display()))
    })?;

    let mounts = fs::read_to_string("/proc/mounts")
        .map_err(|_| GetMftError::VolumeResolutionFailed("Cannot read /proc/mounts".into()))?;

    let mut best: Option<(PathBuf, String, usize)> = None;

    for line in mounts.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }

        let device = parts[0];
        // Canonicalize the mount point from /proc/mounts to match correctly
        let Ok(mount_point) = PathBuf::from(parts[1]).canonicalize() else {
            continue;
        };

        if canonical_target.starts_with(&mount_point) {
            let depth = mount_point.as_os_str().len();
            if best.as_ref().is_none_or(|b| depth > b.2) {
                best = Some((mount_point, device.to_string(), depth));
            }
        }
    }

    let (_, device_path, _) = best.ok_or_else(|| {
        GetMftError::VolumeResolutionFailed(format!(
            "Could not resolve mount point for path '{}'",
            path.display()
        ))
    })?;

    Ok(device_path.into())
}

#[cfg(target_os = "linux")]
fn ensure_ntfs(root: &Path) -> Result<(), GetMftError> {
    let mounts = std::fs::read_to_string("/proc/mounts")
        .map_err(|_| GetMftError::VolumeResolutionFailed("Cannot read /proc/mounts".into()))?;

    // On Linux, root is the block device path (e.g. /dev/sdb1)
    let fs_type = mounts
        .lines()
        .find(|line| {
            line.split_whitespace()
                .next()
                .is_some_and(|dev| dev == root.to_string_lossy())
        })
        .and_then(|l| l.split_whitespace().nth(2))
        .ok_or_else(|| GetMftError::NotNtfsVolume("Failed to find mount entry".to_string()))?;

    // Allow "fuseblk" (used by ntfs-3g), "ntfs3" (kernel 5.15+ driver), and standard "ntfs"
    if !matches!(
        fs_type,
        "ntfs" | "ntfs3" | "fuseblk" | "fuse.ntfs" | "fuse.ntfs-3g"
    ) {
        return Err(GetMftError::NotNtfsVolume(format!(
            "Filesystem is not NTFS: {}",
            fs_type
        )));
    }

    Ok(())
}
