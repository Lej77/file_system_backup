use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use clap::Parser;
use color_eyre::eyre::{Context, bail, eyre};
use edirstat::{arena::FileArenaSnapshot, traversal::TraversalEngine};
use edirstat_core::state::SharedState;
use flate2::{Compression, write::GzEncoder};

use crate::{
    CancelSignal, Result, RsyncableOpts, WizTreeCsvRecord,
    edirstat_snapshot::edirstat_snapshot_to_fs_index,
    fs_index::FsIndexBuildOptions,
    logging::CommonOpt,
    utils::{Rsyncable, create_file},
};

#[derive(Debug, Parser, Clone)]
pub struct EDirStatBackupOpts {
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
    #[clap(long, help_heading = "PROCESSING")]
    pub custom_root: Option<String>,

    /// Restrict directory traversal to the same filesystem/device boundary
    #[arg(
        long,
        short = 'x',
        alias = "one-file-system",
        help_heading = "PROCESSING"
    )]
    same_filesystem: bool,

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
impl EDirStatBackupOpts {
    pub fn run(self, cancel_signal: &CancelSignal) -> Result<()> {
        if !self.scan_path.exists() {
            bail!(
                "Error: Scan path does not exist: {}",
                self.scan_path.display()
            );
        }
        let scan_path = std::fs::canonicalize(&self.scan_path)?;

        let is_mft = scan_path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("$mft"));

        if !is_mft && !scan_path.is_dir() {
            bail!(
                "Error: Scan path is not a directory: {}",
                scan_path.display()
            );
        }

        let is_allowed_mft_scan = can_scan_using_mft(&scan_path);
        {
            if !is_allowed_mft_scan && !self.prefer_non_admin_scan {
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
            scan_path.display(),
            if self.uncompressed { "" } else { "compressed " },
            output_msg,
        );

        let snapshot = edirstat_backup(&scan_path, self.same_filesystem, cancel_signal)
            .context("eDirStat failed to collect filesystem information")?;
        log::info!("Finished filesystem scan using eDirStat, parsing data");
        let fs_index = edirstat_snapshot_to_fs_index(
            snapshot,
            FsIndexBuildOptions {
                recount_children: true,
                recalculate_folder_size: true,
                resort: true,
                custom_root: self.custom_root.as_deref(),
            },
        );

        log::info!("Data parsed, writing to output");

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

pub fn can_scan_using_mft(root_path: &Path) -> bool {
    if !edirstat::engine::mft::is_ntfs(root_path) {
        return false;
    };
    let can_do_raw_scan =
        edirstat::engine::mft::get_volume_path(root_path).is_some_and(|_volume_path| {
            cfg_select! {
                windows => is_elevated::is_elevated(),
                target_os = "linux" => std::fs::File::open(_volume_path).is_ok(),
                _ => false
            }
        });
    can_do_raw_scan
        || (cfg!(target_os = "linux")
            && edirstat::engine::mft::find_mft_file_at_mount(root_path)
                .is_some_and(|mft| std::fs::File::open(mft).is_ok()))
}

pub fn edirstat_backup(
    scan_path: &Path,
    same_filesystem: bool,
    cancel_signal: &CancelSignal,
) -> Result<FileArenaSnapshot> {
    let shared_state = Arc::new(SharedState::new());
    let traversal_engine = Arc::new(TraversalEngine::new(shared_state.scan_stats.clone()));
    let (tx, rx) = crossbeam::channel::unbounded();

    let mut cancel_waiter = cancel_signal.wait_future();
    cancel_waiter.set_waker_from_closure({
        let shared_state = shared_state.clone();
        move || {
            shared_state
                .scan_cancel
                .store(true, std::sync::atomic::Ordering::Release)
        }
    })?;

    let handle = traversal_engine.start_traversal(
        scan_path.to_owned(),
        same_filesystem,
        shared_state.scan_cancel.clone(),
        tx,
    )?;

    let mut coordinator = edirstat::coordinator::Coordinator::new(rx, shared_state.clone());
    coordinator.run_coordinator_loop(&scan_path.to_string_lossy());

    let _ = handle.join();

    let snapshot = FileArenaSnapshot::clone(&shared_state.current_snapshot.load());
    if snapshot.nodes.is_empty() {
        bail!("Error: The completed scan resulted in an empty snapshot.");
    }

    Ok(snapshot)
}
