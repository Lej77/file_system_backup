//! Mount a fake filesystem where files from a backup can be viewed.

#[cfg(windows)]
use std::path::{Component, Prefix};
use std::{
    ffi::OsStr,
    fs::File,
    io::{self, Read},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

use clap::Parser;
use color_eyre::{
    Report, Section,
    eyre::{OptionExt, WrapErr, bail, eyre},
};
use flate2::{Compression, write::GzEncoder};
use indicatif::HumanBytes;

use crate::{
    BackupFileType, CancelSignal, CommonOpt, FileSystemType, Result, WizTreeCsvRecord,
    fs_index::{
        DEFAULT_PATH_SEPARATOR, FsCursor, FsEntryMetadata, FsIndex, FsIndexBuildOptions,
        PATH_SEPARATORS,
    },
    set_progress_bar,
    utils::{TempDirPath, create_progress_bar},
};
#[cfg(windows)]
use crate::{CleanupProcess, utils::WindowsJob};

#[cfg(feature = "web_dav")]
pub mod web_dav_mount;

#[derive(Clone)]
enum CompressibleFsIndex {
    Compressed {
        data: Arc<[u8]>,
        index: Weak<FsIndex>,
    },
    Available {
        data: Weak<[u8]>,
        index: Arc<FsIndex>,
    },
}
impl CompressibleFsIndex {
    pub fn compressed_data(&mut self) -> &Arc<[u8]> {
        match self {
            Self::Available { data, index } => {
                let data = if let Some(data) = data.upgrade() {
                    data
                } else {
                    Arc::from(
                        WizTreeCsvRecord::create_compressed_csv(
                            index.csv_iter(None, DEFAULT_PATH_SEPARATOR, false),
                        )
                        .expect("failed to serialize and compress filesystem index"),
                    )
                };
                *self = Self::Compressed {
                    data,
                    index: Arc::downgrade(index),
                };
                if let Self::Compressed { data, .. } = self {
                    data
                } else {
                    unreachable!()
                }
            }
            Self::Compressed { data, .. } => data,
        }
    }
    pub fn decompressed_index(&mut self) -> &Arc<FsIndex> {
        match self {
            CompressibleFsIndex::Available { index, .. } => index,
            CompressibleFsIndex::Compressed { data, index } => {
                let index = if let Some(index) = index.upgrade() {
                    index
                } else {
                    Arc::new(FsIndex::from_csv_records(
                        WizTreeCsvRecord::parse_compressed_csv(data).map(Result::unwrap),
                        FsIndexBuildOptions::default(),
                    ))
                };
                *self = Self::Available {
                    data: Arc::downgrade(data),
                    index,
                };
                if let Self::Available { index, .. } = self {
                    index
                } else {
                    unreachable!()
                }
            }
        }
    }
}

/// Shared file index data.
struct AutoCompressedFsIndexSharedState {
    /// Info for the root node.
    root: WizTreeCsvRecord,
    /// Cached decompressed info.
    cache: Mutex<CompressibleFsIndex>,
}

/// Contains all info from a backup and allows easy access to info about
/// individual files or folders.
#[derive(Clone)]
pub struct AutoCompressedFsIndex(Arc<AutoCompressedFsIndexSharedState>);
impl AutoCompressedFsIndex {
    /// Create a file index from a Gzip compressed CSV file.
    ///
    /// # Errors
    ///
    /// If the data can't be decompressed or the decompressed data doesn't
    /// contain the expected CSV data.
    pub fn from_compressed(data: Arc<[u8]>) -> crate::Result<Self> {
        let mut root: Option<WizTreeCsvRecord> = None;
        // Scan for errors:
        // TODO(perf): do error checking concurrently with constructing the cache
        for (i, res) in WizTreeCsvRecord::parse_compressed_csv(&data).enumerate() {
            match res {
                // For folders:
                Ok(record) if record.file_name.ends_with(PATH_SEPARATORS) => {
                    // Shortest path is the root node:
                    let prev_len = root
                        .as_ref()
                        .map(|root| root.file_name.len())
                        .unwrap_or(usize::MAX);
                    if record.file_name.len() < prev_len {
                        root = Some(record);
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    return Err(
                        Report::new(e).wrap_err(format!("Failed to parse backup entry #{}", i + 1))
                    );
                }
            }
        }
        let root = root.ok_or_else(|| Report::msg("No info for root node found"))?;

        let cache = FsIndex::from_csv_records_with_root(
            WizTreeCsvRecord::parse_compressed_csv(&data).map(Result::unwrap),
            &root,
            FsIndexBuildOptions::default(),
        );
        {
            log::info!(
                "Fully loaded file system index size is {} times the size of the compressed CSV data",
                cache.estimated_size() as f64 / (data.len() as f64)
            );
            let perfect_recreation = Self::check_for_perfect_recreation_from_cache(
                &cache,
                WizTreeCsvRecord::parse_compressed_csv(&data).map(Result::unwrap),
            );
            if perfect_recreation {
                log::debug!(
                    "Cache can be used to perfectly recreate all CSV records, so it is likely correct!"
                );
            }
        }

        Ok(Self(Arc::new(AutoCompressedFsIndexSharedState {
            root,
            cache: Mutex::new(CompressibleFsIndex::Available {
                data: Arc::downgrade(&data),
                index: Arc::new(cache),
            }),
        })))
    }

    fn check_for_perfect_recreation_from_cache(
        cache: &FsIndex,
        mut real_csv_records: impl Iterator<Item = WizTreeCsvRecord>,
    ) -> bool {
        let mut cached_iter = cache.csv_iter(None, DEFAULT_PATH_SEPARATOR, false);
        let mut perfect_recreation = true;
        for index in 0.. {
            match (real_csv_records.next(), cached_iter.next()) {
                (None, None) => break,
                (None, Some(_)) => {
                    log::warn!("Too many CSV record from cache");
                    perfect_recreation = false;
                    break;
                }
                (Some(_), None) => {
                    log::warn!("Too few CSV record from cache");
                    perfect_recreation = false;
                    break;
                }
                (Some(real), Some(cached)) => {
                    if real != cached {
                        log::warn!(
                            "Reconstructed CSV record from cache doesn't match at index {index}:\nExpected: {real:?}\nActual:   {cached:?}"
                        );
                        perfect_recreation = false;
                        break;
                    }
                }
            }
        }
        perfect_recreation
    }

    fn fs_index(&self) -> Arc<FsIndex> {
        let mut guard = self.0.cache.lock().unwrap();
        guard.decompressed_index().clone()
    }

    /// Request that the filesystem index is compressed to take up less space.
    pub fn compress(&self) {
        let mut guard = self.0.cache.lock().unwrap();
        guard.compressed_data();
    }

    /// The last path segment of the root folder's path. `None` if root path is
    /// `/`.
    pub fn root_name(&self) -> Option<&str> {
        self.0
            .root
            .file_name
            .split(PATH_SEPARATORS)
            .rfind(|seg| !seg.is_empty())
    }
    pub fn root(&self) -> &WizTreeCsvRecord {
        &self.0.root
    }

    /// Get info for a directory. Returns info about the folder and call the
    /// `on_child` callback for each direct child of the folder.
    pub fn get_directory_info(
        &self,
        folder_path: &str,
        mut on_child: impl FnMut(&str, FsEntryMetadata),
    ) -> Option<FsEntryMetadata> {
        log::trace!("Lookup folder at path \"{folder_path}\"");

        let index = self.fs_index();
        let mut cursor = FsCursor::new('/');
        if let Some(root) = index.root() {
            cursor.set_root(root, Some("/"), &index);
        }
        if !cursor.go_to_path(folder_path, &index) {
            return None;
        }
        let entry = cursor
            .current_id()
            .expect("cursor has a root entry")
            .load_metadata(&index)
            .expect("all filesystem entries should have metadata");
        let info = entry.metadata().clone();

        let children = cursor.current_folder_children(&index)?; // early return if entry is not a folder
        for child in children {
            on_child(
                child.file_name(&index),
                child
                    .load_metadata(&index)
                    .expect("all filesystem entries should have metadata")
                    .metadata()
                    .clone(),
            )
        }

        Some(info)
    }

    /// Get metadata for a file or folder
    pub fn get_metadata(&self, name: &str, find_dir: Option<bool>) -> Option<FsEntryMetadata> {
        log::trace!("Lookup file at path \"{name}\"   (it should be a directory: {find_dir:?})");

        let index = self.fs_index();
        let mut cursor = FsCursor::new('/');
        if let Some(root) = index.root() {
            cursor.set_root(root, Some("/"), &index);
        }
        if !cursor.go_to_path(name, &index) {
            return None;
        }
        let entry = cursor
            .current_id()
            .expect("cursor has a root entry")
            .load_metadata(&index)
            .expect("all filesystem entries should have metadata");
        let info = entry.metadata().clone();

        if matches!(find_dir, Some(find_dir) if find_dir != info.is_dir) {
            return None;
        }

        Some(info)
    }
}

#[derive(Debug, Parser, Clone)]
pub struct MountOpts {
    #[clap(flatten)]
    pub common: CommonOpt,
    /// The backup file that should be mounted as a file system.
    #[clap(short, long)]
    pub input: Option<PathBuf>,
    /// Read the backup data from stdin.
    #[clap(
        long,
        required_unless_present = "input",
        requires = "file_type",
        conflicts_with = "input"
    )]
    pub stdin: bool,
    /// The type of the backup file. Normally this can be guessed from the file
    /// extension.
    #[clap(long, value_enum, default_value_t = BackupFileType::Auto)]
    pub file_type: BackupFileType,

    /// Chose what file system implementation to use:
    ///
    /// WebDAV has inbuilt support in Windows so it can be used without
    /// installing any other software.
    ///
    /// WinFsp requires installing a driver before it can be used.
    #[clap(value_enum, long, default_value_t = FileSystemType::WebDav)]
    pub file_system: FileSystemType,
    /// Select where the file system will be mounted. Can be a drive letter like
    /// `X:` or a folder path like `../my-backup`.
    ///
    /// If not specified then the file system will be mounted to the first free
    /// drive letter.
    #[clap(short, long)]
    pub mount_point: Option<PathBuf>,
}
impl MountOpts {
    pub fn run(self, cancel_signal: &CancelSignal) -> Result<()> {
        let mut file_type = self.file_type;
        if BackupFileType::Auto == self.file_type
            && let Some(input) = &self.input
            && let Some(ext_file_type) = BackupFileType::from_file_path_ext(input)
        {
            file_type = ext_file_type;
        }
        if let BackupFileType::Auto = file_type {
            bail!(
                "Failed to determine the type of the backup file, \
                please specify it manually via the `--file-type` option"
            )
        }

        let (mut input, input_size) = if let Some(input) = &self.input {
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
            (file as Box<dyn Read>, size)
        } else {
            (Box::new(io::stdin().lock()) as Box<dyn Read>, None)
        };

        let compressed_data: Vec<u8> = match file_type {
            BackupFileType::Auto => unreachable!("checked for this previously"),
            BackupFileType::CompressedCsv => {
                let mut data = Vec::new();
                input
                    .read_to_end(&mut data)
                    .wrap_err("Failed to read input data")?;
                data
            }
            BackupFileType::UncompressedCsv => {
                let mut data = Vec::new();
                let pb = create_progress_bar(input_size);
                set_progress_bar(&pb);
                pb.set_message("Compressing backup data");
                io::copy(
                    &mut pb.wrap_read(cancel_signal.wrap_io(&mut input)),
                    &mut GzEncoder::new(&mut data, Compression::best()),
                )
                .wrap_err("Failed to read uncompressed input and compress it in memory")?;
                pb.finish();
                data
            }
        };
        log::info!(
            "Loaded compressed backup data into memory: {}",
            HumanBytes(compressed_data.len() as u64)
        );

        let file_index = AutoCompressedFsIndex::from_compressed(compressed_data.into())
            .wrap_err("Failed to create file index from compressed backup data")?;
        log::trace!("Top directories: {:?}", {
            let mut entries = Vec::new();
            file_index.get_directory_info("/", |name, _| {
                entries.push(name.to_string());
            });
            entries
        });

        let mut _temp_symlink_guard: Option<TempDirPath> = None;
        let mut _temp_mounted_drive: Option<MountedDrive> = None;
        #[cfg(feature = "web_dav")]
        let tokio_rt;

        #[cfg(windows)]
        let mut cleanup = None;
        #[cfg(windows)]
        let mut cleanup = || {
            cleanup.get_or_insert_with(|| {
                // Need to create job before starting cleanup process. The current process could
                // already be in a job and it might not have the `JOB_OBJECT_LIMIT_BREAKAWAY_OK`
                // limit enabled. Creating a nested job solves that issue.
                // For example `cargo` (at least version 1.55.0) creates such a job when
                // using `cargo run` which causes us to fail to start the child process
                // unless we create this job first.
                WindowsJob::create(|limits| {
                    // Allow starting child processes that aren't associated with the
                    // current job (allows us to start the cleanup process in such a way
                    // that it isn't killed by the job system):
                    limits.0.BasicLimitInformation.LimitFlags |=
                        winapi::um::winnt::JOB_OBJECT_LIMIT_BREAKAWAY_OK;
                    Ok(())
                }).map_err(|e| {
                    log::error!("Failed to create Windows Job in order to start cleanup process: {e}");
                }).ok()?;

                CleanupProcess::spawn(cancel_signal.clone()).wrap_err(
                "failed to start background cleanup process that would ensure temp files are deleted",
            ).map_err(|e| {
                log::error!("{:?}", e);
            }).ok()}).as_ref().map(Arc::clone)
        };

        match self.file_system {
            #[cfg(windows)]
            FileSystemType::WinFsp => {
                bail!("WinFsp file system support hasn't been implemented yet")
            }
            FileSystemType::WebDav => {
                #[cfg(not(feature = "web_dav"))]
                {
                    bail!("Program was compiled without WebDAV support");
                }
                #[cfg(feature = "web_dav")]
                {
                    let file_system = web_dav_mount::FileSystem {
                        index: Arc::new(file_index),
                    };

                    tokio_rt =
                        tokio::runtime::Runtime::new().wrap_err("Failed to start async runtime")?;
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let tokio_handle = tokio_rt.handle().clone();
                    tokio_rt.spawn(async move {
                        let addr = (std::net::IpAddr::from([127, 0, 0, 1]), 0_u16);

                        let dav_server = dav_server::DavHandler::builder()
                            // .filesystem(dav_server::localfs::LocalFs::new("C:/temp-webdav", true, true, false))
                            .filesystem(Box::new(file_system))
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
                                log::error!(
                                    "Failed to determine address where TCP listener was bound: {e}"
                                );
                                return;
                            }
                        };
                        log::info!("WebDAV in-memory file system served on http://{addr:?}");
                        tx.send(addr).unwrap();

                        while let Ok((stream, from)) = listener.accept().await {
                            let dav_server = dav_server.clone();
                            let service = hyper::service::service_fn(move |req| {
                                let dav_server = dav_server.clone();
                                async move {
                                    Ok::<_, std::convert::Infallible>(dav_server.handle(req).await)
                                }
                            });

                            tokio_handle.spawn(async move {
                                let builder = hyper_util::server::conn::auto::Builder::new(
                                    hyper_util::rt::TokioExecutor::new(),
                                );

                                let result = builder
                                    .serve_connection_with_upgrades(
                                        hyper_util::rt::TokioIo::new(stream),
                                        service,
                                    )
                                    .await;
                                if let Err(e) = result {
                                    log::error!("Failed to serve WebDAV request from {from}: {e}");
                                }
                            });
                        }
                    });

                    let addr = rx
                        .blocking_recv()
                        .wrap_err("failed to wait for WebDAV server to start")?;

                    #[cfg(windows)]
                    let mount_now = || -> Result<()> {
                        if let Some(mount_point) = self.mount_point {
                            'check_drive_letter: {
                                let mut iter = mount_point.components();
                                if let (Some(Component::Prefix(prefix)), None) =
                                    (iter.next(), iter.next())
                                {
                                    let disk_letter = match prefix.kind() {
                                        Prefix::Verbatim(_) | Prefix::DeviceNS(_) => {
                                            break 'check_drive_letter;
                                        }
                                        Prefix::VerbatimUNC(_, _) | Prefix::UNC(_, _) => {
                                            bail!("UNC paths are not valid mount points")
                                        }
                                        Prefix::VerbatimDisk(v) | Prefix::Disk(v) => v as char,
                                    };
                                    let expose_at = format!("http://127.0.0.1:{}", addr.port());
                                    _temp_mounted_drive = Some(mount_network_path_to_drive(
                                        expose_at,
                                        Some(disk_letter),
                                    )?);
                                    log::info!(
                                        "Exposed WebDAV file system under the drive letter: \"{}:/\"",
                                        _temp_mounted_drive.as_ref().unwrap().letter()
                                    )
                                }
                            }
                            if _temp_mounted_drive.is_none() {
                                let network_path =
                                    format!("//127.0.0.1@{}/DavWWWRoot", addr.port());
                                std::os::windows::fs::symlink_dir(network_path, &mount_point)
                                    .wrap_err_with(|| {
                                        format!(
                                            "Failed to expose WebDAV server as folder at \"{}\"",
                                            mount_point.display()
                                        )
                                    })?;
                                log::info!(
                                    "Exposed WebDAV file system at {}",
                                    mount_point.display()
                                );
                                _temp_symlink_guard = Some(TempDirPath::from_path(&mount_point));
                                if let Some(cleanup) = cleanup() {
                                    cleanup.guard_temp_dir(&mount_point).always();
                                }
                            }
                        } else {
                            let expose_at = format!("http://127.0.0.1:{}", addr.port());
                            _temp_mounted_drive =
                                Some(mount_network_path_to_drive(expose_at, None)?);
                            log::info!(
                                "Exposed WebDAV file system as first free drive letter: \"{}:/\"",
                                _temp_mounted_drive.as_ref().unwrap().letter()
                            )
                        }
                        // TODO: tell cleanup process to unmount drive.
                        Ok(())
                    };
                    #[cfg(windows)]
                    if let Err(e) = mount_now() {
                        log::error!("Failed to mount WebDAV file system: {e:?}");
                    }
                    #[cfg(not(windows))]
                    {
                        log::info!("Exposed WebDAV file system at {:?}", addr);
                    }
                }
            }
        }

        log::info!("Press Ctrl+C to unmount the backup and exit");
        loop {
            // TODO: support pressing Enter to make a newline on stdin and exit.
            cancel_signal.wait_timeout(Duration::from_millis(1000000))?;
        }
    }
}

#[must_use = "Drive will be unmounted directly if this isn't used"]
pub struct MountedDrive(Option<char>);
impl MountedDrive {
    pub fn letter(&self) -> char {
        self.0.unwrap()
    }
}
impl Drop for MountedDrive {
    fn drop(&mut self) {
        let Some(drive) = self.0 else { return };
        if let Err(e) = unmount_network_path_from_drive(drive) {
            log::error!("Failed to unmount drive \"{drive}:\": {e}");
        }
    }
}

pub fn mount_network_path_to_drive(
    network_path: impl AsRef<OsStr>,
    drive: Option<char>,
) -> Result<MountedDrive> {
    // When using `net use * http://127.0.0.1:51515` command, the output will be something like
    // Drive Q: is now connected to http://127.0.0.1:51515.
    //
    // The command completed successfully.
    //
    // OR (when unsuccessful):
    // There are no available drive letters left.
    //
    // More help is available by typing NET HELPMSG 3920.
    //
    let output = Command::new("net")
        .arg("use")
        .arg(match drive {
            Some(letter) => format!("{letter}:"),
            None => "*".to_string(),
        })
        .arg(network_path.as_ref())
        .stdin(Stdio::null())
        .output()
        .wrap_err("Failed to run \"net use\" command")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        return Err(Report::msg(format!(
            "\"net use\" command exited with an error when mounting the path \"{}\" to drive \"{}\" (code: {:?})",
            network_path.as_ref().to_string_lossy(),
            drive.unwrap_or('*'),
            output.status.code()
        ))
        .section(format!(
            "Stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ))
        .section(format!("Stdout:\n{stdout}",)));
    }
    let first_line = stdout
        .lines()
        .next()
        .ok_or_else(|| eyre!("Expected \"net use\" command to print info about what drive it mounted the network path to"))?;

    if !first_line.starts_with("Drive ") {
        bail!("Expected first line to start with \"Drive \"");
    }
    let letter = &first_line["Drive ".len()..]
        .chars()
        .next()
        .ok_or_eyre("Expected drive letter in stdout")?;

    Ok(MountedDrive(Some(*letter)))
}

pub fn unmount_network_path_from_drive(drive: char) -> Result<()> {
    let output = Command::new("net")
        .arg("use")
        .arg(format!("{drive}:"))
        .arg("/delete")
        .stdin(Stdio::null())
        .output()
        .wrap_err("Failed to run \"net use\" command")?;
    if !output.status.success() {
        return Err(Report::msg(format!(
            "\"net use\" command exited with an error (code: {:?})",
            output.status.code()
        ))
        .section(format!(
            "Stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ))
        .section(format!(
            "Stdout:\n{}",
            String::from_utf8_lossy(&output.stdout)
        )));
    }
    Ok(())
}
