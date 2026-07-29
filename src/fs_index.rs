use std::{
    fmt::{self, Write as _},
    mem::size_of,
    num::NonZeroU32,
    time::{Duration, SystemTime},
};

use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
#[cfg(feature = "icu_sort")]
use icu::{
    collator::{
        Collator, CollatorBorrowed,
        options::{CollatorOptions, Strength},
    },
    locale::locale,
};
use indicatif::HumanBytes;
use unicase::UniCase;

use super::WizTreeCsvRecord;

/// Used when parsing paths.
pub const PATH_SEPARATORS: &[char] = &['\\', '/'];
/// Used when constructing paths.
pub const DEFAULT_PATH_SEPARATOR: char = '\\';

fn read_dyn_sized_int(bytes: &[u8]) -> (u64, usize) {
    match bytes[0] {
        value @ 0..253 => (u64::from(value), 1),
        253 => (
            u64::from(u16::from_ne_bytes(bytes[1..3].try_into().unwrap())),
            3,
        ),
        254 => (
            u64::from(u32::from_ne_bytes(bytes[1..5].try_into().unwrap())),
            5,
        ),
        255 => (u64::from_ne_bytes(bytes[1..9].try_into().unwrap()), 9),
    }
}

/// Writes the provided value into the buffer and returns the number of used bytes.
///
/// # Panics
///
/// If the buffer is too short. The buffer must be at least 9 bytes long to guarantee this doesn't
/// occur.
fn write_dyn_sized_int(buffer: &mut [u8], value: u64) -> usize {
    const U16_MAX: u64 = u16::MAX as u64 + 1;
    const U32_MAX: u64 = u32::MAX as u64 + 1;

    match value {
        0..253 => {
            buffer[0] = value as u8;
            1
        }
        253..U16_MAX => {
            buffer[0] = 253;
            buffer[1..3].copy_from_slice(&(value as u16).to_ne_bytes());
            3
        }
        U16_MAX..U32_MAX => {
            buffer[0] = 254;
            buffer[1..5].copy_from_slice(&(value as u32).to_ne_bytes());
            5
        }
        U32_MAX.. => {
            buffer[0] = 255;
            buffer[1..9].copy_from_slice(&value.to_ne_bytes());
            9
        }
    }
}

/// Inspects an existing variable length integer at the start of the buffer and
/// writes the provided value there if possible.
///
/// Returns `None` if the existing number at the start of the buffer is too
/// small to fit the new value.
fn update_dyn_sized_int(buffer: &mut [u8], new: u64) -> Option<usize> {
    match buffer[0] {
        0..253 => {
            if new < 253 {
                buffer[0] = new as u8;
                Some(1)
            } else {
                None
            }
        }
        253 => {
            if new <= u16::MAX as u64 {
                buffer[1..3].copy_from_slice(&(new as u16).to_ne_bytes());
                Some(3)
            } else {
                None
            }
        }
        254 => {
            if new <= u32::MAX as u64 {
                buffer[1..5].copy_from_slice(&(new as u32).to_ne_bytes());
                Some(5)
            } else {
                None
            }
        }
        255 => {
            buffer[1..9].copy_from_slice(&new.to_ne_bytes());
            Some(9)
        }
    }
}

/// Metadata for a file or folder.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FsEntryMetadata {
    pub size: u64,
    pub allocated: u64,
    /// Note: nanoseconds won't be saved when this is stored in a [`FsIndex`], nanoseconds is always 0 when read from a CSV file anyway.
    pub modified: NaiveDateTime,
    pub attributes: u64,
    pub files: u64,
    pub folders: u64,
    pub is_dir: bool,
    /// Direct child entries, i.e. files and folders inside this folder, not counting recursively.
    pub children: Option<u32>,
    pub drive_capacity: Option<u64>,
    pub free_space: Option<u64>,
    pub used_space: Option<u64>,
    pub reserved_space: Option<u64>,
}
impl FsEntryMetadata {
    pub fn from_csv_record(record: &WizTreeCsvRecord) -> Self {
        let is_dir = record.file_name.ends_with(PATH_SEPARATORS);
        if !is_dir {
            assert_eq!(
                record.files, 0,
                "expected only folders to contain files, in record: {record:?}"
            );
            assert_eq!(
                record.folders, 0,
                "expected only folders to contain folders, in record: {record:?}"
            );
        }
        Self {
            size: record.size,
            allocated: record.allocated,
            modified: record.modified,
            attributes: record.attributes,
            files: record.files,
            folders: record.folders,
            is_dir,
            children: None,
            drive_capacity: record.drive_capacity,
            free_space: record.free_space,
            used_space: record.used_space,
            reserved_space: record.reserved_space,
        }
    }
    pub fn to_csv_record_without_filename(&self) -> WizTreeCsvRecord {
        WizTreeCsvRecord {
            file_name: String::new(),
            size: self.size,
            allocated: self.allocated,
            modified: self.modified,
            attributes: self.attributes,
            files: self.files,
            folders: self.folders,
            drive_capacity: self.drive_capacity,
            free_space: self.free_space,
            used_space: self.used_space,
            reserved_space: self.reserved_space,
        }
    }
    pub fn into_dir_without_size(mut self) -> Self {
        if self.is_dir {
            self.size = 0;
            self.allocated = 0;
        }
        self
    }
    pub fn into_entry_with_0_files_and_folders(mut self) -> Self {
        self.files = 0;
        self.folders = 0;
        self
    }
    pub fn into_entry_with_0_children(mut self) -> Self {
        self.files = 0;
        self.folders = 0;
        self.children = if self.is_dir { Some(0) } else { None };
        self
    }
    pub fn modified_as_system_time(&self) -> Option<SystemTime> {
        SystemTime::UNIX_EPOCH.checked_add(Duration::from_micros(
            self.modified.and_utc().timestamp_micros() as u64,
        ))
    }
    pub fn has_drive_info(&self) -> bool {
        self.drive_capacity.is_some()
            || self.free_space.is_some()
            || self.used_space.is_some()
            || self.reserved_space.is_some()
    }
}

const _: () =
    assert!(FsEntryMetadata::MAX_BINARY_SIZE <= (FsEntryMetadata::TOTAL_SIZE_MASK as usize));
const _: () = assert!((FsEntryMetadata::TOTAL_SIZE_MASK & FsEntryMetadata::IS_DRIVE_FLAG) == 0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MetadataUpdateError {
    ChangedIsDrive,
    ChangedIsFolder,
    ChangedDriveInfo,
    ChangedChildren,
    TooLargeSize,
    TooLargeAllocated,
    TooLargeAttributes,
    TooLargeFileCount,
    TooLargeFolderCount,
    TooLargeModified,
    TooLargeDriveInfo,
}
impl fmt::Display for MetadataUpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}
impl std::error::Error for MetadataUpdateError {}

/// Store entry metadata inside a byte buffer such as `Vec<u8>`.
impl FsEntryMetadata {
    /// We use variable length numbers to minimize size in the common case so this is a measure for
    /// the worst case scenario where all integers are maximally sized.
    // Note: variable length numbers has at worst 1 byte of overhead per integer.
    pub const MAX_BINARY_SIZE: usize = (size_of::<u64>() + 1) * 5
        + {
            // Children field:
            size_of::<u32>() + 1
        }
        + {
            // `NativeDateTime` consists of `NaiveDate` and `NaiveTime`, each of which can be reconstructed from two 32bit numbers or smaller in some cases.
            (size_of::<u32>() + 1) * 2
        }
        + {
            // Drive info (1 boolean for each info + 4 bytes for each u64):
            (size_of::<bool>() + size_of::<u64>()) * 4
        }
        + {
            // We write size as u8 to skip parsing sometimes:
            size_of::<u8>()
        };
    const TOTAL_SIZE_MASK: u8 = 0b_0111_1111;
    const IS_DRIVE_FLAG: u8 = 0b_1000_0000;

    /// Similar to [`Self::from_ne_bytes`] but only parses the [`Self::children`] field. The
    /// returned size is the size for all the metadata, not just the `children` field.
    pub fn read_children_from_ne_bytes(bytes: &[u8]) -> (Option<u32>, usize) {
        let total_size = bytes[0] & Self::TOTAL_SIZE_MASK;

        let (children, _) = read_dyn_sized_int(&bytes[1..]);

        let mut children = u32::try_from(children).unwrap();
        children = children.saturating_sub(1);
        let children = (children != 0).then_some(children.saturating_sub(1));

        (children, usize::from(total_size))
    }

    pub fn from_ne_bytes(bytes: &[u8]) -> (Self, usize) {
        let total_size = bytes[0] & Self::TOTAL_SIZE_MASK;
        let is_drive = (bytes[0] & Self::IS_DRIVE_FLAG) != 0;

        let mut total_offset = 1; // ignore the length stored at the first byte since we are reading all fields
        macro_rules! read_dyn_sized {
            () => {{
                let (value, offset) = read_dyn_sized_int(&bytes[total_offset..]);
                total_offset += offset;
                value
            }};
        }
        assert_ne!(
            total_size, 0,
            "tried to read entry metadata from zeroed memory"
        );

        let mut children = u32::try_from(read_dyn_sized!()).unwrap();
        let is_dir = children != 0;
        children = children.saturating_sub(1);
        let children = (children != 0).then_some(children.saturating_sub(1));

        let size = read_dyn_sized!();
        let allocated = read_dyn_sized!();

        let modified = {
            // Date as 1 u32:
            let date = read_dyn_sized!() as i32;
            let year = date >> 13;
            let ordinal = date as u32 & 0b1_1111_1111_1111;
            let date = NaiveDate::from_yo_opt(year, ordinal).unwrap();

            // Time:
            let secs = u32::try_from(read_dyn_sized!()).unwrap();
            let nano = 0;
            let time = NaiveTime::from_num_seconds_from_midnight_opt(secs, nano).unwrap();
            NaiveDateTime::new(date, time)
        };

        let attributes = read_dyn_sized!();
        let files = if is_dir { read_dyn_sized!() } else { 0 };
        let folders = if is_dir { read_dyn_sized!() } else { 0 };

        let mut drive_capacity = None;
        let mut free_space = None;
        let mut used_space = None;
        let mut reserved_space = None;

        if is_drive {
            let drive_info = [
                &mut drive_capacity,
                &mut free_space,
                &mut used_space,
                &mut reserved_space,
            ];
            for info in drive_info {
                if read_dyn_sized!() != 0 {
                    *info = Some(read_dyn_sized!());
                }
            }
        }

        debug_assert_eq!(total_size as usize, total_offset);

        (
            Self {
                size,
                allocated,
                modified,
                attributes,
                files,
                folders,
                is_dir,
                children,
                drive_capacity,
                free_space,
                used_space,
                reserved_space,
            },
            total_offset,
        )
    }

    pub fn update_ne_bytes(&self, bytes: &mut [u8]) -> Result<usize, MetadataUpdateError> {
        let prev_total_size = bytes[0] & Self::TOTAL_SIZE_MASK;
        let was_drive = (bytes[0] & Self::IS_DRIVE_FLAG) != 0;

        // Prevent overwrite outside previous size:
        let bytes = &mut bytes[..prev_total_size as usize];

        let mut total_offset = 1; // ignore the length stored at the first byte since we are reading all fields
        macro_rules! update_dyn_sized {
            ($new:expr, $error:expr) => {{
                let offset =
                    update_dyn_sized_int(&mut bytes[total_offset..], $new).ok_or_else(|| $error)?;
                total_offset += offset;
            }};
        }
        macro_rules! read_dyn_sized {
            () => {{
                let (value, offset) = read_dyn_sized_int(&bytes[total_offset..]);
                total_offset += offset;
                value
            }};
        }
        assert_ne!(
            prev_total_size, 0,
            "tried to update entry metadata from zeroed memory"
        );

        let is_drive = self.has_drive_info();
        if is_drive != was_drive {
            // Would change the number of variable length integer, not just their values
            return Err(MetadataUpdateError::ChangedIsDrive);
        }

        let mut children = u32::try_from(read_dyn_sized!()).unwrap();
        let was_dir = children != 0;
        children = children.saturating_sub(1);
        let children = (children != 0).then_some(children.saturating_sub(1));

        if self.is_dir != was_dir {
            // Would change the number of variable length integer, not just their values
            return Err(MetadataUpdateError::ChangedIsFolder);
        }
        if self.children != children {
            return Err(MetadataUpdateError::ChangedChildren);
        }

        update_dyn_sized!(self.size, MetadataUpdateError::TooLargeSize);
        update_dyn_sized!(self.allocated, MetadataUpdateError::TooLargeAllocated);
        {
            // NaiveDateTime:

            // Date as 1 u32:
            let date =
                (self.modified.date().year() << 13) | (self.modified.date().ordinal() as i32);
            assert_ne!(date as u32, 0);
            update_dyn_sized!(date as u32 as u64, MetadataUpdateError::TooLargeModified);

            // Date as 2 u32:
            // bytes[16..20].copy_from_slice(&self.modified.date().year().to_ne_bytes());
            // bytes[20..24].copy_from_slice(&self.modified.date().ordinal().to_ne_bytes());

            // Time:
            update_dyn_sized!(
                u64::from(self.modified.time().num_seconds_from_midnight()),
                MetadataUpdateError::TooLargeModified
            );
            // bytes[28..32].copy_from_slice(&self.modified.time().nanosecond().to_ne_bytes());
            debug_assert_eq!(self.modified.time().nanosecond(), 0);
        }
        update_dyn_sized!(self.attributes, MetadataUpdateError::TooLargeAttributes);
        if self.is_dir {
            update_dyn_sized!(self.files, MetadataUpdateError::TooLargeFileCount);
            update_dyn_sized!(self.folders, MetadataUpdateError::TooLargeFolderCount);
        }

        if is_drive {
            let drive_info = [
                self.drive_capacity,
                self.free_space,
                self.used_space,
                self.reserved_space,
            ];
            for info in drive_info {
                let had_info = read_dyn_sized!() != 0;
                if had_info != info.is_some() {
                    return Err(MetadataUpdateError::ChangedDriveInfo);
                }
                if let Some(info) = info {
                    update_dyn_sized!(info, MetadataUpdateError::TooLargeDriveInfo);
                }
            }
        }

        if total_offset > Self::MAX_BINARY_SIZE {
            panic!(
                "metadata size should be less than or equal to {} bytes but was {}",
                Self::MAX_BINARY_SIZE,
                total_offset
            );
        }

        assert_eq!(
            prev_total_size as usize, total_offset,
            "updating a FsEntryMetadata instance should not change its binary size"
        );
        Ok(total_offset)
    }

    pub fn to_ne_bytes(&self) -> ([u8; Self::MAX_BINARY_SIZE], usize) {
        let mut bytes = [0_u8; Self::MAX_BINARY_SIZE];
        let mut len = 1;
        macro_rules! write_dyn_sized {
            ($value:expr) => {{
                let written = write_dyn_sized_int(&mut bytes[len..], $value);
                len += written;
            }};
        }

        let is_drive = self.has_drive_info();
        let children = if self.is_dir {
            self.children.map(|c| c + 2).unwrap_or(1)
        } else {
            0
        };
        write_dyn_sized!(u64::from(children));

        write_dyn_sized!(self.size);
        write_dyn_sized!(self.allocated);
        {
            // NaiveDateTime:

            // Date as 1 u32:
            let date =
                (self.modified.date().year() << 13) | (self.modified.date().ordinal() as i32);
            assert_ne!(date as u32, 0);
            write_dyn_sized!(date as u32 as u64);

            // Date as 2 u32:
            // bytes[16..20].copy_from_slice(&self.modified.date().year().to_ne_bytes());
            // bytes[20..24].copy_from_slice(&self.modified.date().ordinal().to_ne_bytes());

            // Time:
            write_dyn_sized!(u64::from(self.modified.time().num_seconds_from_midnight()));
            // bytes[28..32].copy_from_slice(&self.modified.time().nanosecond().to_ne_bytes());
            // debug_assert_eq!(self.modified.time().nanosecond(), 0);
        }
        write_dyn_sized!(self.attributes);
        if self.is_dir {
            write_dyn_sized!(self.files);
            write_dyn_sized!(self.folders);
        }

        if is_drive {
            let drive_info = [
                self.drive_capacity,
                self.free_space,
                self.used_space,
                self.reserved_space,
            ];
            for info in drive_info {
                write_dyn_sized!(info.is_some() as u64);
                if let Some(info) = info {
                    write_dyn_sized!(info);
                }
            }
        }

        if len > Self::MAX_BINARY_SIZE {
            panic!(
                "metadata size should be less than or equal to {} bytes but was {}",
                Self::MAX_BINARY_SIZE,
                len
            );
        }

        bytes[0] = (len as u8) | (if is_drive { Self::IS_DRIVE_FLAG } else { 0 });
        (bytes, len)
    }
}

/// Identifies the path segment and metadata related to a filesystem entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsEntryId(u32);
impl FsEntryId {
    /// Get the last path segment for this filesystem entry, i.e. the name of this file/folder.
    pub fn file_name(self, index: &FsIndex) -> &str {
        let ix_len_start = self.0 as usize + 4;
        let ix_len_end = self.0 as usize + 6;
        let len = u16::from_ne_bytes(index.buffer[ix_len_start..ix_len_end].try_into().unwrap());
        std::str::from_utf8(&index.buffer[ix_len_end..ix_len_end + usize::from(len)])
            .expect("all indexed paths are UTF-8")
    }

    /// An id for the metadata associated with the path.
    ///
    /// Returns `None` if that was the last set value. This usually means that
    /// no metadata has been specified for this path yet. A fully constructed
    /// filesystem index should have set the metadata for all paths.
    pub fn metadata_id(self, index: &FsIndex) -> Option<FsMetadataId> {
        let ix = self.0 as usize;
        Some(FsMetadataId(NonZeroU32::new(u32::from_ne_bytes(
            index.buffer[ix..ix + 4].try_into().unwrap(),
        ))?))
    }
    /// Change which metadata is referred to by this path. The old metadata will
    /// still remain so this is mostly useful when the previous metadata was
    /// [`None`].
    pub fn set_metadata_id(self, metadata_id: Option<FsMetadataId>, index: &mut FsIndex) {
        let ix = self.0 as usize;
        let metadata_location = metadata_id.map(|id| id.0.get()).unwrap_or(0);
        index.buffer[ix..ix + 4].copy_from_slice(&metadata_location.to_ne_bytes());
    }
}
/// Helper methods equivalent to manually calling [`Self::metadata_id`] and then
/// calling the same method on the returned [`FsMetadataId`].
impl FsEntryId {
    /// Load metadata from index. The returned [`FsEntry`] allows quick
    /// access to [`FsChildren`].
    ///
    /// Returns `None` if [`Self::metadata_id`] returns `None``.
    ///
    /// Helper function equivalent to manually calling [`Self::metadata_id`]
    /// followed by [`FsMetadataId::load_metadata`].
    pub fn load_metadata(self, index: &FsIndex) -> Option<FsEntry> {
        Some(self.metadata_id(index)?.load_metadata(index))
    }

    /// Get children without loading all metadata.
    ///
    /// Helper function equivalent to manually calling [`Self::metadata_id`]
    /// followed by [`FsMetadataId::children`].
    pub fn children(self, index: &FsIndex) -> Option<FsChildren> {
        self.metadata_id(index)?.children(index)
    }
}

/// Identifies the metadata and children of a filesystem entry but not its path
/// or parents.
///
/// Currently implemented as the metadata's index in the [`FsIndex`] buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// Note: the buffer starts with the prefixed path for root entry so there can
// never be metadata at index 0.
pub struct FsMetadataId(NonZeroU32);
impl FsMetadataId {
    const fn as_index(self) -> usize {
        self.0.get() as usize
    }

    /// Load metadata from index. The returned [`FsEntry`] allows quick
    /// access to [`FsChildren`].
    pub fn load_metadata(self, index: &FsIndex) -> FsEntry {
        let (info, len) = FsEntryMetadata::from_ne_bytes(&index.buffer[self.as_index()..]);

        FsEntry {
            info_index: self,
            info_len: u8::try_from(len).expect("size of metadata is less than 255 bytes"),
            info,
        }
    }

    /// Replaces the stored metadata with new information.
    ///
    /// Note: updating the metadata is quite expensive to do, i.e. similar to
    /// loading it.
    ///
    /// Note: numbers can't change integer classes so usually you can only make
    /// them smaller. Also you can't change the number of direct children of a
    /// folder, i.e. [`FsEntryMetadata::children`] or if an entry is a folder
    /// ([`FsEntryMetadata::is_dir`]).
    pub fn update_metadata(
        self,
        new: FsEntryMetadata,
        index: &mut FsIndex,
    ) -> Result<(), MetadataUpdateError> {
        new.update_ne_bytes(&mut index.buffer[self.as_index()..])
            .map(|_| ())
    }

    /// Get children without loading all metadata.
    ///
    /// Returns `None` if the entry is not a folder.
    pub fn children(self, index: &FsIndex) -> Option<FsChildren> {
        let (children, info_len) =
            FsEntryMetadata::read_children_from_ne_bytes(&index.buffer[self.as_index()..]);

        let children = children?;

        Some(FsChildren {
            start: self.0.get() + (info_len as u32),
            count: children,
        })
    }
}

/// [`FsEntryId`] with parsed metadata.
#[derive(Debug, Clone)]
pub struct FsEntry {
    /// Entry index prefetched from the [`Self::id`] location. This is the index
    /// of the [`FsIndex::buffer`] where the [`Self::info`] data is serialized.
    info_index: FsMetadataId,
    info_len: u8,
    /// Data decoded from the [`Self::info_index`] location.
    ///
    /// Contains info about the number of child entries to expect after the
    /// metadata in the [`FsIndex::buffer`].
    info: FsEntryMetadata,
}
impl FsEntry {
    pub const fn metadata_id(&self) -> FsMetadataId {
        self.info_index
    }
    /// Returns `None` if the entry is not a folder.
    pub const fn children(&self) -> Option<FsChildren> {
        let count = match self.info.children {
            Some(v) => v,
            None => return None,
        };
        Some(FsChildren {
            start: self.info_index.0.get() + (self.info_len as u32),
            count,
        })
    }
    pub const fn metadata(&self) -> &FsEntryMetadata {
        &self.info
    }

    /// Replaces the stored metadata with new information.
    ///
    /// Forwards the call to [`FsMetadataId::update_metadata`].
    pub fn update_metadata(
        &self,
        new: FsEntryMetadata,
        index: &mut FsIndex,
    ) -> Result<(), MetadataUpdateError> {
        if new.is_dir != self.info.is_dir {
            return Err(MetadataUpdateError::ChangedIsFolder);
        }
        if new.children != self.info.children {
            return Err(MetadataUpdateError::ChangedChildren);
        }
        self.info_index.update_metadata(new, index)
    }
}

/// Info about direct children (i.e. files and folders) of a single folder.
#[derive(Debug, Clone)]
pub struct FsChildren {
    /// First prefixed path is at this index of [`FsIndex::buffer`].
    start: u32,
    /// The number of children at the specified location. Each child is a
    /// prefixed path segment.
    count: u32,
}
impl FsChildren {
    /// Contains no paths.
    pub const EMPTY: Self = Self { start: 0, count: 0 };

    /// Remaining length.
    pub fn len(&self) -> usize {
        self.count as usize
    }
    /// `true` if [`Self::len`] is `0`.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    /// Returns `None` if [`Self::is_empty`] is `true`.
    pub fn next(&mut self, index: &FsIndex) -> Option<FsEntryId> {
        if self.count == 0 {
            return None;
        }
        let id = FsEntryId(self.start);

        let len_ix = (self.start + 4) as usize; // skip metadata index
        let len = u16::from_ne_bytes(index.buffer[len_ix..len_ix + 2].try_into().unwrap());

        self.count -= 1;
        self.start += 4 + 2 + u32::from(len); // metadata index + len + utf8 path

        Some(id)
    }
    pub fn iter<'a>(&self, index: &'a FsIndex) -> impl Iterator<Item = FsEntryId> + 'a {
        let mut this = self.clone();
        std::iter::from_fn(move || this.next(index))
    }
}

/// Used to add children to a newly added filesystem folder.
pub struct FsFolderWriter<'a> {
    metadata_id: FsMetadataId,
    remaining_children: usize,
    index: &'a mut FsIndex,
}
impl FsFolderWriter<'_> {
    /// The number of child entities that must be added to this folder before it
    /// is considered complete.
    pub fn remaining_children(&self) -> usize {
        self.remaining_children
    }
    /// Add a child entry to this folder. The `child_info_id` can be
    /// `None` which allows setting it later via [`FsEntryId::set_metadata_id`].
    ///
    /// # Panics
    ///
    /// - If [`Self::remaining_children`] is zero.
    pub fn add_child(
        &mut self,
        child_path: &str,
        child_info_id: Option<FsMetadataId>,
    ) -> FsEntryId {
        assert_ne!(
            self.remaining_children, 0,
            "tried to write more children to a folder than was expected"
        );
        let id = self.index.append_prefixed_path(child_info_id, child_path);
        self.remaining_children -= 1;
        id
    }
    /// # Panics
    ///
    /// - If [`Self::remaining_children`] is **not** zero.
    pub fn finish(self) -> FsMetadataId {
        // Note: We never reveal the metadata id unless all children are written
        // so it is not possible to start reading partial directory entries.
        assert_eq!(
            self.remaining_children, 0,
            "remaining_children must be 0 when finishing a folder"
        );
        self.metadata_id
    }
}

/// Options for builders of [`FsIndex`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct FsIndexBuildOptions<'a> {
    /// Recalculate the number of children inside each directory. This discards
    /// child counts that are provided to the builder.
    pub recount_children: bool,
    /// Recalculate the total size for folders.
    pub recalculate_folder_size: bool,
    /// Resort the children of each directory to match WizTree's sort order
    /// (alphabetically but with folders before files).
    pub resort: bool,
    /// Override the root path segment.
    pub custom_root: Option<&'a str>,
}

/// Filesystem index that knows about all paths from a snapshot of a disk and
/// metadata about each path, such as its file size or for folders the total
/// file size of all its descendants.
pub struct FsIndex {
    /// Data is stored in the following format:
    ///
    /// Data is stored as prefixed paths:
    ///
    /// - `u32` representing the buffer index for the native endian encoded
    ///   [`FsEntryMetadata`] with info about the path.
    /// - `u16` representing the path's byte length.
    /// - Multiple `u8` representing the UTF8 string for the path segment.
    ///
    /// [`FsEntryMetadata`] specifies how many "direct" children each folder
    /// has, lets call this count for `n`. The paths for those children are
    /// placed right after the metadata inside the buffer, i.e. expect to read
    /// `n` prefixed paths after the metadata.
    buffer: Vec<u8>,

    /// The current root path of the file index.
    current_root: Option<FsEntryId>,
    /// Total number of [`FsEntryMetadata`] written.
    metadata_count: u64,
    /// Total number of paths added.
    path_count: u64,
    /// Total length of all paths.
    total_path_len: u64,
}
impl FsIndex {
    /// Construct a new index.
    ///
    /// Note that [`Self::from_csv_records`] can be a more convenient way to
    /// construct an index.
    pub const fn new() -> Self {
        Self {
            buffer: Vec::new(),
            current_root: None,
            metadata_count: 0,
            path_count: 0,
            total_path_len: 0,
        }
    }

    /// The total number of [`FsEntryMetadata`] stored in the index.
    pub fn metadata_count(&self) -> u64 {
        self.metadata_count
    }
    /// The total number of path segments stored in the index.
    pub fn path_count(&self) -> u64 {
        self.path_count
    }
    /// The sum of the length for all path segments.
    pub fn length_of_all_paths(&self) -> u64 {
        self.total_path_len
    }

    /// Add a folder or file.
    ///
    /// # Panics
    ///
    /// - When [`EntryMetadata::is_dir`] is `false`:
    ///   - The `children` argument must be `None`.
    ///   - The [`EntryMetadata::children`] must be `None`.
    ///
    /// - When [`EntryMetadata::is_dir`] is `true`:
    ///   - The `children` argument must be `Some`.
    ///   - The [`EntryMetadata::children`] must be `Some` and the count must
    ///     match the length of the `Vec` in the `children` argument.
    pub fn add_entry(
        &mut self,
        metadata: FsEntryMetadata,
        children: Option<Vec<(String, FsMetadataId)>>,
    ) -> FsMetadataId {
        let info_index = FsMetadataId(
            NonZeroU32::new(u32::try_from(self.buffer.len()).expect("buffer size exceeded 32bit"))
                .expect("buffer always start with a root path and is never empty"),
        );

        assert_eq!(
            metadata.is_dir,
            children.is_some(),
            "children argument must be Some for folders and None for files"
        );
        assert_eq!(
            metadata.children,
            children.as_ref().map(|children| children.len() as u32),
            "metadata must match the number of direct children"
        );
        let (buffer, len) = metadata.to_ne_bytes();
        self.buffer.extend_from_slice(&buffer[..len]);
        self.metadata_count += 1;

        if let Some(children) = children {
            for (name, info_index) in children {
                self.append_prefixed_path(Some(info_index), &name);
            }
        }

        info_index
    }
    /// Add metadata for a file.
    ///
    /// # Panics
    ///
    /// - If the [`EntryMetadata::children`] field is `Some`.
    /// - If the [`EntryMetadata::is_dir`] field is `true`.
    pub fn add_file(&mut self, metadata: FsEntryMetadata) -> FsMetadataId {
        self.add_entry(metadata, None)
    }
    /// Add metadata for a folder and add each child entry of that folder
    /// manually, alternatively use [`Self::add_entry`].
    ///
    /// # Panics
    ///
    /// - If the [`EntryMetadata::children`] field is `None`.
    /// - If the [`EntryMetadata::is_dir`] field is `false`.
    pub fn add_folder(&mut self, metadata: FsEntryMetadata) -> FsFolderWriter<'_> {
        let info_index = FsMetadataId(
            NonZeroU32::new(u32::try_from(self.buffer.len()).expect("buffer size exceeded 32bit"))
                .expect("buffer always start with a root path and is never empty"),
        );

        let Some(children) = metadata.children else {
            panic!("When adding a folder the metadata children=Some(_)");
        };
        if !metadata.is_dir {
            panic!("When adding a folder the metadata must have is_dir set to true");
        }
        let (buffer, len) = metadata.to_ne_bytes();
        self.buffer.extend_from_slice(&buffer[..len]);
        self.metadata_count += 1;

        FsFolderWriter {
            index: self,
            metadata_id: info_index,
            remaining_children: children as usize,
        }
    }

    fn append_prefixed_path(
        &mut self,
        info_index: Option<FsMetadataId>,
        path_segment: &str,
    ) -> FsEntryId {
        let id = FsEntryId(u32::try_from(self.buffer.len()).expect("buffer size exceeded 32bit"));

        let info_index = info_index.map(|id| id.0.get()).unwrap_or(0);

        self.buffer.extend_from_slice(&info_index.to_ne_bytes());
        self.buffer.extend_from_slice(
            &u16::try_from(path_segment.len())
                .expect("path segment must fit inside 16bit number")
                .to_ne_bytes(),
        );
        self.buffer.extend_from_slice(path_segment.as_bytes());

        self.total_path_len += path_segment.len() as u64;
        self.path_count += 1;

        id
    }

    /// Store an orphaned path segment in the file index. This "root" path can
    /// point to a folder's metadata and can in that case be interpreted as a
    /// root of the file system.
    pub fn add_root(&mut self, path_segment: &str, metadata_id: Option<FsMetadataId>) -> FsEntryId {
        let id = self.append_prefixed_path(metadata_id, path_segment);
        self.current_root = Some(id);
        id
    }
    /// Get the current "root" path of the file index. This is usually set when
    /// constructing a file index.
    pub fn root(&self) -> Option<FsEntryId> {
        self.current_root
    }

    /// Construct a file index from WizTree CSV records that might fail to be
    /// produced.
    ///
    /// # Errors
    ///
    /// - An [`std::io::Error`] with the kind
    ///   [`std::io::ErrorKind::UnexpectedEof`] if the iterator doesn't produce
    ///   any CSV records.
    /// - If the iterator produces any errors then the first error is returned.
    pub fn try_from_csv_records<E>(
        iter: impl IntoIterator<Item = Result<WizTreeCsvRecord, E>>,
        options: FsIndexBuildOptions<'_>,
    ) -> Result<Self, E>
    where
        E: From<std::io::Error>,
    {
        let mut iter = iter.into_iter();
        let root = iter.next().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "There must be at least one record in a CSV for a filesystem index, i.e. the root entry"))??;
        Self::try_from_csv_records_with_root(iter, &root, options)
    }

    /// Helper function that calls [`Self::from_csv_records_with_root`] using
    /// the first record in the iterator as the root entry.
    ///
    /// # Panics
    ///
    /// If the iterator doesn't produce any CSV records.
    pub fn from_csv_records(
        iter: impl IntoIterator<Item = WizTreeCsvRecord>,
        options: FsIndexBuildOptions<'_>,
    ) -> Self {
        let mut iter = iter.into_iter();
        let root = iter.next().expect("There must be at least one record in a CSV for a filesystem index, i.e. the root entry");
        Self::from_csv_records_with_root(std::iter::once(root.clone()).chain(iter), &root, options)
    }

    /// Construct a file index from WizTree CSV records that might fail to be
    /// produced.
    pub fn try_from_csv_records_with_root<E>(
        iter: impl IntoIterator<Item = Result<WizTreeCsvRecord, E>>,
        root: &WizTreeCsvRecord,
        options: FsIndexBuildOptions<'_>,
    ) -> Result<Self, E> {
        let mut iter = iter.into_iter();

        let mut first_error = None;
        let iter = std::iter::from_fn(|| {
            if first_error.is_some() {
                return None;
            }
            match iter.next()? {
                Ok(record) => Some(record),
                Err(e) => {
                    first_error = Some(e);
                    None
                }
            }
        });
        let index = Self::from_csv_records_with_root(
            std::iter::once(root.clone()).chain(iter),
            root,
            options,
        );
        match first_error {
            Some(e) => Err(e),
            None => Ok(index),
        }
    }

    /// Construct a file index from WizTree CSV records.
    pub fn from_csv_records_with_root(
        iter: impl IntoIterator<Item = WizTreeCsvRecord>,
        root: &WizTreeCsvRecord,
        options: FsIndexBuildOptions<'_>,
    ) -> Self {
        #[derive(Debug)]
        struct FolderInfo {
            name: String,
            info: FsEntryMetadata,
            children: Vec<(String, bool, FsMetadataId)>,
        }
        fn add_parents(
            index: &mut FsIndex,
            parents: &mut Vec<FolderInfo>,
            root_id: FsEntryId,
            keep: usize,
            #[cfg(feature = "icu_sort")] collator: Option<&CollatorBorrowed<'static>>,
            #[cfg(not(feature = "icu_sort"))] collator: Option<&()>,
            options: &FsIndexBuildOptions<'_>,
        ) {
            while parents.len() > keep {
                let mut folder = parents.pop().unwrap();
                if options.resort
                    && let Some(_collator) = collator
                {
                    folder
                        .children
                        .sort_by(|(a_name, a_is_dir, _), (b_name, b_is_dir, _)| {
                            a_is_dir.cmp(b_is_dir).reverse().then_with(|| {
                                cfg_select! {
                                    feature = "icu_sort" => _collator.compare(a_name, b_name),
                                    feature = "lexical_sort" => lexical_sort::natural_lexical_cmp(a_name, b_name),
                                    _ => a_name.cmp(b_name),
                                }
                            })
                        });
                }
                folder.info.children = Some(folder.children.len() as u32);
                let (size, allocated) = (folder.info.size, folder.info.allocated);
                let info_index = index.add_entry(
                    folder.info,
                    Some(
                        folder
                            .children
                            .into_iter()
                            .map(|(name, _, info)| (name, info))
                            .collect(),
                    ),
                );

                if let Some(parent) = parents.last_mut() {
                    if options.recalculate_folder_size {
                        parent.info.size += size;
                        parent.info.allocated += allocated;
                    }
                    parent.children.push((folder.name, true, info_index));
                } else {
                    // root folder, so we know where to update the metadata location (the start of the buffer):
                    root_id.set_metadata_id(Some(info_index), index);
                }
            }
        }

        #[allow(clippy::unnecessary_lazy_evaluations)]
        let collator = options.resort.then(|| {
            cfg_select! {
                feature = "icu_sort" => {
                    // Force "und" (Undefined/Root locale) for fixed, platform-independent collation
                    let mut options = CollatorOptions::default();
                    options.strength = Some(Strength::Secondary); // Case-insensitive collation

                    Collator::try_new(locale!("und").into(), options).expect("Failed to initialize ICU")
                }
                _ => {},
            }
        });

        let iter = iter.into_iter();
        let mut parents = Vec::<FolderInfo>::new();
        let prefix_len = root.file_name.len().saturating_sub(1);

        // Create index with root path:
        let mut this = Self::new();
        let root_id = {
            let mut root_info = FsEntryMetadata::from_csv_record(root);
            let root_path = &root.file_name[0..prefix_len];

            if options.recount_children {
                root_info = root_info.into_entry_with_0_children();
            }
            if options.recalculate_folder_size {
                root_info = root_info.into_dir_without_size();
            }

            assert!(
                root_info.is_dir,
                "root entry must be a directory but it was a file: {root:?}"
            );

            let root_name = options.custom_root.unwrap_or(root_path);
            parents.push(FolderInfo {
                name: root_name.to_owned(),
                info: root_info,
                children: Vec::new(),
            });

            this.add_root(root_name, None)
        };

        // Add each file/folder from the csv records:
        for item in iter {
            let Some(relative_path) = item.file_name.get(prefix_len..) else {
                continue; // outside of root
            };
            let mut segments = relative_path
                .split(PATH_SEPARATORS)
                .filter(|seg| !seg.is_empty())
                .peekable();
            let Some(file_name) = segments.next_back() else {
                continue; // no segments, i.e. the root folder itself
            };

            let info = FsEntryMetadata::from_csv_record(&item);

            for parent_ix in 1.. {
                match (segments.next(), parents.get_mut(parent_ix)) {
                    // Entry in current folder:
                    (None, None) => {}
                    // New parent folder, need to finish previous folder:
                    (None, Some(_)) => {
                        add_parents(
                            &mut this,
                            &mut parents,
                            root_id,
                            parent_ix,
                            collator.as_ref(),
                            &options,
                        );
                        debug_assert!(parents.get(parent_ix).is_none());
                        debug_assert!(parents.get(parent_ix - 1).is_some());
                        // Now entry in current folder!
                    }
                    // New subfolder item (should never happen since info about
                    // the folder comes before its children):
                    (Some(descendant), None) => unreachable!(
                        "child entry without first visiting its parent is not supported, \
                        descendant: {descendant:?}, tracked parents: {parents:?}",
                    ),
                    // Check that current parent folder has same name as the one in the item:
                    (Some(new), Some(old)) => {
                        assert_eq!(
                            new, old.name,
                            "visited entry of new/previous parent folder, only visit entries of the current parent folder"
                        );
                        continue;
                    }
                }

                if options.recount_children {
                    for parent in &mut parents {
                        if info.is_dir {
                            parent.info.folders += 1;
                        } else {
                            parent.info.files += 1;
                        }
                    }
                    if let Some(parent) = parents.last_mut() {
                        *parent.info.children.get_or_insert(0) += 1;
                    }
                }

                if info.is_dir {
                    // Delay writing metadata until we know paths for all direct children:
                    let mut info = info.into_entry_with_0_children();
                    if options.recalculate_folder_size {
                        info = info.into_dir_without_size();
                    }
                    parents.push(FolderInfo {
                        name: file_name.to_string(),
                        info,
                        children: Vec::new(),
                    });
                } else {
                    let parent = parents.last_mut().unwrap();
                    if options.recalculate_folder_size {
                        parent.info.size += info.size;
                        parent.info.allocated += info.allocated;
                    }
                    let info_index = this.add_entry(info, None);
                    parent
                        .children
                        .push((file_name.to_string(), false, info_index));
                }
                break;
            }
        }

        add_parents(
            &mut this,
            &mut parents,
            root_id,
            0,
            collator.as_ref(),
            &options,
        );
        this.shrink_to_fit();

        log::debug!(
            "Built filesystem index, {} entries occupying {} where strings are {}, string lengths are {}",
            this.metadata_count(),
            HumanBytes(this.buffer.capacity() as u64),
            HumanBytes(this.length_of_all_paths()),
            HumanBytes(this.path_count() * (size_of::<u16>() as u64)),
        );

        this
    }

    /// Compact the memory used by the filesystem index.
    pub fn shrink_to_fit(&mut self) {
        self.buffer.shrink_to_fit();
    }

    pub fn estimated_size(&self) -> usize {
        self.buffer.capacity()
    }

    /// Reconstruct CSV records from the filesystem index.
    pub fn csv_iter<'a>(
        &'a self,
        root_path: Option<&str>,
        path_separator: char,
        force_file_extensions: bool,
    ) -> FsIndexToCsv<'a> {
        FsIndexToCsv::new(self, root_path, path_separator, force_file_extensions)
    }

    /// Reconstruct QDirStat cache lines from the filesystem index.
    pub fn qdirstat_iter<'a>(&'a self, root_path: Option<&str>) -> FsIndexToQDirStat<'a> {
        FsIndexToQDirStat::new(self, root_path)
    }
}
impl Default for FsIndex {
    fn default() -> Self {
        Self::new()
    }
}

/// Cursor for a [`FsIndex`] used to easily traverse it.
///
/// Tracks the current stack of parents and [`FsEntryId`] of their children.
pub struct FsCursor {
    /// For each parent folder:
    ///
    /// - Its id.
    /// - An iterator over its children.
    /// - The length of [`Self::cached_children`] when this parent was pushed.
    /// - The length of [`Self::full_path`] after the folder name was pushed.
    parents: Vec<(FsEntryId, FsChildren, usize, usize)>,
    /// All filesystem entries returned by [`FsChildren`] iterators for some of
    /// the currently tracked parent folders. Only populated for a parent folder
    /// if a filename search is preformed on it.
    cached_children: Vec<FsEntryId>,
    /// The current entry that the [`Self::full_path`] is for.
    current_entry: Option<FsEntryId>,
    full_path: String,
    /// Path separator used when combining path segments. Folder paths will
    /// always end with this separator.
    path_separator: char,
    /// Affects search.
    case_sensitive: bool,
    /// Append dot after file names with any file extension.
    force_file_ext: bool,
}
impl FsCursor {
    pub const fn new(path_separator: char) -> Self {
        Self {
            parents: Vec::new(),
            cached_children: Vec::new(),
            current_entry: None,
            full_path: String::new(),
            path_separator,
            case_sensitive: false,
            force_file_ext: false,
        }
    }

    /// Affects search such as by [`Self::select_child_by_name`].
    pub const fn set_case_sensitivity(&mut self, value: bool) {
        self.case_sensitive = value;
    }

    /// If `true` then [`Self::full_path`] will include a trailing dot for file
    /// paths that don't have any file extension for better compatibility with
    /// WizTree.
    pub const fn set_force_file_extension(&mut self, value: bool) {
        self.force_file_ext = value;
    }

    /// Clear all data from cursor except the `path_separator` but keep buffer
    /// capacities.
    pub fn clear(&mut self) {
        self.current_entry = None;
        self.full_path.clear();
        self.parents.clear();
        self.cached_children.clear();
    }

    /// Returns `false` if the specified root is not a folder and so it could
    /// not be set as the root.
    pub fn set_root(&mut self, root: FsEntryId, root_path: Option<&str>, index: &FsIndex) -> bool {
        self.clear();

        let Some(children) = root.children(index) else {
            return false;
        };

        // Track Id:
        self.current_entry = Some(root);
        // Track Path:
        if let Some(root_path) = root_path {
            self.full_path.push_str(root_path);
        } else {
            self.full_path.push_str(root.file_name(index));
        }
        if !self.full_path.ends_with(PATH_SEPARATORS) {
            self.full_path.push(self.path_separator);
        }
        // Track Parents:
        self.parents.push((root, children, 0, self.full_path.len()));

        true
    }

    /// The root set by [`Self::set_root`]. Note that the root can be forgotten
    /// if [`Self::advance`] is called too many times.
    pub fn root(&self) -> Option<FsEntryId> {
        Some(self.parents.first()?.0)
    }

    /// The ids of all child entries in the current folder. Returns `None` if
    /// the current entry is not a folder.
    pub fn current_folder_children(&mut self, index: &FsIndex) -> Option<&[FsEntryId]> {
        let &(id, _, cache_start, _) = self.parents.last()?;

        if Some(id) != self.current_entry {
            log::trace!(
                "Last parent is not the selected entry, i.e. currently a file is selected, id={id:?}, current_id={:?}, current_path: {:?}",
                self.current_entry,
                self.full_path
            );
            return None;
        }

        if self.cached_children.len() > cache_start {
            return Some(&self.cached_children[cache_start..]);
        }

        let children = id.children(index)?;

        debug_assert_eq!(cache_start, self.cached_children.len());

        self.cached_children.reserve(children.len());
        self.cached_children.extend(children.iter(index));

        Some(&self.cached_children[cache_start..])
    }

    /// The id of the current filesystem entry. Returns `None` of no root has
    /// been set or if the cursor has advanced out of the root.
    pub fn current_id(&self) -> Option<FsEntryId> {
        self.current_entry
    }

    /// The full path of the current filesystem entry.
    pub fn current_path(&self) -> &str {
        self.full_path.as_str()
    }

    /// Selects a child entry of the currently selected folder.
    ///
    /// Warning: assumes the provided id is a child of the current parent
    /// folder.
    fn select_child(&mut self, child: FsEntryId, index: &FsIndex) {
        // Track id:
        self.current_entry = Some(child);
        // Track path:
        let name = child.file_name(index);
        self.full_path.push_str(name);
        if let Some(child_children) = child.children(index) {
            self.full_path.push(self.path_separator);

            // Track parents:
            self.parents.push((
                child,
                child_children,
                self.cached_children.len(),
                self.full_path.len(),
            ));
        } else if self.force_file_ext && !name.contains('.') {
            self.full_path.push('.');
        }
    }

    /// Moves the cursor from a file inside a folder to the folder itself.
    fn select_folder(&mut self) {
        let Some(&mut (parent_id, _, _, folder_name_len)) = self.parents.last_mut() else {
            // No root, never set one or exhausted all entries:
            self.clear();
            return;
        };

        self.current_entry = Some(parent_id); // Track id
        self.full_path.truncate(folder_name_len); // Track path
    }

    /// Move the cursor to a child entry of the current folder based on that
    /// child's name. Returns `false` if the child could **not** be found.
    pub fn select_child_by_name(&mut self, name: &str, index: &FsIndex) -> bool {
        let case_sensitive = self.case_sensitive;

        let Some(children) = self.current_folder_children(index) else {
            log::trace!(
                "Can't select child with name {name:?} since cursor's current entry does not have any children"
            );
            return false;
        };
        let uni_case_name = UniCase::new(name);

        let mut child_id = None;
        for &child in children {
            let is_match = if case_sensitive {
                name == child.file_name(index)
            } else {
                uni_case_name == UniCase::new(child.file_name(index))
            };
            if is_match {
                child_id = Some(child);
                break;
            }
        }
        let Some(child_id) = child_id else {
            log::trace!(
                "Could not select child with name {name:?}, first child in current directory was: {:?}",
                children.first().map(|id| id.file_name(index))
            );
            return false;
        };

        self.select_child(child_id, index);
        true
    }

    /// Number of folders tracked by the cursor.
    ///
    /// Note: the root is included in this count and if the current entry
    /// is a folder then it will also be included.
    pub fn parent_count(&self) -> usize {
        self.parents.len()
    }

    pub fn go_to_parent_at_index(&mut self, ix: usize) {
        if let Some(&(_, _, cache_start, _)) = self.parents.get(ix + 1) {
            self.cached_children.truncate(cache_start);
            self.parents.truncate(ix + 1);
            self.select_folder();
        }
    }

    /// Move the cursor to the filesystem entry at the specified path. Returns
    /// `false` if there is no entry for the path.
    ///
    /// The cursor's selection might be changed even if the return value is
    /// `false`, in that case the closest parent folder is selected.
    pub fn go_to_path(&mut self, mut full_path: &str, index: &FsIndex) -> bool {
        // Remove root path prefix:
        {
            let Some(&(_, _, _, root_full_path)) = self.parents.first() else {
                return false;
            };
            let root_path = &self.full_path[..root_full_path];
            if !full_path.starts_with(root_path) {
                log::trace!(
                    "Not common root path, root_path: {root_path:?}, search_for: {full_path:?}"
                );
                self.go_to_parent_at_index(0); // Go to root (no common parents)
                return false;
            };
            full_path = &full_path[root_path.len()..];
        }

        // forget any file selected in the current folder (i.e. move cursor to closest folder):
        self.select_folder();

        let mut segments = full_path
            .split(PATH_SEPARATORS)
            .filter(|seg| !seg.is_empty())
            .peekable();
        let mut index_iter = 1..; // Skip root segment (we handled that above)
        loop {
            let parent_ix = index_iter.next().unwrap();
            let find_segment = match (segments.next(), self.parents.get(parent_ix)) {
                (None, None) => return true, // already selected
                (None, Some(&(_, _, _, _))) => {
                    self.go_to_parent_at_index(parent_ix - 1);
                    return true; // Was a parent of the current selection
                }
                (Some(segment), None) => segment, // Cursor was at parent folder
                // Maybe matching paths, check:
                (Some(wanted_segment), Some(&(actual_id, _, _, _))) => {
                    if self.case_sensitive {
                        if wanted_segment == actual_id.file_name(index) {
                            continue;
                        }
                    } else {
                        if UniCase::new(wanted_segment) == UniCase::new(actual_id.file_name(index))
                        {
                            continue;
                        }
                    }
                    self.go_to_parent_at_index(parent_ix - 1);
                    wanted_segment
                }
            };
            if !self.select_child_by_name(find_segment, index) {
                log::trace!(
                    "The segment {find_segment:?} was not found when searching for {full_path:?}"
                );
                return false;
            }
        }
    }

    /// Move the cursor forward to the next child in the current folder.
    ///
    /// If the next child is a subfolder then the cursor is moved into that
    /// folder and will continue with its first child the next time the cursor
    /// is advanced.
    ///
    /// If the cursor is at the last child of a folder then advancing will move
    /// the cursor into the next child of an ancestor/parent folder. If there is
    /// no such child then the cursor advances out of the root folder and the
    /// root is forgotten.
    pub fn advance(&mut self, index: &FsIndex) {
        loop {
            let Some(&mut (parent_id, ref mut children, cache_start, folder_name_len)) =
                self.parents.last_mut()
            else {
                // No root, never set one or exhausted all entries:
                self.clear();
                return;
            };

            self.current_entry = Some(parent_id); // Track id
            self.full_path.truncate(folder_name_len); // Track path

            if let Some(child) = children.next(index) {
                self.select_child(child, index);
                return;
            } else {
                // Retry advancing inside parent folder:
                self.cached_children.truncate(cache_start);
                self.parents.pop();
            }
        }
    }
}

pub struct FsIndexToCsv<'a> {
    index: &'a FsIndex,
    cursor: FsCursor,
}
impl<'a> FsIndexToCsv<'a> {
    pub fn new(
        index: &'a FsIndex,
        root_path: Option<&str>,
        path_separator: char,
        force_file_extensions: bool,
    ) -> Self {
        let mut cursor = FsCursor::new(path_separator);
        if let Some(root) = index.root() {
            cursor.set_root(root, root_path, index);
        }
        cursor.set_force_file_extension(force_file_extensions);
        Self { index, cursor }
    }
}
impl Iterator for FsIndexToCsv<'_> {
    type Item = WizTreeCsvRecord;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let id = self.cursor.current_id()?; // maybe exhausted all entries
            let full_path = self.cursor.current_path();
            let Some(info) = id.load_metadata(self.index) else {
                // This child doesn't have any metadata associated with it.
                self.cursor.advance(self.index);
                continue;
            };
            let mut record = info.metadata().to_csv_record_without_filename();

            // Build full path:
            record.file_name = full_path.to_string();
            self.cursor.advance(self.index);
            return Some(record);
        }
    }
}

/// Converts an [`FsIndex`] into line-by-line formatted strings for a QDirStat v2.0 cache file.
pub struct FsIndexToQDirStat<'a> {
    index: &'a FsIndex,
    cursor: FsCursor,
    next_file_index: Option<usize>,
    default_uid: u32,
    default_gid: u32,
    is_first: bool,
}

impl<'a> FsIndexToQDirStat<'a> {
    pub fn new(index: &'a FsIndex, root_path: Option<&str>) -> Self {
        let mut cursor = FsCursor::new('/');
        if let Some(root) = index.root() {
            cursor.set_root(root, root_path, index);
        }
        Self {
            index,
            cursor,
            next_file_index: None,
            default_uid: 1000,
            default_gid: 1000,
            is_first: true,
        }
    }

    /// Sets the default User ID and Group ID used in the QDirStat output (default is 1000:1000).
    pub fn with_owner(mut self, uid: u32, gid: u32) -> Self {
        self.default_uid = uid;
        self.default_gid = gid;
        self
    }
}

impl Iterator for FsIndexToQDirStat<'_> {
    type Item = String;

    fn next(&mut self) -> Option<Self::Item> {
        // Step 1: Yield the QDirStat 2.0 header first
        if self.is_first {
            self.is_first = false;
            return Some(
                "[qdirstat 2.0 cache file]\n# Type\tpath\tsize\tuid\tgid\tperm\tmtime".to_string(),
            );
        }

        loop {
            // Yield files from last yielded folder:
            if let Some(next_child) = self.next_file_index
                && let Some(children) = self.cursor.current_folder_children(self.index)
                && let Some(id) = children.get(next_child).copied()
            {
                self.next_file_index = Some(next_child + 1);
                let Some(entry) = id.load_metadata(self.index) else {
                    continue;
                };

                let meta = entry.metadata();
                if meta.is_dir {
                    continue; // only yield files
                }

                // QDirStat Files ('F'):
                // 1. Must use relative basename only (e.g. "Cargo.toml", not "/path/to/Cargo.toml").
                // 2. Uses the file's actual byte size.
                let file_basename = id.file_name(self.index).trim_matches('/');

                let mtime = meta.modified.and_utc().timestamp();
                let mut line = String::with_capacity(128);

                write!(
                    &mut line,
                    "F\t{}\t{}\t{}\t{}\t0644\t{:#x}",
                    urlencoding::encode(file_basename),
                    meta.size,
                    self.default_uid,
                    self.default_gid,
                    mtime
                )
                .unwrap();

                return Some(line);
            }
            if self.next_file_index.is_some() {
                // Next folder (well sometimes file but we skip those since they were handled above):
                self.cursor.advance(self.index);
                self.next_file_index = None;
            }

            // QDirStat Directories ('D'):
            // 1. Must use full absolute path starting with '/' and no trailing slash.
            // 2. Directory size MUST be 0 so QDirStat computes parent sums from files.

            let id = self.cursor.current_id()?; // None when all entries are exhausted

            let full_path = self.cursor.current_path();

            let Some(entry) = id.load_metadata(self.index) else {
                // Missing metadata -> ignore entry
                self.cursor.advance(self.index);
                continue;
            };

            let meta = entry.metadata();
            if !meta.is_dir {
                self.cursor.advance(self.index);
                continue;
            }
            let raw_path = full_path.replace('\\', "/");
            let clean_path = raw_path.trim_matches('/');

            if clean_path.is_empty() {
                // Root entry?
                self.cursor.advance(self.index);
                continue;
            }

            let mtime = meta.modified.and_utc().timestamp();
            let mut line = String::with_capacity(128);

            write!(
                &mut line,
                "D /{}\t0\t{}\t{}\t0755\t{:#x}",
                urlencoding::encode(clean_path),
                self.default_uid,
                self.default_gid,
                mtime
            )
            .unwrap();

            // yield this folder's files next time iterator is resumed:
            self.next_file_index = Some(0);
            return Some(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_and_deserialize_entry_metadata() {
        let original = FsEntryMetadata {
            size: 1,
            allocated: 2,
            modified: NaiveDateTime::default(),
            attributes: 4,
            files: 10_011,
            folders: 23456,
            is_dir: true,
            children: Some(10),
            drive_capacity: Some(1357),
            free_space: Some(5432),
            used_space: Some(4321),
            reserved_space: None,
        };
        let (buffer, len) = original.to_ne_bytes();
        let buffer = &buffer[..len];
        let (restored, size) = FsEntryMetadata::from_ne_bytes(buffer);
        assert_eq!(size, buffer.len());
        assert_eq!(original, restored);
    }
}
