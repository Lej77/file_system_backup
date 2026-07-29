//! Reads filesystem info to gather similar data as collected by WizTree.

use std::{
    collections::HashMap,
    error::Error,
    ffi::OsString,
    fmt,
    fs::{File, Metadata},
    io::{self, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    range::Range,
};

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use clap::Parser;
#[cfg(windows)]
use color_eyre::eyre::bail;
use color_eyre::eyre::{Context, eyre};
use flate2::{Compression, write::GzEncoder};
use indicatif::HumanBytes;
use jwalk::DirEntry;
use ntfs::{
    Ntfs, NtfsAttributeType, NtfsError, NtfsFile, NtfsFileFlags,
    attribute_value::NtfsAttributeValue,
    structured_values::{NtfsFileName, NtfsFileNamespace},
};

use crate::{
    CancelSignal, CommonOpt, Result, RsyncableOpts, WizTreeCsvRecord, fs_index::{FsEntryMetadata, FsIndex, FsIndexBuildOptions}, utils::{Rsyncable, create_file},
};

mod sector_reader;

#[derive(Debug, Parser, Clone)]
pub struct BackupOpts {
    #[clap(flatten)]
    pub common: CommonOpt,

    /// Don't compress the output file.
    #[clap(short, long, conflicts_with = "rsyncable", help_heading = "PROCESSING")]
    pub uncompressed: bool,
    #[clap(flatten)]
    pub rsyncable: RsyncableOpts,
    /// The compression level to use for the output file.
    ///
    /// The integer here is typically on a scale of 0-9 where 0 means "no
    /// compression" and 9 means "take as long as you'd like".
    ///
    /// Defaults to the best possible compression.
    #[clap(
        short,
        long,
        conflicts_with = "uncompressed",
        help_heading = "PROCESSING"
    )]
    pub compression: Option<u32>,

    /// Specify a custom root path instead of using the scan path.
    ///
    /// This can be useful if the mount path sometimes changes in order to keep
    /// the difference between backups of the same drive as small as possible.
    ///
    /// Example: "/mnt/ntfs-drive/" or "C:\"
    #[clap(long)]
    pub custom_root: Option<String>,

    /// Admin rights is required to preform fast MFT file scanning. If this
    /// flag is NOT passed then this program will exit with an error instead of
    /// preforming a slower scan.
    #[clap(short, long, help_heading = "PERFORMANCE")]
    pub allow_non_admin_scan: bool,
    /// Preform a slower can without using the MFT even if the program is
    /// started with admin rights.
    #[clap(short, long, help_heading = "PERFORMANCE")]
    pub prefer_non_admin_scan: bool,
    /// By default this program will run everything with the lowest priority possible
    /// so that the backup doesn't cause performance issues for other running programs.
    /// If this flag is enabled however then everything will be preformed with the
    /// normal priority.
    #[clap(long, help_heading = "PERFORMANCE")]
    pub normal_priority: bool,

    /// Overwrite the output file if it already exists.
    #[clap(
        long,
        requires = "output",
        visible_alias = "ow",
        help_heading = "OUTPUT"
    )]
    pub overwrite: bool,
    /// Don't try to add a file extension to the output path.
    ///
    /// Normally a file extension is only added to the output path if it doesn't
    /// already specify a one so this is only useful if you want an output file
    /// without any file extension at all.
    #[clap(long, requires = "output", help_heading = "OUTPUT")]
    pub no_file_extension: bool,
    /// Where to write the created backup file.
    #[clap(
        short,
        long,
        help_heading = "OUTPUT",
        required_unless_present = "stdout",
        conflicts_with = "stdout"
    )]
    pub output: Option<PathBuf>,

    /// Write the backup output to stdout.
    #[clap(long, help_heading = "OUTPUT")]
    pub stdout: bool,

    /// The drive or directory whose content should be backed up.
    #[clap(help_heading = "INPUT")]
    pub scan_path: PathBuf,
}
impl BackupOpts {
    pub fn run(self, cancel_signal: &CancelSignal) -> Result<()> {
        #[cfg(windows)]
        {
            let is_admin = ::is_elevated::is_elevated();
            if !is_admin && !self.prefer_non_admin_scan {
                if self.allow_non_admin_scan {
                    log::info!(
                        "Program doesn't have Admin rights but is continuing anyway with a slower scan."
                    );
                } else {
                    bail!(
                        "Admin rights is required to preform fast MFT file scanning, pass the `--allow-non-admin-scan` flag to allow slower scanning or start this program with elevated permissions"
                    );
                }
            }
        }
        #[cfg(not(windows))]
        let is_admin = true;

        let _low_priority_guard = if self.normal_priority || cfg!(not(windows)) {
            None
        } else {
            // Processes spawned while this process is in background mode will also
            // have background mode enabled.
            match thread_priority::process::BackgroundProcessPriority::set_background_priority() {
                Ok(guard) => {
                    log::info!(
                        "Running backup with background priority, to run with normal priority pass the `--normal-priority` flag"
                    );
                    Some(guard)
                }
                Err(e) => {
                    // Could be because the process were started in background mode already:
                    log::error!(
                        "Failed to enable background priority mode (pass the `--normal-priority` flag to not use background priority): {}",
                        e
                    );
                    None
                }
            }
        };

        if !self.uncompressed
            && let Some(level) = self.compression
            && level > 9
        {
            log::warn!(
                "Compression level should be a number between 0 and 9 but {} was specified",
                level
            );
        }

        // Create output file (exit early if we can't overwrite or create it):
        let mut output_path = self.output.clone();
        let stdout_handle;
        let mut stdout_guard;
        let mut output_file;
        let output: &mut dyn Write = if let Some(output_path) = &mut output_path {
            if output_path.extension().is_none() {
                if self.no_file_extension {
                    log::trace!(
                        "Output file doesn't have a file extension and we aren't adding one automatically because the `--no-file-extension` flag is specified"
                    );
                } else if self.uncompressed {
                    output_path.set_extension("csv");
                } else {
                    output_path.set_extension("csv.gz");
                }
            }
            output_file = Some(create_file(self.overwrite, &output_path).wrap_err_with(|| {
                format!(
                    r#"Failed to create output file at: "{}""#,
                    output_path.display()
                )
            })?);
            output_file.as_mut().unwrap()
        } else {
            stdout_handle = io::stdout();
            stdout_guard = stdout_handle.lock();
            &mut stdout_guard
        };
        let output_msg = if let Some(path) = &output_path {
            format!(r#"a file at "{}""#, path.display())
        } else {
            "stdout".to_string()
        };

        // Scan filesystem:
        log::info!(
            r#"Scanning "{}" and writing the {}results to {}"#,
            self.scan_path.display(),
            if self.uncompressed { "" } else { "compressed " },
            output_msg,
        );

        log::warn!(
            "This command is experimental and might not produce correct output, prefer the \"wiz-tree-backup\" command"
        );

        let fs_index = if is_admin && !self.prefer_non_admin_scan {
            scan_using_mft(
                &self.scan_path.to_string_lossy(),
                2,
                self.custom_root.as_deref(),
                cancel_signal,
            )?
        } else {
            scan_cross_platform(
                &self.scan_path.to_string_lossy(),
                self.custom_root.as_deref(),
                cancel_signal,
            )?
        };

        log::info!("Finished filesystem scan, writing to output");

        // Enable compression:
        let mut gz_encoder;
        let mut rsyncable;
        let output: &mut dyn Write = if self.uncompressed {
            output
        } else {
            let compression_level = self
                .compression
                .map(Compression::new)
                .unwrap_or_else(Compression::best);

            gz_encoder = GzEncoder::new(output, compression_level);

            if self.rsyncable.rsyncable {
                rsyncable = Rsyncable::new(gz_encoder);
                &mut rsyncable
            } else {
                &mut gz_encoder
            }
        };

        let output = cancel_signal.wrap_io(output);

        // Create CSV records from result of filesystem scan:
        WizTreeCsvRecord::write_csv_to(fs_index.csv_iter(None, '\\', true), output)
            .map_err(|e| eyre!(e.to_string()))
            .wrap_err("failed to write CSV with filesystem info")?;

        Ok(())
    }
}

fn get_windows_attributes(metadata: &Metadata) -> u32 {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes()
    }

    #[cfg(not(windows))]
    {
        // Synthesize standard flags
        let mut attr: u32 = 0;
        if metadata.permissions().readonly() {
            attr |= 0x01; // FILE_ATTRIBUTE_READONLY (1)
        }
        if metadata.is_dir() {
            attr |= 0x10; // FILE_ATTRIBUTE_DIRECTORY (16)
        } else {
            attr |= 0x20; // FILE_ATTRIBUTE_ARCHIVE (32)
        }
        attr
    }
}

/// Cross platform filesystem scan.
pub fn scan_cross_platform(
    scan_path: &str,
    custom_root: Option<&str>,
    cancel_signal: &CancelSignal,
) -> Result<FsIndex> {
    // Multi-threaded directory walking via jwalk
    let walker = jwalk::WalkDir::new(scan_path)
        .skip_hidden(false)
        .sort(false)
        .process_read_dir({
            let cancel_signal = cancel_signal.clone();
            move |_, _, _, children| {
                if cancel_signal.check() {
                    children.clear();
                }
            }
        });

    let csv_records = walker.try_into_iter()?.filter_map(|entry| {
        if cancel_signal.check() {
            return None;
        }

        let entry: DirEntry<_> = match entry {
            Ok(e) => e,
            Err(_) => return None, // Gracefully skip unreadable/permission-denied paths
        };

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => return None,
        };

        let modified = metadata
            .modified()
            .map(|system_time| DateTime::<Utc>::from(system_time).naive_utc())
            .unwrap_or_default();

        // Extract native path as a clean string
        let mut full_path = entry.path().to_string_lossy().into_owned();

        // Standard size and allocated cluster sizes (fallback to standard size if layout isn't available)
        let size = metadata.len();

        #[cfg(unix)]
        let allocated = {
            use std::os::unix::fs::MetadataExt;
            metadata.blocks() * 512
        };
        #[cfg(not(unix))]
        let allocated = size; // Simplification for non-Unix without diving into OS-specific APIs

        let attributes = u64::from(get_windows_attributes(&metadata));

        if entry.file_type().is_dir() && !full_path.ends_with(['/', '\\']) {
            full_path.push('/');
        }

        let record = WizTreeCsvRecord {
            file_name: full_path,
            size,
            allocated,
            modified,
            attributes,
            // We will have to update these later:
            files: 0,
            folders: 0,
            // Ignore these for now (they weren't used by older WizTree versions anyway):
            drive_capacity: None,
            free_space: None,
            used_space: None,
            reserved_space: None,
        };
        log::trace!("Visited entry {record:?}");
        Some(record)
    });

    Ok(FsIndex::from_csv_records(
        csv_records,
        FsIndexBuildOptions {
            recount_children: true,
            recalculate_folder_size: true,
            resort: true,
            custom_root,
        },
    ))
}

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

        match parse_mft(data, custom_root.unwrap_or(scan_path), cancel_signal) {
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
    }

    let mut entry_map: HashMap<u64, Entry> = HashMap::new();
    let cluster_size = u64::from(ntfs.cluster_size());

    for record in 0u64..total_records {
        cancel_signal.as_error()?;

        let file = match ntfs.file(&mut volume, record) {
            Ok(file) => file,
            Err(NtfsError::VcnOutOfBoundsInIndexAllocation { .. }) => break,
            Err(NtfsError::InvalidFileRecordNumber { .. }) => break, // read too far
            Err(e) => {
                return Err(e)
                    .wrap_err(format!("Failed to parse record {record}/{total_records}",));
            }
        };

        if !file.flags().contains(NtfsFileFlags::IN_USE) {
            // Ignore unallocated / deleted MFT records
            continue;
        }

        let Ok(info) = file.info() else {
            // Extension records don't have $STANDARD_INFORMATION; skip them.
            // TODO: find better way to detect extension records.
            continue;
        };

        // FIXME: a file can have multiple parents (hard links and so on) so we
        // should handle that better (currently only selects one path per file/folder)
        let Some(Ok(name_attr)) = find_best_name(&file, &mut volume) else {
            log::debug!("No valid name attribute found for MFT record {record}/{total_records}");
            continue;
        };

        let (size, allocated) = if file.is_directory() {
            (0, 0)
        } else {
            // Standard $DATA lookup, traverse all attributes (this seamlessly resolves $ATTRIBUTE_LIST entries)
            let mut attributes = file.attributes();
            let size_info = loop {
                let Some(item) = attributes.next(&mut volume) else {
                    break None;
                };
                let Ok(item) = item else { continue };
                let Ok(attr) = item.to_attribute() else {
                    continue;
                };

                // Look specifically for $DATA attribute
                let Ok(kind) = attr.ty() else { continue };
                if kind != NtfsAttributeType::Data {
                    continue;
                }

                // We want the unnamed data stream
                let Some(name) = attr.name().ok() else {
                    continue;
                };
                if name.is_empty() {
                    let logical_size = attr.value_length();

                    attr.value(&mut volume).unwrap().len();
                    let allocated_size = if attr.is_resident() {
                        logical_size
                    } else if logical_size == 0 {
                        0
                    } else if let Ok(val) = attr.value(&mut volume) {
                        match val {
                            NtfsAttributeValue::Resident(res) => res.len(),
                            NtfsAttributeValue::NonResident(non_res) => {
                                non_res.len().div_ceil(cluster_size) * cluster_size
                            }
                            NtfsAttributeValue::AttributeListNonResident(attr_list) => {
                                attr_list.len().div_ceil(cluster_size) * cluster_size
                            }
                        }
                    } else {
                        // Non-resident data rounds up to nearest cluster
                        logical_size.div_ceil(cluster_size) * cluster_size
                    };

                    break Some((logical_size, allocated_size));
                } else {
                    break None;
                }
            };

            if let Some(values) = size_info {
                values
            } else {
                // TIER 2: Fallback to cached size stored inside the $FILE_NAME attribute
                // Linux drivers reliably update name_attr.data_size() even when $DATA headers are out of sync!
                let cached_size = name_attr.data_size();
                let cached_allocated = name_attr.allocated_size();

                if cached_size > 0 {
                    log::warn!(
                        "Fallback to possible stale cached size information for MFT record {record}/{total_records} with file name \"{}\"",
                        name_attr.name().to_string_lossy()
                    );
                    let allocated = if cached_allocated > 0 {
                        cached_allocated
                    } else {
                        cached_size.div_ceil(cluster_size) * cluster_size
                    };
                    (cached_size, allocated)
                } else {
                    // Fallback: If no $DATA attribute exists (e.g. 0-byte or sparse placeholder file)
                    log::warn!(
                        "No size information for MFT record {record}/{total_records} with file name \"{}\"",
                        name_attr.name().to_string_lossy()
                    );
                    (0, 0)
                }
            }
        };

        let metadata = FsEntryMetadata {
            size,
            allocated,
            modified: nt_time_to_chrono_datetime(info.modification_time().nt_timestamp())
                .unwrap_or_default(),
            attributes: u64::from(info.file_attributes().bits()),
            files: 0,
            folders: 0,
            drive_capacity: None,
            free_space: None,
            used_space: None,
            reserved_space: None,
            is_dir: file.is_directory(),
            children: None,
        };

        let entry_info = Entry {
            metadata,
            name: name_attr.name().to_string_lossy(),
            parent: name_attr.parent_directory_reference().file_record_number(),
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

    let root = 5;

    let mut full_path = root_path.to_owned();
    if !full_path.ends_with(['/', '\\']) {
        full_path.push('\\');
    }

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
    ensure_ntfs(volume_root.as_ref())?;

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
