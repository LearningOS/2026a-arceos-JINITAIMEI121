//! Root directory of the filesystem
//!
//! TODO: it doesn't work very well if the mount points have containment relationships.

use alloc::{string::String, sync::Arc, vec::Vec};
use axerrno::{ax_err, AxError, AxResult};
use axfs_vfs::{VfsNodeAttr, VfsNodeOps, VfsNodeRef, VfsNodeType, VfsOps, VfsResult};
use axsync::Mutex;
use lazyinit::LazyInit;

use crate::{api::FileType, fs, mounts};

static CURRENT_DIR_PATH: Mutex<String> = Mutex::new(String::new());
static CURRENT_DIR: LazyInit<Mutex<VfsNodeRef>> = LazyInit::new();

struct MountPoint {
    path: &'static str,
    fs: Arc<dyn VfsOps>,
}

struct RootDirectory {
    main_fs: Arc<dyn VfsOps>,
    mounts: Vec<MountPoint>,
}

static ROOT_DIR: LazyInit<Arc<RootDirectory>> = LazyInit::new();

impl MountPoint {
    pub fn new(path: &'static str, fs: Arc<dyn VfsOps>) -> Self {
        Self { path, fs }
    }
}

impl Drop for MountPoint {
    fn drop(&mut self) {
        self.fs.umount().ok();
    }
}

impl RootDirectory {
    pub const fn new(main_fs: Arc<dyn VfsOps>) -> Self {
        Self {
            main_fs,
            mounts: Vec::new(),
        }
    }

    pub fn mount(&mut self, path: &'static str, fs: Arc<dyn VfsOps>) -> AxResult {
        if path == "/" {
            return ax_err!(InvalidInput, "cannot mount root filesystem");
        }
        if !path.starts_with('/') {
            return ax_err!(InvalidInput, "mount path must start with '/'");
        }
        if self.mounts.iter().any(|mp| mp.path == path) {
            return ax_err!(InvalidInput, "mount point already exists");
        }
        // create the mount point in the main filesystem if it does not exist
        self.main_fs.root_dir().create(path, FileType::Dir)?;
        fs.mount(path, self.main_fs.root_dir().lookup(path)?)?;
        self.mounts.push(MountPoint::new(path, fs));
        Ok(())
    }

    pub fn _umount(&mut self, path: &str) {
        self.mounts.retain(|mp| mp.path != path);
    }

    pub fn contains(&self, path: &str) -> bool {
        self.mounts.iter().any(|mp| mp.path == path)
    }

    fn rename_from(&self, current_dir: &str, old: &str, new: &str) -> AxResult {
        if old.is_empty() || new.is_empty() {
            return ax_err!(NotFound);
        }
        let absolute = |path: &str| {
            if path.starts_with('/') {
                String::from(path)
            } else {
                String::from(current_dir) + path
            }
        };
        self.rename(&absolute(old), &absolute(new))
    }

    fn rename_path(&self, path: &str) -> AxResult<String> {
        if path.is_empty() {
            return ax_err!(NotFound);
        }
        let trimmed = path.trim_end_matches('/');
        let (parent, name) = trimmed.rsplit_once('/').unwrap_or(("", trimmed));
        if matches!(name, "." | "..") {
            return ax_err!(InvalidInput);
        }
        let mut resolved = String::new();
        for component in parent.split('/') {
            match component {
                "" | "." => {}
                ".." => resolved.truncate(resolved.rfind('/').unwrap_or(0)),
                _ => {
                    resolved.push('/');
                    resolved.push_str(component);
                    // Validate each directory before allowing a later '..' to cancel it.
                    let node =
                        self.lookup_mounted_fs(&resolved, |fs, rest| fs.root_dir().lookup(rest))?;
                    if !node.get_attr()?.is_dir() {
                        return ax_err!(NotADirectory);
                    }
                }
            }
        }
        resolved.push('/');
        resolved.push_str(name);
        if path.ends_with('/') && !name.is_empty() {
            resolved.push('/');
        }
        Ok(resolved)
    }

    fn lookup_mounted_fs<F, T>(&self, path: &str, f: F) -> AxResult<T>
    where
        F: FnOnce(Arc<dyn VfsOps>, &str) -> AxResult<T>,
    {
        debug!("lookup at root: {}", path);
        let path = path.trim_start_matches('/');
        if let Some(rest) = path.strip_prefix("./") {
            return self.lookup_mounted_fs(rest, f);
        }

        let mut idx = 0;
        let mut max_len = 0;

        // Find the filesystem that has the longest mounted path match
        // TODO: more efficient, e.g. trie
        for (i, mp) in self.mounts.iter().enumerate() {
            if let Some(rest) = path.strip_prefix(&mp.path[1..]) {
                if (rest.is_empty() || rest.starts_with('/')) && mp.path.len() - 1 > max_len {
                    max_len = mp.path.len() - 1;
                    idx = i;
                }
            }
        }

        if max_len == 0 {
            f(self.main_fs.clone(), path) // not matched any mount point
        } else {
            f(
                self.mounts[idx].fs.clone(),
                path[max_len..].trim_start_matches('/'),
            )
        }
    }
}

impl VfsNodeOps for RootDirectory {
    axfs_vfs::impl_vfs_dir_default! {}

    fn get_attr(&self) -> VfsResult<VfsNodeAttr> {
        self.main_fs.root_dir().get_attr()
    }

    fn lookup(self: Arc<Self>, path: &str) -> VfsResult<VfsNodeRef> {
        self.lookup_mounted_fs(path, |fs, rest_path| fs.root_dir().lookup(rest_path))
    }

    fn create(&self, path: &str, ty: VfsNodeType) -> VfsResult {
        self.lookup_mounted_fs(path, |fs, rest_path| {
            if rest_path.is_empty() {
                Ok(()) // already exists
            } else {
                fs.root_dir().create(rest_path, ty)
            }
        })
    }

    fn remove(&self, path: &str) -> VfsResult {
        self.lookup_mounted_fs(path, |fs, rest_path| {
            if rest_path.is_empty() {
                ax_err!(PermissionDenied) // cannot remove mount points
            } else {
                fs.root_dir().remove(rest_path)
            }
        })
    }

    fn rename(&self, src_path: &str, dst_path: &str) -> VfsResult {
        let src_path = self.rename_path(src_path)?;
        let dst_path = self.rename_path(dst_path)?;
        self.lookup_mounted_fs(&src_path, |src_fs, src_rest| {
            self.lookup_mounted_fs(&dst_path, |dst_fs, dst_rest| {
                if src_rest.is_empty() || dst_rest.is_empty() {
                    ax_err!(PermissionDenied) // cannot rename mount points
                } else if !Arc::ptr_eq(&src_fs, &dst_fs) {
                    ax_err!(Unsupported)
                } else {
                    src_fs.root_dir().rename(src_rest, dst_rest)
                }
            })
        })
    }
}

pub(crate) fn init_rootfs(disk: crate::dev::Disk) {
    cfg_if::cfg_if! {
        if #[cfg(feature = "myfs")] { // override the default filesystem
            let main_fs = fs::myfs::new_myfs(disk);
        } else if #[cfg(feature = "fatfs")] {
            static FAT_FS: LazyInit<Arc<fs::fatfs::FatFileSystem>> = LazyInit::new();
            FAT_FS.init_once(Arc::new(fs::fatfs::FatFileSystem::new(disk)));
            FAT_FS.init();
            let main_fs = FAT_FS.clone();
        }
    }

    let mut root_dir = RootDirectory::new(main_fs);

    #[cfg(feature = "devfs")]
    root_dir
        .mount("/dev", mounts::devfs())
        .expect("failed to mount devfs at /dev");

    #[cfg(feature = "ramfs")]
    root_dir
        .mount("/tmp", mounts::ramfs())
        .expect("failed to mount ramfs at /tmp");

    // Mount another ramfs as procfs
    #[cfg(feature = "procfs")]
    root_dir // should not fail
        .mount("/proc", mounts::procfs().unwrap())
        .expect("fail to mount procfs at /proc");

    // Mount another ramfs as sysfs
    #[cfg(feature = "sysfs")]
    root_dir // should not fail
        .mount("/sys", mounts::sysfs().unwrap())
        .expect("fail to mount sysfs at /sys");

    ROOT_DIR.init_once(Arc::new(root_dir));
    CURRENT_DIR.init_once(Mutex::new(ROOT_DIR.clone()));
    *CURRENT_DIR_PATH.lock() = "/".into();
}

fn parent_node_of(dir: Option<&VfsNodeRef>, path: &str) -> VfsNodeRef {
    if path.starts_with('/') {
        ROOT_DIR.clone()
    } else {
        dir.cloned().unwrap_or_else(|| CURRENT_DIR.lock().clone())
    }
}

pub(crate) fn absolute_path(path: &str) -> AxResult<String> {
    if path.starts_with('/') {
        Ok(axfs_vfs::path::canonicalize(path))
    } else {
        let path = CURRENT_DIR_PATH.lock().clone() + path;
        Ok(axfs_vfs::path::canonicalize(&path))
    }
}

pub(crate) fn lookup(dir: Option<&VfsNodeRef>, path: &str) -> AxResult<VfsNodeRef> {
    if path.is_empty() {
        return ax_err!(NotFound);
    }
    let node = parent_node_of(dir, path).lookup(path)?;
    if path.ends_with('/') && !node.get_attr()?.is_dir() {
        ax_err!(NotADirectory)
    } else {
        Ok(node)
    }
}

pub(crate) fn create_file(dir: Option<&VfsNodeRef>, path: &str) -> AxResult<VfsNodeRef> {
    if path.is_empty() {
        return ax_err!(NotFound);
    } else if path.ends_with('/') {
        return ax_err!(NotADirectory);
    }
    let parent = parent_node_of(dir, path);
    parent.create(path, VfsNodeType::File)?;
    parent.lookup(path)
}

pub(crate) fn create_dir(dir: Option<&VfsNodeRef>, path: &str) -> AxResult {
    match lookup(dir, path) {
        Ok(_) => ax_err!(AlreadyExists),
        Err(AxError::NotFound) => parent_node_of(dir, path).create(path, VfsNodeType::Dir),
        Err(e) => Err(e),
    }
}

pub(crate) fn remove_file(dir: Option<&VfsNodeRef>, path: &str) -> AxResult {
    let node = lookup(dir, path)?;
    let attr = node.get_attr()?;
    if attr.is_dir() {
        ax_err!(IsADirectory)
    } else if !attr.perm().owner_writable() {
        ax_err!(PermissionDenied)
    } else {
        parent_node_of(dir, path).remove(path)
    }
}

pub(crate) fn remove_dir(dir: Option<&VfsNodeRef>, path: &str) -> AxResult {
    if path.is_empty() {
        return ax_err!(NotFound);
    }
    let path_check = path.trim_matches('/');
    if path_check.is_empty() {
        return ax_err!(DirectoryNotEmpty); // rm -d '/'
    } else if path_check == "."
        || path_check == ".."
        || path_check.ends_with("/.")
        || path_check.ends_with("/..")
    {
        return ax_err!(InvalidInput);
    }
    if ROOT_DIR.contains(&absolute_path(path)?) {
        return ax_err!(PermissionDenied);
    }

    let node = lookup(dir, path)?;
    let attr = node.get_attr()?;
    if !attr.is_dir() {
        ax_err!(NotADirectory)
    } else if !attr.perm().owner_writable() {
        ax_err!(PermissionDenied)
    } else {
        parent_node_of(dir, path).remove(path)
    }
}

pub(crate) fn current_dir() -> AxResult<String> {
    Ok(CURRENT_DIR_PATH.lock().clone())
}

pub(crate) fn set_current_dir(path: &str) -> AxResult {
    let mut abs_path = absolute_path(path)?;
    if !abs_path.ends_with('/') {
        abs_path += "/";
    }
    if abs_path == "/" {
        *CURRENT_DIR.lock() = ROOT_DIR.clone();
        *CURRENT_DIR_PATH.lock() = "/".into();
        return Ok(());
    }

    let node = lookup(None, &abs_path)?;
    let attr = node.get_attr()?;
    if !attr.is_dir() {
        ax_err!(NotADirectory)
    } else if !attr.perm().owner_executable() {
        ax_err!(PermissionDenied)
    } else {
        *CURRENT_DIR.lock() = node;
        *CURRENT_DIR_PATH.lock() = abs_path;
        Ok(())
    }
}

pub(crate) fn rename(old: &str, new: &str) -> AxResult {
    let current_dir = CURRENT_DIR_PATH.lock().clone();
    ROOT_DIR.rename_from(&current_dir, old, new)
}

#[cfg(all(test, feature = "ramfs"))]
mod tests {
    use super::*;
    use axfs_ramfs::RamFileSystem;

    #[test]
    fn rename_routes_both_paths_from_one_root() -> AxResult {
        let main = Arc::new(RamFileSystem::new());
        let tmp = Arc::new(RamFileSystem::new());
        let data = Arc::new(RamFileSystem::new());
        let mut root = RootDirectory::new(main.clone());
        root.mount("/tmp", tmp.clone())?;
        root.mount("/data", data.clone())?;
        tmp.root_dir().create("f", VfsNodeType::File)?;
        tmp.root_dir().create("sub", VfsNodeType::Dir)?;
        let file = tmp.root_dir().lookup("f")?;
        for (cwd, src, dst, local) in [
            ("/", "/tmp/f", "/tmp/g", "g"),
            ("/tmp/", "g", "/tmp/f", "f"),
            ("/tmp/", "/tmp/f", "g", "g"),
            ("/tmp/sub/", "../g", "/./tmp/sub/../f", "f"),
            ("/tmp/", "../tmp/f", "./f", "f"),
        ] {
            root.rename_from(cwd, src, dst)?;
            assert!(Arc::ptr_eq(&file, &tmp.root_dir().lookup(local)?));
        }
        tmp.root_dir().create("target", VfsNodeType::File)?;
        let target = tmp.root_dir().lookup("target")?;
        target.write_at(0, b"old")?;
        root.rename_from("/tmp/", "f", "target")?;
        assert!(Arc::ptr_eq(&file, &tmp.root_dir().lookup("target")?));
        assert_eq!(tmp.root_dir().lookup("f").err(), Some(AxError::NotFound));
        let mut buf = [0; 3];
        target.read_at(0, &mut buf)?;
        assert_eq!(&buf, b"old");

        main.root_dir().create("tmp-other", VfsNodeType::Dir)?;
        for (dir, prefix) in [
            (main.root_dir().lookup("tmp-other")?, "/tmp-other/"),
            (data.root_dir(), "/data/"),
        ] {
            dir.create("f", VfsNodeType::File)?;
            let file = dir.clone().lookup("f")?;
            root.rename_from(prefix, "f", "g")?;
            assert!(Arc::ptr_eq(&file, &dir.lookup("g")?));
        }
        Ok(())
    }

    #[test]
    fn rename_route_errors_leave_entries_intact() -> AxResult {
        let main = Arc::new(RamFileSystem::new());
        let tmp = Arc::new(RamFileSystem::new());
        let data = Arc::new(RamFileSystem::new());
        let mut root = RootDirectory::new(main.clone());
        root.mount("/tmp", tmp.clone())?;
        root.mount("/data", data.clone())?;
        tmp.root_dir().create("f", VfsNodeType::File)?;
        tmp.root_dir().create("target", VfsNodeType::File)?;
        tmp.root_dir().create("empty", VfsNodeType::Dir)?;
        tmp.root_dir().create("full", VfsNodeType::Dir)?;
        tmp.root_dir().create("full/child", VfsNodeType::File)?;
        data.root_dir().create("f", VfsNodeType::File)?;
        main.root_dir().create("f", VfsNodeType::File)?;
        let paths = ["f", "target", "empty", "full", "full/child"];
        let before: Vec<_> = paths
            .iter()
            .map(|p| tmp.root_dir().lookup(p).unwrap())
            .collect();
        let other = data.root_dir().lookup("f")?;
        let main_file = main.root_dir().lookup("f")?;
        for (src, dst, error) in [
            ("missing", "target", AxError::NotFound),
            ("f/", "target", AxError::NotADirectory),
            ("f", "target/", AxError::NotADirectory),
            ("f/../f", "target", AxError::NotADirectory),
            ("f", "f/../target", AxError::NotADirectory),
            ("f", "full/.", AxError::InvalidInput),
            ("full/..", "target", AxError::InvalidInput),
            ("empty/", "full/", AxError::DirectoryNotEmpty),
            ("f", "/data/f", AxError::Unsupported),
            ("/data/f", "target", AxError::Unsupported),
            ("f", "../f", AxError::Unsupported),
            ("/tmp/", "target", AxError::PermissionDenied),
            ("f", "/tmp///", AxError::PermissionDenied),
            ("/", "target", AxError::PermissionDenied),
            ("f", "/", AxError::PermissionDenied),
            ("", "target", AxError::NotFound),
            ("f", "", AxError::NotFound),
        ] {
            assert_eq!(
                root.rename_from("/tmp/", src, dst),
                Err(error),
                "{src} -> {dst}"
            );
            for (path, node) in paths.iter().zip(&before) {
                assert!(Arc::ptr_eq(node, &tmp.root_dir().lookup(path)?));
            }
            assert!(Arc::ptr_eq(&other, &data.root_dir().lookup("f")?));
            assert!(Arc::ptr_eq(&main_file, &main.root_dir().lookup("f")?));
        }
        Ok(())
    }
}
