//! Reads filesystem info to gather similar data as collected by WizTree.

use std::{
    fs::Metadata,
    io::{self, Write},
    path::PathBuf,
};

use chrono::{DateTime, Utc};
use clap::Parser;
use color_eyre::eyre::{Context, bail, eyre};
use flate2::{Compression, write::GzEncoder};
use jwalk::DirEntry;

use crate::{
    CancelSignal, Result, RsyncableOpts, WizTreeCsvRecord,
    fs_index::{FsIndex, FsIndexBuildOptions},
    logging::CommonOpt,
    utils::{Rsyncable, create_file},
};

#[cfg(feature = "manual_backup_mft")]
pub mod mft;
#[cfg(feature = "manual_backup_mft")]
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
        let is_admin = ::is_elevated::is_elevated();
        #[cfg(not(windows))]
        let is_admin = true;
        {
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

        let fs_index = match () {
            #[cfg(feature = "manual_backup_mft")]
            _ if is_admin && !self.prefer_non_admin_scan => mft::scan_using_mft(
                &self.scan_path.to_string_lossy(),
                2,
                self.custom_root.as_deref(),
                cancel_signal,
            )?,
            _ => scan_cross_platform(
                &self.scan_path.to_string_lossy(),
                self.custom_root.as_deref(),
                cancel_signal,
            )?,
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
