//! Ensure cleanup operations are preformed even if the current process is
//! terminated unexpectedly.

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::{
    ffi::{OsStrExt, OsStringExt},
    process::CommandExt,
};
use std::{
    collections::HashSet,
    convert::TryFrom,
    ffi::{OsStr, OsString},
    fmt, fs,
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{ChildStdin, Command, Stdio},
    sync::{Arc, Mutex, OnceLock, RwLock},
    thread,
    time::{Duration, Instant},
};

use color_eyre::eyre::{Context, eyre};
use num_derive::{FromPrimitive, ToPrimitive};
use num_traits::{FromPrimitive, ToPrimitive};

use crate::{CancelSignal, Result};

#[derive(FromPrimitive, ToPrimitive, Debug, Eq, PartialEq, Clone, Copy)]
enum CleanupCommand {
    /// Remember path of new temp file.
    AddTempFile,
    /// Remember path of new temp directory.
    AddTempDir,
    /// Forget a path to a temp file (parent process has deleted it already).
    RemoveTempFile,
    /// Forget a path to a temp directory (parent process has deleted it already).
    RemoveTempDir,
    /// All cleanup handled by parent process.
    Quit,
}
impl fmt::Display for CleanupCommand {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}
impl From<CleanupCommand> for u32 {
    fn from(value: CleanupCommand) -> Self {
        ToPrimitive::to_u32(&value).unwrap()
    }
}
impl TryFrom<u32> for CleanupCommand {
    type Error = u32;
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        FromPrimitive::from_u32(value).ok_or(value)
    }
}

fn get_cleanup_process_argument() -> &'static RwLock<OsString> {
    static CLEANUP_PROCESS_ARGUMENT: OnceLock<RwLock<OsString>> = OnceLock::new();
    CLEANUP_PROCESS_ARGUMENT.get_or_init(|| RwLock::new("private-internal-cleanup".into()))
}

#[must_use = "If this is dropped then the temp file will be ignored by the background process"]
pub struct CleanupProcessTempFile {
    cleanup: Arc<CleanupProcess>,
    path: Vec<u8>,
}
impl CleanupProcessTempFile {
    /// Never tell the background process to stop guarding this temp file.
    ///
    /// Note that if the [`CleanupProcess`] struct is dropped and sends a `Quit`
    /// message then the background process still won't do anything at all.
    pub fn always(mut self) {
        self.path.clear();
    }
}
impl Drop for CleanupProcessTempFile {
    fn drop(&mut self) {
        if self.path.is_empty() {
            return;
        }
        let _guard = self.cleanup.guard.lock();
        if let Err(e) = self
            .cleanup
            ._write_command(CleanupCommand::RemoveTempFile)
            .and_then(|_| self.cleanup._write_length_prefixed(&self.path))
        {
            drop(_guard);
            log::warn!(
                r#"failed to notify background cleanup process that the temp file at "{}" has been deleted: {}"#,
                String::from_utf8_lossy(&self.path),
                e
            )
        }
    }
}

#[must_use = "If this is dropped then the temp directory will be ignored by the background process"]
pub struct CleanupProcessTempDir {
    cleanup: Arc<CleanupProcess>,
    path: Vec<u8>,
}
impl CleanupProcessTempDir {
    /// Never tell the background process to stop guarding this temp directory.
    ///
    /// Note that if the [`CleanupProcess`] struct is dropped and sends a `Quit`
    /// message then the background process still won't do anything at all.
    pub fn always(mut self) {
        self.path.clear();
    }
}
impl Drop for CleanupProcessTempDir {
    fn drop(&mut self) {
        if self.path.is_empty() {
            return;
        }
        let _guard = self.cleanup.guard.lock();
        if let Err(e) = self
            .cleanup
            ._write_command(CleanupCommand::RemoveTempDir)
            .and_then(|_| self.cleanup._write_length_prefixed(&self.path))
        {
            drop(_guard);
            log::warn!(
                r#"failed to notify background cleanup process that the temp directory at "{}" has been deleted: {}"#,
                String::from_utf8_lossy(&self.path),
                e
            )
        }
    }
}

fn bytes_from_os_string(text: &OsStr) -> Vec<u8> {
    #[cfg(windows)]
    {
        text.encode_wide()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>();
    }
    #[cfg(unix)]
    {
        OsStrExt::as_bytes(text).to_owned()
    }
}
fn bytes_to_os_string(buf: &[u8]) -> OsString {
    #[cfg(windows)]
    {
        OsString::from_wide(
            &buf.chunks_exact(2)
                .map(|a| u16::from_le_bytes([a[0], a[1]]))
                .collect::<Vec<_>>(),
        )
    }
    #[cfg(unix)]
    {
        <OsStr as OsStrExt>::from_bytes(buf).to_owned()
    }
}
/// Spawns this executable with a special argument that makes it run the cleanup
/// work. This involves reading commands from stdin and executing cleanup
/// operations if stdin is ever closed (without first sending a quit command).
pub struct CleanupProcess {
    // Hold this whole writing with a shared reference to prevent overlapped writes.
    guard: Mutex<()>,
    stdin: Option<ChildStdin>,
}
impl CleanupProcess {
    /// Start cleanup process, if the returned handle is dropped then the cleanup
    /// process will never preform any actions. The cleanup process is therefore
    /// only useful if this process is killed unexpectedly.
    ///
    /// # Errors
    ///
    /// The started process specifies the `CREATE_BREAKAWAY_FROM_JOB` flag which
    /// requires that the parent process is in a job that has the
    /// `JOB_OBJECT_LIMIT_BREAKAWAY_OK` limit enabled, you might need to create
    /// a new job that the current process is associated with before calling this
    /// function.
    ///
    /// If the spawned process encounters any issues it will log them and if the
    /// spawned process exits with an error then the provided cancel signal will
    /// be canceled with a message that indicates the exit code and any content
    /// that was written to stderr.
    pub fn spawn(cancel_signal: CancelSignal) -> Result<Arc<Self>> {
        let mut command = Command::new(
            std::env::current_exe().wrap_err("failed to get the name of the current executable")?,
        );
        #[cfg(windows)]
        {
            // Ensure new process isn't killed together with this parent process (if
            // the sub-process is killed with the current process then there is really
            // no reason to start it at all):
            // `CREATE_BREAKAWAY_FROM_JOB`: Don't associate with parent job.
            // `DETACHED_PROCESS`: prevent a closing terminal from killing the new process
            // together with this parent process.
            command.creation_flags(
                winapi::um::winbase::CREATE_BREAKAWAY_FROM_JOB
                    | winapi::um::winbase::DETACHED_PROCESS,
            );
        }
        let mut child = command
            // The main program should handle this argument:
            .arg(Self::get_cleanup_argument())
            // To send commands:
            .stdin(Stdio::piped())
            // If main returns with an error:
            .stderr(Stdio::piped())
            // Logging and warnings:
            .stdout(Stdio::piped())
            .spawn()
            .wrap_err("failed to start cleanup process")?;
        let stdin = child.stdin.take();

        // Log warnings:
        thread::spawn({
            let stdout = child.stdout.take().unwrap();
            move || {
                let stdout = BufReader::new(stdout);
                for line in stdout.lines() {
                    match line {
                        Ok(line) => {
                            log::warn!("Cleanup process logged an error: {}", line);
                        }
                        Err(e) => {
                            log::error!("Failed to read log messages from cleanup process: {}", e);
                            break;
                        }
                    }
                }
            }
        });
        // Log errors and trigger cancel signal if background process is killed or fails:
        thread::spawn(move || match child.wait_with_output() {
            Ok(output) => {
                if output.status.success() {
                    return;
                }
                cancel_signal.cancel_with_reason(format!(
                    "background cleanup process exiting with an error {}, stderr:\n{}",
                    if let Some(code) = output.status.code() {
                        format!("(code: {})", code)
                    } else {
                        "".to_string()
                    },
                    String::from_utf8_lossy(&output.stderr),
                ));
            }
            Err(e) => {
                cancel_signal.cancel_with_reason(format!(
                    "failed to wait on background cleanup process: {}",
                    e
                ));
            }
        });

        Ok(Arc::new(Self {
            guard: Mutex::new(()),
            stdin,
        }))
    }

    pub fn guard_temp_file(self: &Arc<Self>, temp_file: &Path) -> CleanupProcessTempFile {
        let mut path = bytes_from_os_string(temp_file.as_os_str());
        if path.is_empty() {
            panic!("Can't guard empty temp path");
        }
        {
            let _guard = self.guard.lock();
            if let Err(e) = self
                ._write_command(CleanupCommand::AddTempFile)
                .and_then(|_| self._write_length_prefixed(&path))
            {
                drop(_guard);
                log::warn!(
                    r#"failed to guard temp file at "{}" via background cleanup process: {}"#,
                    temp_file.display(),
                    e
                );
                // guard doesn't need to tell background process to not delete this file
                path.clear();
            }
        }
        CleanupProcessTempFile {
            cleanup: self.clone(),
            path,
        }
    }
    pub fn guard_temp_dir(self: &Arc<Self>, temp_dir: &Path) -> CleanupProcessTempDir {
        let mut path = bytes_from_os_string(temp_dir.as_os_str());
        if path.is_empty() {
            panic!("Can't guard a temp directory that has an empty path");
        }
        {
            let _guard = self.guard.lock();
            if let Err(e) = self
                ._write_command(CleanupCommand::AddTempDir)
                .and_then(|_| self._write_length_prefixed(&path))
            {
                drop(_guard);
                log::warn!(
                    r#"failed to guard temp directory at "{}" via background cleanup process: {}"#,
                    temp_dir.display(),
                    e
                );
                // guard doesn't need to tell background process to not delete this folder:
                path.clear();
            }
        }
        CleanupProcessTempDir {
            cleanup: self.clone(),
            path,
        }
    }

    fn _write_command(&self, command: CleanupCommand) -> io::Result<()> {
        let buf = u32::from(command).to_le_bytes();
        self.stdin.as_ref().unwrap().write_all(&buf)
    }
    fn _write_length_prefixed(&self, buf: &[u8]) -> io::Result<()> {
        let mut stdin = self.stdin.as_ref().unwrap();
        let len_buf = (buf.len() as u32).to_le_bytes();
        stdin.write_all(&len_buf)?;
        stdin.write_all(buf)
    }

    /// This should be the main function of the cleanup process.
    ///
    /// The implementation reads commands from stdin and assumes the parent process
    /// has terminated unexpectedly if stdin is closed without sending the "quit"
    /// message. Stdout is used to write warning messages and stderr should be
    /// used to write information about the error that is returned from this
    /// method. The implementation is careful to not panic if stdout is closed
    /// when writing its warning messages.
    pub fn handle_cleanup() -> Result<()> {
        let stdin = io::stdin();
        let mut stdin = stdin.lock();
        let stdout = io::stdout();
        let mut stdout = stdout.lock();

        fn is_eof(result: Result<(), io::Error>) -> Result<bool, io::Error> {
            if let Err(e) = result {
                if e.kind() == io::ErrorKind::UnexpectedEof {
                    Ok(true)
                } else {
                    Err(e)
                }
            } else {
                Ok(false)
            }
        }
        /// Return `Ok(None)` if EOF was reached (parent process was killed).
        fn read_length_prefixed(
            mut reader: impl Read,
            buffer: &mut Vec<u8>,
        ) -> Result<Option<&mut [u8]>> {
            let mut buf = [0; 4];
            if is_eof(reader.read_exact(&mut buf)).wrap_err("failed to read length of content")? {
                return Ok(None);
            }
            let len = u32::from_le_bytes(buf);

            buffer.resize(len as usize, 0);
            if is_eof(reader.read_exact(&mut *buffer))
                .wrap_err("failed to read content of message")?
            {
                return Ok(None);
            }

            Ok(Some(&mut buffer[..len as usize]))
        }
        let mut buffer = Vec::<u8>::new();
        let mut temp_files = HashSet::<PathBuf>::new();
        let mut temp_dirs = HashSet::<PathBuf>::new();

        loop {
            let mut buf = [0; 4];
            if is_eof(stdin.read_exact(&mut buf))
                .wrap_err("failed to read length of next command")?
            {
                break;
            }
            let msg = CleanupCommand::try_from(u32::from_le_bytes(buf))
                .map_err(|msg| eyre!("invalid message type: {}", msg))?;
            match msg {
                CleanupCommand::AddTempFile => {
                    match read_length_prefixed(&mut stdin, &mut buffer).wrap_err(
                        "failed to get path of temp file that should be added/remembered",
                    )? {
                        Some(name) => {
                            if !temp_files.insert(PathBuf::from(bytes_to_os_string(name))) {
                                writeln!(
                                    stdout,
                                    r#"Warning: added temp file path while it already existed: "{}""#,
                                    PathBuf::from(bytes_to_os_string(name)).display()
                                )
                                .ok();
                            }
                        }
                        None => break,
                    }
                }
                CleanupCommand::AddTempDir => {
                    match read_length_prefixed(&mut stdin, &mut buffer).wrap_err(
                        "failed to get path of temp directory that should be added/remembered",
                    )? {
                        Some(name) => {
                            if !temp_dirs.insert(PathBuf::from(bytes_to_os_string(name))) {
                                writeln!(
                                    stdout,
                                    r#"Warning: added temp directory path while it already existed: "{}""#,
                                    PathBuf::from(bytes_to_os_string(name)).display()
                                )
                                .ok();
                            }
                        }
                        None => break,
                    }
                }
                CleanupCommand::RemoveTempFile => {
                    match read_length_prefixed(&mut stdin, &mut buffer).wrap_err(
                        "failed to get path of temp file that should be removed/forgotten",
                    )? {
                        Some(name) => {
                            let path = PathBuf::from(bytes_to_os_string(name));
                            if !temp_files.remove(&path) {
                                writeln!(stdout,
                                    r#"Warning: tried to remove/forget temp path "{}" that didn't exist (was never added)"#,
                                    path.display()
                                ).ok();
                            }
                        }
                        None => break,
                    }
                }
                CleanupCommand::RemoveTempDir => {
                    match read_length_prefixed(&mut stdin, &mut buffer).wrap_err(
                        "failed to get path of temp directory that should be removed/forgotten",
                    )? {
                        Some(name) => {
                            let path = PathBuf::from(bytes_to_os_string(name));
                            if !temp_dirs.remove(&path) {
                                writeln!(stdout,
                                    r#"Warning: tried to remove/forget temp directory path "{}" that didn't exist (was never added)"#,
                                    path.display()
                                ).ok();
                            }
                        }
                        None => break,
                    }
                }
                // Skip cleanup work (parent completed all its work successfully):
                CleanupCommand::Quit => return Ok(()),
            }
        }

        fn preform_cleanup(
            mut stdout: io::StdoutLock<'static>,
            temp_files: &mut HashSet<PathBuf>,
            temp_dirs: &mut HashSet<PathBuf>,
        ) {
            let started = Instant::now();
            loop {
                temp_files.retain(|temp_file| match fs::remove_file(temp_file) {
                    // Skip the error case where the file was already removed:
                    Err(e) if e.kind() != io::ErrorKind::NotFound => {
                        writeln!(
                            stdout,
                            r#"Warning: failed to remove temp file at "{}": {}"#,
                            temp_file.display(),
                            e
                        )
                        .ok();
                        // Try again later:
                        true
                    }
                    _ => false,
                });
                temp_dirs.retain(|temp_dir| match fs::remove_dir_all(temp_dir) {
                    // Skip the error case where the file was already removed:
                    Err(e) if e.kind() != io::ErrorKind::NotFound => {
                        writeln!(
                            stdout,
                            r#"Warning: failed to remove temp directory at "{}": {}"#,
                            temp_dir.display(),
                            e
                        )
                        .ok();
                        // Try again later:
                        true
                    }
                    _ => false,
                });
                if temp_files.is_empty() && temp_dirs.is_empty() {
                    break;
                }
                if started.elapsed() > Duration::from_millis(10_000) {
                    writeln!(
                        stdout,
                        "cleanup failed for some items due to timeout\n\
                        Affected files: {temp_files:?}\n\
                        Affected directories: {temp_dirs:?}",
                    )
                    .ok();
                    break;
                }
                // Try again later:
                thread::sleep(Duration::from_millis(500));
            }
        }
        preform_cleanup(stdout, &mut temp_files, &mut temp_dirs);

        Ok(())
    }

    pub fn set_cleanup_argument(arg: OsString) {
        *get_cleanup_process_argument().write().unwrap() = arg;
    }
    pub fn get_cleanup_argument() -> OsString {
        get_cleanup_process_argument().read().unwrap().clone()
    }
}
impl Drop for CleanupProcess {
    fn drop(&mut self) {
        if self.stdin.is_none() {
            return;
        }
        if let Err(e) = self._write_command(CleanupCommand::Quit) {
            log::warn!("failed to write `quit` command to cleanup process: {}", e);
        }
    }
}
