#[cfg(windows)]
use std::{
    env,
    ffi::OsString,
    iter,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

#[cfg(windows)]
use color_eyre::eyre::{Context, bail};
use indicatif::{ProgressBar, ProgressStyle};

use crate::Result;

#[cfg(windows)]
use crate::CancelSignal;

/// Flushes the wrapped stream when certain patterns are recognized in the input
/// data. The `flate2` handles
/// [`Write::flush`](https://docs.rs/flate2/1.0.28/src/flate2/zio.rs.html#257)
/// and [translates it](https://docs.rs/flate2/1.0.28/src/flate2/zio.rs.html#96)
/// into a
/// [`Compress::compress_vec`](https://docs.rs/flate2/1.0.28/flate2/struct.Compress.html#method.compress_vec)
/// call with the
/// [`FlushCompress::Sync`](https://docs.rs/flate2/1.0.28/flate2/enum.FlushCompress.html)
/// flush. This should be enough for related data in the uncompressed file to be
/// compressed in a similar way.
///
/// # Info about `--rsyncable` flag
///
/// Mentioned under `Determining which parts of a file have changed` at [rsync -
/// Wikipedia](https://en.wikipedia.org/wiki/Rsync#Determining_which_parts_of_a_file_have_changed).
///
/// Some information about possible space savings at [Documentation: Best
/// practice wrt pre-compressed data · Issue #2886 ·
/// restic/restic](https://github.com/restic/restic/issues/2886).
///
/// General info about how it works at [Rsyncable gzip | BeezNest Open-Source
/// specialists](https://beeznest.wordpress.com/2005/02/03/rsyncable-gzip/).
///
/// Info about why the parallel gzip library `pigz` makes use of something
/// similar [Gzip streams support dictionary resets which means you can
/// concatenate individua... | Hacker
/// News](https://news.ycombinator.com/item?id=33240066).
///
/// Some info about the hash used to determine when to reset the compressor at
/// [hash_roll::gzip -
/// Rust](https://docs.rs/hash-roll/0.3.0/hash_roll/gzip/index.html)
pub struct Rsyncable<T> {
    inner: T,
    hash: u32,
}
impl<T> Rsyncable<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            hash: Self::RSYNCHIT,
        }
    }
}
/// Constants from the [pgiz
/// library](https://github.com/madler/pigz/blob/fe4894f57739e3039a2ffc2a2a360d35e19bacbe/pigz.c#L469-L514).
impl<T> Rsyncable<T> {
    const RSYNCBITS: u32 = 12;
    const RSYNCMASK: u32 = (1_u32 << Self::RSYNCBITS) - 1;
    const RSYNCHIT: u32 = (Self::RSYNCMASK >> 1);
}
impl<T> std::io::Write for Rsyncable<T>
where
    T: std::io::Write,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // pigz code:
        // https://github.com/madler/pigz/blob/fe4894f57739e3039a2ffc2a2a360d35e19bacbe/pigz.c#L2121
        //
        // Stack Overflow question: https://stackoverflow.com/questions/40764396/rsyncable-compression-library
        // Readme of C# port: https://github.com/GrzegorzBlok/FastRsyncNet/tree/d0d6b344ded2666448ac371f0f267a5915d8ee9d#gzip-compression-that-is-rsync-compatible-
        // Relevant code: https://github.com/GrzegorzBlok/FastRsyncNet/blob/d0d6b344ded2666448ac371f0f267a5915d8ee9d/source/FastRsync.Compression/GZip.cs#L45-L65
        //
        // Might be some relevant info about `Z_SYNC_FLUSH` at:
        // https://comp.compression.narkive.com/36atBv7N/zlib-gzip-recovering-after-inflatesync

        let mut hash = self.hash;
        for (i, byte) in buf.iter().copied().enumerate() {
            hash = ((hash << 1) ^ u32::from(byte)) & Self::RSYNCMASK;

            if hash == Self::RSYNCHIT {
                self.inner.write_all(&buf[..i + 1])?;
                self.hash = hash;

                self.inner.flush()?; // Emit `flate2::FlushCompress::Sync`
                return Ok(i + 1);
            }
        }
        self.inner.write_all(buf)?;
        self.hash = hash;

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub fn create_progress_bar(size: Option<u64>) -> ProgressBar {
    if let Some(size) = size {
        let pb = ProgressBar::new(size);
        pb.set_style(ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] {wide_bar:.cyan/blue} {bytes}/{total_bytes} ({bytes_per_sec}, {percent}%, ETA: {eta})")
            .unwrap()
        );
        pb
    } else {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.blue} [{elapsed_precise}] {msg} {bytes} ({bytes_per_sec})")
                .unwrap(),
        );
        pb
    }
}

pub fn is_64bit_os() -> Result<bool> {
    #[cfg(any(not(windows), target_pointer_width = "64"))]
    {
        Ok(true)
    }
    #[cfg(all(windows, not(target_pointer_width = "64")))]
    {
        // Copied code from `heim::host::platform` function.
        // https://github.com/heim-rs/heim/blob/b292f1535bb27c03800cdb7509fa81a40859fbbb/heim-host/src/sys/windows/platform.rs
        use winapi::um::{sysinfoapi, winnt};

        let mut info = std::mem::MaybeUninit::<sysinfoapi::SYSTEM_INFO>::uninit();
        let info = unsafe {
            // https://docs.microsoft.com/en-us/windows/win32/api/sysinfoapi/nf-sysinfoapi-getnativesysteminfo
            // Returns nothing and can't fail, apparently
            sysinfoapi::GetNativeSystemInfo(info.as_mut_ptr());
            info.assume_init()
        };

        let is_32bit = match unsafe { info.u.s() }.wProcessorArchitecture {
            // While there are other `PROCESSOR_ARCHITECTURE_*` consts exists,
            // MSDN described only the following.
            // https://docs.microsoft.com/ru-ru/windows/desktop/api/sysinfoapi/ns-sysinfoapi-_system_info#members
            winnt::PROCESSOR_ARCHITECTURE_AMD64 => false, // Arch::X86_64,
            winnt::PROCESSOR_ARCHITECTURE_ARM => true,    // Arch::ARM,
            winnt::PROCESSOR_ARCHITECTURE_ARM64 => false, // Arch::AARCH64,
            // TODO: Is it okay to match Ia64 to unknown arch?
            // `platforms::Arch` enum does not have specific member for Itanium.
            winnt::PROCESSOR_ARCHITECTURE_IA64 => false, //Arch::Unknown,
            winnt::PROCESSOR_ARCHITECTURE_INTEL => true, // Arch::X86,
            _ => false,                                  // Arch::Unknown,
        };

        Ok(!is_32bit)
    }
}

#[cfg(windows)]
pub fn wiz_tree_exe_name() -> Result<OsString> {
    let exe_name = if is_64bit_os()? {
        "WizTree64.exe"
    } else {
        "WizTree.exe"
    };
    Ok(exe_name.into())
}

/// Get possible paths where WizTree could be located.
#[cfg(windows)]
pub fn possible_wiz_tree_paths() -> Result<Vec<PathBuf>> {
    let exe_name = wiz_tree_exe_name()?;

    let scoop_paths: Vec<_> =
    // Try specified user directory:
    Option::into_iter(env::var_os("UserProfile").map(PathBuf::from))
        // Try guessing its on the `C` drive and use the specified user name:
        .chain(env::var_os("UserName").map(|name| PathBuf::from(r"C:\Users\").join(name)))
        // Scoop is always inside the user folder (hopefully):
        .map(|user_folder| user_folder.join("scoop"))
        .collect();

    let wiz_tree_paths: Vec<PathBuf> =
        // Try current directory (also looks at PATH):
        iter::once(PathBuf::new())
            // Try in the program files directory (Chocolatey installs to this
            // locations as well):
            .chain(env::var_os("ProgramFiles").map(|prog| PathBuf::from(prog).join("WizTree")))
            .chain(iter::once(PathBuf::from(r"C:\Program Files\WizTree\")))
            // Try in scoop's install location (scoop's version doesn't work
            // quite as well since it will always start a child instance of
            // itself):
            .chain(scoop_paths.iter().map(|scoop| scoop.join("shims")))
            .chain(scoop_paths.iter().map(|scoop| scoop.join(r"apps\wiztree\current")))
            // Then try starting the WizTree executable:
            .map(|path| path.join(&exe_name))
            .collect();
    Ok(wiz_tree_paths)
}

/// Run WizTree.
///
/// # Windows' Job Object
///
/// Specify the `current_job` argument to determine if the WizTree program
/// spawns any background processes. If WizTree spawns a background program then
/// it will likely still be in the same job and so we can just query what
/// processes are in the current process's job.
///
/// # File Share Mode for files used by WizTree
///
/// Any file paths passed to WizTree will be opened with very restricted file
/// share modes so they can't be opened in read or write mode by any other
/// processes and can't be deleted either. This prevents using
/// `FILE_FLAG_DELETE_ON_CLOSE` to make Windows automatically cleanup temporary
/// files. It is easy to observe this behavior by just trying to copy or delete
/// the file that WizTree is using via the file explorer which will fail with an
/// error about the file being in use.
///
/// ## Export file system info
///
/// Disallows reading (copying a file) and deleting.
///
/// ## Read/Import CSV to view in UI
///
/// Disallows deleting the file while its being loaded, once WizTree is finished
/// loading it is possible to delete the file. Its possible to copy the file the
/// entire time its being loaded.
#[cfg(windows)]
pub fn run_wiz_tree(
    wiz_tree_paths: Vec<PathBuf>,
    mut configure: impl FnMut(&mut Command) -> Result<()>,
    current_job: Option<&WindowsJob>,
    cancel_signal: &CancelSignal,
) -> Result<()> {
    if wiz_tree_paths.is_empty() {
        bail!("Couldn't find where WizTree was installed");
    }

    let mut wiz_tree = {
        // Try to spawn wiz tree from a specified path:
        let mut spawn_wiz_tree = |wiz_tree_path: &Path| {
            log::trace!(
                r#"Trying to start WizTree from: "{}""#,
                wiz_tree_path.display()
            );
            let mut command = Command::new(wiz_tree_path);
            configure(&mut command)?;
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .wrap_err("failed to start WizTree")
        };

        let mut wiz_tree_iter = wiz_tree_paths
            .iter()
            .map(|path| spawn_wiz_tree(path))
            .peekable();
        loop {
            match wiz_tree_iter.next() {
                // First success => use it!
                Some(Ok(v)) => break v,
                // Last error => report it!
                Some(Err(e)) => {
                    if wiz_tree_iter.peek().is_none() {
                        Err(e).wrap_err_with(|| {
                            format!(
                                "failed to start WizTree from any of the following locations: {:?}",
                                wiz_tree_paths
                            )
                        })?
                    }
                }
                // Should reach one of the above arms as long as there are more then 1 path to try:
                None => unreachable!(
                    "should be at least 1 path specified where WizTree could be located and the following paths were specified: {:?}",
                    wiz_tree_paths
                ),
            }
            cancel_signal.as_error()?;
        }
    };

    let spawned_at = Instant::now();
    let spawned_id = wiz_tree.id();

    let mut count: u32 = 0;
    // Waits for child process to exit while also handling cancel signal.
    // This could be done more efficiently with [shared_child - crates.io: Rust Package Registry](https://crates.io/crates/shared_child)
    let status = loop {
        if let Some(status) = wiz_tree
            .try_wait()
            .wrap_err("Failed to wait for WizTree to quit")?
        {
            break status;
        }
        if cancel_signal
            .wait_timeout(Duration::from_millis(if count > 20 { 100 } else { 25 }))
            .is_err()
        {
            let reason = cancel_signal.reason();
            let reason = reason.as_deref().unwrap_or("cancellation request");
            wiz_tree.kill().wrap_err_with(|| {
                format!("failed to kill WizTree program in response to {}", reason)
            })?;
            wiz_tree
                .wait()
                .wrap_err_with(|| format!("failed to wait for WizTree program to exit after it was killed in response to {}", reason))?;
            bail!("Killed WizTree program is response to {}", reason);
        }
        count = count.saturating_add(1);
    };

    let time_spent_running = spawned_at.elapsed();
    if !status.success() {
        bail!(
            "WizTree exited with an error{} after {:?}",
            if let Some(code) = status.code() {
                format!(" (exit code: {})", code)
            } else {
                String::new()
            },
            time_spent_running
        );
    }
    log::debug!("WizTree exited successfully after {:?}", time_spent_running);

    if let Some(job) = current_job {
        let mut spawned_processes = {
            let mut temp = job.process_id_list()?;
            let current_id = std::process::id() as usize;
            temp.retain(|&id| id != current_id && id != 0);
            temp
        };

        if !spawned_processes.is_empty() {
            use sysinfo::{Pid, PidExt, ProcessExt, System, SystemExt};

            log::trace!(
                "WizTree spawned processes with the following process ids \
                {:?} checking if it spawned another instance of WizTree to do \
                the real work...",
                spawned_processes
            );

            let mut info = System::new();

            spawned_processes.retain(|pid| {
                let pid = Pid::from_u32(*pid as u32);
                if !info.refresh_process(pid) {
                    // Process has been closed
                    log::trace!("Spawned process with pid {} has already closed", pid);
                    false
                } else if let Some(info) = info.process(pid) {
                    log::trace!("Info about process that was spawned by WizTree: {:?}", info);

                    let is_wiz_tree_process = info
                        .exe()
                        .file_name()
                        .map(|name| name.to_string_lossy().to_lowercase().starts_with("wiztree"))
                        .unwrap_or(false);
                    let was_spawned_by_our_child = info
                        .parent()
                        .map(|parent| parent.as_u32() == spawned_id)
                        .unwrap_or(true);
                    let should_wait_for_it = is_wiz_tree_process && was_spawned_by_our_child;

                    if should_wait_for_it {
                        log::trace!("Process with id {} was spawned directly by our WizTree process \
                            and seems to also be a WizTree process, therefore we will wait for it to \
                            exit as well.", pid);
                    }
                    should_wait_for_it
                } else {
                    log::trace!("Spawned process with pid {} has already closed", pid);
                    false
                }
            });

            drop(info);

            if !spawned_processes.is_empty() {
                // WizTree spawned new instance of itself.
                log::warn!(
                    "WizTree spawned a new instance of itself as a child process \
                        to do all of its work and then exited early. This program \
                        will therefore start waiting for the spawned background \
                        program to exit, this has some drawbacks such as making it \
                        harder to detect if WizTree exits with an error. WizTree \
                        can start a child instance of itself if you installed \
                        WizTree using the scoop package manager or if \"WizTree.exe\" \
                        instead of \"WizTree64.exe\" is started on a 64bit system. \
                        You could try to install it via the Chocolatey package manager \
                        using a command like \"choco install wiztree\".",
                );
                let started_waiting = Instant::now();

                // Wait until we can no longer find the spawned processes.
                while job
                    .process_id_list()?
                    .iter()
                    .any(|pid| spawned_processes.contains(pid))
                {
                    let _ = cancel_signal.wait_timeout(Duration::from_millis(500));
                    cancel_signal
                        .as_error()
                        .wrap_err("failed to wait for WizTree child instance to exit")?;
                }

                log::debug!(
                    "WizTree child instance exited, {:?} after the main WizTree process exited.",
                    started_waiting.elapsed()
                );
            }
        }
    } else if time_spent_running < Duration::from_millis(1_000) {
        log::warn!(
            "WizTree exited really quickly, it could be that it spawned \
                a child process to do all of its work. If you are using \
                a version of WizTree installed via scoop then that is \
                likely the cause."
        );
    }

    // TODO: if WizTree exits successfully we should leave any spawned processes
    // opened since they might be user programs.

    Ok(())
}

/// Create a new file.
///
/// If `overwrite` is `false` then an atomic operation is used to ensure that no other file existed where the new file is created.
pub fn create_file(overwrite: bool, path: impl AsRef<Path>) -> io::Result<File> {
    // Configure how new files are crated:
    let mut new_file_options = OpenOptions::new();
    new_file_options.write(true);
    if overwrite {
        new_file_options
            // If file exists then remove all of its content:
            .truncate(true)
            // If not file exists then create one:
            .create(true);
    } else {
        // Ensure we don't overwrite anything (atomic operation that ensures that we are creating a new file):
        new_file_options.create_new(true);
    }

    new_file_options.open(path)
}

#[cfg(windows)]
pub struct WindowsJob {
    job: Option<win32job::Job>,
}
#[cfg(windows)]
impl WindowsJob {
    /// Create a Windows "Job Object" to enforce limits for the current process and any
    /// processes created by it (unless they use a special creation flag).
    ///
    /// Some information about the different limits can be found at
    /// [Class: Win32::Job — Documentation for win32-job (0.1.2)](https://rdoc.info/gems/win32-job/0.1.2/Win32/Job)
    pub fn create(
        f: impl FnOnce(&mut win32job::ExtendedLimitInfo) -> Result<()>,
    ) -> Result<WindowsJob> {
        use win32job::*;

        let job = Job::create().wrap_err("failed to create new Windows Job Object")?;
        let mut info = job
            .query_extended_limit_info()
            .wrap_err("failed to get default limits for new Windows Job Object")?;

        f(&mut info).wrap_err("failed to configure limits for Windows Job Object")?;

        job.set_extended_limit_info(&mut info)
            .wrap_err("failed to set new limits to Windows Job Object")?;
        job.assign_current_process()
            .wrap_err("failed to assign new Windows Job Object to current process")?;

        Ok(WindowsJob { job: Some(job) })
    }

    pub fn process_id_list(&self) -> Result<Vec<usize>> {
        self.job.as_ref().unwrap().query_process_id_list().wrap_err(
            "failed to get list of processes associated with the current Windows Job Object",
        )
    }
    pub fn modify_limits(
        &mut self,
        f: impl FnOnce(&mut win32job::ExtendedLimitInfo) -> Result<()>,
    ) -> Result<()> {
        let mut info = self
            .job
            .as_mut()
            .unwrap()
            .query_extended_limit_info()
            .wrap_err("failed to get current limits for Windows Job Object")?;

        f(&mut info).wrap_err("failed to configure limits for Windows Job Object")?;

        self.job
            .as_mut()
            .unwrap()
            .set_extended_limit_info(&mut info)
            .wrap_err("failed to set new limits to Windows Job Object")?;

        Ok(())
    }
    /// Remove the Job limit that would kill all processes when this job is
    /// closed.
    pub fn clear_kill_on_job_close(&mut self) -> Result<()> {
        self.modify_limits(|limits| {
            // Allow all bits except the one that specifies this limit:
            limits.0.BasicLimitInformation.LimitFlags &=
                !winapi::um::winnt::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            Ok(())
        }).wrap_err("failed to clear the KILL_ON_JOB_CLOSE limit which will kill all processes belonging to the current Windows Job Object")
    }
}
#[cfg(windows)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        // If job is dropped then the current process will be terminated as
        // described in the docs for the `limit_kill_on_job_close` method.
        // Therefore we forget the job (which means that this handle will be
        // closed after the current process exits):
        self.job.take().unwrap().into_handle();
    }
}

/// Similar to [`tempfile::TempPath`] but works for folders instead of files.
pub struct TempDirPath {
    path: Box<Path>,
}
impl TempDirPath {
    /// Copied from <https://docs.rs/tempfile/3.9.0/src/tempfile/error.rs.html>.
    fn with_err_path<T, F, P>(result: Result<T, io::Error>, path: F) -> Result<T, io::Error>
    where
        F: FnOnce() -> P,
        P: Into<PathBuf>,
    {
        #[derive(Debug)]
        struct PathError {
            path: PathBuf,
            err: io::Error,
        }

        impl fmt::Display for PathError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{} at path {:?}", self.err, self.path)
            }
        }

        impl std::error::Error for PathError {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                self.err.source()
            }
        }

        result.map_err(|e| {
            io::Error::new(
                e.kind(),
                PathError {
                    path: path().into(),
                    err: e,
                },
            )
        })
    }
    /// Close and remove the temporary directory.
    ///
    /// Use this if you want to detect errors in deleting the directory.
    ///
    /// # Errors
    ///
    /// If the file cannot be deleted, `Err` is returned.
    pub fn close(mut self) -> io::Result<()> {
        let result = Self::with_err_path(fs::remove_dir_all(&self.path), || &*self.path);
        self.path = PathBuf::new().into_boxed_path();
        std::mem::forget(self);
        result
    }
    /// Keep the temporary directory from being deleted.
    pub fn keep(mut self) -> PathBuf {
        let v = std::mem::replace(&mut self.path, PathBuf::new().into_boxed_path());
        std::mem::forget(self);
        v.into()
    }
    /// Create a new TempDirPath from an existing path. This can be done even if
    /// no directory exists at the given path.
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into().into_boxed_path(),
        }
    }
}
impl Drop for TempDirPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
impl std::ops::Deref for TempDirPath {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}
