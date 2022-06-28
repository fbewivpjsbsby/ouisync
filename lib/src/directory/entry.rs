use super::{
    entry_data::{EntryData, EntryDirectoryData, EntryFileData, EntryTombstoneData},
    inner::Inner,
    parent_context::ParentContext,
    Directory, Mode,
};
use crate::{
    blob_id::BlobId,
    branch::Branch,
    crypto::sign::PublicKey,
    db,
    error::{Error, Result},
    file::File,
    locator::Locator,
    version_vector::VersionVector,
};
use std::fmt;

/// Info about a directory entry.
#[derive(Copy, Clone, Debug)]
pub enum EntryRef<'a> {
    File(FileRef<'a>),
    Directory(DirectoryRef<'a>),
    Tombstone(TombstoneRef<'a>),
}

impl<'a> EntryRef<'a> {
    pub(super) fn new(
        parent_outer: &'a Directory,
        parent_inner: &'a Inner,
        name: &'a str,
        entry_data: &'a EntryData,
    ) -> Self {
        let inner = RefInner {
            parent_outer,
            parent_inner,
            name,
        };

        match entry_data {
            EntryData::File(entry_data) => Self::File(FileRef { entry_data, inner }),
            EntryData::Directory(entry_data) => Self::Directory(DirectoryRef { entry_data, inner }),
            EntryData::Tombstone(entry_data) => Self::Tombstone(TombstoneRef { entry_data, inner }),
        }
    }

    pub fn name(&self) -> &'a str {
        match self {
            Self::File(r) => r.name(),
            Self::Directory(r) => r.name(),
            Self::Tombstone(r) => r.name(),
        }
    }

    pub fn version_vector(&self) -> &'a VersionVector {
        match self {
            Self::File(f) => f.version_vector(),
            Self::Directory(d) => d.version_vector(),
            Self::Tombstone(t) => t.version_vector(),
        }
    }

    pub fn file(self) -> Result<FileRef<'a>> {
        match self {
            Self::File(r) => Ok(r),
            Self::Directory(_) => Err(Error::EntryIsDirectory),
            Self::Tombstone(_) => Err(Error::EntryNotFound),
        }
    }

    pub fn directory(self) -> Result<DirectoryRef<'a>> {
        match self {
            Self::File(_) => Err(Error::EntryIsFile),
            Self::Directory(r) => Ok(r),
            Self::Tombstone(_) => Err(Error::EntryNotFound),
        }
    }

    pub fn is_file(&self) -> bool {
        matches!(self, Self::File(_))
    }

    pub fn is_directory(&self) -> bool {
        matches!(self, Self::Directory(_))
    }

    pub fn is_tombstone(&self) -> bool {
        matches!(self, Self::Tombstone(_))
    }

    pub fn branch_id(&self) -> &PublicKey {
        self.inner().branch_id()
    }

    pub fn parent(&self) -> &Directory {
        self.inner().parent_outer
    }

    pub(crate) fn clone_data(&self) -> EntryData {
        match self {
            Self::File(e) => EntryData::File(e.data().clone()),
            Self::Directory(e) => EntryData::Directory(e.data().clone()),
            Self::Tombstone(e) => EntryData::Tombstone(e.data().clone()),
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        match self {
            Self::File(e) => e.is_open(),
            Self::Directory(_) | Self::Tombstone(_) => false,
        }
    }

    fn inner(&self) -> &RefInner {
        match self {
            Self::File(r) => &r.inner,
            Self::Directory(r) => &r.inner,
            Self::Tombstone(r) => &r.inner,
        }
    }
}

#[derive(Copy, Clone)]
pub struct FileRef<'a> {
    entry_data: &'a EntryFileData,
    inner: RefInner<'a>,
}

impl<'a> FileRef<'a> {
    pub fn name(&self) -> &'a str {
        self.inner.name
    }

    pub(crate) fn locator(&self) -> Locator {
        Locator::head(*self.blob_id())
    }

    pub(crate) fn blob_id(&self) -> &'a BlobId {
        &self.entry_data.blob_id
    }

    pub fn version_vector(&self) -> &'a VersionVector {
        &self.entry_data.version_vector
    }

    pub async fn open(&self, conn: &mut db::Connection) -> Result<File> {
        let shared = self.branch().fetch_blob_shared(*self.blob_id());
        let parent_context = self.inner.parent_context();
        let branch = self.inner.parent_inner.blob.branch().clone();
        let locator = self.locator();

        File::open(conn, branch, locator, parent_context, shared).await
    }

    pub fn branch(&self) -> &Branch {
        self.inner.branch()
    }

    pub fn parent(&self) -> &Directory {
        self.inner.parent_outer
    }

    pub(crate) fn data(&self) -> &EntryFileData {
        self.entry_data
    }

    pub(crate) fn is_open(&self) -> bool {
        self.branch().is_blob_open(self.blob_id())
    }
}

impl fmt::Debug for FileRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("FileRef")
            .field("name", &self.inner.name)
            .field("vv", &self.entry_data.version_vector)
            .field("locator", &self.locator())
            .finish()
    }
}

#[derive(Copy, Clone)]
pub struct DirectoryRef<'a> {
    entry_data: &'a EntryDirectoryData,
    inner: RefInner<'a>,
}

impl<'a> DirectoryRef<'a> {
    pub fn name(&self) -> &'a str {
        self.inner.name
    }

    pub(crate) fn locator(&self) -> Locator {
        Locator::head(*self.blob_id())
    }

    pub(crate) fn blob_id(&self) -> &'a BlobId {
        &self.entry_data.blob_id
    }

    pub(crate) async fn open(&self, conn: &mut db::Connection) -> Result<Directory> {
        match self.inner.parent_outer.mode {
            Mode::ReadWrite => self.open_read_write(conn).await,
            Mode::ReadOnly => self.open_read_only(conn).await,
        }
    }

    async fn open_read_write(&self, conn: &mut db::Connection) -> Result<Directory> {
        self.inner
            .parent_inner
            .open_directories
            .open(
                conn,
                self.branch().clone(),
                self.locator(),
                self.inner.parent_context(),
            )
            .await
    }

    async fn open_read_only(&self, conn: &mut db::Connection) -> Result<Directory> {
        Directory::open(
            conn,
            self.branch().clone(),
            self.locator(),
            Some(self.inner.parent_context()),
            Mode::ReadOnly,
        )
        .await
    }

    pub fn branch(&self) -> &'a Branch {
        self.inner.branch()
    }

    pub(crate) fn data(&self) -> &EntryDirectoryData {
        self.entry_data
    }

    pub fn version_vector(&self) -> &'a VersionVector {
        &self.entry_data.version_vector
    }
}

impl fmt::Debug for DirectoryRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("DirectoryRef")
            .field("name", &self.inner.name)
            .finish()
    }
}

#[derive(Copy, Clone)]
pub struct TombstoneRef<'a> {
    entry_data: &'a EntryTombstoneData,
    inner: RefInner<'a>,
}

impl<'a> TombstoneRef<'a> {
    pub fn name(&self) -> &'a str {
        self.inner.name
    }

    pub fn branch_id(&self) -> &'a PublicKey {
        self.inner.branch_id()
    }

    pub fn data(&self) -> &EntryTombstoneData {
        self.entry_data
    }

    pub fn version_vector(&self) -> &'a VersionVector {
        &self.entry_data.version_vector
    }
}

impl fmt::Debug for TombstoneRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("TombstoneRef")
            .field("vv", &self.entry_data.version_vector)
            .finish()
    }
}

#[derive(Copy, Clone)]
struct RefInner<'a> {
    parent_outer: &'a Directory,
    parent_inner: &'a Inner,
    name: &'a str,
}

impl<'a> RefInner<'a> {
    fn parent_context(&self) -> ParentContext {
        ParentContext::new(self.parent_outer.clone(), self.name.into())
    }

    fn branch(&self) -> &'a Branch {
        self.parent_inner.blob.branch()
    }

    pub fn branch_id(&self) -> &'a PublicKey {
        self.parent_inner.blob.branch().id()
    }
}

impl PartialEq for RefInner<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.parent_inner.blob.branch().id() == other.parent_inner.blob.branch().id()
            && self.parent_inner.blob.locator() == other.parent_inner.blob.locator()
            && self.name == other.name
    }
}

impl Eq for RefInner<'_> {}
