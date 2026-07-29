#![warn(clippy::all)]

use std::{
    fmt,
    io::{self, Write},
    path::Path,
    str::FromStr,
    sync::RwLock,
};

use clap::{ArgAction, Args, Parser, ValueEnum};
#[cfg(windows)]
use color_eyre::eyre::Context;
use color_eyre::{Help, eyre::Report};
use indicatif::{ProgressBar, WeakProgressBar};

pub mod backup;
pub mod cancellation;
pub mod cleanup;
pub mod diff;
pub mod fs_index;
pub mod mount;
#[cfg(unix)]
pub mod qdirstat_open;
pub mod utils;
#[cfg(feature = "web_dav")]
pub mod webdav_memfs;
#[cfg(all(windows, feature = "winfsp"))]
pub mod winfsp_memfs;
#[cfg(windows)]
pub mod wiztree_backup;
mod wiztree_csv;
#[cfg(windows)]
pub mod wiztree_open;

pub type Result<T, E = Report> = core::result::Result<T, E>;

pub use wiztree_csv::WizTreeCsvRecord;

pub use cancellation::CancelSignal;
pub use cleanup::CleanupProcess;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum FileSystemType {
    #[value(name = "WinFsp")]
    #[cfg(windows)]
    WinFsp,
    #[value(name = "WebDAV")]
    WebDav,
    // And eventually dokany via [dokan-dev/dokan-rust: Dokan Rust Wrapper](https://github.com/dokan-dev/dokan-rust)
}

#[derive(Debug, Parser, Clone)]
#[clap(version, author, about, name = "file-system-backup")]
pub enum Opts {
    /// Use the `WizTree` program to make a backup of file info.
    ///
    /// Features:
    /// - Use background/low priority for CPU and IO to ensure that other
    ///   programs won't be disturbed because of the backup.
    /// - Compress the output file using the gzip format.
    /// - Create an in memory filesystem where WizTree writes its output after
    ///   which the data is compressed and written to the final destination.
    ///   This helps prevent the system drive from being worn out.
    /// - A nicer command line interface (than the basic one provided by WizTree
    ///   itself).
    ///   - Proper `--help` messages and suggestions for typos.
    ///   - Progress bars/indicators when preforming actions that take some
    ///     time.
    ///   - Can write output to stdout which makes it easier to use from other
    ///     processes or to use it in pipelines.
    /// - Find WizTree's install location.
    /// - Wait for WizTree to complete its work.
    ///   - By default WizTree can spawn a new instance of itself and just exit
    ///     early from the original command under some circumstances. (For
    ///     example, we make sure to check if we should run the 32bit or 64bit
    ///     version of WizTree.)
    /// - Gracefully handle cancellation by terminating WizTree and removing
    ///   temporary files.
    ///   - Listens for Ctrl-C signals and exit with an error message.
    ///   - Spawn a background "cleanup" process that will cleanup temporary
    ///     files if the parent process is killed unexpectedly.
    ///   - Use Windows' "Job" system to ensure the WizTree process is killed if
    ///     this parent process is killed.
    #[clap(version, author)]
    #[clap(verbatim_doc_comment)]
    #[cfg(windows)]
    WizTreeBackup(wiztree_backup::WizTreeBackupOpts),
    /// Open a backup file with WizTree's UI.
    ///
    ///
    /// WizTree Versions:
    ///
    /// WizTree version 4.00 and later can be a lot slower at parsing exported
    /// csv files (about 4 times slower). As a workaround you could try to
    /// install version 3.41 of Wiztree which is the version that was released
    /// before v4.00.
    ///
    /// Note that while the scoop package manager can sometimes install older
    /// versions using a command like "scoop install wiztree@3.41" this won't
    /// work correctly for WizTree and instead the latest version will always be
    /// installed.
    ///
    /// The Chocolatey package manager on the other hand should work using a
    /// command like "choco install wiztree --version 3.41" though you might
    /// need to manually uninstall the current version first using "choco
    /// uninstall wiztree".
    ///
    ///
    /// Features:
    /// - Open compressed files.
    ///   - This will decompress the data to a temporary file that is removed after
    ///     WizTree is closed.
    /// - A nicer command line interface (than the basic one provided by WizTree
    ///   itself).
    ///   - Proper `--help` messages and suggestions for typos.
    ///   - Progress bars/indicators when preforming actions that take some time.
    ///   - Can write output to stdout and read input from stdin which makes it
    ///     easier to use from other processes or to use it in pipelines.
    /// - Find WizTree's install location.
    /// - Wait for WizTree to complete its work.
    ///   - By default WizTree can spawn a new instance of itself and just exit
    ///     early from the original command under some circumstances. (For example
    ///     we make sure to check if we should run the 32bit or 64bit version of
    ///     WizTree.)
    /// - Gracefully handle cancellation by terminating WizTree and removing
    ///   temporary files.
    ///   - Listens for Ctrl-C signals and exit with an error message.
    ///   - Spawn a background "cleanup" process that will cleanup temporary files
    ///     if the parent process is killed unexpectedly.
    ///   - Use Windows' "Job" system to ensure the WizTree process is killed if
    ///     this parent process is killed.
    #[clap(version, author)]
    #[clap(verbatim_doc_comment)]
    #[cfg(windows)]
    WizTreeOpen(wiztree_open::WizTreeOpenOpts),
    /// Open a backup file with QDirStat's UI.
    #[clap(version, author)]
    #[cfg(unix)]
    QDirStatOpen(qdirstat_open::QDirStatOpenOpts),
    /// Make a backup of file info by scanning the filesystem using OS APIs or
    /// more quickly by parsing the MFT of the disk if the program has admin
    /// rights.
    #[clap(version, author)]
    Backup(backup::BackupOpts),
    /// Mount a backup file as a fake filesystem to easily inspects it content.
    #[clap(version, author)]
    Mount(mount::MountOpts),
    /// Generate a new backup file that contains only folders and files that
    /// have changed when comparing two existing backups.
    #[clap(version, author)]
    Diff(diff::DiffOpts),
    /// Setup a filesystem using WinFsp.
    #[cfg(all(windows, feature = "winfsp"))]
    TestWinFsp,
}
impl Opts {
    pub fn run(self, cancel_signal: &CancelSignal) -> Result<()> {
        match self {
            #[cfg(windows)]
            Self::WizTreeBackup(v) => v.run(cancel_signal),
            #[cfg(windows)]
            Self::WizTreeOpen(v) => v.run(cancel_signal),
            #[cfg(unix)]
            Self::QDirStatOpen(v) => v.run(cancel_signal),
            Self::Backup(v) => v.run(cancel_signal),
            Self::Mount(v) => v.run(cancel_signal),
            Self::Diff(v) => v.run(cancel_signal),
            #[cfg(all(windows, feature = "winfsp"))]
            Self::TestWinFsp => {
                winfsp::winfsp_init().map_err(|e| {
                    let report = color_eyre::eyre::eyre!("{e:?}\n{e}");
                    if let winfsp::FspError::WIN32(1285) = e {
                        report.wrap_err(
                            "The error code corresponds to ERROR_DELAY_LOAD_FAILED which means we failed to load \
                            WinFsp's dynamically linked library (.dll), make sure that WinFsp is correctly installed."
                        )
                    } else {
                        report
                    }
                }).wrap_err("Failed to initialize WinFsp")?;
                let mut cx = winfsp_memfs::WinFspMemFsContext::new();
                {
                    let cx = std::sync::Arc::get_mut(&mut cx.shared)
                        .unwrap()
                        .get_mut()
                        .unwrap();
                    // Example files:
                    let _ = cx.make_node(
                        &winfsp::U16CString::from_str("test").unwrap(),
                        false,
                        Vec::new(),
                    );
                    let _ = cx.make_node(
                        &winfsp::U16CString::from_str("a folder").unwrap(),
                        true,
                        Vec::new(),
                    );
                    let _ = cx.make_node(
                        &winfsp::U16CString::from_str("a folder/hello_world.txt").unwrap(),
                        false,
                        b"Hello world!".to_vec(),
                    );
                }
                let mut mem_fs = winfsp_memfs::WinFspMemFs::create_host(cx)
                    .wrap_err("Failed to create WinFsp MemFs file system host")?;
                mem_fs
                    .fs
                    .mount(winfsp::host::MountPoint::NextFreeDrive)
                    .map_err(|e| color_eyre::eyre::eyre!("{} (HRESULT {})", e.message(), e.code()))
                    .wrap_err("Failed to mount WinFsp file system")?;
                mem_fs
                    .fs
                    .start()
                    .wrap_err("Failed to start WinFsp file system")?;

                //winfsp_memfs::WinFspMemFs::create_service(winfsp_memfs::CreationOptions {
                //    init_token: None,
                //    mount_point: "C:/WinFsp-MemFs-Test".into(),
                //    service_name: "test-WinFsp-MemFs".into(),
                //}).map_err(|e| {
                //    let report = color_eyre::eyre::eyre!("{e:?}\n{e}");
                //    if let winfsp::FspError::WIN32(1285) = e {
                //        report.wrap_err(
                //            "Underlying error code means ERROR_DELAY_LOAD_FAILED which means we failed to load \
                //            WinFsp's dynamically linked library (.dll), make sure that WinFsp is correctly installed."
                //        )
                //    } else {
                //        report
                //    }
                //}).wrap_err("failed to start WinFsp file system")?;

                log::info!("Press Ctrl+C to exit");
                loop {
                    if cancel_signal
                        .wait_timeout(std::time::Duration::from_millis(100))
                        .is_err()
                    {
                        return Ok(());
                    }
                }
            }
        }
    }
    pub fn configure_logging(&self) {
        match self {
            #[cfg(windows)]
            Self::WizTreeBackup(v) => v.common.configure_logging(),
            #[cfg(windows)]
            Self::WizTreeOpen(v) => v.common.configure_logging(),
            #[cfg(unix)]
            Self::QDirStatOpen(v) => v.common.configure_logging(),
            Self::Backup(v) => v.common.configure_logging(),
            Self::Mount(v) => v.common.configure_logging(),
            Self::Diff(v) => v.common.configure_logging(),
            #[cfg(all(windows, feature = "winfsp"))]
            Self::TestWinFsp => CommonOpt {
                quiet: 0,
                verbose: 2,
            }
            .configure_logging(),
        }
    }
}

#[derive(Debug, Parser, Clone)]
pub struct RsyncableOpts {
    /// Make rsync-friendly archive, similar to gzip's --rsyncable flag.
    #[clap(short, long, help_heading = "PROCESSING")]
    pub rsyncable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum BackupFileType {
    #[value(name = "auto")]
    Auto,
    #[value(name = "compressed")]
    CompressedCsv,
    #[value(name = "uncompressed")]
    UncompressedCsv,
}
impl BackupFileType {
    /// Guess backup file type from a file extension (without any leading dots).
    pub fn from_file_ext(ext: &str) -> Option<Self> {
        match ext.to_lowercase().as_str() {
            "gz" => Some(BackupFileType::CompressedCsv),
            "csv" => Some(BackupFileType::UncompressedCsv),
            _ => None,
        }
    }
    /// Guess backup file type from a path's file extension.
    pub fn from_file_path_ext(path: impl AsRef<Path>) -> Option<Self> {
        Self::from_file_ext(path.as_ref().extension()?.to_str()?)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::CompressedCsv => "compressed",
            Self::UncompressedCsv => "uncompressed",
        }
    }
    pub fn all() -> impl Iterator<Item = Self> {
        macro_rules! all {
            ($($name:ident),* $(,)?) => {{
                let _ = |this: Self| match this {
                    $(Self::$name => (),)*
                };
                [$(Self::$name,)*]
            }};
        }
        IntoIterator::into_iter(all!(Auto, CompressedCsv, UncompressedCsv,))
    }
}
impl FromStr for BackupFileType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let lower = s.to_lowercase();
        for v in Self::all() {
            if v.as_str().to_lowercase() == lower {
                return Ok(v);
            }
        }
        Err(format!(r#""{}" is not a valid backup file type"#, s))
    }
}
impl fmt::Display for BackupFileType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_str(), f)
    }
}

#[derive(Debug, Args, Clone)]
pub struct CommonOpt {
    /// Provide more verbose logging. Can be specified up to 2 times to increase
    /// verbosity level.
    #[clap(short, long, action = ArgAction::Count, help_heading = "LOGGING")]
    verbose: u8,
    /// Quiet mode, suppresses some logging. Specify once to only show warnings
    /// and errors. If you specify it 3 times then all logging will be suppressed
    /// but note that if the program exits with an error info about that will still
    /// be written to stderr.
    #[clap(
        short,
        long,
        action = ArgAction::Count,
        conflicts_with = "verbose",
        help_heading = "LOGGING"
    )]
    quiet: u8,
}
impl CommonOpt {
    /// Enable logging based on specified verbosity arguments.
    pub fn configure_logging(&self) {
        let verbosity_level_number = 3_i32 - (self.quiet as i32) + (self.verbose as i32);
        let verbosity_level = verbosity_level(verbosity_level_number.max(0) as u32);
        init_logger(verbosity_level);

        if verbosity_level_number < 0 {
            // You normally won't see this, but it can probably be enabled via environment variables:
            log::warn!(
                "Specified logging level {} but 0 is the lowest level",
                verbosity_level_number
            )
        }
        if verbosity_level_number > 5 {
            log::warn!(
                "Specified logging level {} but 5 is the highest level",
                verbosity_level_number
            )
        }
        log::info!(
            "Logging with verbosity level: {} - {}",
            verbosity_level_number.min(5),
            verbosity_level
                .map(|level| level.to_string())
                .unwrap_or_else(|| "Off".to_string())
        );
    }
}

/// Add a note in the error about how to enable backtraces via environment variables.
pub fn add_backtrace_note_to_error<T>(result: Result<T>) -> Result<T> {
    result.note(
        "backtraces are controlled via environment variables:\n\
            If you want panics and errors to both have backtraces, set RUST_BACKTRACE=1.\n\
            If you want only errors to have backtraces, set RUST_LIB_BACKTRACE=1.\n\
            If you want only panics to have backtraces, set RUST_BACKTRACE=1 and RUST_LIB_BACKTRACE=0.\n\
            If you want backtraces to be printed with source locations, set RUST_LIB_BACKTRACE=full.\n\
        ",
    )
}

pub fn verbosity_level(verbose: u32) -> Option<log::Level> {
    use log::Level::*;
    Some(match verbose {
        0 => return None,
        1 => Error,
        2 => Warn,
        3 => Info,
        4 => Debug,
        _ => Trace,
    })
}

static CURRENT_PROGRESS_BAR: RwLock<Option<WeakProgressBar>> = RwLock::new(None);

pub fn set_progress_bar<'a>(pb: impl Into<Option<&'a ProgressBar>>) {
    let pb = pb
        .into()
        // Don't care about hidden progress bars:
        .filter(|pb| !pb.is_hidden())
        .map(|pb| pb.downgrade());
    *CURRENT_PROGRESS_BAR.write().unwrap() = pb;
}
pub fn get_progress_bar() -> Option<ProgressBar> {
    CURRENT_PROGRESS_BAR.read().unwrap().as_ref()?.upgrade()
}

pub fn init_logger(default_level: Option<log::Level>) {
    use chrono::Local;
    use env_logger::{Builder, Env};
    use log::Level;

    let default_filter = if let Some(default_level) = default_level {
        if default_level == Level::Trace {
            "trace\
                    ,dav_server=debug\
                    ,xml=debug\
                    ,file_system_backup::mount::web_dav_mount=debug"
                .to_lowercase()
        } else if default_level == Level::Debug {
            format!(
                "{:?}\
                    ,dav_server=info\
                    ,xml=info\
                    ,mft::mft=info",
                default_level
            )
            .to_lowercase()
        } else {
            format!("{default_level:?}").to_lowercase()
        }
    } else {
        "off".to_string()
    };
    let mut builder = Builder::from_env(Env::new().default_filter_or(default_filter.as_str()));

    builder.format(|formatter, record| {
        let pg = get_progress_bar().filter(|pb| !pb.is_hidden() && !pb.is_finished());
        let mut msg = Vec::new();
        struct Wrapper<T1, T2> {
            first: Option<T1>,
            second: T2,
        }
        impl<T1, T2> Write for Wrapper<T1, T2>
        where
            T1: Write,
            T2: Write,
        {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if let Some(first) = &mut self.first {
                    first.write(buf)
                } else {
                    self.second.write(buf)
                }
            }

            fn flush(&mut self) -> io::Result<()> {
                if let Some(first) = &mut self.first {
                    first.flush()
                } else {
                    self.second.flush()
                }
            }
        }

        let mut wrapper = Wrapper {
            first: pg.is_some().then_some(&mut msg),
            second: formatter,
        };

        let level_style = wrapper.second.default_level_style(record.level());
        let bold_style = env_logger::fmt::style::Style::new().bold();

        writeln!(
            wrapper,
            " {} {level_style}[{}]{level_style:#}{} {bold_style}({}){bold_style:#}: {}",
            Local::now().format("%Y-%m-%d %H:%M:%S"),
            record.level(),
            match record.level() {
                Level::Debug | Level::Error | Level::Trace => "",
                Level::Info | Level::Warn => " ",
            },
            record.target(),
            record.args()
        )?;

        if let Some(pg) = pg {
            pg.println(std::str::from_utf8(&msg).expect("log message should be UTF8"));
        }

        Ok(())
    });

    builder.init();

    log::trace!(
        "Default log filter at this level (used if RUST_LOG is not specified): {default_filter}"
    );
}
