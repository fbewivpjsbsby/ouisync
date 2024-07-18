use super::{EntryHandle, EntryIdGenerator, VirtualFilesystem};
use crate::{MountError, MultiRepoMount};
use deadlock::BlockingRwLock;
use dokan::{
    init, shutdown, unmount, CreateFileInfo, DiskSpaceInfo, FileInfo, FileSystemHandler,
    FileSystemMountError, FileSystemMounter, FileTimeOperation, FillDataResult, FindData,
    MountOptions, OperationInfo, OperationResult, VolumeInfo, IO_SECURITY_CONTEXT,
};
use ouisync_lib::Repository;
use std::io;
use std::{
    collections::{hash_map, HashMap},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc,
    },
    thread,
    time::UNIX_EPOCH,
};
use tokio::runtime::Handle as RuntimeHandle;
use widestring::{U16CStr, U16CString, U16Str};
use winapi::{shared::ntstatus::*, um::winnt};

struct RepoMap {
    // Invariant that must hold: if there exists a key in `name_to_repo`, then there must exist a
    // exactly one entry in `path_to_name` with the same value.
    name_to_repo: HashMap<U16CString, Arc<VirtualFilesystem>>,
    path_to_name: HashMap<PathBuf, U16CString>,
}

impl RepoMap {
    fn new() -> Self {
        Self {
            name_to_repo: Default::default(),
            path_to_name: Default::default(),
        }
    }
}

pub struct MultiRepoVFS {
    entry_id_generator: Arc<EntryIdGenerator>,
    runtime_handle: RuntimeHandle,
    repos: Arc<BlockingRwLock<RepoMap>>,
    unmount_tx: mpsc::SyncSender<()>,
    // It's `Option` so we can move it out of there in `Drop::drop`.
    join_handle: Option<thread::JoinHandle<()>>,
}

impl MultiRepoMount for MultiRepoVFS {
    fn create(
        mount_point: impl AsRef<Path>,
    ) -> Pin<Box<dyn Future<Output = Result<Self, MountError>> + Send>> {
        let mount_point = U16CString::from_os_str(mount_point.as_ref().as_os_str());

        Box::pin(async move {
            let options = MountOptions {
                single_thread: false,
                flags: super::default_mount_flags(),
                ..Default::default()
            };

            let mount_point = match mount_point {
                Ok(mount_point) => mount_point,
                Err(error) => {
                    tracing::error!("Failed to convert mount point to U16CString: {error:?}");
                    return Err(MountError::InvalidMountPoint);
                }
            };

            let (on_mount_tx, on_mount_rx) = tokio::sync::oneshot::channel();
            let (unmount_tx, unmount_rx) = mpsc::sync_channel(1);

            let entry_id_generator = Arc::new(EntryIdGenerator::new());
            let repos = Arc::new(BlockingRwLock::new(RepoMap::new()));
            let root_id = entry_id_generator.generate_id();

            let join_handle = thread::spawn({
                let repos = repos.clone();
                move || {
                    // TODO: Ensure this is done only once.
                    init();

                    let handler = Handler {
                        root_id,
                        repos,
                        next_debug_id: AtomicU64::new(0),
                        debug_type: DebugType::None,
                    };

                    let mut mounter = FileSystemMounter::new(&handler, &mount_point, &options);

                    let file_system = match mounter.mount() {
                        Ok(file_system) => file_system,
                        Err(error) => {
                            tracing::error!("Failed to mount: {error:?}");
                            on_mount_tx.send(Err(error)).unwrap_or(());
                            return;
                        }
                    };

                    // Tell the main thread we've successfully mounted.
                    on_mount_tx.send(Ok(())).unwrap_or(());

                    // Wait here to preserve `file_system`'s lifetime.
                    unmount_rx.recv().unwrap_or(());

                    // If we don't do this then dropping `file_system` will block.
                    if !unmount(&mount_point) {
                        tracing::warn!("Failed to unmount {mount_point:?}");
                    }

                    drop(file_system);

                    shutdown();
                }
            });

            // Unwrap is OK because we make sure we always send to `on_mount_tx` above.
            if let Err(error) = on_mount_rx.await.unwrap() {
                return Err(error.into());
            }

            Ok(Self {
                entry_id_generator,
                runtime_handle: RuntimeHandle::current(),
                repos,
                unmount_tx,
                join_handle: Some(join_handle),
            })
        })
    }

    fn insert(&self, store_path: PathBuf, repo: Arc<Repository>) -> Result<(), io::Error> {
        let name = match store_path.file_stem() {
            Some(name) => name,
            None => {
                return Err(io::Error::new(
                    // InvalidFilename would have been better, but it's unstable.
                    io::ErrorKind::InvalidInput,
                    format!("Not a valid repository file name {:?}", store_path),
                ));
            }
        };

        let name = match U16CString::from_os_str(name) {
            Ok(name) => name,
            Err(_) => {
                return Err(io::Error::new(
                    // InvalidFilename would have been better, but it's unstable.
                    io::ErrorKind::InvalidInput,
                    format!("Filename contains nulls {:?}", name),
                ));
            }
        };

        let mut repos_lock = self.repos.write().unwrap();

        let RepoMap {
            name_to_repo,
            path_to_name,
        } = &mut *repos_lock;

        let path_to_name_entry = match path_to_name.entry(store_path.clone()) {
            hash_map::Entry::Vacant(entry) => entry,
            hash_map::Entry::Occupied(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("Repository {store_path:?} already added"),
                ))
            }
        };

        match name_to_repo.entry(name.clone()) {
            hash_map::Entry::Vacant(name_to_repo_entry) => {
                let repo = Arc::new(VirtualFilesystem::new(
                    self.runtime_handle.clone(),
                    self.entry_id_generator.clone(),
                    repo,
                ));
                name_to_repo_entry.insert(repo);
                path_to_name_entry.insert(name);
            }
            hash_map::Entry::Occupied(entry) =>
            // TODO: We could use an alternative name such as "<name> (2)".
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "Repository with the name {:?} already exists",
                        entry.key().to_string_lossy()
                    ),
                ))
            }
        }

        Ok(())
    }

    fn remove(&self, store_path: &Path) -> Result<(), io::Error> {
        let mut repos_lock = self.repos.write().unwrap();
        let RepoMap {
            name_to_repo,
            path_to_name,
        } = &mut *repos_lock;

        let name = path_to_name.get(store_path).ok_or(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Repository {store_path:?} not found"),
        ))?;

        name_to_repo.remove(name);
        path_to_name.remove(store_path);

        Ok(())
    }
}

impl Drop for MultiRepoVFS {
    fn drop(&mut self) {
        if let Some(join_handle) = self.join_handle.take() {
            self.unmount_tx.try_send(()).unwrap_or(());
            join_handle.join().unwrap_or(());
        }
    }
}

#[allow(dead_code)]
#[derive(Eq, PartialEq)]
enum DebugType {
    None,
    Full,
    Error,
}

struct Handler {
    root_id: u64,
    repos: Arc<BlockingRwLock<RepoMap>>,
    next_debug_id: AtomicU64,
    debug_type: DebugType,
}

impl Handler {
    fn get_repo_and_path(
        &self,
        path: &U16CStr,
    ) -> OperationResult<(Option<Arc<VirtualFilesystem>>, U16CString)> {
        let (repo_name, path) = match decompose_path(path)? {
            (Some(repo_name), path) => (repo_name, path),
            (None, path) => return Ok((None, path)),
        };

        let repos = self.repos.read().unwrap();

        match repos.name_to_repo.get(&repo_name) {
            Some(repo) => Ok((Some(repo.clone()), path)),
            None => Err(STATUS_OBJECT_NAME_NOT_FOUND),
        }
    }

    fn generate_debug_id(&self) -> u64 {
        self.next_debug_id.fetch_add(1, Ordering::Relaxed)
    }
}

enum MultiRepoEntryHandle {
    EntryHandle {
        vfs: Arc<VirtualFilesystem>,
        handle: EntryHandle,
    },
    RepoList,
}

impl MultiRepoEntryHandle {
    fn as_inner_repo_handle<'a>(
        &'a self,
        file_name: &U16CStr,
    ) -> OperationResult<(Arc<VirtualFilesystem>, U16CString, &'a EntryHandle)> {
        let (vfs_name, file_name) = decompose_path(file_name)?;

        // Just check that `file_name` refers to a file inside a repo.
        vfs_name.ok_or(STATUS_ACCESS_DENIED)?;

        let (vfs, handle) = match self {
            MultiRepoEntryHandle::EntryHandle { vfs, handle } => (vfs, handle),
            MultiRepoEntryHandle::RepoList => return Err(STATUS_ACCESS_DENIED),
        };

        Ok((vfs.clone(), file_name, handle))
    }
}

impl Handler {
    #[allow(clippy::too_many_arguments)]
    fn create_file_<'c, 'h: 'c>(
        &'h self,
        file_name: &U16CStr,
        security_context: &IO_SECURITY_CONTEXT,
        desired_access: winnt::ACCESS_MASK,
        file_attributes: u32,
        share_access: u32,
        create_disposition: u32,
        create_options: u32,
        _info: &mut OperationInfo<'c, 'h, Self>,
    ) -> OperationResult<CreateFileInfo<MultiRepoEntryHandle>> {
        let (vfs, file_name) = self.get_repo_and_path(file_name)?;
        let vfs = match vfs {
            Some(vfs) => vfs,
            None => {
                if file_name.as_slice() == ['\\' as u16] {
                    return Ok(CreateFileInfo {
                        context: MultiRepoEntryHandle::RepoList,
                        is_dir: true,
                        new_file_created: false,
                    });
                } else {
                    // Request for a file in the root that is not a repository.
                    return Err(STATUS_OBJECT_NAME_NOT_FOUND);
                }
            }
        };

        let r = vfs.create_file(
            &file_name,
            security_context,
            desired_access,
            file_attributes,
            share_access,
            create_disposition,
            create_options,
        );

        let CreateFileInfo {
            context,
            is_dir,
            new_file_created,
        } = r?;

        Ok(CreateFileInfo {
            context: MultiRepoEntryHandle::EntryHandle {
                vfs,
                handle: context,
            },
            is_dir,
            new_file_created,
        })
    }

    fn cleanup_<'c, 'h: 'c>(
        &'h self,
        file_name: &U16CStr,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c MultiRepoEntryHandle,
    ) {
        let context = match context {
            MultiRepoEntryHandle::EntryHandle { vfs: _, handle } => handle,
            MultiRepoEntryHandle::RepoList => return,
        };

        let (vfs, file_name) = match self.get_repo_and_path(file_name) {
            Ok((Some(vfs), file_name)) => (vfs, file_name),
            Ok((None, _file_name)) => return,
            Err(error) => {
                tracing::error!("Failed to close file {:?}: {error:?}", file_name,);
                return;
            }
        };

        vfs.cleanup(&file_name, info, context)
    }

    fn close_file_<'c, 'h: 'c>(
        &'h self,
        file_name: &U16CStr,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c MultiRepoEntryHandle,
    ) {
        let context = match context {
            MultiRepoEntryHandle::EntryHandle { vfs: _, handle } => handle,
            MultiRepoEntryHandle::RepoList => return,
        };

        let (vfs, file_name) = match self.get_repo_and_path(file_name) {
            Ok((Some(vfs), file_name)) => (vfs, file_name),
            Ok((None, _file_name)) => return,
            Err(error) => {
                tracing::error!("Failed to close file {:?}: {error:?}", file_name,);
                return;
            }
        };

        vfs.close_file(&file_name, info, context)
    }

    fn read_file_<'c, 'h: 'c>(
        &'h self,
        file_name: &U16CStr,
        offset: i64,
        buffer: &mut [u8],
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c MultiRepoEntryHandle,
    ) -> OperationResult<u32> {
        let (vfs, file_name, handle) = context.as_inner_repo_handle(file_name)?;
        vfs.read_file(&file_name, offset, buffer, info, handle)
    }

    fn write_file_<'c, 'h: 'c>(
        &'h self,
        file_name: &U16CStr,
        offset: i64,
        buffer: &[u8],
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c MultiRepoEntryHandle,
    ) -> OperationResult<u32> {
        let (vfs, file_name, handle) = context.as_inner_repo_handle(file_name)?;
        vfs.write_file(&file_name, offset, buffer, info, handle)
    }

    fn flush_file_buffers_<'c, 'h: 'c>(
        &'h self,
        file_name: &U16CStr,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c MultiRepoEntryHandle,
    ) -> OperationResult<()> {
        let (vfs, file_name, handle) = context.as_inner_repo_handle(file_name)?;
        vfs.flush_file_buffers(&file_name, info, handle)
    }

    fn get_file_information_<'c, 'h: 'c>(
        &'h self,
        file_name: &U16CStr,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c MultiRepoEntryHandle,
    ) -> OperationResult<FileInfo> {
        match context {
            context @ MultiRepoEntryHandle::EntryHandle { .. } => {
                let (vfs, file_name, handle) = context.as_inner_repo_handle(file_name)?;
                vfs.get_file_information(&file_name, info, handle)
            }
            MultiRepoEntryHandle::RepoList => {
                Ok(FileInfo {
                    attributes: winnt::FILE_ATTRIBUTE_DIRECTORY,
                    // TODO
                    creation_time: UNIX_EPOCH,
                    last_access_time: UNIX_EPOCH,
                    last_write_time: UNIX_EPOCH,
                    file_size: 0,
                    number_of_links: 1,
                    file_index: self.root_id,
                })
            }
        }
    }

    fn find_files_<'c, 'h: 'c>(
        &'h self,
        file_name: &U16CStr,
        mut fill_find_data: impl FnMut(&FindData) -> FillDataResult,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c MultiRepoEntryHandle,
    ) -> OperationResult<()> {
        match context {
            context @ MultiRepoEntryHandle::EntryHandle { .. } => {
                let (vfs, file_name, handle) = context.as_inner_repo_handle(file_name)?;
                vfs.find_files(&file_name, fill_find_data, info, handle)
            }
            MultiRepoEntryHandle::RepoList => {
                let repos_lock = self.repos.read().unwrap();

                for repo_name in repos_lock.name_to_repo.keys() {
                    fill_find_data(&FindData {
                        attributes: winnt::FILE_ATTRIBUTE_DIRECTORY,
                        // TODO
                        creation_time: UNIX_EPOCH,
                        last_access_time: UNIX_EPOCH,
                        last_write_time: UNIX_EPOCH,
                        file_size: 0,
                        file_name: repo_name.clone(),
                    })
                    .or_else(super::ignore_name_too_long)?;
                }

                Ok(())
            }
        }
    }

    fn find_files_with_pattern_<'c, 'h: 'c>(
        &'h self,
        file_name: &U16CStr,
        pattern: &U16CStr,
        mut fill_find_data: impl FnMut(&FindData) -> FillDataResult,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c MultiRepoEntryHandle,
    ) -> OperationResult<()> {
        match context {
            context @ MultiRepoEntryHandle::EntryHandle { .. } => {
                let (vfs, file_name, handle) = context.as_inner_repo_handle(file_name)?;
                vfs.find_files_with_pattern(&file_name, pattern, fill_find_data, info, handle)
            }
            MultiRepoEntryHandle::RepoList => {
                let repos_lock = self.repos.read().unwrap();

                for (repo_name, vfs) in repos_lock.name_to_repo.iter() {
                    if !vfs.repo.access_mode().can_read() {
                        continue;
                    }

                    let ignore_case = true;
                    if !dokan::is_name_in_expression(pattern, repo_name, ignore_case) {
                        continue;
                    }

                    fill_find_data(&FindData {
                        attributes: winnt::FILE_ATTRIBUTE_DIRECTORY,
                        // TODO
                        creation_time: UNIX_EPOCH,
                        last_access_time: UNIX_EPOCH,
                        last_write_time: UNIX_EPOCH,
                        file_size: 0,
                        file_name: repo_name.clone(),
                    })
                    .or_else(super::ignore_name_too_long)?;
                }

                Ok(())
            }
        }
    }

    fn set_file_attributes_<'c, 'h: 'c>(
        &'h self,
        file_name: &U16CStr,
        file_attributes: u32,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c MultiRepoEntryHandle,
    ) -> OperationResult<()> {
        match context {
            context @ MultiRepoEntryHandle::EntryHandle { .. } => {
                let (vfs, file_name, handle) = context.as_inner_repo_handle(file_name)?;
                vfs.set_file_attributes(&file_name, file_attributes, info, handle)
            }
            MultiRepoEntryHandle::RepoList => Err(STATUS_NOT_IMPLEMENTED),
        }
    }

    fn set_file_time_<'c, 'h: 'c>(
        &'h self,
        file_name: &U16CStr,
        creation_time: FileTimeOperation,
        last_access_time: FileTimeOperation,
        last_write_time: FileTimeOperation,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c MultiRepoEntryHandle,
    ) -> OperationResult<()> {
        match context {
            context @ MultiRepoEntryHandle::EntryHandle { .. } => {
                let (vfs, file_name, handle) = context.as_inner_repo_handle(file_name)?;
                vfs.set_file_time(
                    &file_name,
                    creation_time,
                    last_access_time,
                    last_write_time,
                    info,
                    handle,
                )
            }
            MultiRepoEntryHandle::RepoList => Err(STATUS_NOT_IMPLEMENTED),
        }
    }

    fn delete_file_<'c, 'h: 'c>(
        &'h self,
        file_name: &U16CStr,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c MultiRepoEntryHandle,
    ) -> OperationResult<()> {
        match context {
            context @ MultiRepoEntryHandle::EntryHandle { .. } => {
                let (vfs, file_name, handle) = context.as_inner_repo_handle(file_name)?;
                vfs.delete_file(&file_name, info, handle)
            }
            MultiRepoEntryHandle::RepoList => Err(STATUS_ACCESS_DENIED),
        }
    }

    fn delete_directory_<'c, 'h: 'c>(
        &'h self,
        file_name: &U16CStr,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c MultiRepoEntryHandle,
    ) -> OperationResult<()> {
        match context {
            context @ MultiRepoEntryHandle::EntryHandle { .. } => {
                let (vfs, file_name, handle) = context.as_inner_repo_handle(file_name)?;
                vfs.delete_directory(&file_name, info, handle)
            }
            MultiRepoEntryHandle::RepoList => {
                // TODO: Or maybe we can delete the entire repo from here?
                Err(STATUS_ACCESS_DENIED)
            }
        }
    }

    fn move_file_<'c, 'h: 'c>(
        &'h self,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_existing: bool,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c MultiRepoEntryHandle,
    ) -> OperationResult<()> {
        match context {
            context @ MultiRepoEntryHandle::EntryHandle { .. } => {
                let (src_vfs, src_file_name, handle) = context.as_inner_repo_handle(file_name)?;
                let (dst_vfs, dst_file_name) = self.get_repo_and_path(new_file_name)?;

                let dst_vfs = match dst_vfs {
                    Some(dst_vfs) => dst_vfs,
                    // Moving not supported out of a repository.
                    None => return Err(STATUS_NOT_IMPLEMENTED),
                };

                if !Arc::ptr_eq(&src_vfs, &dst_vfs) {
                    // Moving from one repo to another is not supported.
                    return Err(STATUS_NOT_IMPLEMENTED);
                }

                src_vfs.move_file(
                    &src_file_name,
                    &dst_file_name,
                    replace_if_existing,
                    info,
                    handle,
                )
            }
            MultiRepoEntryHandle::RepoList => {
                // TODO: Or maybe we can delete the entire repo from here?
                Err(STATUS_ACCESS_DENIED)
            }
        }
    }

    fn set_end_of_file_<'c, 'h: 'c>(
        &'h self,
        file_name: &U16CStr,
        offset: i64,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c MultiRepoEntryHandle,
    ) -> OperationResult<()> {
        let (vfs, file_name, handle) = context.as_inner_repo_handle(file_name)?;
        vfs.set_end_of_file(&file_name, offset, info, handle)
    }

    fn set_allocation_size_<'c, 'h: 'c>(
        &'h self,
        file_name: &U16CStr,
        alloc_size: i64,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c MultiRepoEntryHandle,
    ) -> OperationResult<()> {
        let (vfs, file_name, handle) = context.as_inner_repo_handle(file_name)?;
        vfs.set_allocation_size(&file_name, alloc_size, info, handle)
    }

    fn get_disk_free_space_<'c, 'h: 'c>(
        &'h self,
        _info: &OperationInfo<'c, 'h, Self>,
    ) -> OperationResult<DiskSpaceInfo> {
        Err(STATUS_NOT_IMPLEMENTED)
    }

    fn get_volume_information_<'c, 'h: 'c>(
        &'h self,
        _info: &OperationInfo<'c, 'h, Self>,
    ) -> OperationResult<VolumeInfo> {
        Ok(VolumeInfo {
            name: U16CString::from_str("Ouisync").unwrap(),
            serial_number: 0,
            max_component_length: super::MAX_COMPONENT_LENGTH,
            fs_flags: winnt::FILE_CASE_PRESERVED_NAMES
                | winnt::FILE_CASE_SENSITIVE_SEARCH
                | winnt::FILE_UNICODE_ON_DISK,
            // Copy/paste from dokan-rust memfs example, the comment there was:
            // "Custom names don't play well with UAC".
            fs_name: U16CString::from_str("NTFS").unwrap(),
        })
    }

    fn mounted_<'c, 'h: 'c>(
        &'h self,
        _mount_point: &U16CStr,
        _info: &OperationInfo<'c, 'h, Self>,
    ) -> OperationResult<()> {
        Ok(())
    }

    fn unmounted_<'c, 'h: 'c>(
        &'h self,
        _info: &OperationInfo<'c, 'h, Self>,
    ) -> OperationResult<()> {
        Ok(())
    }
}

//  https://dokan-dev.github.io/dokany-doc/html/struct_d_o_k_a_n___o_p_e_r_a_t_i_o_n_s.html
impl<'c, 'h: 'c> FileSystemHandler<'c, 'h> for Handler {
    type Context = MultiRepoEntryHandle;

    fn create_file(
        &'h self,
        file_name: &U16CStr,
        security_context: &IO_SECURITY_CONTEXT,
        desired_access: winnt::ACCESS_MASK,
        file_attributes: u32,
        share_access: u32,
        create_disposition: u32,
        create_options: u32,
        info: &mut OperationInfo<'c, 'h, Self>,
    ) -> OperationResult<CreateFileInfo<Self::Context>> {
        let debug_id = self.generate_debug_id();

        if self.debug_type == DebugType::Full {
            println!(
                "{debug_id} Enter: create_file {:?}",
                file_name.to_string_lossy()
            );
        }

        let r = self.create_file_(
            file_name,
            security_context,
            desired_access,
            file_attributes,
            share_access,
            create_disposition,
            create_options,
            info,
        );

        match self.debug_type {
            DebugType::None => (),
            DebugType::Full => match r {
                Ok(_) => println!("{debug_id} Leave: create_file -> Ok"),
                Err(error) => println!(
                    "{debug_id} Leave: create_file -> {:?}",
                    super::Error::NtStatus(error)
                ),
            },
            DebugType::Error => {
                if let Err(error) = r {
                    println!(
                        "{debug_id} Leave: create_file -> {:?}",
                        super::Error::NtStatus(error)
                    );
                }
            }
        }

        r
    }

    fn cleanup(
        &'h self,
        file_name: &U16CStr,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) {
        let debug_id = self.generate_debug_id();

        if self.debug_type == DebugType::Full {
            println!(
                "{debug_id} Enter: cleanup {:?}",
                file_name.to_string_lossy()
            );
        }

        self.cleanup_(file_name, info, context);

        if self.debug_type == DebugType::Full {
            println!("{debug_id} Leave: cleanup");
        }
    }

    fn close_file(
        &'h self,
        file_name: &U16CStr,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) {
        let debug_id = self.generate_debug_id();

        if self.debug_type == DebugType::Full {
            println!("{debug_id} Enter: close_file {:?}", file_name.as_ustr());
        }

        self.close_file_(file_name, info, context);

        if self.debug_type == DebugType::Full {
            println!("{debug_id} Leave: close_file");
        }
    }

    fn read_file(
        &'h self,
        file_name: &U16CStr,
        offset: i64,
        buffer: &mut [u8],
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) -> OperationResult<u32> {
        let debug_id = self.generate_debug_id();

        if self.debug_type == DebugType::Full {
            let id = match context {
                MultiRepoEntryHandle::EntryHandle { handle, .. } => handle.id,
                MultiRepoEntryHandle::RepoList => self.root_id,
            };

            println!(
                "{debug_id} Enter: read_file id:{id} offset:{offset} buffer.len:{} {:?}",
                buffer.len(),
                file_name.to_string_lossy()
            );
        }

        let r = self.read_file_(file_name, offset, buffer, info, context);

        match self.debug_type {
            DebugType::None => (),
            DebugType::Full => match r {
                Ok(read) => println!("{debug_id} Leave: read_file -> Ok({read})"),
                Err(error) => println!(
                    "{debug_id} Leave: read_file -> {:?}",
                    super::Error::NtStatus(error)
                ),
            },
            DebugType::Error => {
                if let Err(error) = r {
                    println!(
                        "{debug_id} Leave: read_file -> {:?}",
                        super::Error::NtStatus(error)
                    );
                }
            }
        }

        r
    }

    fn write_file(
        &'h self,
        file_name: &U16CStr,
        offset: i64,
        buffer: &[u8],
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) -> OperationResult<u32> {
        let debug_id = self.generate_debug_id();

        if self.debug_type == DebugType::Full {
            println!(
                "{debug_id} Enter: write_file {:?} offset:{offset:?} len:{}",
                file_name.to_string_lossy(),
                buffer.len(),
            );
        }

        let r = self.write_file_(file_name, offset, buffer, info, context);

        match self.debug_type {
            DebugType::None => (),
            DebugType::Full => match r {
                Ok(_) => println!("{debug_id} Leave: write_file -> Ok"),
                Err(error) => println!(
                    "{debug_id} Leave: write_file -> {:?}",
                    super::Error::NtStatus(error)
                ),
            },
            DebugType::Error => {
                if let Err(error) = r {
                    println!(
                        "{debug_id} Leave: write_file -> {:?}",
                        super::Error::NtStatus(error)
                    );
                }
            }
        }

        r
    }

    fn flush_file_buffers(
        &'h self,
        file_name: &U16CStr,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) -> OperationResult<()> {
        let debug_id = self.generate_debug_id();

        if self.debug_type == DebugType::Full {
            println!(
                "{debug_id} Enter: flush_file_buffers {:?}",
                file_name.to_string_lossy()
            );
        }

        let r = self.flush_file_buffers_(file_name, info, context);

        match self.debug_type {
            DebugType::None => (),
            DebugType::Full => match r {
                Ok(_) => println!("{debug_id} Leave: flush_file_buffers -> Ok"),
                Err(error) => println!(
                    "{debug_id} Leave: flush_file_buffers -> {:?}",
                    super::Error::NtStatus(error)
                ),
            },
            DebugType::Error => {
                if let Err(error) = r {
                    println!(
                        "{debug_id} Leave: flush_file_buffers -> {:?}",
                        super::Error::NtStatus(error)
                    );
                }
            }
        }

        r
    }

    fn get_file_information(
        &'h self,
        file_name: &U16CStr,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) -> OperationResult<FileInfo> {
        let debug_id = self.generate_debug_id();

        if self.debug_type == DebugType::Full {
            println!(
                "{debug_id} Enter: get_file_information {:?}",
                file_name.to_string_lossy()
            );
        }

        let r = self.get_file_information_(file_name, info, context);

        match self.debug_type {
            DebugType::None => (),
            DebugType::Full => match r {
                Ok(_) => println!("{debug_id} Leave: get_file_information -> Ok"),
                Err(error) => println!(
                    "{debug_id} Leave: get_file_information -> {:?}",
                    super::Error::NtStatus(error)
                ),
            },
            DebugType::Error => {
                if let Err(error) = r {
                    println!(
                        "{debug_id} Leave: get_file_information -> {:?}",
                        super::Error::NtStatus(error)
                    );
                }
            }
        }

        r
    }

    fn find_files(
        &'h self,
        file_name: &U16CStr,
        fill_find_data: impl FnMut(&FindData) -> FillDataResult,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) -> OperationResult<()> {
        let debug_id = self.generate_debug_id();

        if self.debug_type == DebugType::Full {
            println!(
                "{debug_id} Enter: find_files {:?}",
                file_name.to_string_lossy()
            );
        }

        let r = self.find_files_(file_name, fill_find_data, info, context);

        match self.debug_type {
            DebugType::None => (),
            DebugType::Full => match r {
                Ok(_) => println!("{debug_id} Leave: find_files -> Ok"),
                Err(error) => println!(
                    "{debug_id} Leave: find_files -> {:?}",
                    super::Error::NtStatus(error)
                ),
            },
            DebugType::Error => {
                if let Err(error) = r {
                    println!(
                        "{debug_id} Leave: find_files -> {:?}",
                        super::Error::NtStatus(error)
                    );
                }
            }
        }

        r
    }

    fn find_files_with_pattern(
        &'h self,
        file_name: &U16CStr,
        pattern: &U16CStr,
        fill_find_data: impl FnMut(&FindData) -> FillDataResult,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) -> OperationResult<()> {
        let debug_id = self.generate_debug_id();

        if self.debug_type == DebugType::Full {
            println!(
                "{debug_id} Enter: find_files_with_pattern {:?}",
                file_name.as_ustr()
            );
        }

        let r = self.find_files_with_pattern_(file_name, pattern, fill_find_data, info, context);

        match self.debug_type {
            DebugType::None => (),
            DebugType::Full => match r {
                Ok(_) => println!("{debug_id} Leave: find_files_with_pattern -> Ok"),
                Err(error) => println!(
                    "{debug_id} Leave: find_files_with_pattern -> {:?}",
                    super::Error::NtStatus(error)
                ),
            },
            DebugType::Error => {
                if let Err(error) = r {
                    println!(
                        "{debug_id} Leave: find_files_with_pattern -> {:?}",
                        super::Error::NtStatus(error)
                    );
                }
            }
        }

        r
    }

    fn set_file_attributes(
        &'h self,
        file_name: &U16CStr,
        file_attributes: u32,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) -> OperationResult<()> {
        let debug_id = self.generate_debug_id();

        if self.debug_type == DebugType::Full {
            println!(
                "{debug_id} Enter: set_file_attributes {:?}",
                file_name.as_ustr()
            );
        }

        let r = self.set_file_attributes_(file_name, file_attributes, info, context);

        match self.debug_type {
            DebugType::None => (),
            DebugType::Full => match r {
                Ok(_) => println!("{debug_id} Leave: set_file_attributes -> Ok"),
                Err(error) => println!(
                    "{debug_id} Leave: set_file_attributes -> {:?}",
                    super::Error::NtStatus(error)
                ),
            },
            DebugType::Error => {
                if let Err(error) = r {
                    println!(
                        "{debug_id} Leave: set_file_attributes -> {:?}",
                        super::Error::NtStatus(error)
                    );
                }
            }
        }

        r
    }

    fn set_file_time(
        &'h self,
        file_name: &U16CStr,
        creation_time: FileTimeOperation,
        last_access_time: FileTimeOperation,
        last_write_time: FileTimeOperation,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) -> OperationResult<()> {
        let debug_id = self.generate_debug_id();

        if self.debug_type == DebugType::Full {
            println!("{debug_id} Enter: set_file_time {:?}", file_name.as_ustr());
        }

        let r = self.set_file_time_(
            file_name,
            creation_time,
            last_access_time,
            last_write_time,
            info,
            context,
        );

        match self.debug_type {
            DebugType::None => (),
            DebugType::Full => match r {
                Ok(_) => println!("{debug_id} Leave: set_file_time -> Ok"),
                Err(error) => println!(
                    "{debug_id} Leave: set_file_time -> {:?}",
                    super::Error::NtStatus(error)
                ),
            },
            DebugType::Error => {
                if let Err(error) = r {
                    println!(
                        "{debug_id} Leave: set_file_time -> {:?}",
                        super::Error::NtStatus(error)
                    );
                }
            }
        }

        r
    }

    fn delete_file(
        &'h self,
        file_name: &U16CStr,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) -> OperationResult<()> {
        let debug_id = self.generate_debug_id();

        if self.debug_type == DebugType::Full {
            println!("{debug_id} Enter: delete_file {:?}", file_name.as_ustr());
        }

        let r = self.delete_file_(file_name, info, context);

        match self.debug_type {
            DebugType::None => (),
            DebugType::Full => match r {
                Ok(_) => println!("{debug_id} Leave: delete_file -> Ok"),
                Err(error) => println!(
                    "{debug_id} Leave: delete_file -> {:?}",
                    super::Error::NtStatus(error)
                ),
            },
            DebugType::Error => {
                if let Err(error) = r {
                    println!(
                        "{debug_id} Leave: delete_file -> {:?}",
                        super::Error::NtStatus(error)
                    );
                }
            }
        }

        r
    }

    fn delete_directory(
        &'h self,
        file_name: &U16CStr,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) -> OperationResult<()> {
        let debug_id = self.generate_debug_id();

        if self.debug_type == DebugType::Full {
            println!(
                "{debug_id} Enter: delete_directory {:?}",
                file_name.as_ustr()
            );
        }

        let r = self.delete_directory_(file_name, info, context);

        match self.debug_type {
            DebugType::None => (),
            DebugType::Full => match r {
                Ok(_) => println!("{debug_id} Leave: delete_directory -> Ok"),
                Err(error) => println!(
                    "{debug_id} Leave: delete_directory -> {:?}",
                    super::Error::NtStatus(error)
                ),
            },
            DebugType::Error => {
                if let Err(error) = r {
                    println!(
                        "{debug_id} Leave: delete_directory -> {:?}",
                        super::Error::NtStatus(error)
                    );
                }
            }
        }

        r
    }

    fn move_file(
        &'h self,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_existing: bool,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) -> OperationResult<()> {
        let debug_id = self.generate_debug_id();

        if self.debug_type == DebugType::Full {
            println!(
                "{debug_id} Enter: move_file {:?} -> {:?} replace_if_existing:{replace_if_existing:?}",
                file_name.to_string_lossy(),
                new_file_name.to_string_lossy(),
            );
        }

        let r = self.move_file_(file_name, new_file_name, replace_if_existing, info, context);

        match self.debug_type {
            DebugType::None => (),
            DebugType::Full => match r {
                Ok(_) => println!("{debug_id} Leave: move_file -> Ok"),
                Err(error) => println!(
                    "{debug_id} Leave: move_file -> {:?}",
                    super::Error::NtStatus(error)
                ),
            },
            DebugType::Error => {
                if let Err(error) = r {
                    println!(
                        "{debug_id} Leave: move_file -> {:?}",
                        super::Error::NtStatus(error)
                    );
                }
            }
        }

        r
    }

    fn set_end_of_file(
        &'h self,
        file_name: &U16CStr,
        offset: i64,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) -> OperationResult<()> {
        let debug_id = self.generate_debug_id();

        if self.debug_type == DebugType::Full {
            println!(
                "{debug_id} Enter: set_end_of_file {:?}",
                file_name.to_string_lossy()
            );
        }

        let r = self.set_end_of_file_(file_name, offset, info, context);

        match self.debug_type {
            DebugType::None => (),
            DebugType::Full => match r {
                Ok(_) => println!("{debug_id} Leave: set_end_of_file -> Ok"),
                Err(error) => println!(
                    "{debug_id} Leave: set_end_of_file -> {:?}",
                    super::Error::NtStatus(error)
                ),
            },
            DebugType::Error => {
                if let Err(error) = r {
                    println!(
                        "{debug_id} Leave: set_end_of_file -> {:?}",
                        super::Error::NtStatus(error)
                    );
                }
            }
        }

        r
    }

    fn set_allocation_size(
        &'h self,
        file_name: &U16CStr,
        alloc_size: i64,
        info: &OperationInfo<'c, 'h, Self>,
        context: &'c Self::Context,
    ) -> OperationResult<()> {
        let debug_id = self.generate_debug_id();

        if self.debug_type == DebugType::Full {
            println!(
                "{debug_id} Enter: set_allocation_size {:?}",
                file_name.to_string_lossy()
            );
        }

        let r = self.set_allocation_size_(file_name, alloc_size, info, context);

        match self.debug_type {
            DebugType::None => (),
            DebugType::Full => match r {
                Ok(_) => println!("{debug_id} Leave: set_allocation_size -> Ok"),
                Err(error) => println!(
                    "{debug_id} Leave: set_allocation_size -> {:?}",
                    super::Error::NtStatus(error)
                ),
            },
            DebugType::Error => {
                if let Err(error) = r {
                    println!(
                        "{debug_id} Leave: set_allocation_size -> {:?}",
                        super::Error::NtStatus(error)
                    );
                }
            }
        }

        r
    }

    fn get_disk_free_space(
        &'h self,
        info: &OperationInfo<'c, 'h, Self>,
    ) -> OperationResult<DiskSpaceInfo> {
        let debug_id = self.generate_debug_id();

        if self.debug_type == DebugType::Full {
            println!("{debug_id} Enter: get_disk_free_space");
        }

        let r = self.get_disk_free_space_(info);

        match self.debug_type {
            DebugType::None => (),
            DebugType::Full => match r {
                Ok(_) => println!("{debug_id} Leave: get_disk_free_space -> Ok"),
                Err(error) => println!(
                    "{debug_id} Leave: get_disk_free_space -> {:?}",
                    super::Error::NtStatus(error)
                ),
            },
            DebugType::Error => {
                if let Err(error) = r {
                    println!(
                        "{debug_id} Leave: get_disk_free_space -> {:?}",
                        super::Error::NtStatus(error)
                    );
                }
            }
        }

        r
    }

    fn get_volume_information(
        &'h self,
        info: &OperationInfo<'c, 'h, Self>,
    ) -> OperationResult<VolumeInfo> {
        let debug_id = self.generate_debug_id();

        if self.debug_type == DebugType::Full {
            println!("{debug_id} Enter: get_volume_information");
        }

        let r = self.get_volume_information_(info);

        match self.debug_type {
            DebugType::None => (),
            DebugType::Full => match r {
                Ok(_) => println!("{debug_id} Leave: get_volume_information -> Ok"),
                Err(error) => println!(
                    "{debug_id} Leave: get_volume_information -> {:?}",
                    super::Error::NtStatus(error)
                ),
            },
            DebugType::Error => {
                if let Err(error) = r {
                    println!(
                        "{debug_id} Leave: get_volume_information -> {:?}",
                        super::Error::NtStatus(error)
                    );
                }
            }
        }

        r
    }

    fn mounted(
        &'h self,
        mount_point: &U16CStr,
        info: &OperationInfo<'c, 'h, Self>,
    ) -> OperationResult<()> {
        let debug_id = self.generate_debug_id();

        if self.debug_type == DebugType::Full {
            println!(
                "{debug_id} Enter: mounted {:?}",
                mount_point.to_string_lossy()
            );
        }

        let r = self.mounted_(mount_point, info);

        match self.debug_type {
            DebugType::None => (),
            DebugType::Full => match r {
                Ok(_) => println!("{debug_id} Leave: mounted -> Ok"),
                Err(error) => println!(
                    "{debug_id} Leave: mounted -> {:?}",
                    super::Error::NtStatus(error)
                ),
            },
            DebugType::Error => {
                if let Err(error) = r {
                    println!(
                        "{debug_id} Leave: mounted -> {:?}",
                        super::Error::NtStatus(error)
                    );
                }
            }
        }

        r
    }

    fn unmounted(&'h self, info: &OperationInfo<'c, 'h, Self>) -> OperationResult<()> {
        let debug_id = self.generate_debug_id();

        if self.debug_type == DebugType::Full {
            println!("{debug_id} Enter: unmounted");
        }

        let r = self.unmounted_(info);

        match self.debug_type {
            DebugType::None => (),
            DebugType::Full => match r {
                Ok(_) => println!("{debug_id} Leave: unmounted -> Ok"),
                Err(error) => println!(
                    "{debug_id} Leave: unmounted -> {:?}",
                    super::Error::NtStatus(error)
                ),
            },
            DebugType::Error => {
                if let Err(error) = r {
                    println!(
                        "{debug_id} Leave: unmounted -> {:?}",
                        super::Error::NtStatus(error)
                    );
                }
            }
        }

        r
    }
}
// Input looks like "\", "\desktop.ini", "\reponame\desktop.ini",...
// Returns (Some(repository name), path in repository) if there is at least one subdirectory, and
// (None, path to element in root) if no repository is used.
fn decompose_path(path: &U16CStr) -> OperationResult<(Option<U16CString>, U16CString)> {
    let slice = path.as_slice();

    if slice.is_empty() {
        tracing::error!("MultiRepoVFS path is too short, should start with '\\' {path:?}");
        return Err(STATUS_INVALID_PARAMETER);
    }

    if slice[0] != '\\' as u16 {
        tracing::error!("MultiRepoVFS path does not start with '\\' {path:?}");
        return Err(STATUS_INVALID_PARAMETER);
    }

    let slice = &slice[1..];

    // TODO: We don't really need null terminated strings in our functions, which could simplify
    // this code and perhaps also remove allocations.
    if slice.is_empty() {
        let path = U16CString::from_ustr(U16Str::from_slice(path.as_slice()))
            .map_err(|_| STATUS_INVALID_PARAMETER)?;
        Ok((None, path))
    } else if let Some(next_backslash) = slice.iter().position(|x| *x == '\\' as u16) {
        let (name, path) = slice.split_at(next_backslash);
        let name = U16CString::from_ustr(U16Str::from_slice(name))
            .map_err(|_| STATUS_INVALID_PARAMETER)?;
        let path = U16CString::from_ustr(U16Str::from_slice(path))
            .map_err(|_| STATUS_INVALID_PARAMETER)?;
        Ok((Some(name), path))
    } else {
        let name = U16CString::from_ustr(U16Str::from_slice(slice))
            .map_err(|_| STATUS_INVALID_PARAMETER)?;
        let path = U16CString::from_str("\\").map_err(|_| STATUS_INVALID_PARAMETER)?;
        Ok((Some(name), path))
    }
}

// https://github.com/dokan-dev/dokan-rust/blob/master/dokan/src/file_system.rs
impl From<FileSystemMountError> for MountError {
    fn from(error: FileSystemMountError) -> Self {
        match error {
            FileSystemMountError::DriveLetter
            | FileSystemMountError::Mount
            | FileSystemMountError::MountPoint => MountError::InvalidMountPoint,
            FileSystemMountError::DriverInstall => MountError::DriverInstall,
            FileSystemMountError::Start
            | FileSystemMountError::General
            | FileSystemMountError::Version => MountError::Backend(Box::new(error)),
        }
    }
}
