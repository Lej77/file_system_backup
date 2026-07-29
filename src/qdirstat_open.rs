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

use crate::{
    BackupFileType, CancelSignal, CommonOpt, Result,
    fs_index::{FsIndex, FsIndexBuildOptions},
    set_progress_bar,
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
    pub fn run(self, cancel_signal: &CancelSignal) -> Result<()> {
        let temp_path;

        // 1. Detect file type
        let mut file_type = self.file_type;
        if BackupFileType::Auto == self.file_type
            && let Some(ext) = self
                .input
                .as_ref()
                .and_then(|i| i.extension())
                .and_then(|ext| ext.to_str())
        {
            file_type = match ext.to_lowercase().as_str() {
                "gz" => BackupFileType::CompressedCsv,
                "csv" | "cache" => BackupFileType::UncompressedCsv,
                _ => BackupFileType::Auto,
            };
        }

        if file_type == BackupFileType::Auto {
            bail!("Failed to determine file type. Specify manually via `--file-type`.");
        }

        // 2. Open input reader (File or Stdin)
        let mut _stdin = None;
        let (input_reader, input_size): (Box<dyn Read>, Option<u64>) =
            if let Some(input) = &self.input {
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
        let cancel_reader = cancel_signal.wrap_io(input_reader);
        let pb = create_progress_bar(input_size);
        set_progress_bar(&pb);
        pb.set_message("Converting WizTree CSV to compressed QDirStat cache...");

        let pb_reader = pb.wrap_read(cancel_reader);
        let buf_reader = BufReader::new(pb_reader);

        // 3. Create target temp file with `.qdirstat.cache.gz` extension so QDirStat detects compression
        let temp_file = tempfile::Builder::new()
            .prefix("qdirstat-cache-")
            .suffix(".qdirstat.cache.gz")
            .tempfile()
            .wrap_err("Failed to create temporary compressed file for QDirStat")?;

        {
            // 4. Setup Gzip Encoder
            let gz_writer = GzEncoder::new(temp_file.as_file(), Compression::fast());
            let mut writer = BufWriter::new(gz_writer);

            // 5. Build iterator
            let records_iter: Box<dyn Iterator<Item = csv::Result<WizTreeCsvRecord>>> =
                match file_type {
                    BackupFileType::CompressedCsv => {
                        Box::new(WizTreeCsvRecord::parse_uncompressed_csv(
                            flate2::read::MultiGzDecoder::new(buf_reader),
                        ))
                    }
                    BackupFileType::UncompressedCsv => {
                        Box::new(WizTreeCsvRecord::parse_uncompressed_csv(buf_reader))
                    }
                    _ => unreachable!(),
                };

            let fs_index =
                FsIndex::try_from_csv_records(records_iter, FsIndexBuildOptions::default())?;

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
                "Converted and compressed cache size on disk: {}",
                HumanBytes(meta.len())
            );
        }

        // Convert TempFile to TempPath so the file handle is closed prior to spawning QDirStat
        temp_path = temp_file.into_temp_path();
        let file_to_open = temp_path.to_path_buf();

        // 7. Resolve `qdirstat` executable path
        let qdirstat_bin = self
            .qdirstat_path
            .unwrap_or_else(|| PathBuf::from("qdirstat"));

        log::info!("Launching QDirStat to view: {}", file_to_open.display());

        // 8. Spawn QDirStat with `--cache`
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

        // 9. Handle Process Lifecycle & Cancellation Signals
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
