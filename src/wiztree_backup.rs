use std::{
    ffi::OsString,
    fs::{self, File},
    io::{self, BufRead, BufReader, Write},
    os::windows::process::CommandExt,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use clap::Parser;
use color_eyre::{
    Help,
    eyre::{Context, Report, bail},
};
use flate2::{Compression, write::GzEncoder};
use indicatif::{HumanBytes, ProgressBar, ProgressStyle};

use crate::utils::{
    Rsyncable, TempDirPath, WindowsJob, create_file, possible_wiz_tree_paths, run_wiz_tree,
    wiz_tree_exe_name,
};
use crate::{
    CancelSignal, CleanupProcess, FileSystemType, Result, RsyncableOpts,
    logging::{CommonOpt, set_progress_bar},
};

#[derive(Debug, Parser, Clone)]
pub struct WizTreeBackupOpts {
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
    /// WizTree can write an extra line at the start of its output file asking
    /// for donations, by default this line is removed from the output. If this
    /// flag is specified then that line is NOT removed.
    #[clap(long, help_heading = "PROCESSING")]
    pub keep_donation_line: bool,
    /// Specify the path of the folder that contains the WizTree program.
    #[clap(long, help_heading = "PROCESSING")]
    pub wiz_tree_path: Option<PathBuf>,

    /// WizTree requires admin rights to preform fast MFT file scanning. If this
    /// flag is NOT passed then this program will exit with an error instead of
    /// preforming a slower scan.
    #[clap(short, long, help_heading = "PERFORMANCE")]
    pub allow_non_admin_scan: bool,
    /// By default this program will run everything with the lowest priority possible
    /// so that the backup doesn't cause performance issues for other running programs.
    /// If this flag is enabled however then everything will be preformed with the
    /// normal priority.
    #[clap(long, help_heading = "PERFORMANCE")]
    pub normal_priority: bool,
    /// Setup an in-memory file system to store temporary files written by
    /// WizTree instead of storing such files in the user's TEMP directory.
    ///
    /// There are different file system implementations to chose from:
    ///
    /// WebDAV is slow but Windows has an inbuilt client so it can be used
    /// without installing any other software. Unfortunately the inbuilt client
    /// has a max file size limit of 50MB so that will probably have to be
    /// changed before it can be used. The inbuilt client also seems to write
    /// all data to a temporary file before uploading it which doesn't make this
    /// temporary file system very useful.
    ///
    /// WinFsp requires installing a driver before it can be used.
    #[clap(value_enum, long, help_heading = "PERFORMANCE")]
    pub temp_file_system: Option<FileSystemType>,

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
impl WizTreeBackupOpts {
    pub fn run(self, cancel_signal: &CancelSignal) -> Result<()> {
        let is_admin = ::is_elevated::is_elevated();
        if !is_admin {
            if self.allow_non_admin_scan {
                log::info!(
                    "Program doesn't have Admin rights but is continuing anyway with a slower scan."
                );
            } else {
                bail!(
                    "WizTree requires Admin rights to preform fast MFT file scanning, pass the `--allow-non-admin-scan` flag to allow slower scanning or start this program with elevated permissions"
                );
            }
        }

        let _low_priority_guard = if self.normal_priority {
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

        // Need to create job before starting cleanup process. The current process could
        // already be in a job and it might not have the `JOB_OBJECT_LIMIT_BREAKAWAY_OK`
        // limit enabled. Creating a nested job solves that issue.
        // For example `cargo` (at least version 1.55.0) creates such a job when
        // using `cargo run` which causes us to fail to start the child process
        // unless we create this job first.
        let current_job = WindowsJob::create(|limits| {
            limits.limit_kill_on_job_close();
            // Allow starting child processes that aren't associated with the
            // current job (allows us to start the cleanup process in such a way
            // that it isn't killed by the job system):
            limits.0.BasicLimitInformation.LimitFlags |=
                winapi::um::winnt::JOB_OBJECT_LIMIT_BREAKAWAY_OK;
            Ok(())
        })?;
        let cleanup = CleanupProcess::spawn(cancel_signal.clone()).wrap_err(
            "failed to start background cleanup process that would ensure temp files are deleted",
        ).map_err(|e| {
            log::error!("{:?}", e);
        }).ok();

        let mut temp_symlink_guard: Option<TempDirPath> = None;
        let mut file_system_mounted_at: Option<PathBuf> = None;
        #[cfg(feature = "winfsp")]
        let mut win_fsp_host;
        #[cfg(feature = "web_dav")]
        let tokio_rt;
        if let Some(temp_file_system) = self.temp_file_system {
            #[cfg(any(feature = "winfsp", feature = "web_dav"))]
            let expose_at = {
                let s: String = std::iter::repeat_with(fastrand::alphanumeric)
                    .take(10)
                    .collect();
                std::env::temp_dir().join(format!("WizTree-file-index-export-{s}"))
            };

            match temp_file_system {
                FileSystemType::WinFsp => {
                    #[cfg(feature = "winfsp")]
                    {
                        winfsp::winfsp_init()
                            .map_err(|e| {
                                let report = color_eyre::eyre::eyre!("{e:?}\n{e}");
                                if let winfsp::FspError::WIN32(1285) = e {
                                    report.wrap_err(
                                        "The error code corresponds to ERROR_DELAY_LOAD_FAILED \
                                        which means we failed to load WinFsp's dynamically linked \
                                        library (.dll), make sure that WinFsp is correctly \
                                        installed.",
                                    )
                                } else {
                                    report
                                }
                            })
                            .wrap_err("Failed to initialize WinFsp")
                            .suggestion("Check that WinFsp is install correctly, you can get it at https://github.com/winfsp/winfsp")?;
                        let cx: crate::winfsp_memfs::WinFspMemFsContext =
                            crate::winfsp_memfs::WinFspMemFsContext::new();
                        win_fsp_host = crate::winfsp_memfs::WinFspMemFs::create_host(cx)
                            .wrap_err("Failed to create WinFsp MemFs file system host")?;
                        win_fsp_host
                            .fs
                            .mount(&expose_at)
                            .map_err(|e| {
                                color_eyre::eyre::eyre!("{} (HRESULT {})", e.message(), e.code())
                            })
                            .wrap_err_with(|| {
                                format!(
                                    "Failed to mount WinFsp file system at {}",
                                    expose_at.display()
                                )
                            })?;
                        log::info!(
                            "Exposed temporary WinFsp file system at {}",
                            expose_at.display()
                        );
                        win_fsp_host
                            .fs
                            .start()
                            .wrap_err("Failed to start WinFsp file system")?;
                        file_system_mounted_at = Some(expose_at.clone());
                    }
                    #[cfg(not(feature = "winfsp"))]
                    {
                        bail!("Program was compiled without WinFsp support");
                    }
                }
                FileSystemType::WebDav => {
                    #[cfg(feature = "web_dav")]
                    {
                        tokio_rt = tokio::runtime::Runtime::new()
                            .wrap_err("Failed to start async runtime")?;
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        let tokio_handle = tokio_rt.handle().clone();
                        tokio_rt.spawn(async move {
                            let addr = (std::net::IpAddr::from([127, 0, 0, 1]), 0_u16);
                            let dav_server = dav_server::DavHandler::builder()
                                // .filesystem(dav_server::localfs::LocalFs::new("C:/temp-webdav", true, true, false))
                                .filesystem(crate::webdav_memfs::MemFs::new())
                                .locksystem(dav_server::fakels::FakeLs::new())
                                .build_handler();

                            let listener = match tokio::net::TcpListener::bind(addr).await {
                                Ok(v) => v,
                                Err(e) => {
                                    log::error!("Failed to create TCP listener at {addr:?}: {e}");
                                    return;
                                }
                            };
                            let addr = match listener.local_addr() {
                                Ok(addr) => addr,
                                Err(e) => {
                                    log::error!("Failed to determine address where TCP listener was bound: {e}");
                                    return;
                                }
                            };
                            log::info!("WebDAV in-memory file system served on http://{addr:?}");
                            tx.send(addr).unwrap();
                            // When using `net use * http://127.0.0.1:51515` command, the output will be something like
                            // Drive Q: is now connected to http://127.0.0.1:51515.
                            //
                            // The command completed successfully.
                            //
                            // OR (when unsuccessful):
                            // There are no available drive letters left.
                            //
                            // More help is available by typing NET HELPMSG 3920.
                            while let Ok((stream, from)) = listener.accept().await {
                                let dav_server = dav_server.clone();
                                let service = hyper::service::service_fn(move |req| {
                                    let dav_server = dav_server.clone();
                                    async move {
                                        Ok::<_, std::convert::Infallible>(
                                            dav_server.handle(req).await,
                                        )
                                    }
                                });

                                tokio_handle.spawn(async move {
                                    let result = hyper_util::server::conn::auto::Builder::new(
                                        hyper_util::rt::TokioExecutor::new(),
                                    )
                                    .serve_connection_with_upgrades(
                                        hyper_util::rt::TokioIo::new(stream),
                                        service,
                                    ).await;
                                    if let Err(e) = result {
                                        log::error!("Failed to serve WebDAV request from {from}: {e}");
                                    }
                                });
                            }
                        });

                        let addr = rx
                            .blocking_recv()
                            .wrap_err("failed to wait for WebDAV server to start")?;

                        std::os::windows::fs::symlink_dir(
                            format!("//127.0.0.1@{}/DavWWWRoot", addr.port()),
                            &expose_at,
                        )
                        .wrap_err_with(|| {
                            format!(
                                "Failed to expose WebDAV server as folder at \"{}\"",
                                expose_at.display()
                            )
                        })?;
                        log::info!(
                            "Exposed temporary WebDAV file system at {}",
                            expose_at.display()
                        );
                        temp_symlink_guard = Some(TempDirPath::from_path(&expose_at));
                        if let Some(cleanup) = &cleanup {
                            cleanup.guard_temp_dir(&expose_at).always();
                        }
                        file_system_mounted_at = Some(expose_at.clone());
                    }
                    #[cfg(not(feature = "web_dav"))]
                    {
                        bail!("Program was compiled without WebDAV support");
                    }
                }
            }
        }

        let mut temp_path_guard = None;
        let temp_file_system_path;
        let temp_path = if let Some(symlink) = &file_system_mounted_at {
            temp_file_system_path = symlink.join("WizTree-file-index-export.csv");
            drop(File::create(&temp_file_system_path).wrap_err_with(|| {
                format!(
                    "failed to create temporary file in WebDAV file system at: {}",
                    temp_file_system_path.display()
                )
            })?);
            &*temp_file_system_path
        } else {
            let tempfile = tempfile::Builder::new()
                .prefix("WizTree-file-index-export-")
                .suffix(".csv")
                .tempfile()
                .wrap_err("Failed to create temporary file for WizTree program")?;
            let temp_path = &**temp_path_guard.insert(tempfile.into_temp_path());
            log::debug!("Created temporary file at: {}", temp_path.display());
            if let Some(cleanup) = &cleanup {
                cleanup.guard_temp_file(temp_path).always()
            }
            temp_path
        };

        let mut output_path = self.output.clone();
        let stdout_handle;
        let mut stdout_guard;
        let mut output_file = None;
        let mut has_output_file = false;
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
            has_output_file = true;
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

        log::info!(
            r#"Using WizTree to scan "{}" and writing the {}results to {}"#,
            self.scan_path.display(),
            if self.uncompressed { "" } else { "compressed " },
            output_msg,
        );
        log::debug!(
            r#"Note that WizTree will write its output to a temporary file at "{}" which will then be copied to the final output file"#,
            temp_path.display()
        );

        {
            // Prepare arguments:
            let mut output_path_arg = OsString::new();
            output_path_arg.push("/export=");
            output_path_arg.push(temp_path);

            let pb = ProgressBar::new_spinner();
            set_progress_bar(&pb);
            pb.set_style(
                ProgressStyle::default_spinner()
                    .template("{spinner:.blue} [{elapsed_precise}] {msg} {bytes} ({bytes_per_sec})")
                    .unwrap(),
            );
            pb.set_message("Waiting for WizTree to export file system info to a temporary file...");

            let pb_signal = cancel_signal.new_child_signal().cancel_on_drop();
            let pb_thread = if pb.is_hidden() {
                log::trace!("Progress spinner is disabled");
                None
            } else {
                Some(thread::spawn({
                    let pb_signal = pb_signal.clone();
                    let temp_path = temp_path.to_path_buf();
                    move || {
                        let mut last_update = Instant::now();
                        let is_hidden = pb.is_hidden();

                        while let Ok(()) = pb_signal.wait_timeout(Duration::from_millis(25)) {
                            if !is_hidden && last_update.elapsed() > Duration::from_millis(500) {
                                match fs::metadata(&temp_path) {
                                    Ok(data) => {
                                        let previous = pb.position();
                                        let len = data.len();
                                        pb.set_position(len);
                                        if previous == 0 && len != 0 {
                                            // It takes a while before WizTree starts writing any data:
                                            pb.reset_eta();
                                        }
                                    }
                                    Err(e) => log::trace!(
                                        "Can't get file information about temporary WizTree file: {}",
                                        e
                                    ),
                                }
                                last_update = Instant::now();
                            }
                            pb.tick();
                        }
                    }
                }))
            };

            // Run WizTree:
            run_wiz_tree(
                if let Some(path) = &self.wiz_tree_path {
                    vec![path.join(wiz_tree_exe_name()?)]
                } else {
                    possible_wiz_tree_paths()?
                },
                {
                    |command| {
                        // CLI arguments info:
                        // https://www.diskanalyzer.com/guide
                        command
                            .arg(&self.scan_path)
                            .arg(&output_path_arg)
                            .arg("/exportUTCTime=1");
                        if !self.normal_priority {
                            // This sets CPU priority to low and IO priority will be inherited from this process if we are in background mode.
                            command.creation_flags(u32::from(
                                thread_priority::process::ProcessPriority::IDLE_PRIORITY_CLASS,
                            ));
                        }
                        Ok(())
                    }
                },
                Some(&current_job),
                cancel_signal,
            )?;

            drop(pb_signal);
            if let Some(pb_thread) = pb_thread {
                pb_thread.join().unwrap();
            }

            log::info!(
                "WizTree exited successfully{}",
                match std::fs::metadata(temp_path) {
                    Ok(meta) => format!(
                        " after writing {} of file system information to a temporary file",
                        HumanBytes(meta.len()),
                    ),
                    Err(e) => {
                        log::trace!("failed to get size of temporary WizTree file: {}", e);
                        "".to_string()
                    }
                }
            );
        }

        // Scope where we re-open the temporary file that WizTree wrote to:
        {
            let mut temp_file = File::open(temp_path).map_err(|e| {
                let web_dav_issue = temp_symlink_guard.is_some() && matches!(e.raw_os_error(), Some(223));
                let e = Report::new(e).wrap_err(format!(
                    r#"Failed to reopen temporary output file at "{}""#,
                    temp_path.display()
                ));
                if web_dav_issue {
                    e.with_note(||
                        "WebDAV doesn't seem to support files larger than 50MB on Windows by default, \
                        see: https://sharepoint.stackexchange.com/questions/119302/error-0x800700df-the-file-size-exceeds-the-limit-allowed-and-cannot-be-saved"
                    )
                } else {
                    e
                }
            })?;

            let pb = ProgressBar::new(temp_file.metadata().wrap_err("failed to get size of the temporary file that WizTree wrote its results to")?.len());
            pb.set_style(ProgressStyle::default_bar()
                .template("{spinner:.green} [{elapsed_precise}] {wide_bar:.cyan/blue} {bytes}/{total_bytes} ({bytes_per_sec}, {percent}%, ETA: {eta})").unwrap());

            let compression_level = self
                .compression
                .map(Compression::new)
                .unwrap_or_else(Compression::best);
            // If writing to stdout then track output size manually:
            let track_output_size =
                (!has_output_file && !self.uncompressed).then(ProgressBar::hidden);
            {
                let mut output_size_tracker;
                let mut rsyncable;
                let mut encoder = None;
                {
                    let output: &mut dyn Write = if self.uncompressed {
                        &mut *output
                    } else {
                        let output: &mut dyn Write = if let Some(tracker) = &track_output_size {
                            output_size_tracker = tracker.wrap_write(&mut *output);
                            &mut output_size_tracker
                        } else {
                            &mut *output
                        };

                        encoder = Some(GzEncoder::new(output, compression_level));
                        let encoder = encoder.as_mut().unwrap();
                        if self.rsyncable.rsyncable {
                            rsyncable = Rsyncable::new(encoder);
                            &mut rsyncable
                        } else {
                            encoder
                        }
                    };

                    if self.uncompressed {
                        log::info!(
                            r#"Copying WizTree's output to {}"#,
                            if let Some(path) = &output_path {
                                format!(r#"the final output file at "{}""#, path.display())
                            } else {
                                "stdout".to_string()
                            }
                        );
                    } else {
                        log::info!(
                            r#"Compressing WizTree's output and writing it to {}"#,
                            output_msg
                        );
                    }

                    let mut reader = cancel_signal.wrap_io(BufReader::new(&mut temp_file));

                    (|| -> Result<()> {
                        let pb_tracker = ProgressBar::hidden();
                        if !self.keep_donation_line {
                            let mut buffer = vec![];

                            // Can't show progress bar yet since we might need to log some things:
                            pb_tracker.wrap_read(&mut reader).read_until(b'\n', &mut buffer)?;

                            match std::str::from_utf8(&buffer) {
                                Ok(line) => {
                                    if line.contains("hide this message by making a donation") {
                                        log::info!("Removed line about making a donation in WizTree's output file.");
                                        buffer.clear();
                                    }
                                }
                                Err(e) => {
                                    log::warn!(
                                        r#"First line of WizTree's export file isn't valid UTF8 ({}), first line: "{}""#,
                                        e,
                                        String::from_utf8_lossy(&buffer),
                                    );
                                }
                            }
                            if !buffer.is_empty() {
                                // First line wasn't about donations, so write it like normal:
                                io::copy(&mut &*buffer, output)?;
                            }
                        }
                        // Update progress bar with any previous reads, this will also show the progress bar (don't log anything after this):
                        set_progress_bar(&pb);
                        pb.inc(pb_tracker.position());
                        io::copy(&mut pb.wrap_read(reader), output)?;
                        Ok(())
                    })()
                    .wrap_err_with(|| {
                        format!(
                            "Failed to {} results from temporary WizTree file to the final output file",
                            if self.uncompressed {
                                "copy"
                            } else {
                                "compress"
                            }
                        )
                    })?;
                }

                if let Some(encoder) = encoder {
                    encoder
                        .finish()
                        .wrap_err("Failed to finish compressing output")?;
                } else {
                    drop(encoder);

                    output.flush().wrap_err("Failed to flush output")?;
                }
            }
            if self.uncompressed {
                pb.finish_with_message("copied");
            } else {
                pb.finish_with_message("compressed");
                let print_size = |size| {
                    log::info!(
                        "Finished compressing WizTree data with compression level {}, final output size is {}",
                        compression_level.level(),
                        HumanBytes(size),
                    );
                };
                if let Some(meta) = output_file.as_ref().and_then(|file| file.metadata().ok()) {
                    print_size(meta.len());
                } else if let Some(tracker) = &track_output_size {
                    print_size(tracker.position());
                }
            }
        }

        if let Some(temp_guard) = temp_path_guard {
            temp_guard
                .close()
                .wrap_err("Failed to delete temporary file")?;
        }
        if let Some(temp_guard) = temp_symlink_guard {
            temp_guard
                .close()
                .wrap_err("Failed to delete temporary symlink to WebDAV file system server")?;
        }

        Ok(())
    }
}
