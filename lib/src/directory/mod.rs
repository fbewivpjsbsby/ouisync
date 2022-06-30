mod content;
mod entry;
mod entry_data;
mod entry_type;
mod parent_context;
#[cfg(test)]
mod tests;

pub use self::{
    entry::{DirectoryRef, EntryRef, FileRef},
    entry_type::EntryType,
};
pub(crate) use self::{entry_data::EntryData, parent_context::ParentContext};

use self::content::Content;
use crate::{
    blob::{Blob, Shared},
    branch::Branch,
    crypto::sign::PublicKey,
    db,
    debug_printer::DebugPrinter,
    error::{Error, Result},
    file::File,
    locator::Locator,
    version_vector::VersionVector,
};
use async_recursion::async_recursion;
use sqlx::Connection;
use std::mem;

/// Directory access mode
///
/// Note: when a directory is opening in `ReadOnly` mode, it also bypasses the cache. This is
/// because the purpose of the cache is to make sure all instances of the same directory are in
/// sync, but when the directory is read-only, it's not possible to perform any modifications on it
/// that would affect the other instances so the cache is unnecessary. This is also useful when one
/// needs to open the most up to date version of the directory.
#[derive(Clone, Copy)]
pub(crate) enum Mode {
    ReadOnly,
    ReadWrite,
}

/// What to do with the existing entry when inserting a new entry in its place.
pub(crate) enum OverwriteStrategy {
    // Remove it
    Remove,
    // Keep it (useful when inserting a tombstone oven an entry which is to be moved somewhere
    // else)
    Keep,
}

#[derive(Clone)]
pub struct Directory {
    mode: Mode,
    blob: Blob,
    parent: Option<ParentContext>,
    entries: Content,
}

#[allow(clippy::len_without_is_empty)]
impl Directory {
    /// Opens the root directory.
    /// For internal use only. Use [`Branch::open_root`] instead.
    pub(crate) async fn open_root(
        conn: &mut db::Connection,
        owner_branch: Branch,
        mode: Mode,
    ) -> Result<Self> {
        Self::open(conn, owner_branch, Locator::ROOT, None, mode).await
    }

    /// Opens the root directory or creates it if it doesn't exist.
    /// For internal use only. Use [`Branch::open_or_create_root`] instead.
    pub(crate) async fn open_or_create_root(
        conn: &mut db::Connection,
        branch: Branch,
    ) -> Result<Self> {
        // TODO: make sure this is atomic
        let locator = Locator::ROOT;

        match Self::open(conn, branch.clone(), locator, None, Mode::ReadWrite).await {
            Ok(dir) => Ok(dir),
            Err(Error::EntryNotFound) => Ok(Self::create(branch, locator, None)),
            Err(error) => Err(error),
        }
    }

    /// Reloads this directory from the db.
    #[cfg(test)]
    pub(crate) async fn refresh(&mut self, conn: &mut db::Connection) -> Result<()> {
        let Self {
            mode,
            blob,
            parent,
            entries,
        } = Self::open(
            conn,
            self.branch().clone(),
            *self.locator(),
            self.parent.as_ref().cloned(),
            self.mode,
        )
        .await?;

        self.mode = mode;
        self.blob = blob;
        self.parent = parent;
        self.entries = entries;

        Ok(())
    }

    /// Lookup an entry of this directory by name.
    pub fn lookup(&self, name: &'_ str) -> Result<EntryRef> {
        self.entries
            .get_key_value(name)
            .map(|(name, data)| EntryRef::new(self, name, data))
            .ok_or(Error::EntryNotFound)
    }

    /// Returns iterator over the entries of this directory.
    pub fn entries(&self) -> impl Iterator<Item = EntryRef> + DoubleEndedIterator + Clone {
        self.entries
            .iter()
            .map(move |(name, data)| EntryRef::new(self, name, data))
    }

    /// Creates a new file inside this directory.
    pub async fn create_file(&mut self, conn: &mut db::Connection, name: String) -> Result<File> {
        let mut tx = conn.begin().await?;

        let blob_id = rand::random();
        let data = EntryData::file(blob_id, VersionVector::first(*self.branch().id()));
        let parent =
            ParentContext::new(*self.locator().blob_id(), name.clone(), self.parent.clone());
        let shared = self.branch().fetch_blob_shared(blob_id);

        let mut file = File::create(
            self.branch().clone(),
            Locator::head(blob_id),
            parent,
            shared,
        );

        let mut content = self.load(&mut tx).await?;
        content.insert(self.branch(), name, data)?;
        file.save(&mut tx).await?;
        self.save(&mut tx, &content, OverwriteStrategy::Remove)
            .await?;
        self.commit(tx, content, VersionVector::new()).await?;

        Ok(file)
    }

    /// Creates a new subdirectory of this directory.
    pub async fn create_directory(
        &mut self,
        conn: &mut db::Connection,
        name: String,
    ) -> Result<Self> {
        let mut tx = conn.begin().await?;

        let blob_id = rand::random();
        let data = EntryData::directory(blob_id, VersionVector::first(*self.branch().id()));
        let parent =
            ParentContext::new(*self.locator().blob_id(), name.clone(), self.parent.clone());

        let mut dir =
            Directory::create(self.branch().clone(), Locator::head(blob_id), Some(parent));

        let mut content = self.load(&mut tx).await?;
        content.insert(self.branch(), name, data)?;
        dir.save(&mut tx, &Content::empty(), OverwriteStrategy::Remove)
            .await?;
        self.save(&mut tx, &content, OverwriteStrategy::Remove)
            .await?;
        self.commit(tx, content, VersionVector::new()).await?;

        Ok(dir)
    }

    /// Removes a file or subdirectory from this directory. If the entry to be removed is a
    /// directory, it needs to be empty or a `DirectoryNotEmpty` error is returned.
    ///
    /// Note: This operation does not simply remove the entry, instead, version vector of the local
    /// entry with the same name is increased to be "happens after" `vv`. If the local version does
    /// not exist, or if it is the one being removed (branch_id == self.branch_id), then a
    /// tombstone is created.
    pub async fn remove_entry(
        &mut self,
        conn: &mut db::Connection,
        name: &str,
        branch_id: &PublicKey,
        vv: VersionVector,
    ) -> Result<()> {
        let tx = conn.begin().await?;
        self.remove_entry_with_overwrite_strategy(
            tx,
            name,
            branch_id,
            vv,
            OverwriteStrategy::Remove,
        )
        .await
    }

    /// Adds a tombstone to where the entry is being moved from and creates a new entry at the
    /// destination.
    ///
    /// Note on why we're passing the `src_data` to the function instead of just looking it up
    /// using `src_name`: it's because the version vector inside of the `src_data` is expected to
    /// be the one the caller of this function is trying to move. It could, in theory, happen that
    /// the source entry has been modified between when the caller last released the lock to the
    /// entry and when we would do the lookup.
    ///
    /// Thus using the "caller provided" version vector, we ensure that we don't accidentally
    /// delete data.
    ///
    /// To move an entry within the same directory, clone `self` and pass it as `dst_dir`.
    ///
    /// # Cancel safety
    ///
    /// This function is partially cancel safe in the sense that it will never lose data. However,
    /// in the case of cancellation, it can happen that the entry ends up being already inserted
    /// into the destination but not yet removed from the source. This will behave similarly to a
    /// hard link with the important difference that removing the entry from either location  would
    /// leave it dangling in the other. The only way to currently resolve the situation without
    /// removing the data is to copy it somewhere else, then remove both source and destination
    /// entries and then move the data back.
    ///
    /// TODO: Improve cancel-safety either by making the whole operation atomic or by implementing
    /// proper hard links.
    pub(crate) async fn move_entry(
        &mut self,
        conn: &mut db::Connection,
        src_name: &str,
        src_data: EntryData,
        dst_dir: &mut Directory,
        dst_name: &str,
        dst_vv: VersionVector,
    ) -> Result<()> {
        let mut dst_data = src_data;
        let src_vv = mem::replace(dst_data.version_vector_mut(), dst_vv);

        {
            let tx = conn.begin().await?;
            dst_dir
                .insert_entry_with_overwrite_strategy(
                    tx,
                    dst_name.to_owned(),
                    dst_data,
                    OverwriteStrategy::Remove,
                )
                .await?;
        }

        {
            let tx = conn.begin().await?;
            let branch_id = *self.branch().id();
            self.remove_entry_with_overwrite_strategy(
                tx,
                src_name,
                &branch_id,
                src_vv,
                OverwriteStrategy::Keep,
            )
            .await?;
        }

        Ok(())
    }

    // Forks this directory into the local branch.
    // TODO: change this method to modify self instead of returning a new instance.
    #[async_recursion]
    pub(crate) async fn fork(
        &self,
        conn: &mut db::Connection,
        local_branch: &Branch,
    ) -> Result<Directory> {
        if local_branch.id() == self.branch().id() {
            return Ok(self.clone());
        }

        let parent = if let Some(parent) = &self.parent {
            let dir = parent.directory(conn, self.branch().clone()).await?;
            let entry_name = parent.entry_name().to_owned();

            Some((dir, entry_name))
        } else {
            None
        };

        if let Some((parent_dir, entry_name)) = parent {
            let mut parent_dir = parent_dir.fork(conn, local_branch).await?;

            match parent_dir.lookup(&entry_name) {
                Ok(EntryRef::Directory(entry)) => entry.open(conn).await,
                Ok(EntryRef::File(_)) => {
                    // TODO: return some kind of `Error::Conflict`
                    Err(Error::EntryIsFile)
                }
                Ok(EntryRef::Tombstone(_)) | Err(Error::EntryNotFound) => {
                    parent_dir.create_directory(conn, entry_name).await
                }
                Err(error) => Err(error),
            }
        } else {
            local_branch.open_or_create_root(conn).await
        }
    }

    /// Updates the version vector of this directory by merging it with `vv`.
    pub(crate) async fn bump(
        &mut self,
        conn: &mut db::Connection,
        vv: VersionVector,
    ) -> Result<()> {
        let tx = conn.begin().await?;
        self.commit(tx, Content::empty(), vv).await
    }

    pub async fn parent(&self, conn: &mut db::Connection) -> Result<Option<Directory>> {
        if let Some(parent) = &self.parent {
            Ok(Some(parent.directory(conn, self.branch().clone()).await?))
        } else {
            Ok(None)
        }
    }

    async fn open(
        conn: &mut db::Connection,
        owner_branch: Branch,
        locator: Locator,
        parent: Option<ParentContext>,
        mode: Mode,
    ) -> Result<Self> {
        let (blob, entries) = load(conn, owner_branch, locator).await?;

        Ok(Self {
            mode,
            blob,
            parent,
            entries,
        })
    }

    fn create(owner_branch: Branch, locator: Locator, parent: Option<ParentContext>) -> Self {
        let blob = Blob::create(owner_branch, locator, Shared::uninit());

        Directory {
            mode: Mode::ReadWrite,
            blob,
            parent,
            entries: Content::empty(),
        }
    }

    #[async_recursion]
    pub async fn debug_print(&self, conn: &mut db::Connection, print: DebugPrinter) {
        for (name, entry_data) in &self.entries {
            print.display(&format_args!("{:?}: {:?}", name, entry_data));

            match entry_data {
                EntryData::File(file_data) => {
                    let print = print.indent();

                    let parent_context = ParentContext::new(
                        *self.locator().blob_id(),
                        name.into(),
                        self.parent.clone(),
                    );

                    let file = File::open(
                        conn,
                        self.blob.branch().clone(),
                        Locator::head(file_data.blob_id),
                        parent_context,
                        Shared::uninit(),
                    )
                    .await;

                    match file {
                        Ok(mut file) => {
                            let mut buf = [0; 32];
                            let lenght_result = file.read(conn, &mut buf).await;
                            match lenght_result {
                                Ok(length) => {
                                    let file_len = file.len().await;
                                    let ellipsis = if file_len > length as u64 { ".." } else { "" };
                                    print.display(&format!(
                                        "Content: {:?}{}",
                                        std::str::from_utf8(&buf[..length]),
                                        ellipsis
                                    ));
                                }
                                Err(e) => {
                                    print.display(&format!("Failed to read {:?}", e));
                                }
                            }
                        }
                        Err(e) => {
                            print.display(&format!("Failed to open {:?}", e));
                        }
                    }
                }
                EntryData::Directory(data) => {
                    let print = print.indent();

                    let parent_context = ParentContext::new(
                        *self.locator().blob_id(),
                        name.into(),
                        self.parent.clone(),
                    );

                    let dir = Directory::open(
                        conn,
                        self.blob.branch().clone(),
                        Locator::head(data.blob_id),
                        Some(parent_context),
                        Mode::ReadOnly,
                    )
                    .await;

                    match dir {
                        Ok(dir) => {
                            dir.debug_print(conn, print).await;
                        }
                        Err(e) => {
                            print.display(&format!("Failed to open {:?}", e));
                        }
                    }
                }
                EntryData::Tombstone(_) => {}
            }
        }
    }

    /// Branch of this directory
    pub fn branch(&self) -> &Branch {
        self.blob.branch()
    }

    /// Locator of this directory
    pub(crate) fn locator(&self) -> &Locator {
        self.blob.locator()
    }

    /// Length of this directory in bytes. Does not include the content, only the size of directory
    /// itself.
    pub async fn len(&self) -> u64 {
        self.blob.len().await
    }

    pub(crate) async fn version_vector(&self, conn: &mut db::Connection) -> Result<VersionVector> {
        if let Some(parent) = &self.parent {
            parent
                .entry_version_vector(conn, self.branch().clone())
                .await
        } else {
            self.branch().data().load_version_vector(conn).await
        }
    }

    async fn remove_entry_with_overwrite_strategy(
        &mut self,
        mut tx: db::Transaction<'_>,
        name: &str,
        branch_id: &PublicKey,
        vv: VersionVector,
        overwrite: OverwriteStrategy,
    ) -> Result<()> {
        // If we are removing a directory, ensure it's empty (recursive removal can still be
        // implemented at the upper layers).
        let old_dir = match self.lookup(name) {
            Ok(EntryRef::Directory(entry)) => Some(entry.open(&mut tx).await?),
            Ok(_) | Err(Error::EntryNotFound) => None,
            Err(error) => return Err(error),
        };

        if let Some(dir) = &old_dir {
            if matches!(overwrite, OverwriteStrategy::Remove)
                && dir.entries().any(|entry| !entry.is_tombstone())
            {
                return Err(Error::DirectoryNotEmpty);
            }
        }

        let new_entry = if branch_id == self.branch().id() {
            EntryData::tombstone(vv.incremented(*self.branch().id()))
        } else {
            match self.lookup(name) {
                Ok(old_entry) => {
                    let mut new_entry = old_entry.clone_data();
                    new_entry.version_vector_mut().merge(&vv);
                    new_entry
                }
                Err(Error::EntryNotFound) => {
                    EntryData::tombstone(vv.incremented(*self.branch().id()))
                }
                Err(e) => return Err(e),
            }
        };

        self.insert_entry_with_overwrite_strategy(tx, name.to_owned(), new_entry, overwrite)
            .await
    }

    /// Atomically inserts the given entry into this directory and commits the change.
    async fn insert_entry_with_overwrite_strategy(
        &mut self,
        mut tx: db::Transaction<'_>,
        name: String,
        entry: EntryData,
        overwrite: OverwriteStrategy,
    ) -> Result<()> {
        let mut content = self.load(&mut tx).await?;
        content.insert(self.branch(), name, entry)?;
        self.save(&mut tx, &content, overwrite).await?;
        self.commit(tx, content, VersionVector::new()).await
    }

    async fn load(&mut self, conn: &mut db::Connection) -> Result<Content> {
        if self.blob.is_dirty() {
            Ok(self.entries.clone())
        } else {
            let (blob, content) = load(conn, self.branch().clone(), *self.locator()).await?;
            self.blob = blob;
            Ok(content)
        }
    }

    async fn save(
        &mut self,
        tx: &mut db::Transaction<'_>,
        content: &Content,
        overwrite: OverwriteStrategy,
    ) -> Result<()> {
        // Remove overwritten blob
        if matches!(overwrite, OverwriteStrategy::Remove) {
            for blob_id in content.overwritten_blobs() {
                match Blob::remove(tx, self.branch(), Locator::head(*blob_id)).await {
                    // If we get `EntryNotFound` or `BlockNotFound` it most likely means the
                    // blob is already removed which can legitimately happen due to several
                    // reasons so we don't treat it as an error.
                    Ok(()) | Err(Error::EntryNotFound | Error::BlockNotFound(_)) => (),
                    Err(error) => return Err(error),
                }
            }
        }

        // Save the directory content into the store
        let buffer = content.serialize();
        self.blob.truncate(tx, 0).await?;
        self.blob.write(tx, &buffer).await?;
        self.blob.flush(tx).await?;

        Ok(())
    }

    /// Atomically commits any pending changes in this directory and updates the version vectors of
    /// it and all its ancestors.
    #[async_recursion]
    async fn commit<'a>(
        &'a mut self,
        tx: db::Transaction<'a>,
        content: Content,
        bump: VersionVector,
    ) -> Result<()> {
        // Update the version vector of this directory and all it's ancestors
        if let Some(ctx) = self.parent.as_mut() {
            ctx.commit(tx, self.blob.branch().clone(), bump).await?;
        } else {
            let write_keys = self
                .branch()
                .keys()
                .write()
                .ok_or(Error::PermissionDenied)?;

            self.branch().data().bump(tx, &bump, write_keys).await?;
        }

        if !content.is_empty() {
            self.entries = content;
            self.entries.clear_overwritten_blobs();
        }

        Ok(())
    }
}

async fn load(
    conn: &mut db::Connection,
    branch: Branch,
    locator: Locator,
) -> Result<(Blob, Content)> {
    let mut blob = Blob::open(conn, branch, locator, Shared::uninit()).await?;
    let buffer = blob.read_to_end(conn).await?;
    let content = Content::deserialize(&buffer)?;

    Ok((blob, content))
}
