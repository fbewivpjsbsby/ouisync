// From experience, this lint is almost never useful. Disabling it globally.
#![allow(clippy::large_enum_variant)]

#[macro_use]
mod macros;

pub mod crypto;
pub mod db;
pub mod device_id;
pub mod network;
pub mod path;
pub mod sync;

mod access_control;
mod blob;
mod blob_id;
mod block;
mod branch;
mod config;
mod conflict;
mod deadlock;
mod debug;
mod directory;
mod error;
mod event;
mod file;
mod format;
mod index;
mod iterator;
mod joint_directory;
mod joint_entry;
mod locator;
mod metadata;
mod progress;
mod repository;
mod scoped_task;
mod state_monitor;
mod store;
#[cfg(test)]
mod test_utils;
#[cfg_attr(test, macro_use)]
mod version_vector;

pub use self::{
    access_control::{
        AccessMode, AccessSecrets, LocalAccess, LocalSecret, ShareToken, WriteSecrets,
    },
    blob::HEADER_SIZE as BLOB_HEADER_SIZE,
    block::BLOCK_SIZE,
    branch::Branch,
    config::ConfigStore,
    debug::DebugPrinter,
    directory::{Directory, EntryRef, EntryType},
    error::{Error, Result},
    event::{Event, Payload},
    file::File,
    joint_directory::{JointDirectory, JointEntryRef, MissingVersionStrategy},
    joint_entry::JointEntry,
    network::peer_addr::PeerAddr,
    repository::{Repository, RepositoryDb},
    state_monitor::{tracing_layer::TracingLayer, MonitorId, MonitoredValue, StateMonitor},
    store::Store,
};
