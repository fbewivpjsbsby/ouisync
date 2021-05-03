use crate::{
    blob::Blob,
    crypto::Cryptor,
    db,
    entry::{Entry, EntryType},
    error::{Error, Result},
    file::File,
    locator::Locator,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{btree_map, BTreeMap},
    ffi::{OsStr, OsString},
};

pub struct Directory {
    blob: Blob,
    content: Content,
}

#[allow(clippy::len_without_is_empty)]
impl Directory {
    /// Opens existing directory.
    pub(crate) async fn open(pool: db::Pool, cryptor: Cryptor, locator: Locator) -> Result<Self> {
        let mut blob = Blob::open(pool, cryptor, locator).await?;
        let buffer = blob.read_to_end().await?;
        let content = bincode::deserialize(&buffer).map_err(Error::MalformedDirectory)?;

        Ok(Self { blob, content })
    }

    /// Creates new directory.
    pub(crate) fn create(pool: db::Pool, cryptor: Cryptor, locator: Locator) -> Self {
        let blob = Blob::create(pool, cryptor, locator);

        Self {
            blob,
            content: Content {
                dirty: true,
                ..Default::default()
            },
        }
    }

    /// Flushed this directory ensuring that any pending changes are written to the store.
    pub async fn flush(&mut self) -> Result<()> {
        if !self.content.dirty {
            return Ok(());
        }

        let buffer =
            bincode::serialize(&self.content).expect("failed to serialize directory content");

        self.blob.truncate().await?;
        self.blob.write(&buffer).await?;
        self.blob.flush().await?;

        self.content.dirty = false;

        Ok(())
    }

    /// Returns iterator over the entries of this directory.
    pub fn entries(
        &self,
    ) -> impl Iterator<Item = EntryInfo> + DoubleEndedIterator + ExactSizeIterator + Clone {
        self.content
            .entries
            .iter()
            .map(move |(name, data)| EntryInfo {
                parent_blob: &self.blob,
                name,
                data,
            })
    }

    /// Lookup an entry of this directory by name.
    pub fn lookup(&self, name: &'_ OsStr) -> Result<EntryInfo> {
        self.content
            .entries
            .get_key_value(name)
            .map(|(name, data)| EntryInfo {
                parent_blob: &self.blob,
                name,
                data,
            })
            .ok_or(Error::EntryNotFound)
    }

    /// Creates a new file inside this directory.
    pub fn create_file(&mut self, name: OsString) -> Result<File> {
        let seq = self.content.insert(name, EntryType::File)?;

        Ok(File::create(
            self.blob.db_pool().clone(),
            self.blob.cryptor().clone(),
            Locator::Head(*self.blob.head_name(), seq),
        ))
    }

    /// Creates a new subdirectory of this directory.
    pub fn create_subdirectory(&mut self, name: OsString) -> Result<Self> {
        let seq = self.content.insert(name, EntryType::Directory)?;

        Ok(Self::create(
            self.blob.db_pool().clone(),
            self.blob.cryptor().clone(),
            Locator::Head(*self.blob.head_name(), seq),
        ))
    }

    /// Removes the entry with `name` from this directory and also deletes it from the repository.
    pub async fn remove_entry(&mut self, name: &OsStr) -> Result<()> {
        let _seq = self.content.remove(name)?;

        // TODO: actualy delete the entry blob from the database

        Ok(())
    }

    /// Length of this directory in bytes. Does not include the content, only the size of directory
    /// itself.
    pub fn len(&self) -> u64 {
        self.blob.len()
    }

    /// Locator of this directory
    pub fn locator(&self) -> &Locator {
        self.blob.locator()
    }
}

/// Info about a directory entry.
pub struct EntryInfo<'a> {
    parent_blob: &'a Blob,
    name: &'a OsStr,
    data: &'a EntryData,
}

impl<'a> EntryInfo<'a> {
    pub fn name(&self) -> &'a OsStr {
        self.name
    }

    pub fn entry_type(&self) -> EntryType {
        self.data.entry_type
    }

    pub fn locator(&self) -> Locator {
        Locator::Head(*self.parent_blob.head_name(), self.data.seq)
    }

    /// Open the entry.
    pub async fn open(&self) -> Result<Entry> {
        match self.data.entry_type {
            EntryType::File => Ok(Entry::File(
                File::open(
                    self.parent_blob.db_pool().clone(),
                    self.parent_blob.cryptor().clone(),
                    self.locator(),
                )
                .await?,
            )),
            EntryType::Directory => Ok(Entry::Directory(
                Directory::open(
                    self.parent_blob.db_pool().clone(),
                    self.parent_blob.cryptor().clone(),
                    self.locator(),
                )
                .await?,
            )),
        }
    }
}

#[derive(Default, Deserialize, Serialize)]
struct Content {
    entries: BTreeMap<OsString, EntryData>,
    #[serde(skip)]
    dirty: bool,
}

impl Content {
    fn insert(&mut self, name: OsString, entry_type: EntryType) -> Result<u32> {
        let seq = self.next_seq();

        match self.entries.entry(name) {
            btree_map::Entry::Vacant(entry) => {
                entry.insert(EntryData { entry_type, seq });
                self.dirty = true;

                Ok(seq)
            }
            btree_map::Entry::Occupied(_) => Err(Error::EntryExists),
        }
    }

    fn remove(&mut self, name: &OsStr) -> Result<u32> {
        let seq = self
            .entries
            .remove(name)
            .map(|data| data.seq)
            .ok_or(Error::EntryNotFound)?;
        self.dirty = true;

        Ok(seq)
    }

    // Returns next available seq number.
    fn next_seq(&self) -> u32 {
        // TODO: reuse previously deleted entries

        match self.entries.values().map(|data| data.seq).max() {
            Some(seq) => seq.checked_add(1).expect("directory entry limit exceeded"), // TODO: return error instead
            None => 0,
        }
    }
}

#[derive(Deserialize, Serialize)]
struct EntryData {
    entry_type: EntryType,
    seq: u32,
    // TODO: metadata
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{block, index};
    use std::collections::BTreeSet;

    #[tokio::test(flavor = "multi_thread")]
    async fn create_and_list_entries() {
        let pool = setup().await;

        // Create the root directory and put some file in it.
        let mut dir = Directory::create(pool.clone(), Cryptor::Null, Locator::Root);

        let mut file_dog = dir.create_file("dog.txt".into()).unwrap();
        file_dog.write(b"woof").await.unwrap();
        file_dog.flush().await.unwrap();

        let mut file_cat = dir.create_file("cat.txt".into()).unwrap();
        file_cat.write(b"meow").await.unwrap();
        file_cat.flush().await.unwrap();

        dir.flush().await.unwrap();

        // Reopen the dir and try to read the files.
        let dir = Directory::open(pool, Cryptor::Null, Locator::Root)
            .await
            .unwrap();

        let expected_names: BTreeSet<_> = vec![OsStr::new("dog.txt"), OsStr::new("cat.txt")]
            .into_iter()
            .collect();
        let actual_names: BTreeSet<_> = dir.entries().map(|entry| entry.name()).collect();
        assert_eq!(actual_names, expected_names);

        for &(file_name, expected_content) in &[
            (OsStr::new("dog.txt"), b"woof"),
            (OsStr::new("cat.txt"), b"meow"),
        ] {
            let entry = dir.lookup(file_name).unwrap().open().await.unwrap();
            let mut file = match entry {
                Entry::File(file) => file,
                _ => panic!("expecting File, got {:?}", entry.entry_type()),
            };

            let actual_content = file.read_to_end().await.unwrap();
            assert_eq!(actual_content, expected_content);
        }
    }

    // TODO: test update existing directory
    #[tokio::test(flavor = "multi_thread")]
    async fn add_entry_to_existing_directory() {
        let pool = setup().await;

        // Create empty directory
        let mut dir = Directory::create(pool.clone(), Cryptor::Null, Locator::Root);
        dir.flush().await.unwrap();

        // Reopen it and add a file to it.
        let mut dir = Directory::open(pool.clone(), Cryptor::Null, Locator::Root)
            .await
            .unwrap();
        let _ = dir.create_file("none.txt".into()).unwrap();
        dir.flush().await.unwrap();

        // Reopen it again and check the file is still there.
        let dir = Directory::open(pool, Cryptor::Null, Locator::Root)
            .await
            .unwrap();
        assert!(dir.lookup(OsStr::new("none.txt")).is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remove_entry_from_existing_directory() {
        let pool = setup().await;

        let name = OsStr::new("monkey.txt");

        // Create a directory with a single entry.
        let mut parent_dir = Directory::create(pool.clone(), Cryptor::Null, Locator::Root);
        let mut file = parent_dir.create_file(name.into()).unwrap();
        file.flush().await.unwrap();
        parent_dir.flush().await.unwrap();

        // Reopen and remove the entry
        let mut parent_dir = Directory::open(pool.clone(), Cryptor::Null, Locator::Root)
            .await
            .unwrap();
        parent_dir.remove_entry(name).await.unwrap();
        parent_dir.flush().await.unwrap();

        // Reopen again and check the file was removed.
        let parent_dir = Directory::open(pool, Cryptor::Null, Locator::Root)
            .await
            .unwrap();
        match parent_dir.lookup(name) {
            Err(Error::EntryNotFound) => (),
            Err(error) => panic!("unexpected error {:?}", error),
            Ok(_) => panic!("entry should not exists but it does"),
        }

        assert_eq!(parent_dir.entries().len(), 0);
    }

    async fn setup() -> db::Pool {
        let pool = db::Pool::connect(":memory:").await.unwrap();
        index::init(&pool).await.unwrap();
        block::init(&pool).await.unwrap();
        pool
    }
}
