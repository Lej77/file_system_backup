#![warn(clippy::all)]

use std::{fmt, path::Path, str::FromStr};

use clap::{Parser, ValueEnum};
use color_eyre::{Help, eyre::Report};

pub mod backup;
pub mod cancellation;
pub mod cleanup;
pub mod diff;
#[cfg(any(feature = "edirstat", feature = "edirstat_backup"))]
pub mod edirstat_snapshot;
#[cfg(feature = "edirstat_backup")]
pub mod embedded_edirstat_backup;
pub mod fs_index;
pub mod logging;
pub mod mount;
#[cfg(unix)]
pub mod qdirstat_open;
#[cfg(all(windows, feature = "winfsp"))]
pub mod test_winfsp;
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

    /// Use code from eDirStat to backup filesystem information.
    ///
    /// Note: since the scanning code is built into the executable eDirStat does
    /// NOT have to be installed.
    #[cfg(feature = "edirstat_backup")]
    EmbeddedEDirStatBackup(embedded_edirstat_backup::EDirStatBackupOpts),

    /// Mount a backup file as a fake filesystem to easily inspects it content.
    #[clap(version, author)]
    Mount(mount::MountOpts),

    /// Generate a new backup file that contains only folders and files that
    /// have changed when comparing two existing backups.
    #[clap(version, author)]
    Diff(diff::DiffOpts),

    /// Setup an in-memory filesystem using WinFsp, i.e. a RAM disk.
    #[cfg(all(windows, feature = "winfsp"))]
    TestWinFsp(test_winfsp::TestWinFspOpts),
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
            #[cfg(feature = "edirstat_backup")]
            Self::EmbeddedEDirStatBackup(v) => v.run(cancel_signal),
            Self::Mount(v) => v.run(cancel_signal),
            Self::Diff(v) => v.run(cancel_signal),
            #[cfg(all(windows, feature = "winfsp"))]
            Self::TestWinFsp(v) => v.run(cancel_signal),
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
            #[cfg(feature = "edirstat_backup")]
            Self::EmbeddedEDirStatBackup(v) => v.common.configure_logging(),
            Self::Mount(v) => v.common.configure_logging(),
            Self::Diff(v) => v.common.configure_logging(),
            #[cfg(all(windows, feature = "winfsp"))]
            Self::TestWinFsp(v) => v.common.configure_logging(),
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
    #[value(name = Self::Auto.as_str())]
    Auto,
    #[value(name = Self::WizTreeCsv.as_str())]
    WizTreeCsv,
    #[value(name = Self::WizTreeCsvGzip.as_str())]
    WizTreeCsvGzip,
    #[value(name = Self::QDirStatCache.as_str())]
    QDirStatCache,
    #[value(name = Self::QDirStatCacheGzip.as_str())]
    QDirStatCacheGzip,
    #[value(name = Self::EDirStatSnapshot.as_str())]
    EDirStatSnapshot,
    #[value(name = Self::EDirStatSnapshotZstd.as_str())]
    EDirStatSnapshotZstd,
}
impl BackupFileType {
    /// Guess backup file type from a file name.
    pub fn from_file_name(name: &str) -> Option<Self> {
        let name = name.to_lowercase();
        if name.ends_with(".csv.gz") {
            Some(BackupFileType::WizTreeCsvGzip)
        } else if name.ends_with(".csv") {
            Some(BackupFileType::WizTreeCsv)
        } else if name.ends_with(".cache.gz") {
            Some(BackupFileType::QDirStatCacheGzip)
        } else if name.ends_with(".cache") {
            Some(BackupFileType::QDirStatCache)
        } else if name.ends_with(".edst.zst") {
            Some(BackupFileType::EDirStatSnapshotZstd)
        } else if name.ends_with(".edst") {
            Some(BackupFileType::EDirStatSnapshot)
        } else {
            None
        }
    }
    /// Guess backup file type from a path's file name (multiple layers of file extensions).
    pub fn from_file_path(path: impl AsRef<Path>) -> Option<Self> {
        Self::from_file_name(path.as_ref().file_name()?.to_str()?)
    }

    /// Return an error if the file type is not one of the valid types.
    pub fn ensure_valid_type(self, valid_types: &[BackupFileType]) -> Result<()> {
        if valid_types.contains(&self) {
            Ok(())
        } else {
            color_eyre::eyre::bail!(
                "File type was {self} but only the following formats are supported: {}",
                valid_types
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::WizTreeCsv => "wiztree-csv",
            Self::WizTreeCsvGzip => "wiztree-csv-gzip",
            Self::QDirStatCache => "qdirstat-cache",
            Self::QDirStatCacheGzip => "qdirstat-cache-gzip",
            Self::EDirStatSnapshot => "edirstat-snapshot",
            Self::EDirStatSnapshotZstd => "edirstat-snapshot-zstd",
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
        IntoIterator::into_iter(all![
            Auto,
            WizTreeCsv,
            WizTreeCsvGzip,
            QDirStatCache,
            QDirStatCacheGzip,
            EDirStatSnapshot,
            EDirStatSnapshotZstd,
        ])
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
