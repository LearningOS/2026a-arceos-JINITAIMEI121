use std::sync::Arc;

use axfs_vfs::{VfsError, VfsNodeType, VfsResult};

use crate::*;

fn test_ramfs_ops(devfs: &RamFileSystem) -> VfsResult {
    const N: usize = 32;
    const N_HALF: usize = N / 2;
    let mut buf = [1; N];

    let root = devfs.root_dir();
    assert!(root.get_attr()?.is_dir());
    assert_eq!(root.get_attr()?.file_type(), VfsNodeType::Dir);
    assert_eq!(
        root.clone().lookup("urandom").err(),
        Some(VfsError::NotFound)
    );
    assert_eq!(
        root.clone().lookup("f1/").err(),
        Some(VfsError::NotADirectory)
    );

    let node = root.lookup("////f1")?;
    assert_eq!(node.get_attr()?.file_type(), VfsNodeType::File);
    assert!(!node.get_attr()?.is_dir());
    assert_eq!(node.get_attr()?.size(), 0);
    assert_eq!(node.read_at(0, &mut buf)?, 0);
    assert_eq!(buf, [1; N]);

    assert_eq!(node.write_at(N_HALF as _, &buf[..N_HALF])?, N_HALF);
    assert_eq!(node.read_at(0, &mut buf)?, N);
    assert_eq!(buf[..N_HALF], [0; N_HALF]);
    assert_eq!(buf[N_HALF..], [1; N_HALF]);
    assert_eq!(node.lookup("/").err(), Some(VfsError::NotADirectory));

    let foo = devfs.root_dir().lookup(".///.//././/.////foo")?;
    assert!(foo.get_attr()?.is_dir());
    assert_eq!(
        foo.read_at(10, &mut buf).err(),
        Some(VfsError::IsADirectory)
    );
    assert!(Arc::ptr_eq(
        &foo.clone().lookup("/f3")?,
        &devfs.root_dir().lookup(".//./foo///f3")?,
    ));
    assert_eq!(
        foo.clone().lookup("/bar//f4")?.get_attr()?.file_type(),
        VfsNodeType::File
    );
    assert_eq!(
        foo.lookup("/bar///")?.get_attr()?.file_type(),
        VfsNodeType::Dir
    );

    Ok(())
}

fn test_get_parent(devfs: &RamFileSystem) -> VfsResult {
    let root = devfs.root_dir();
    assert!(root.parent().is_none());

    let node = root.clone().lookup("f1")?;
    assert!(node.parent().is_none());

    let node = root.clone().lookup(".//foo/bar")?;
    assert!(node.parent().is_some());
    let parent = node.parent().unwrap();
    assert!(Arc::ptr_eq(&parent, &root.clone().lookup("foo")?));
    assert!(parent.lookup("bar").is_ok());

    let node = root.clone().lookup("foo/..")?;
    assert!(Arc::ptr_eq(&node, &root.clone().lookup(".")?));

    assert!(Arc::ptr_eq(
        &root.clone().lookup("/foo/..")?,
        &devfs.root_dir().lookup(".//./foo/././bar/../..")?,
    ));
    assert!(Arc::ptr_eq(
        &root.clone().lookup("././/foo//./../foo//bar///..//././")?,
        &devfs.root_dir().lookup(".//./foo/")?,
    ));
    assert!(Arc::ptr_eq(
        &root.clone().lookup("///foo//bar///../f3")?,
        &root.lookup("foo/.//f3")?,
    ));

    Ok(())
}

#[test]
fn test_ramfs() {
    // .
    // ├── foo
    // │   ├── bar
    // │   │   └── f4
    // │   └── f3
    // ├── f1
    // └── f2

    let ramfs = RamFileSystem::new();
    let root = ramfs.root_dir();
    root.create("f1", VfsNodeType::File).unwrap();
    root.create("f2", VfsNodeType::File).unwrap();
    root.create("foo", VfsNodeType::Dir).unwrap();

    let dir_foo = root.lookup("foo").unwrap();
    dir_foo.create("f3", VfsNodeType::File).unwrap();
    dir_foo.create("bar", VfsNodeType::Dir).unwrap();

    let dir_bar = dir_foo.lookup("bar").unwrap();
    dir_bar.create("f4", VfsNodeType::File).unwrap();

    let mut entries = ramfs.root_dir_node().get_entries();
    entries.sort();
    assert_eq!(entries, ["f1", "f2", "foo"]);

    test_ramfs_ops(&ramfs).unwrap();
    test_get_parent(&ramfs).unwrap();

    let root = ramfs.root_dir();
    assert_eq!(root.remove("f1"), Ok(()));
    assert_eq!(root.remove("//f2"), Ok(()));
    assert_eq!(root.remove("f3").err(), Some(VfsError::NotFound));
    assert_eq!(root.remove("foo").err(), Some(VfsError::DirectoryNotEmpty));
    assert_eq!(root.remove("foo/..").err(), Some(VfsError::InvalidInput));
    assert_eq!(
        root.remove("foo/./bar").err(),
        Some(VfsError::DirectoryNotEmpty)
    );
    assert_eq!(root.remove("foo/bar/f4"), Ok(()));
    assert_eq!(root.remove("foo/bar"), Ok(()));
    assert_eq!(root.remove("./foo//.//f3"), Ok(()));
    assert_eq!(root.remove("./foo"), Ok(()));
    assert!(ramfs.root_dir_node().get_entries().is_empty());
}

#[test]
fn rename_preserves_inode_and_open_handle() -> VfsResult {
    let fs = RamFileSystem::new();
    let root = fs.root_dir();
    root.create("f1", VfsNodeType::File)?;
    let open = root.clone().lookup("f1")?;
    open.write_at(0, b"hello")?;

    root.rename("/f1", "//f2")?;
    assert_eq!(root.clone().lookup("f1").err(), Some(VfsError::NotFound));
    let renamed = root.clone().lookup("f2")?;
    assert!(Arc::ptr_eq(&open, &renamed));
    let mut buf = [0; 5];
    assert_eq!(renamed.read_at(0, &mut buf)?, 5);
    assert_eq!(&buf, b"hello");
    open.write_at(0, b"H")?;
    renamed.read_at(0, &mut buf)?;
    assert_eq!(&buf, b"Hello");
    Ok(())
}

#[test]
fn rename_resolves_same_parent() -> VfsResult {
    let fs = RamFileSystem::new();
    let root = fs.root_dir();
    root.create("a", VfsNodeType::Dir)?;
    root.create("a/b", VfsNodeType::Dir)?;
    root.create("a/f1", VfsNodeType::File)?;
    let original = root.clone().lookup("a/f1")?;
    root.rename("//a/./f1", "a//b/../f2")?;
    assert!(Arc::ptr_eq(&original, &root.clone().lookup("a/f2")?));
    let subdir = root.clone().lookup("a/b")?;
    subdir.rename("../f2", "./../f3")?;
    assert!(Arc::ptr_eq(&original, &root.lookup("a/f3")?));
    Ok(())
}

#[test]
fn rename_to_self_requires_source() -> VfsResult {
    let fs = RamFileSystem::new();
    let root = fs.root_dir();
    root.create("f", VfsNodeType::File)?;
    let original = root.clone().lookup("f")?;
    root.rename("f", "./f")?;
    assert!(Arc::ptr_eq(&original, &root.clone().lookup("f")?));
    assert_eq!(root.rename("missing", "missing"), Err(VfsError::NotFound));
    assert_eq!(root.rename("missing", "f"), Err(VfsError::NotFound));
    assert!(Arc::ptr_eq(&original, &root.clone().lookup("f")?));
    root.create("d", VfsNodeType::Dir)?;
    root.create("d/child", VfsNodeType::File)?;
    root.rename("d", "./d/")?;
    Ok(())
}

#[test]
fn rename_replaces_file_without_changing_open_target() -> VfsResult {
    let fs = RamFileSystem::new();
    let root = fs.root_dir();
    root.create("src", VfsNodeType::File)?;
    root.create("dst", VfsNodeType::File)?;
    let src = root.clone().lookup("src")?;
    let dst = root.clone().lookup("dst")?;
    src.write_at(0, b"new")?;
    dst.write_at(0, b"old")?;
    root.rename("src", "dst")?;
    assert!(Arc::ptr_eq(&src, &root.clone().lookup("dst")?));
    assert_eq!(root.lookup("src").err(), Some(VfsError::NotFound));
    let mut buf = [0; 3];
    dst.read_at(0, &mut buf)?;
    assert_eq!(&buf, b"old");
    Ok(())
}

#[test]
fn rename_directory_preserves_children_and_parent() -> VfsResult {
    let fs = RamFileSystem::new();
    let root = fs.root_dir();
    root.create("src", VfsNodeType::Dir)?;
    root.create("src/child", VfsNodeType::File)?;
    root.create("dst", VfsNodeType::Dir)?;
    let src = root.clone().lookup("src")?;
    let child = root.clone().lookup("src/child")?;
    root.rename("src/", "dst/")?;
    assert!(Arc::ptr_eq(&src, &root.clone().lookup("dst")?));
    assert!(Arc::ptr_eq(&child, &root.clone().lookup("dst/child")?));
    assert!(Arc::ptr_eq(&root, &src.parent().unwrap()));
    root.rename("dst", "renamed")?;
    assert!(Arc::ptr_eq(&child, &root.lookup("renamed/child")?));
    Ok(())
}

#[test]
fn rename_errors_preserve_source_and_destination() -> VfsResult {
    let fs = RamFileSystem::new();
    let root = fs.root_dir();
    root.create("file", VfsNodeType::File)?;
    root.create("empty", VfsNodeType::Dir)?;
    root.create("full", VfsNodeType::Dir)?;
    root.create("full/child", VfsNodeType::File)?;
    root.create("other", VfsNodeType::Dir)?;
    root.create("other/file", VfsNodeType::File)?;

    for (src, dst, error) in [
        ("file", "empty", VfsError::IsADirectory),
        ("empty", "file", VfsError::NotADirectory),
        ("empty", "full", VfsError::DirectoryNotEmpty),
        ("file", "other/file", VfsError::Unsupported),
        ("full/child", "other/file", VfsError::Unsupported),
    ] {
        let before_src = root.clone().lookup(src)?;
        let before_dst = root.clone().lookup(dst)?;
        assert_eq!(root.rename(src, dst), Err(error));
        assert!(Arc::ptr_eq(&before_src, &root.clone().lookup(src)?));
        assert!(Arc::ptr_eq(&before_dst, &root.clone().lookup(dst)?));
    }
    let file = root.clone().lookup("file")?;
    for (dst, error) in [
        ("missing/dst", VfsError::NotFound),
        ("other/new", VfsError::Unsupported),
        ("file/dst", VfsError::NotADirectory),
        ("file/../dst", VfsError::NotADirectory),
        ("new/", VfsError::NotADirectory),
    ] {
        assert_eq!(root.rename("file", dst), Err(error));
        assert!(Arc::ptr_eq(&file, &root.clone().lookup("file")?));
        assert!(root.clone().lookup(dst).is_err());
    }
    assert_eq!(root.rename("file/", "new"), Err(VfsError::NotADirectory));
    assert!(Arc::ptr_eq(&file, &root.lookup("file")?));
    Ok(())
}

#[test]
fn rename_rejects_special_names() -> VfsResult {
    let fs = RamFileSystem::new();
    let root = fs.root_dir();
    root.create("f", VfsNodeType::File)?;
    root.create("d", VfsNodeType::Dir)?;
    let file = root.clone().lookup("f")?;
    for path in ["", "/", "///", ".", "..", "d/.", "d/..", "d/./"] {
        assert_eq!(root.rename("f", path), Err(VfsError::InvalidInput));
        assert_eq!(root.rename(path, "f"), Err(VfsError::InvalidInput));
        assert!(Arc::ptr_eq(&file, &root.clone().lookup("f")?));
    }
    Ok(())
}

#[test]
fn rename_mounted_fs_receives_symmetric_local_paths() -> VfsResult {
    let main_fs = RamFileSystem::new();
    let main_root = main_fs.root_dir();
    main_root.create("data", VfsNodeType::Dir)?;
    let fs = RamFileSystem::new();
    fs.mount("/data", main_root.lookup("data")?)?;
    let root = fs.root_dir();
    root.create("f1", VfsNodeType::File)?;
    let original = root.clone().lookup("f1")?;
    original.write_at(0, b"hello")?;
    root.rename("/f1", "/f2")?;
    assert_eq!(root.clone().lookup("f1").err(), Some(VfsError::NotFound));
    assert!(Arc::ptr_eq(&original, &root.clone().lookup("f2")?));
    let mut buf = [0; 5];
    root.clone().lookup("f2")?.read_at(0, &mut buf)?;
    assert_eq!(&buf, b"hello");

    root.create("sub", VfsNodeType::Dir)?;
    root.create("sub/f1", VfsNodeType::File)?;
    root.rename("/sub/f1", "/sub/f2")?;
    assert!(root.clone().lookup("sub/f2").is_ok());
    assert_eq!(root.rename("/f2", "/sub/f3"), Err(VfsError::Unsupported));
    root.create("tmp", VfsNodeType::Dir)?;
    root.create("tmp/f1", VfsNodeType::File)?;
    root.rename("/tmp/f1", "/tmp/f2")?;
    assert!(root.clone().lookup("tmp/f2").is_ok());
    assert_eq!(root.rename("/f2", "/tmp/f2"), Err(VfsError::Unsupported));
    assert!(Arc::ptr_eq(&original, &root.lookup("f2")?));
    Ok(())
}

#[test]
fn rename_unmounted_tmp_is_an_ordinary_directory() -> VfsResult {
    let fs = RamFileSystem::new();
    let root = fs.root_dir();
    root.create("tmp", VfsNodeType::Dir)?;
    root.create("f1", VfsNodeType::File)?;
    assert_eq!(root.rename("/f1", "/tmp/f2"), Err(VfsError::Unsupported));
    root.create("tmp/f1", VfsNodeType::File)?;
    root.rename("/tmp/f1", "/tmp/f2")?;
    assert!(root.clone().lookup("tmp/f2").is_ok());
    assert!(root.lookup("f1").is_ok());
    Ok(())
}
