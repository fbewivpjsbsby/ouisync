// From experience, this lint is almost never useful. Disabling it globally.
#![allow(clippy::large_enum_variant)]
// This affects lots of parts of the code but it's just aesthetic so silencing it for now.
// We should consider enabling it again at some point.
#![allow(clippy::uninlined_format_args)]

#[macro_use]
mod macros;

pub mod crypto;
pub mod db;
pub mod network;
pub mod path;
pub mod protocol;
pub mod sync;

mod access_control;
mod blob;
mod block_tracker;
mod branch;
mod collections;
mod conflict;
mod debug;
mod device_id;
mod directory;
mod error;
mod event;
mod file;
mod format;
mod future;
mod iterator;
mod joint_directory;
mod joint_entry;
mod progress;
mod repository;
mod storage_size;
mod store;
#[cfg(test)]
mod test_utils;
mod time;
#[cfg_attr(test, macro_use)]
mod version_vector;
mod versioned;

pub use self::{
    access_control::{
        Access, AccessChange, AccessMode, AccessSecrets, KeyAndSalt, LocalSecret, SetLocalSecret,
        ShareToken, WriteSecrets,
    },
    blob::HEADER_SIZE as BLOB_HEADER_SIZE,
    branch::Branch,
    db::SCHEMA_VERSION,
    debug::DebugPrinter,
    device_id::DeviceId,
    directory::{Directory, EntryRef, EntryType, DIRECTORY_VERSION},
    error::{Error, Result},
    event::{Event, Payload},
    file::File,
    joint_directory::{JointDirectory, JointEntryRef},
    joint_entry::JointEntry,
    network::{peer_addr::PeerAddr, PeerInfo, PeerInfoCollector, PublicRuntimeId, SecretRuntimeId},
    progress::Progress,
    protocol::BLOCK_SIZE,
    repository::{
        delete as delete_repository, Credentials, Metadata, Repository, RepositoryHandle,
        RepositoryId, RepositoryParams,
    },
    storage_size::StorageSize,
    store::{Error as StoreError, DATA_VERSION},
    version_vector::VersionVector,
};
