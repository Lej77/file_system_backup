use std::{
    fs::File,
    io::{self, Read, Write},
    path::PathBuf,
};

use clap::Parser;
use color_eyre::eyre::{Context, bail};
use flate2::read::GzDecoder;
use indicatif::HumanBytes;

use crate::utils::{
    WindowsJob, create_progress_bar, possible_wiz_tree_paths, run_wiz_tree, wiz_tree_exe_name,
};
use crate::{BackupFileType, CancelSignal, CleanupProcess, CommonOpt, Result, set_progress_bar};

#[derive(Debug, Parser, Clone)]
pub struct WizTreeOpenOpts {
    #[clap(flatten)]
    pub common: CommonOpt,
    /// The backup file that should be opened/viewed with WizTree. If not specified
    /// then data will be read from stdin.
    #[clap(short, long)]
    pub input: Option<PathBuf>,
    /// The type of the backup file. Normally this can be guessed from the file
    /// extension.
    #[clap(long, value_enum, default_value_t = BackupFileType::Auto)]
    pub file_type: BackupFileType,
    /// Specify the path of the folder that contains the WizTree program.
    #[clap(long)]
    pub wiz_tree_path: Option<PathBuf>,
}
impl WizTreeOpenOpts {
    pub fn run(self, cancel_signal: &CancelSignal) -> Result<()> {
        let is_admin = ::is_elevated::is_elevated();
        if !is_admin {
            // If we don't have admin rights then WizTree will just start a new
            // instance of itself in the background and then exit immediately.
            // This is undesirable since we then can't cancel it or remove temp
            // files after it has exited.
            bail!(
                "WizTree requires Admin rights to open files, please start this program with elevated permissions"
            );
        }

        let mut current_job = WindowsJob::create(|limits| {
            limits.limit_kill_on_job_close();
            // Allow starting child processes that aren't associated with the
            // current job (allows us to start the cleanup process in such a way
            // that it isn't killed by the job system):
            limits.0.BasicLimitInformation.LimitFlags |=
                winapi::um::winnt::JOB_OBJECT_LIMIT_BREAKAWAY_OK;
            Ok(())
        })?;
        let mut cleanup = None;
        let mut cleanup_guard = |path: &_| {
            if cleanup.is_none() {
                cleanup = CleanupProcess::spawn(cancel_signal.clone()).wrap_err(
                    "failed to start background cleanup process that would ensure temp files are deleted",
                ).map_err(|e| {
                    log::error!("{:?}", e);
                }).ok();
            }
            if let Some(cleanup) = &cleanup {
                cleanup.guard_temp_file(path).always();
            }
        };

        let temp_path;
        let file_to_open: PathBuf;
        {
            // Guess file type:
            let mut file_type = self.file_type;
            if BackupFileType::Auto == self.file_type
                && let Some(ext) = self
                    .input
                    .as_ref()
                    .and_then(|i| i.extension())
                    .and_then(|ext| ext.to_str())
            {
                let guessed_type = match ext.to_lowercase().as_str() {
                    "gz" => Some(BackupFileType::CompressedCsv),
                    "csv" => Some(BackupFileType::UncompressedCsv),
                    _ => None,
                };
                if let Some(guessed) = guessed_type {
                    file_type = guessed;
                }
            }

            // Open input file / stdin:
            let mut stdin = None;
            /// Provide a closure type to fix type inference.
            fn open_input_fn<F>(f: F) -> F
            where
                F: for<'a> FnOnce(
                    &'a mut Option<io::Stdin>,
                ) -> Result<(Box<dyn Read + 'a>, Option<u64>)>,
            {
                f
            }
            let open_input = open_input_fn(|stdin| {
                Ok(if let Some(input) = &self.input {
                    let file = Box::new(File::open(input).wrap_err_with(|| {
                        format!(r#"failed to open input file at: "{}""#, input.display())
                    })?);
                    let size = file
                        .metadata()
                        .map_err(|e| {
                            log::error!("failed to get size of the input file: {}", e);
                        })
                        .map(|meta| meta.len())
                        .ok();
                    (file, size)
                } else {
                    *stdin = Some(io::stdin());
                    (Box::new(stdin.as_mut().unwrap().lock()), None)
                })
            });

            // Helper that creates a temp file, opens input file and then shows a
            // progress bar:
            struct MakeTempFileArgs<'a> {
                prefix: &'a str,
                info_start: &'a str,
                spinner_msg: &'a str,
                preform:
                    &'a mut dyn FnMut(&mut dyn Read, &mut tempfile::NamedTempFile) -> Result<()>,
            }
            let mut make_temp_file = |args: MakeTempFileArgs<'_>| -> Result<_> {
                // Create temp file:
                let mut temp_file = tempfile::Builder::new()
                    .prefix(args.prefix)
                    .suffix(".csv")
                    .tempfile()
                    .wrap_err("Failed to create temporary file to store WizTree data in")?;
                // Register it with the background cleanup process (to ensure cleanup):
                cleanup_guard(temp_file.path());
                {
                    // Open input file (and get its size):
                    let (mut input_data, input_size) = open_input(&mut stdin)?;

                    let pb = create_progress_bar(input_size);

                    log::info!(
                        r#"{} {} to a temporary WizTree file"#,
                        args.info_start,
                        if let Some(input) = &self.input {
                            format!(r#"input file at "{}""#, input.display())
                        } else {
                            "data from stdin".to_string()
                        }
                    );

                    set_progress_bar(&pb);
                    pb.set_message(args.spinner_msg.to_string());

                    // Read from input and write output to temp file (while showing
                    // progress and handling cancellation):
                    (args.preform)(
                        &mut pb.wrap_read(cancel_signal.wrap_io(&mut input_data)),
                        &mut temp_file,
                    )?;

                    temp_file
                        .flush()
                        .wrap_err("failed to flush data to temp file")?;

                    pb.finish();
                }
                Ok(temp_file)
            };

            match file_type {
                BackupFileType::Auto => bail!(
                    "Failed to determine the type of the backup file, please specify it manually via the `--file-type` option"
                ),
                BackupFileType::CompressedCsv => {
                    let temp_file = make_temp_file(MakeTempFileArgs {
                        prefix: "WizTree-file-index-uncompressed-",
                        info_start: "Decompressing",
                        spinner_msg: "Decompressing input data to a temporary file...",
                        preform: &mut |input, output| -> Result<()> {
                            io::copy(&mut GzDecoder::new(input), output)
                                .wrap_err("failed to decompress input data to a temporary file")?;
                            Ok(())
                        },
                    })?;
                    match temp_file.as_file().metadata() {
                        Ok(meta) => {
                            log::info!(
                                "Decompressed input data, total uncompressed size: {}",
                                HumanBytes(meta.len())
                            );
                        }
                        Err(e) => {
                            log::trace!("Failed to get size of decompressed temp file: {}", e);
                        }
                    }

                    // Shouldn't keep file open since it will be read by WizTree:
                    temp_path = temp_file.into_temp_path();
                    file_to_open = temp_path.to_path_buf();
                }
                BackupFileType::UncompressedCsv => {
                    if let Some(input) = &self.input {
                        file_to_open = input.clone();
                    } else {
                        let temp_file = make_temp_file(MakeTempFileArgs {
                            prefix: "WizTree-file-index-",
                            info_start: "Writing",
                            spinner_msg: "Writing input data to a temporary file...",
                            preform: &mut |input, output| -> Result<()> {
                                io::copy(input, output)
                                    .wrap_err("failed to write input data to a temporary file")?;
                                Ok(())
                            },
                        })?;
                        match temp_file.as_file().metadata() {
                            Ok(meta) => {
                                log::info!(
                                    "Copied input data, total temp file size: {}",
                                    HumanBytes(meta.len())
                                );
                            }
                            Err(e) => {
                                log::trace!("Failed to get size of written temp file: {}", e);
                            }
                        }

                        // Shouldn't keep file open since it will be read by WizTree:
                        temp_path = temp_file.into_temp_path();
                        file_to_open = temp_path.to_path_buf();
                    }
                }
            }
        }

        log::info!(
            "Using WizTree to view backup file. Press Ctrl-C to terminate WizTree and preform cleanup work."
        );
        // Run WizTree:
        run_wiz_tree(
            if let Some(path) = &self.wiz_tree_path {
                vec![path.join(wiz_tree_exe_name()?)]
            } else {
                possible_wiz_tree_paths()?
            },
            |command| {
                command.arg(&file_to_open);
                Ok(())
            },
            Some(&current_job),
            cancel_signal,
        )?;
        log::info!("WizTree exited successfully.");

        // WizTree has exited successfully so don't kill any programs it started
        // (like cmd.exe that can be started in folders):
        current_job.clear_kill_on_job_close()?;
        log::debug!(
            "Removed Windows Job Object limit that would kill any processes \
            that were spawned by WizTree."
        );

        Ok(())
    }
}
