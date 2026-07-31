use std::{
    fs::File,
    io::{self, BufReader, BufWriter, Read, Write},
    path::PathBuf,
    process::Command,
};

use clap::Parser;
use color_eyre::eyre::{Context, bail};
use flate2::{Compression, write::GzEncoder};
use indicatif::HumanBytes;
use tempfile::TempPath;

use crate::{
    BackupFileType, CancelSignal, Result,
    fs_index::{FsIndex, FsIndexBuildOptions},
    logging::{CommonOpt, set_progress_bar},
    utils::create_progress_bar,
    wiztree_csv::WizTreeCsvRecord,
};

#[derive(Debug, Parser, Clone)]
pub struct QDirStatOpenOpts {
    #[clap(flatten)]
    pub common: CommonOpt,

    /// The backup file (CSV or QDirStat cache) to open. Reads from stdin if omitted.
    #[clap(short, long)]
    pub input: Option<PathBuf>,

    /// The backup file type. Guessed automatically if omitted.
    #[clap(long, value_enum, default_value_t = BackupFileType::Auto)]
    pub file_type: BackupFileType,

    /// Override the root path in the backup input file.
    ///
    /// This can be useful if the mount path has changed since the backup was
    /// made to allow opening folder and files in external programs from
    /// QDirStat's UI.
    ///
    /// Example: "/mnt/ntfs-drive/"
    #[clap(long)]
    pub root: Option<String>,

    /// Override binary path for qdirstat (defaults to searching $PATH)
    #[clap(long)]
    pub qdirstat_path: Option<PathBuf>,
}

impl QDirStatOpenOpts {
    /// Ensure there is a file with a QDirStat cache. Creates a temporary file
    /// if necessary.
    pub fn convert_to_qdirstat_cache(
        &self,
        cancel_signal: &CancelSignal,
    ) -> Result<(Option<TempPath>, PathBuf)> {
        // 1. Detect file type
        let mut file_type = self.file_type;
        if BackupFileType::Auto == self.file_type
            && let Some(input) = &self.input
            && let Some(guessed_type) = BackupFileType::from_file_path(input)
        {
            file_type = guessed_type;
        }

        if file_type == BackupFileType::Auto {
            bail!("Failed to determine file type. Specify manually via `--file-type`.");
        }
        file_type
            .ensure_valid_type(&[
                BackupFileType::WizTreeCsv,
                BackupFileType::WizTreeCsvGzip,
                BackupFileType::QDirStatCache,
                BackupFileType::QDirStatCacheGzip,
            ])
            .context("Invalid file type for input")?;

        // 2. Open input reader (File or Stdin)
        let mut _stdin = None;
        let (reader, input_size): (Box<dyn Read>, Option<u64>) = if let Some(input) = &self.input {
            let file = File::open(input).wrap_err_with(|| {
                format!(r#"failed to open input file at: "{}""#, input.display())
            })?;
            let size = file.metadata().ok().map(|m| m.len());
            (Box::new(file), size)
        } else {
            _stdin = Some(io::stdin());
            (Box::new(_stdin.as_mut().unwrap().lock()), None)
        };

        // Wrap input reader with progress tracking and cancellation handling
        let pb = create_progress_bar(input_size);
        set_progress_bar(&pb);
        pb.set_message("Creating temp file with compressed QDirStat cache...");

        let mut reader = BufReader::new(pb.wrap_read(cancel_signal.wrap_io(reader)));

        // 3. Create target temp file with `.qdirstat.cache.gz` extension so QDirStat detects compression
        let temp_file = tempfile::Builder::new()
            .prefix("qdirstat-cache-")
            .suffix(".qdirstat.cache.gz")
            .tempfile()
            .wrap_err("Failed to create temporary compressed file for QDirStat")?;

        if [
            BackupFileType::QDirStatCache,
            BackupFileType::QDirStatCacheGzip,
        ]
        .contains(&file_type)
        {
            // TODO: if custom_root is specified then we should really convert the cache file.

            if let Some(input) = &self.input {
                // Forward existing file (QDirStat knows how to read it):
                return Ok((None, input.clone()));
            }
            // QDirStat knows how to read the input data but it can't read from stdin, write to a temp file:
            let mut gz_writer = None;
            let mut writer: Box<dyn Write> = if file_type == BackupFileType::QDirStatCache {
                // Compress to write less data to disk. (Also matches the file extension we use later.)
                Box::new(BufWriter::new(gz_writer.get_or_insert(GzEncoder::new(
                    temp_file.as_file(),
                    Compression::fast(),
                ))))
            } else {
                Box::new(temp_file.as_file())
            };

            std::io::copy(&mut reader, &mut writer)
                .context("Failed to write stdin to temporary file")?;

            writer.flush().wrap_err("Failed to flush cache writer")?;
            drop(writer);
            if let Some(gz_writer) = gz_writer {
                gz_writer
                    .finish()
                    .wrap_err("Failed to finish Gzip compression stream")?;
            }
        } else {
            // 4. Build in-memory FsIndex
            let fs_index: FsIndex = match file_type {
                BackupFileType::WizTreeCsv => FsIndex::try_from_csv_records(
                    WizTreeCsvRecord::parse_uncompressed_csv(reader),
                    FsIndexBuildOptions::default(),
                )?,
                BackupFileType::WizTreeCsvGzip => FsIndex::try_from_csv_records(
                    WizTreeCsvRecord::parse_compressed_csv(reader),
                    FsIndexBuildOptions::default(),
                )?,
                _ => unreachable!("handled this earlier"),
            };

            // 5. Setup Gzip Encoder
            let gz_writer = GzEncoder::new(temp_file.as_file(), Compression::fast());
            let mut writer = BufWriter::new(gz_writer);

            for line in fs_index.qdirstat_iter(self.root.as_deref()) {
                writeln!(writer, "{}", line)?;
            }

            writer.flush().wrap_err("Failed to flush cache writer")?;
            let gz_encoder = writer
                .into_inner()
                .map_err(|e| color_eyre::eyre::eyre!("BufWriter error: {}", e))?;
            gz_encoder
                .finish()
                .wrap_err("Failed to finish Gzip compression stream")?;
        }

        pb.finish();

        if let Ok(meta) = temp_file.as_file().metadata() {
            log::info!(
                "Size on disk of compressed temporary cache file: {}",
                HumanBytes(meta.len())
            );
        }

        // Convert TempFile to TempPath so the file handle is closed prior to spawning QDirStat
        let temp_path = temp_file.into_temp_path();
        let file_to_open = temp_path.to_path_buf();
        Ok((Some(temp_path), file_to_open))
    }

    pub fn run(self, cancel_signal: &CancelSignal) -> Result<()> {
        let (_temp_path_guard, file_to_open) = self.convert_to_qdirstat_cache(cancel_signal)?;

        // Resolve `qdirstat` executable path
        let qdirstat_bin = self
            .qdirstat_path
            .unwrap_or_else(|| PathBuf::from("qdirstat"));

        log::info!("Launching QDirStat to view: {}", file_to_open.display());

        // Spawn QDirStat with `--cache`
        let mut child = Command::new(&qdirstat_bin)
            .arg("--cache")
            .arg(&file_to_open)
            .spawn()
            .wrap_err_with(|| {
                format!(
                    "Failed to launch QDirStat at '{}'. Is it installed (`sudo dnf install qdirstat`)?",
                    qdirstat_bin.display()
                )
            })?;

        // Handle Process Lifecycle & Cancellation Signals
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                log::info!("QDirStat exited with status: {}", status);
                if !status.success() {
                    bail!("QDirStat exited with an non-zero exit code");
                }
                break;
            }

            if let Err(e) = cancel_signal.wait_timeout(std::time::Duration::from_millis(100)) {
                log::info!("Cancellation requested. Terminating QDirStat...");
                let _ = child.kill();
                return Err(e.into());
            }
        }

        // `temp_path` drops here and automatically deletes the gzipped temp file
        Ok(())
    }
}
