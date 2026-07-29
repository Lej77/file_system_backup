use std::{sync::Arc, time::SystemTime};

use dav_server::{
    davpath::DavPath,
    fs::{
        DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsResult, FsStream,
        OpenOptions, ReadDirMeta,
    },
};
use futures_util::{future, future::FutureExt};

use super::{AutoCompressedFsIndex, FsEntryMetadata};

#[derive(Debug, Clone)]
struct IndexMetadata {
    mtime: SystemTime,
    crtime: SystemTime,
    is_dir: bool,
    size: u64,
}
impl From<FsEntryMetadata> for IndexMetadata {
    fn from(info: FsEntryMetadata) -> Self {
        IndexMetadata {
            mtime: info
                .modified_as_system_time()
                .unwrap_or(SystemTime::UNIX_EPOCH),
            crtime: info
                .modified_as_system_time()
                .unwrap_or(SystemTime::UNIX_EPOCH),
            is_dir: info.is_dir,
            size: info.size,
        }
    }
}
impl DavMetaData for IndexMetadata {
    fn len(&self) -> u64 {
        if self.is_dir { 0 } else { self.size }
    }

    fn created(&self) -> FsResult<SystemTime> {
        Ok(self.crtime)
    }

    fn modified(&self) -> FsResult<SystemTime> {
        Ok(self.mtime)
    }

    fn is_dir(&self) -> bool {
        self.is_dir
    }
}
impl DavFile for IndexMetadata {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        future::ok(Box::new(self.clone()) as Box<dyn DavMetaData>).boxed()
    }

    fn write_buf(&mut self, _buf: Box<dyn bytes::Buf + Send>) -> FsFuture<'_, ()> {
        future::err(FsError::Forbidden).boxed()
    }

    fn write_bytes(&mut self, _buf: bytes::Bytes) -> FsFuture<'_, ()> {
        future::err(FsError::Forbidden).boxed()
    }

    fn read_bytes(&mut self, _count: usize) -> FsFuture<'_, bytes::Bytes> {
        future::err(FsError::Forbidden).boxed()
    }

    fn seek(&mut self, _pos: std::io::SeekFrom) -> FsFuture<'_, u64> {
        future::err(FsError::Forbidden).boxed()
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        future::err(FsError::Forbidden).boxed()
    }
}
struct IndexDirEntry {
    meta: IndexMetadata,
    name: Vec<u8>,
}
impl DavDirEntry for IndexDirEntry {
    fn metadata(&self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta = self.meta.clone();
        future::ok(Box::new(meta) as Box<dyn DavMetaData>).boxed()
    }

    fn name(&self) -> Vec<u8> {
        self.name.clone()
    }
}

#[derive(Clone)]
pub struct FileSystem {
    pub index: Arc<AutoCompressedFsIndex>,
}
impl DavFileSystem for FileSystem {
    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        _options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        log::trace!("open [path={path:?}]");
        let is_dir = path.is_collection();
        let Ok(path) = std::str::from_utf8(path.as_bytes()) else {
            return future::err(FsError::NotFound).boxed();
        };
        let Ok(path) = urlencoding::decode(path) else {
            return future::err(FsError::NotFound).boxed();
        };
        let Some(info) = self.index.get_metadata(&path, is_dir.then_some(true)) else {
            return future::err(FsError::NotFound).boxed();
        };
        future::ok(Box::new(IndexMetadata::from(info)) as Box<dyn DavFile>).boxed()
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        log::trace!("read_dir [path={path:?}]");
        let Ok(path) = std::str::from_utf8(path.as_bytes()) else {
            return future::err(FsError::NotFound).boxed();
        };
        let Ok(path) = urlencoding::decode(path) else {
            return future::err(FsError::NotFound).boxed();
        };
        let mut v: Vec<Result<Box<dyn DavDirEntry>, FsError>> = Vec::new();
        self.index.get_directory_info(&path, |name, entry| {
            v.push(Ok(Box::new(IndexDirEntry {
                meta: IndexMetadata::from(entry),
                name: name.as_bytes().to_vec(),
            })));
        });
        log::trace!(
            "read_dir::entries_len={}, entries={:?}",
            v.len(),
            v.iter()
                .map(|v| String::from_utf8(v.as_ref().unwrap().name()).unwrap_or_default())
                .collect::<Vec<_>>()
        );

        let strm = futures_util::stream::iter(v);
        let strm: FsStream<Box<dyn DavDirEntry>> = Box::pin(strm);
        future::ok(strm).boxed()
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        log::trace!("metadata [path={path:?}]");
        let is_dir = path.is_collection();
        let Ok(path) = std::str::from_utf8(path.as_bytes()) else {
            return future::err(FsError::NotFound).boxed();
        };
        let Ok(path) = urlencoding::decode(path) else {
            return future::err(FsError::NotFound).boxed();
        };
        let Some(info) = self.index.get_metadata(&path, is_dir.then_some(true)) else {
            return future::err(FsError::NotFound).boxed();
        };
        log::trace!("metadata [path={path:?}] success with info={info:?}");
        future::ok(Box::new(IndexMetadata::from(info)) as Box<dyn DavMetaData>).boxed()
    }

    fn get_quota(&self) -> FsFuture<'_, (u64, Option<u64>)> {
        let size = self.index.root().size;
        future::ok((size, None)).boxed()
    }

    fn have_props<'a>(
        &'a self,
        _path: &'a DavPath,
    ) -> std::pin::Pin<Box<dyn futures_util::Future<Output = bool> + Send + 'a>> {
        log::trace!("have_props");
        future::ready(false).boxed()
    }

    fn patch_props<'a>(
        &'a self,
        _path: &'a DavPath,
        _patch: Vec<(bool, dav_server::fs::DavProp)>,
    ) -> FsFuture<'a, Vec<(http::StatusCode, dav_server::fs::DavProp)>> {
        log::trace!("patch_props");
        future::err(FsError::NotImplemented).boxed()
    }

    fn get_props<'a>(
        &'a self,
        _path: &'a DavPath,
        _do_content: bool,
    ) -> FsFuture<'a, Vec<dav_server::fs::DavProp>> {
        log::trace!("get_props");
        future::err(FsError::NotImplemented).boxed()
    }

    fn get_prop<'a>(
        &'a self,
        _path: &'a DavPath,
        _prop: dav_server::fs::DavProp,
    ) -> FsFuture<'a, Vec<u8>> {
        log::trace!("get_prop");
        future::err(FsError::NotImplemented).boxed()
    }
}
