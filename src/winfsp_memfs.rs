//! An in-memory filesystem for WinFsp (a FUSE implementation for Windows).
//!
//! Somewhat inspired by [cgofuse/examples/memfs/memfs.go at
//! f87f5db493b56c5f4ebe482a1b7d02c7e5d572fa ·
//! winfsp/cgofuse](https://github.com/winfsp/cgofuse/blob/f87f5db493b56c5f4ebe482a1b7d02c7e5d572fa/examples/memfs/memfs.go)
//! which uses the MIT license.
#![allow(dead_code)] // TODO: remove this

use std::{
    cmp::min,
    collections::HashMap,
    ffi::OsString,
    sync::{Arc, Mutex},
};

use windows::Win32::{
    Foundation::STATUS_NONCONTINUABLE_EXCEPTION, Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY,
};
use winfsp::{
    FspError, FspInit,
    filesystem::{FileSecurity, FileSystemContext, WideNameInfo},
    host::{DebugMode, FileSystemHost, FileSystemParams, VolumeParams},
    service::{FileSystemService, FileSystemServiceBuilder},
    winfsp_init,
};

pub type FspResult<T = (), E = FspError> = std::result::Result<T, E>;

/// Copied from `winfsp-sys-0.2.2+winfsp-2.0\src\lib.rs`
#[allow(non_camel_case_types)]
pub type FILE_ACCESS_RIGHTS = u32;

/// Copied from `winfsp-sys-0.2.2+winfsp-2.0\src\lib.rs`
#[allow(non_camel_case_types)]
pub type FILE_FLAGS_AND_ATTRIBUTES = u32;

/// A file system that keeps data in RAM.
pub struct WinFspMemFs {
    /// The host for this file system.
    pub fs: FileSystemHost<WinFspMemFsContext>,
}
impl WinFspMemFs {
    /// Create a new [`FileSystemHost`] for an in-memory file system.
    ///
    /// # Based on code from
    ///
    /// [winfsp-rs/filesystems/ntptfs-winfsp-rs/src/fs/ntptfs.rs at
    /// f7efba0f0897744197c602ef34ab43510a6ddc25 ·
    /// SnowflakePowered/winfsp-rs](https://github.com/SnowflakePowered/winfsp-rs/blob/f7efba0f0897744197c602ef34ab43510a6ddc25/filesystems/ntptfs-winfsp-rs/src/fs/ntptfs.rs)
    pub fn create_host(context: WinFspMemFsContext) -> FspResult<WinFspMemFs> {
        let mut volume_params = VolumeParams::new();
        volume_params
            .filesystem_name("memfs")
            .file_info_timeout(1000)
            .case_preserved_names(true);
        Ok(WinFspMemFs {
            fs: FileSystemHost::new_with_options(
                FileSystemParams::default_params_debug(volume_params, DebugMode::all()),
                context,
            )?,
        })
    }

    /// Create a new [`FileSystemService`] for an in-memory file system.
    ///
    /// Note that this will intercept `Ctrl+C` signals to stop the started
    /// service, prefer `create_host` for less intrusive integration with your
    /// program.
    ///
    /// # Based on code from
    ///
    /// [winfsp-rs/filesystems/ntptfs-winfsp-rs/src/service.rs at
    /// f7efba0f0897744197c602ef34ab43510a6ddc25 ·
    /// SnowflakePowered/winfsp-rs](https://github.com/SnowflakePowered/winfsp-rs/blob/f7efba0f0897744197c602ef34ab43510a6ddc25/filesystems/ntptfs-winfsp-rs/src/service.rs)
    ///
    /// [winfsp-rs/filesystems/ntptfs-winfsp-rs/src/main.rs at
    /// f7efba0f0897744197c602ef34ab43510a6ddc25 ·
    /// SnowflakePowered/winfsp-rs](https://github.com/SnowflakePowered/winfsp-rs/blob/f7efba0f0897744197c602ef34ab43510a6ddc25/filesystems/ntptfs-winfsp-rs/src/main.rs#L42-L55)
    pub fn create_service(options: ServiceOptions) -> FspResult<FileSystemService<WinFspMemFs>> {
        let init = match options.init_token {
            Some(v) => v,
            None => winfsp_init()?,
        };
        let mut fsp = FileSystemServiceBuilder::new()
            .with_start(move || {
                WinFspMemFs::create_host(WinFspMemFsContext::new())
                    .and_then(|mut mem_fs| {
                        mem_fs.fs.mount(options.mount_point.as_os_str())?;
                        mem_fs.fs.start()?;
                        Ok(mem_fs)
                    })
                    .map_err(|_e| STATUS_NONCONTINUABLE_EXCEPTION.into())
            })
            .with_stop(|fs| {
                if let Some(f) = fs {
                    f.fs.stop();
                }
                Ok(())
            })
            .build(options.service_name, init)?;

        fsp.start()?;
        Ok(fsp)
    }
}

/// Options when creating an in-memory file system service.
pub struct ServiceOptions {
    pub mount_point: OsString,
    pub init_token: Option<FspInit>,
    pub service_name: OsString,
}

/// Data associated with a file in the in-memory file system.
#[doc(alias = "node_t")]
#[derive(Debug)]
pub struct WinFspMemFsFile {
    pub data: Vec<u8>,
    id: WinFspMemFsFileId,
    parent_id: Option<WinFspMemFsFileId>,
    children: HashMap<winfsp::U16CString, WinFspMemFsFileId>,
    /// If this is `0` and `parent_id` is `None` then delete this file.
    open_count: u64,
    is_dir: bool,
}
impl WinFspMemFsFile {
    #[doc(alias = "newNode")]
    pub fn new(id: WinFspMemFsFileId, is_dir: bool) -> Self {
        Self {
            data: Vec::new(),
            id,
            parent_id: None,
            children: HashMap::new(),
            open_count: 0,
            is_dir,
        }
    }
    pub fn can_delete(&self) -> bool {
        self.open_count == 0 && self.parent_id.is_none() && self.id != WinFspMemFsFileId::ROOT
    }
    pub fn file_attributes(&self) -> u32 {
        if self.is_dir {
            FILE_ATTRIBUTE_DIRECTORY.0
        } else {
            0
        }
    }
    pub fn fill_in_file_info(&self, info: &mut winfsp::filesystem::FileInfo) {
        info.file_attributes = self.file_attributes();
        info.hard_links = 1;
        info.file_size = if self.is_dir {
            0
        } else {
            u64::try_from(self.data.len()).unwrap_or(u64::MAX)
        };
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WinFspMemFsFileId(u64);
impl WinFspMemFsFileId {
    pub const ROOT: Self = Self(1);
}

pub struct FoundNode<'a> {
    pub parent: WinFspMemFsFileId,
    /// The path to the parent directory.
    pub parent_path: &'a [u16],
    /// Name of the file/directory that we were last looking for. If `node` is
    /// `Some` then this is the name for that node.
    pub name: winfsp::U16CString,
    pub node: Option<WinFspMemFsFileId>,
}

#[derive(Debug)]
#[doc(alias = "Memfs")]
pub struct WinFspMemFsContextState {
    files: HashMap<WinFspMemFsFileId, WinFspMemFsFile>,
    prev_file_id: u64,
}
impl WinFspMemFsContextState {
    #[doc(alias = "NewMemfs")]
    pub fn new() -> Self {
        let root = WinFspMemFsFile::new(WinFspMemFsFileId::ROOT, true);
        let mut files = HashMap::new();
        files.insert(root.id, root);
        Self {
            files,
            prev_file_id: 1,
        }
    }

    /// Find a node and its parent node from a file path.
    ///
    /// # Errors
    ///
    /// If the path is too long.
    pub fn lookup_node<'a>(
        &self,
        path: &'a winfsp::U16CStr,
        ancestor: Option<WinFspMemFsFileId>,
    ) -> FspResult<FoundNode<'a>> {
        let mut parent = WinFspMemFsFileId::ROOT;
        let mut name = winfsp::U16CString::new();
        let mut node = Some(WinFspMemFsFileId::ROOT);
        let path = path.as_slice();

        let mut len_so_far = 0;
        let mut len_last_seg = 0;
        let mut first = true;
        for c in path.split(|&c| c == u16::from(b'/') || c == u16::from(b'\\')) {
            if !first {
                // Previous separator:
                len_so_far += 1;
            }
            first = false;

            if !c.is_empty() {
                if 255 < c.len() {
                    // return Err(FspError::HRESULT(windows::Win32::Foundation::CO_E_PATHTOOLONG));
                    return Err(FspError::NTSTATUS(
                        windows::Win32::Foundation::STATUS_NAME_TOO_LONG.0,
                    )); // fuse.ENAMETOOLONG
                }
                let Some(n) = node else { break };
                parent = n;
                // TODO(perf): don't allocate a new string just to add a nul
                // byte at the end (change how we lookup paths instead)
                name = winfsp::U16CString::from_vec(c.to_vec())
                    .expect("path should not contain nul values");
                node = self
                    .files
                    .get(&n)
                    .unwrap_or_else(|| {
                        panic!("should not have invalid ids but had no data for {n:?}")
                    })
                    .children
                    .get(&name)
                    .copied();

                if matches!(ancestor, Some(ancestor) if ancestor == n) {
                    name = winfsp::U16CString::new(); // special case loop condition
                    break;
                }
            }

            len_so_far += len_last_seg;
            len_last_seg = c.len();
        }
        Ok(FoundNode {
            parent,
            parent_path: &path[..len_so_far],
            name,
            node,
        })
    }

    pub fn make_node(
        &mut self,
        path: &winfsp::U16CStr,
        is_dir: bool,
        data: Vec<u8>,
    ) -> FspResult<WinFspMemFsFileId> {
        let FoundNode {
            parent, name, node, ..
        } = self.lookup_node(path, None)?;
        if node.is_some() {
            return Err(FspError::IO(std::io::ErrorKind::AlreadyExists)); // fuse.EEXIST
        };
        self.prev_file_id = self.prev_file_id.checked_add(1).unwrap_or(2);
        let node_id = WinFspMemFsFileId(self.prev_file_id);
        let mut node = WinFspMemFsFile::new(node_id, is_dir);
        node.data = data;
        node.parent_id = Some(parent);
        let parent = self.files.get_mut(&parent).unwrap();
        parent.children.insert(name, node_id);
        self.files.insert(node_id, node);
        Ok(node_id)
    }

    pub fn remove_node(&mut self, path: &winfsp::U16CStr, is_dir: Option<bool>) -> FspResult {
        let FoundNode {
            parent, name, node, ..
        } = self.lookup_node(path, None)?;
        let Some(node) = node else {
            return Err(FspError::IO(std::io::ErrorKind::NotFound)); // -fuse.ENOENT
        };
        let node = self.files.get_mut(&node).unwrap();
        if let Some(is_dir) = is_dir {
            if !is_dir && node.is_dir {
                return Err(FspError::NTSTATUS(
                    windows::Win32::Foundation::STATUS_FILE_IS_A_DIRECTORY.0,
                )); // -fuse.EISDIR
            }
            if is_dir && !node.is_dir {
                return Err(FspError::NTSTATUS(
                    windows::Win32::Foundation::STATUS_NOT_A_DIRECTORY.0,
                )); // -fuse.ENOTDIR
            }
        }
        if !node.children.is_empty() {
            // return  Err(FspError::WIN32(windows::Win32::Foundation::ERROR_DIR_NOT_EMPTY));
            return Err(FspError::NTSTATUS(
                windows::Win32::Foundation::STATUS_DIRECTORY_NOT_EMPTY.0,
            )); // -fuse.ENOTEMPTY
        }
        node.parent_id = None;

        let can_delete = node.can_delete();
        let node_id = node.id;

        let parent = self.files.get_mut(&parent).unwrap();
        assert!(
            parent.children.remove(&name).is_some(),
            "parent should have found node as child"
        );
        if can_delete {
            self.files.remove(&node_id);
        }
        Ok(())
    }

    fn open_node(
        &mut self,
        path: &winfsp::U16CStr,
        is_dir: Option<bool>,
    ) -> FspResult<WinFspMemFsFileId> {
        let Some(node) = self.lookup_node(path, None)?.node else {
            return Err(FspError::IO(std::io::ErrorKind::NotFound)); // -fuse.ENOENT
        };
        let node = self.files.get_mut(&node).unwrap();
        if let Some(is_dir) = is_dir {
            if !is_dir && node.is_dir {
                return Err(FspError::NTSTATUS(
                    windows::Win32::Foundation::STATUS_FILE_IS_A_DIRECTORY.0,
                )); // -fuse.EISDIR
            }
            if is_dir && !node.is_dir {
                return Err(FspError::NTSTATUS(
                    windows::Win32::Foundation::STATUS_NOT_A_DIRECTORY.0,
                )); // -fuse.ENOTDIR
            }
        }
        node.open_count += 1;
        Ok(node.id)
    }

    fn close_node(&mut self, id: WinFspMemFsFileId) {
        let Some(file) = self.files.get_mut(&id) else {
            return;
        };
        file.open_count = file.open_count.saturating_sub(1);
        if file.can_delete() {
            self.files.remove(&id);
        }
    }

    pub fn get_node(
        &self,
        path: &winfsp::U16CStr,
        id: Option<WinFspMemFsFileId>,
    ) -> FspResult<Option<&WinFspMemFsFile>> {
        let id = if let Some(id) = id {
            id
        } else {
            match self.lookup_node(path, None)?.node {
                Some(id) => id,
                None => return Ok(None),
            }
        };
        Ok(self.files.get(&id))
    }
    pub fn get_node_mut(
        &mut self,
        path: &winfsp::U16CStr,
        id: Option<WinFspMemFsFileId>,
    ) -> FspResult<Option<&mut WinFspMemFsFile>> {
        let id = if let Some(id) = id {
            id
        } else {
            match self.lookup_node(path, None)?.node {
                Some(id) => id,
                None => return Ok(None),
            }
        };
        Ok(self.files.get_mut(&id))
    }
}
impl Default for WinFspMemFsContextState {
    fn default() -> Self {
        Self::new()
    }
}

/// Implements the in-memory file system logic.
#[derive(Debug, Clone, Default)]
pub struct WinFspMemFsContext {
    pub shared: Arc<Mutex<WinFspMemFsContextState>>,
}
impl WinFspMemFsContext {
    pub fn new() -> Self {
        WinFspMemFsContext {
            shared: Arc::new(Mutex::new(WinFspMemFsContextState::new())),
        }
    }
}

/// For documentation about methods in this trait see:
/// [winfsp/doc/WinFsp-API-winfsp.h.md at
/// 7551193ad754c76eeabb5b7499d622d78ab39771 ·
/// winfsp/winfsp](https://github.com/winfsp/winfsp/blob/7551193ad754c76eeabb5b7499d622d78ab39771/doc/WinFsp-API-winfsp.h.md)
impl FileSystemContext for WinFspMemFsContext {
    type FileContext = WinFspMemFsFileId;

    ////////////////////////////////////////////////////////////////////////////////
    // REQUIRED
    ////////////////////////////////////////////////////////////////////////////////

    fn get_security_by_name(
        &self,
        file_name: &winfsp::U16CStr,
        _security_descriptor: Option<&mut [libc::c_void]>,
        _reparse_point_resolver: impl FnOnce(&winfsp::U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        log::trace!("get_security_by_name [file_name={file_name:?}]");
        /*
        if let Some(security) = reparse_point_resolver(file_name) {
            return Ok(security);
        }
        */
        let guard = &mut *self.shared.lock().unwrap();
        let node = guard
            .lookup_node(file_name, None)?
            .node
            .map(|id| guard.files.get(&id).unwrap())
            .ok_or(FspError::IO(std::io::ErrorKind::NotFound))?;

        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: 0,
            attributes: node.file_attributes(),
            // attributes: node.map(|node| node.file_attributes()).unwrap_or(0),
        })
    }

    fn open(
        &self,
        file_name: &winfsp::U16CStr,
        create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        file_info: &mut winfsp::filesystem::OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        log::trace!("open [file_name={file_name:?}] [create_options={create_options:#x}]");
        let guard = &mut *self.shared.lock().unwrap();
        let id = guard.open_node(file_name, None)?;
        let node = guard.files.get(&id).unwrap();
        node.fill_in_file_info(file_info.as_mut());

        Ok(id)
    }

    fn close(&self, context: Self::FileContext) {
        log::trace!("close [context={context:?}]");
        let guard = &mut *self.shared.lock().unwrap();
        guard.close_node(context);
    }

    ////////////////////////////////////////////////////////////////////////////////
    // HAS DEFAULT
    ////////////////////////////////////////////////////////////////////////////////

    fn create(
        &self,
        file_name: &winfsp::U16CStr,
        create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        _file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        _security_descriptor: Option<&[libc::c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut winfsp::filesystem::OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        log::trace!("create");
        let guard = &mut *self.shared.lock().unwrap();
        let is_dir = create_options & windows::Wdk::Storage::FileSystem::FILE_DIRECTORY_FILE.0 != 0;
        let id = guard.make_node(file_name, is_dir, Vec::new())?;
        let node = guard.files.get(&id).unwrap();
        node.fill_in_file_info(file_info.as_mut());
        Ok(id)
    }

    fn cleanup(
        &self,
        context: &Self::FileContext,
        _file_name: Option<&winfsp::U16CStr>,
        _flags: u32,
    ) {
        log::trace!("cleanup [context={context:?}]");
    }

    fn flush(
        &self,
        context: Option<&Self::FileContext>,
        file_info: &mut winfsp::filesystem::FileInfo,
    ) -> winfsp::Result<()> {
        log::trace!("flush");
        let Some(context) = context else {
            // Flushed volume so don't need to return file info:
            return Ok(());
        };
        let guard = &mut *self.shared.lock().unwrap();
        let node = guard.files.get(context).unwrap();
        node.fill_in_file_info(file_info);
        Ok(())
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut winfsp::filesystem::FileInfo,
    ) -> winfsp::Result<()> {
        log::trace!("get_file_info");
        let guard = &mut *self.shared.lock().unwrap();
        let node = guard.files.get(context).unwrap();
        node.fill_in_file_info(file_info);

        Ok(())
    }

    fn get_security(
        &self,
        _context: &Self::FileContext,
        _security_descriptor: Option<&mut [libc::c_void]>,
    ) -> winfsp::Result<u64> {
        log::trace!("get_security");
        Err(windows::Win32::Foundation::STATUS_INVALID_DEVICE_REQUEST.into())
    }

    fn set_security(
        &self,
        _context: &Self::FileContext,
        _security_information: u32,
        _modification_descriptor: winfsp::filesystem::ModificationDescriptor,
    ) -> winfsp::Result<()> {
        log::trace!("set_security");
        Err(windows::Win32::Foundation::STATUS_INVALID_DEVICE_REQUEST.into())
    }

    fn overwrite(
        &self,
        context: &Self::FileContext,
        _file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        // If false then OR the old and new file attributes:
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut winfsp::filesystem::FileInfo,
    ) -> winfsp::Result<()> {
        log::trace!("overwrite");
        let guard = &mut *self.shared.lock().unwrap();
        let node = guard.files.get(context).unwrap();
        node.fill_in_file_info(file_info);
        Ok(())
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&winfsp::U16CStr>,
        marker: winfsp::filesystem::DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        log::trace!("read_directory");
        //Err(windows::Win32::Foundation::STATUS_INVALID_DEVICE_REQUEST.into())

        let guard = &mut *self.shared.lock().unwrap();
        let node = guard.files.get(context).unwrap();

        let dir_buffer = winfsp::filesystem::DirBuffer::new();
        if let Ok(dir_buffer) = dir_buffer.acquire(false, None) {
            for (name, child_id) in &node.children {
                let child_node = guard.files.get(child_id).unwrap();
                let mut info = <winfsp::filesystem::DirInfo>::new();
                info.set_name_raw(name.as_slice_with_nul())?;
                child_node.fill_in_file_info(info.file_info_mut());
                dir_buffer.write(&mut info)?;
            }
        }
        Ok(dir_buffer.read(marker, buffer))
    }

    fn rename(
        &self,
        context: &Self::FileContext,
        file_name: &winfsp::U16CStr,
        new_file_name: &winfsp::U16CStr,
        replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        log::trace!(
            "rename [old_name={file_name:?}] [new_name={new_file_name:?}] [replace={replace_if_exists}] [context={context:?}]"
        );
        let guard = &mut *self.shared.lock().unwrap();
        // TODO: gets capital letter new_file_name paths, need case insensitive search (maybe the bstr crate?)
        let destination = guard.lookup_node(new_file_name, None)?;

        // Remove existing file at destination:
        if let Some(dst_id) = destination.node {
            if !replace_if_exists {
                return Err(FspError::IO(std::io::ErrorKind::AlreadyExists));
            }
            let dst = guard.files.get_mut(&dst_id).unwrap();
            if !dst.children.is_empty() || dst_id == WinFspMemFsFileId::ROOT {
                // return Err(FspError::WIN32(windows::Win32::Foundation::ERROR_DIR_NOT_EMPTY));
                return Err(FspError::NTSTATUS(
                    windows::Win32::Foundation::STATUS_DIRECTORY_NOT_EMPTY.0,
                ));
            }
            dst.parent_id = None;
            if dst.can_delete() {
                guard.files.remove(&dst_id);
            }
        } else {
            // The target folder might not exist:
            let remaining_path = &new_file_name.as_slice()[destination.parent_path.len()..];
            log::trace!(
                "rename::remaining_path: {}",
                String::from_utf16_lossy(remaining_path)
            );
            if remaining_path.contains(&u16::from(b'\\'))
                || remaining_path.contains(&u16::from(b'/'))
            {
                return Err(FspError::NTSTATUS(
                    windows::Win32::Foundation::STATUS_NOT_A_DIRECTORY.0,
                ));
            }
            // Only the last path segment wasn't found...
        }
        // Update new parent to point to child:
        {
            let parent = guard.files.get_mut(&destination.parent).unwrap();
            parent.children.insert(destination.name, *context);
        }
        // Update child to point to new parent:
        {
            let node = guard.files.get_mut(context).unwrap();
            if let Some(parent_id) = node.parent_id.replace(destination.parent) {
                // Update previous parent to not point to this node:
                let parent = guard.files.get_mut(&parent_id).unwrap();
                parent.children.retain(|_name, id| id != context);
            }
        }
        Ok(())
    }

    fn set_basic_info(
        &self,
        _context: &Self::FileContext,
        _file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        _last_write_time: u64,
        _last_change_time: u64,
        _file_info: &mut winfsp::filesystem::FileInfo,
    ) -> winfsp::Result<()> {
        log::trace!("set_basic_info");
        Err(windows::Win32::Foundation::STATUS_INVALID_DEVICE_REQUEST.into())
    }

    fn set_delete(
        &self,
        _context: &Self::FileContext,
        _file_name: &winfsp::U16CStr,
        _delete_file: bool,
    ) -> winfsp::Result<()> {
        log::trace!("set_delete");
        Err(windows::Win32::Foundation::STATUS_INVALID_DEVICE_REQUEST.into())
    }

    fn set_file_size(
        &self,
        _context: &Self::FileContext,
        _new_size: u64,
        _set_allocation_size: bool,
        _file_info: &mut winfsp::filesystem::FileInfo,
    ) -> winfsp::Result<()> {
        log::trace!("set_file_size");
        Err(windows::Win32::Foundation::STATUS_INVALID_DEVICE_REQUEST.into())
    }

    fn read(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> winfsp::Result<u32> {
        log::trace!("read");
        let guard = &mut *self.shared.lock().unwrap();
        let node = guard.files.get(context).unwrap();

        // Data after offset:
        let data = &node.data[min(
            node.data.len(),
            usize::try_from(offset).unwrap_or(node.data.len()),
        )..];
        // Length to copy to buffer:
        let len = min(
            min(data.len(), buffer.len()),
            usize::try_from(u32::MAX).unwrap_or(usize::MAX),
        );
        // Copy data:
        buffer[..len].copy_from_slice(&data[..len]);

        // We ensured len is less or equal to u32::MAX
        Ok(len as u32)
    }

    fn write(
        &self,
        context: &Self::FileContext,
        // Data to write:
        buffer: &[u8],
        // Start write at:
        offset: u64,
        // Append:
        write_to_eof: bool,
        // Don't grow:
        constrained_io: bool,
        // New file info:
        file_info: &mut winfsp::filesystem::FileInfo,
    ) -> winfsp::Result<u32> {
        log::trace!("write");
        let guard = &mut *self.shared.lock().unwrap();
        let node = guard.files.get_mut(context).unwrap();

        let mut written: u32 = 0;
        if !write_to_eof {
            // Overwrite previous content:
            let node_data_len = node.data.len();
            let tail = &mut node.data[min(offset as usize, node_data_len)..];
            let len = min(buffer.len(), tail.len());
            tail[..len].copy_from_slice(&buffer[..len]);
            written += len as u32;
        }
        if constrained_io {
            return Ok(written);
        }
        let buffer_tail = &buffer[(written as usize)..];
        node.data.extend_from_slice(buffer_tail);

        node.fill_in_file_info(file_info);

        Ok(buffer.len() as u32)
    }

    fn get_dir_info_by_name(
        &self,
        _context: &Self::FileContext,
        _file_name: &winfsp::U16CStr,
        _out_dir_info: &mut winfsp::filesystem::DirInfo,
    ) -> winfsp::Result<()> {
        log::trace!("get_dir_info_by_name");
        Err(windows::Win32::Foundation::STATUS_INVALID_DEVICE_REQUEST.into())
    }

    fn get_volume_info(
        &self,
        out_volume_info: &mut winfsp::filesystem::VolumeInfo,
    ) -> winfsp::Result<()> {
        log::trace!("get_volume_info");
        out_volume_info.set_volume_label("WinFsp MemFs");

        out_volume_info.total_size = 1_000_000_000; // 1 GB
        let guard = &mut *self.shared.lock().unwrap();
        let used_space = guard
            .files
            .values()
            .map(|file| file.data.len() as u64)
            .sum();
        out_volume_info.free_size = out_volume_info.total_size.saturating_sub(used_space);
        log::trace!("get_volume_info::free_size={}", out_volume_info.free_size);
        Ok(())
    }

    fn set_volume_label(
        &self,
        volume_label: &winfsp::U16CStr,
        _volume_info: &mut winfsp::filesystem::VolumeInfo,
    ) -> winfsp::Result<()> {
        log::trace!("set_volume_label [new_name={volume_label:?}]");
        Err(windows::Win32::Foundation::STATUS_INVALID_DEVICE_REQUEST.into())
    }

    fn get_stream_info(
        &self,
        _context: &Self::FileContext,
        _buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        log::trace!("get_stream_info");
        Err(windows::Win32::Foundation::STATUS_INVALID_DEVICE_REQUEST.into())
    }

    fn get_reparse_point_by_name(
        &self,
        _file_name: &winfsp::U16CStr,
        _is_directory: bool,
        _buffer: &mut [u8],
    ) -> winfsp::Result<u64> {
        log::trace!("get_reparse_point_by_name");
        Err(windows::Win32::Foundation::STATUS_INVALID_DEVICE_REQUEST.into())
    }

    fn get_reparse_point(
        &self,
        _context: &Self::FileContext,
        _file_name: &winfsp::U16CStr,
        _buffer: &mut [u8],
    ) -> winfsp::Result<u64> {
        log::trace!("get_reparse_point");
        Err(windows::Win32::Foundation::STATUS_INVALID_DEVICE_REQUEST.into())
    }

    fn set_reparse_point(
        &self,
        _context: &Self::FileContext,
        _file_name: &winfsp::U16CStr,
        _buffer: &[u8],
    ) -> winfsp::Result<()> {
        log::trace!("set_reparse_point");
        Err(windows::Win32::Foundation::STATUS_INVALID_DEVICE_REQUEST.into())
    }

    fn delete_reparse_point(
        &self,
        _context: &Self::FileContext,
        _file_name: &winfsp::U16CStr,
        _buffer: &[u8],
    ) -> winfsp::Result<()> {
        log::trace!("delete_reparse_point");
        Err(windows::Win32::Foundation::STATUS_INVALID_DEVICE_REQUEST.into())
    }

    fn get_extended_attributes(
        &self,
        _context: &Self::FileContext,
        _buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        log::trace!("get_extended_attributes");
        Err(windows::Win32::Foundation::STATUS_INVALID_DEVICE_REQUEST.into())
    }

    fn set_extended_attributes(
        &self,
        _context: &Self::FileContext,
        _buffer: &[u8],
        _file_info: &mut winfsp::filesystem::FileInfo,
    ) -> winfsp::Result<()> {
        log::trace!("set_extended_attributes");
        Err(windows::Win32::Foundation::STATUS_INVALID_DEVICE_REQUEST.into())
    }

    fn control(
        &self,
        _context: &Self::FileContext,
        _control_code: u32,
        _input: &[u8],
        _output: &mut [u8],
    ) -> winfsp::Result<u32> {
        log::trace!("control");
        Err(windows::Win32::Foundation::STATUS_INVALID_DEVICE_REQUEST.into())
    }

    fn dispatcher_stopped(&self, _normally: bool) {
        log::trace!("dispatcher_stopped");
    }
}
