// Copyright 2020-2022 Ant Group. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0
//

#[cfg(all(feature = "fusedev", target_os = "linux"))]
#[macro_use]
extern crate log;

mod example;

#[cfg(all(feature = "fusedev", target_os = "linux"))]
mod fusedev_tests {
    use std::ffi::CString;
    use std::fs::File;
    use std::io::Result;
    use std::num::NonZeroUsize;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use fuse_backend_rs::abi::fuse_abi::{InHeader, Opcode, OutHeader};
    use fuse_backend_rs::api::server::{MetricsHook, Server};
    use fuse_backend_rs::api::{Vfs, VfsOptions};
    use fuse_backend_rs::overlayfs::{self, BoxedLayer, OverlayFs};
    use fuse_backend_rs::passthrough::{self, Config, PassthroughFs};
    use fuse_backend_rs::transport::FuseSession;
    use nix::mount::MsFlags;
    use nix::sys::mman::{mmap, munmap, MapFlags, ProtFlags};
    use vmm_sys_util::tempdir::TempDir;

    use crate::example::passthroughfs;
    use fuse_backend_rs::abi::fuse_abi::FsOptions;

    fn validate_two_git_directory(src: &str, dest: &str) -> bool {
        let str = format!(
            "cd {}; git config --global --add safe.directory {}; git ls-files; cd - > /dev/null",
            src, src
        );
        let src_files = exec(str.as_str()).unwrap();
        let str = format!(
            "cd {}; git config --global --add safe.directory {}; git ls-files; cd - > /dev/null",
            dest, dest
        );
        let dest_files = exec(str.as_str()).unwrap();
        if src_files != dest_files {
            error!(
                "src {}:\n{}\ndest {}:\n{}",
                src, src_files, dest, dest_files
            );
            return false;
        }

        let src_md5 = exec(
            format!(
                "cd {}; git ls-files --recurse-submodules | grep --invert-match rust-vmm-ci | xargs md5sum; cd - > /dev/null",
                src
            )
            .as_str(),
        )
        .unwrap();
        let dest_md5 = exec(
            format!(
                "cd {}; git ls-files --recurse-submodules | grep --invert-match rust-vmm-ci | xargs md5sum; cd - > /dev/null",
                dest
            )
            .as_str(),
        )
        .unwrap();
        if src_md5 != dest_md5 {
            error!("src {}:\n{}\ndest {}:\n{}", src, src_md5, dest, dest_md5,);
            return false;
        }

        return true;
    }

    fn exec(cmd: &str) -> Result<String> {
        debug!("exec: {}", cmd);
        let output = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .env("RUST_BACKTRACE", "1")
            .output()?;

        if !output.status.success() || output.stderr.len() > 0 {
            let msg = std::str::from_utf8(&output.stderr).unwrap();
            panic!("exec failed: {}: {}", cmd, msg);
        }
        let stdout = std::str::from_utf8(&output.stdout).unwrap();

        return Ok(stdout.to_string());
    }

    /// Validates that the mounted filesystem has the expected mount flags
    fn validate_mount_flags(mountpoint: &str, expected_flags: MsFlags) -> bool {
        // Use findmnt to get the mount flags
        let cmd = format!("findmnt -no OPTIONS {}", mountpoint);
        let output = match exec(&cmd) {
            Ok(out) => out,
            Err(_) => return false,
        };

        // Convert the expected flags to a string representation
        let expected_flags_str = msflags_to_string_set(expected_flags);

        // Check if all expected flags are present in the output
        for flag in expected_flags_str {
            if !output.contains(&flag) {
                error!(
                    "Expected flag '{}' not found in mount options: {}",
                    flag, output
                );
                return false;
            }
        }

        true
    }

    /// Converts MsFlags to a set of string representations
    fn msflags_to_string_set(flags: MsFlags) -> Vec<String> {
        let mut result = Vec::new();

        if flags.contains(MsFlags::MS_RDONLY) {
            result.push("ro".to_string());
        }
        if flags.contains(MsFlags::MS_NOSUID) {
            result.push("nosuid".to_string());
        }
        if flags.contains(MsFlags::MS_NODEV) {
            result.push("nodev".to_string());
        }
        if flags.contains(MsFlags::MS_NOEXEC) {
            result.push("noexec".to_string());
        }
        if flags.contains(MsFlags::MS_SYNCHRONOUS) {
            result.push("sync".to_string());
        }
        if flags.contains(MsFlags::MS_NOATIME) {
            result.push("noatime".to_string());
        }

        result
    }

    fn running_as_root() -> bool {
        unsafe { libc::geteuid() == 0 }
    }

    fn kernel_release() -> String {
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|release| release.trim().to_string())
            .unwrap_or_else(|_| "unknown".to_string())
    }

    fn kernel_at_least(major: u32, minor: u32) -> bool {
        let release = kernel_release();
        let mut fields = release.split('.');
        match (
            fields.next().and_then(|f| f.parse::<u32>().ok()),
            fields.next().and_then(|f| f.parse::<u32>().ok()),
        ) {
            (Some(kmaj), Some(kmin)) => (kmaj, kmin) >= (major, minor),
            _ => false,
        }
    }

    #[test]
    #[ignore] // it depends on privileged mode to pass through /dev/fuse
    fn integration_test_tree_gitrepo() -> Result<()> {
        // test the fuse-rs repository
        let src = Path::new(".").canonicalize().unwrap();
        let src_dir = src.to_str().unwrap();
        let tmp_dir = TempDir::new().unwrap();
        let mnt_dir = tmp_dir.as_path().to_str().unwrap();
        info!(
            "test passthroughfs src {:?} mountpoint {}",
            src_dir, mnt_dir
        );

        let mut daemon = passthroughfs::Daemon::new(src_dir, mnt_dir, 2).unwrap();
        daemon.mount().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        assert!(validate_two_git_directory(src_dir, mnt_dir));
        daemon.umount().unwrap();
        Ok(())
    }

    #[test]
    #[ignore]
    fn integration_test_mount_flags() -> Result<()> {
        // Test custom mount flags
        let src = Path::new(".").canonicalize().unwrap();
        let src_dir = src.to_str().unwrap();
        let tmp_dir = TempDir::new().unwrap();
        let mnt_dir = tmp_dir.as_path().to_str().unwrap();
        info!("test mount flags src {:?} mountpoint {}", src_dir, mnt_dir);

        // Create a set of custom mount flags
        let custom_flags = MsFlags::MS_NODEV | MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC;

        let mut daemon = passthroughfs::Daemon::new(src_dir, mnt_dir, 2).unwrap();

        // Set the custom mount flags
        daemon.set_mount_flags(custom_flags);

        // Mount the filesystem
        daemon.mount().unwrap();

        // Wait for the mount to complete
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Validate that the mounted filesystem has the expected flags
        assert!(validate_mount_flags(mnt_dir, custom_flags));

        // Unmount the filesystem
        daemon.umount().unwrap();

        Ok(())
    }

    // Group of the setgid directory in the FUSE_CREATE_SUPP_GROUP tests, used
    // as a supplementary group of the unprivileged creator.  It must not be a
    // group of the daemon itself, otherwise the create would succeed without
    // the extension.
    const SUPP_GID: libc::gid_t = 12345;

    /// Whether the FUSE_CREATE_SUPP_GROUP test `name` can run here.  It needs
    /// root (CAP_SETGID/CAP_CHOWN) and a kernel >= 6.3, both of which the
    /// GitHub CI runners provide.
    fn can_test_create_supp_group(name: &str) -> bool {
        if !running_as_root() {
            eprintln!("skipping {}: not running as root", name);
            return false;
        }
        if !kernel_at_least(6, 3) {
            eprintln!(
                "skipping {}: kernel {} does not support FUSE_CREATE_SUPP_GROUP (needs >= 6.3)",
                name,
                kernel_release()
            );
            return false;
        }
        true
    }

    /// Creates `root/setgid-dir`, a setgid directory owned by root:SUPP_GID
    /// with mode 2770, so only group members may create entries in it, and
    /// lets the unprivileged creator traverse `root`.
    fn make_setgid_dir(root: &Path) {
        let setgid_dir = root.join("setgid-dir");
        std::fs::create_dir(&setgid_dir).unwrap();
        let setgid_dir_c = CString::new(setgid_dir.as_os_str().as_bytes()).unwrap();
        assert_eq!(
            unsafe { libc::chown(setgid_dir_c.as_ptr(), 0, SUPP_GID) },
            0,
            "failed to chown the setgid directory"
        );
        std::fs::set_permissions(&setgid_dir, std::fs::Permissions::from_mode(0o2770)).unwrap();
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Creates `mnt/setgid-dir/file` as nobody, with SUPP_GID as its only
    /// supplementary group, and checks that the create succeeds.
    fn create_as_supp_group_member(mnt: &str) {
        // nobody's primary group differs from SUPP_GID.
        let uid: libc::uid_t = 65534;
        let mut cmd = Command::new("touch");
        cmd.arg(format!("{}/setgid-dir/file", mnt));
        unsafe {
            cmd.pre_exec(move || {
                // Add SUPP_GID to the supplementary groups while the process
                // still has CAP_SETGID, then drop all privileges.
                let groups = [SUPP_GID];
                if libc::setgroups(1, groups.as_ptr()) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setresgid(uid, uid, uid) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setresuid(uid, uid, uid) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let status = cmd.status().unwrap();
        assert!(
            status.success(),
            "create through the FUSE mount failed: {}",
            status
        );
    }

    #[test]
    #[ignore] // it depends on privileged mode to pass through /dev/fuse
    fn integration_test_create_supp_group() -> Result<()> {
        // End-to-end test for FUSE_CREATE_SUPP_GROUP: an unprivileged
        // process creates a file through the FUSE mount inside a setgid
        // directory whose group is only in the creator's supplementary
        // groups.  The kernel (>= 6.3) sends that group in a
        // FUSE_EXT_GROUPS extension, and the server must temporarily
        // adopt it, so that the create succeeds and the new file inherits
        // the group of the setgid directory.  Without the extension the
        // server-side create fails with EACCES, since the server adopts
        // the creator's credentials, which lack the directory's group.
        if !can_test_create_supp_group("integration_test_create_supp_group") {
            return Ok(());
        }

        let src_dir = TempDir::new().unwrap();
        let mnt_dir = TempDir::new().unwrap();
        let src = src_dir.as_path().to_str().unwrap().to_string();
        let mnt = mnt_dir.as_path().to_str().unwrap().to_string();
        make_setgid_dir(src_dir.as_path());

        let mut daemon = passthroughfs::Daemon::new(&src, &mnt, 1).unwrap();
        daemon.mount().unwrap();
        create_as_supp_group_member(&mnt);

        // The file must have been created on the source side with the group
        // of the setgid directory.
        let md = std::fs::metadata(src_dir.as_path().join("setgid-dir/file")).unwrap();
        assert_eq!(md.gid(), SUPP_GID);

        daemon.umount().unwrap();
        Ok(())
    }

    /// Opens an unnamed file in `dir` with O_TMPFILE and `extra` flags.
    fn open_tmpfile(dir: &Path, extra: i32) -> Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_TMPFILE | extra)
            .mode(0o600)
            .open(dir)
    }

    /// Gives the unnamed `file` the name `path`, like `linkat(2)` documents for O_TMPFILE.
    fn link_tmpfile(file: &std::fs::File, path: &Path) -> nix::Result<()> {
        use nix::fcntl::{AtFlags, AT_FDCWD};
        use std::os::fd::AsRawFd;

        let fd_path = Path::new("/proc/self/fd").join(file.as_raw_fd().to_string());
        nix::unistd::linkat(
            AT_FDCWD,
            fd_path.as_path(),
            AT_FDCWD,
            path,
            AtFlags::AT_SYMLINK_FOLLOW,
        )
    }

    #[test]
    #[ignore] // it depends on privileged mode to pass through /dev/fuse
    fn integration_test_tmpfile() -> Result<()> {
        // An O_TMPFILE open sends FUSE_TMPFILE. Without a reply to it, the open fails with
        // EOPNOTSUPP.
        if !kernel_at_least(6, 1) {
            eprintln!(
                "skipping integration_test_tmpfile: kernel {} does not send FUSE_TMPFILE (needs >= 6.1)",
                kernel_release()
            );
            return Ok(());
        }

        let src_dir = TempDir::new().unwrap();
        let mnt_dir = TempDir::new().unwrap();
        let src = src_dir.as_path().to_str().unwrap().to_string();
        let mnt = mnt_dir.as_path().to_str().unwrap().to_string();
        let mut daemon = passthroughfs::Daemon::new(&src, &mnt, 1).unwrap();
        daemon.mount().unwrap();

        let mut file =
            open_tmpfile(mnt_dir.as_path(), 0).expect("O_TMPFILE open through the mount");
        std::io::Write::write_all(&mut file, b"data").unwrap();
        assert_eq!(std::fs::read_dir(src_dir.as_path()).unwrap().count(), 0);
        link_tmpfile(&file, &mnt_dir.as_path().join("linked")).expect("linkat of the tmpfile");
        drop(file);
        assert_eq!(
            std::fs::read(src_dir.as_path().join("linked")).unwrap(),
            b"data"
        );

        // With O_EXCL the file can never get a name.
        let file = open_tmpfile(mnt_dir.as_path(), libc::O_EXCL).unwrap();
        let err = link_tmpfile(&file, &mnt_dir.as_path().join("excl")).unwrap_err();
        assert_eq!(err, nix::errno::Errno::ENOENT);
        drop(file);

        daemon.umount().unwrap();
        Ok(())
    }

    #[test]
    #[ignore] // it depends on privileged mode to pass through /dev/fuse
    fn integration_test_overlay_create_supp_group() -> Result<()> {
        // integration_test_create_supp_group on a standalone OverlayFs,
        // which has to negotiate FUSE_CREATE_SUPP_GROUP itself instead of
        // relying on a Vfs.  The setgid directory is in the upper layer, so
        // the create needs no copy-up.
        if !can_test_create_supp_group("integration_test_overlay_create_supp_group") {
            return Ok(());
        }

        let upper_dir = TempDir::new().unwrap();
        let lower_dir = TempDir::new().unwrap();
        let mnt_dir = TempDir::new().unwrap();
        let mnt = mnt_dir.as_path().to_str().unwrap().to_string();
        make_setgid_dir(upper_dir.as_path());

        let layer = |dir: &TempDir| -> Arc<BoxedLayer> {
            let fs = PassthroughFs::<()>::new(passthrough::Config {
                root_dir: dir.as_path().to_str().unwrap().to_string(),
                xattr: true,
                ..Default::default()
            })
            .unwrap();
            fs.import().unwrap();
            Arc::new(Box::new(fs))
        };
        let config = overlayfs::config::Config {
            do_import: true,
            ..Default::default()
        };
        let fs = OverlayFs::new(Some(layer(&upper_dir)), vec![layer(&lower_dir)], config)?;

        let mut daemon = passthroughfs::Daemon::with_fs(fs, &mnt, 1);
        daemon.mount().unwrap();
        create_as_supp_group_member(&mnt);

        let md = std::fs::metadata(upper_dir.as_path().join("setgid-dir/file")).unwrap();
        assert_eq!(md.gid(), SUPP_GID);

        daemon.umount().unwrap();
        Ok(())
    }

    #[test]
    #[ignore] // it depends on privileged mode to pass through /dev/fuse
    fn integration_test_statx_btime() -> Result<()> {
        // A statx(2) that asks for the birth time sends FUSE_STATX. Without a reply to it, the
        // kernel answers from GETATTR, which has no birth time.
        if !kernel_at_least(6, 6) {
            eprintln!(
                "skipping integration_test_statx_btime: kernel {} does not send FUSE_STATX (needs >= 6.6)",
                kernel_release()
            );
            return Ok(());
        }

        let src_dir = TempDir::new().unwrap();
        let mnt_dir = TempDir::new().unwrap();
        let src = src_dir.as_path().to_str().unwrap().to_string();
        let mnt = mnt_dir.as_path().to_str().unwrap().to_string();
        let src_file = src_dir.as_path().join("file");
        std::fs::write(&src_file, b"data").unwrap();
        let Ok(btime) = std::fs::metadata(&src_file).and_then(|md| md.created()) else {
            eprintln!(
                "skipping integration_test_statx_btime: {} has no birth times",
                src
            );
            return Ok(());
        };

        let mut daemon = passthroughfs::Daemon::new(&src, &mnt, 1).unwrap();
        daemon.mount().unwrap();
        let created = std::fs::metadata(mnt_dir.as_path().join("file")).and_then(|md| md.created());
        daemon.umount().unwrap();
        assert_eq!(created.expect("birth time through the mount"), btime);
        Ok(())
    }

    #[test]
    #[ignore] // it depends on privileged mode to pass through /dev/fuse
    fn integration_test_setxattr_acl_kill_sgid() -> Result<()> {
        // A caller outside the file's group sets a POSIX ACL. The kernel asks the server to
        // clear the set-group-ID bit, which a daemon with CAP_FSETID would otherwise keep.
        if !running_as_root() {
            eprintln!("skipping integration_test_setxattr_acl_kill_sgid: not running as root");
            return Ok(());
        }
        let (uid, file_gid, caller_gid) = (65534, 12345, 65534);

        let src_dir = TempDir::new().unwrap();
        let mnt_dir = TempDir::new().unwrap();
        let src = src_dir.as_path().to_str().unwrap().to_string();
        let mnt = mnt_dir.as_path().to_str().unwrap().to_string();
        std::fs::set_permissions(src_dir.as_path(), std::fs::Permissions::from_mode(0o755))?;
        let src_file = src_dir.as_path().join("file");
        std::fs::write(&src_file, b"")?;
        std::os::unix::fs::chown(&src_file, Some(uid), Some(file_gid))?;
        std::fs::set_permissions(&src_file, std::fs::Permissions::from_mode(0o2755))?;

        let opts = FsOptions::POSIX_ACL | FsOptions::SETXATTR_EXT;
        let mut daemon = passthroughfs::Daemon::with_options(&src, &mnt, 1, opts, true).unwrap();
        daemon.mount().unwrap();

        let path = CString::new(mnt_dir.as_path().join("file").as_os_str().as_bytes()).unwrap();
        let name = CString::new("system.posix_acl_access").unwrap();
        // The xattr form of user::rwx, group::r-x, other::r-x.
        let mut acl = 2u32.to_le_bytes().to_vec();
        for (tag, perm) in [(0x01u16, 7u16), (0x04, 5), (0x20, 5)] {
            acl.extend_from_slice(&tag.to_le_bytes());
            acl.extend_from_slice(&perm.to_le_bytes());
            acl.extend_from_slice(&u32::MAX.to_le_bytes());
        }
        // The raw syscalls change the credentials of this thread only, and a uid other than 0
        // drops its capabilities.
        let res = std::thread::spawn(move || {
            // SAFETY: the calls take no pointers but `path`, the name and `acl`, which outlive
            // them.
            let res = unsafe {
                if libc::syscall(libc::SYS_setgroups, 0, std::ptr::null::<libc::gid_t>()) < 0
                    || libc::syscall(libc::SYS_setresgid, -1, caller_gid, -1) < 0
                    || libc::syscall(libc::SYS_setresuid, -1, uid, -1) < 0
                {
                    -1
                } else {
                    libc::setxattr(
                        path.as_ptr(),
                        name.as_ptr(),
                        acl.as_ptr().cast(),
                        acl.len(),
                        0,
                    )
                }
            };
            if res < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        })
        .join()
        .unwrap();
        daemon.umount().unwrap();
        res?;

        let mode = std::fs::metadata(&src_file)?.permissions().mode();
        assert_eq!(mode & 0o7777, 0o755);
        Ok(())
    }

    #[test]
    #[ignore] // it depends on privileged mode to pass through /dev/fuse
    fn integration_test_direct_io_shared_mmap() -> Result<()> {
        // An O_DIRECT open of a file on the mount gets DIRECT_IO, and the kernel fails a shared
        // mmap of such a file with ENODEV unless the server negotiated FUSE_DIRECT_IO_ALLOW_MMAP.
        // xfstests generic/091 maps files that it opens with O_DIRECT.
        if !kernel_at_least(6, 6) {
            eprintln!(
                "skipping integration_test_direct_io_shared_mmap: kernel {} does not support FUSE_DIRECT_IO_ALLOW_MMAP (needs >= 6.6)",
                kernel_release()
            );
            return Ok(());
        }

        let src_dir = TempDir::new().unwrap();
        let mnt_dir = TempDir::new().unwrap();
        let src = src_dir.as_path().to_str().unwrap().to_string();
        let mnt = mnt_dir.as_path().to_str().unwrap().to_string();
        let data: Vec<u8> = (0..8192u32).map(|i| i as u8).collect();
        std::fs::write(src_dir.as_path().join("file"), &data).unwrap();

        let mut daemon = passthroughfs::Daemon::new(&src, &mnt, 1).unwrap();
        daemon.mount().unwrap();

        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECT)
            .open(mnt_dir.as_path().join("file"))
            .expect("open with O_DIRECT through the mount");
        // SAFETY: a new read-only mapping of `file`, unmapped below.
        let addr = unsafe {
            mmap(
                None,
                NonZeroUsize::new(data.len()).unwrap(),
                ProtFlags::PROT_READ,
                MapFlags::MAP_SHARED,
                &file,
                0,
            )
        }
        .expect("shared mmap of an O_DIRECT file");
        // SAFETY: the mapping covers `data.len()` readable bytes.
        let mapped = unsafe { std::slice::from_raw_parts(addr.as_ptr() as *const u8, data.len()) };
        assert_eq!(mapped, &data[..]);
        // SAFETY: `addr` and the length are those of the mapping above.
        unsafe { munmap(addr, data.len()) }.unwrap();

        daemon.umount().unwrap();
        Ok(())
    }

    /// Counts the COPY_FILE_RANGE requests the server gets.
    #[derive(Default)]
    struct CopyRequests(AtomicUsize);

    impl MetricsHook for CopyRequests {
        fn collect(&self, ih: &InHeader) {
            if ih.opcode == Opcode::CopyFileRange as u32 {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        fn release(&self, _oh: Option<&OutHeader>) {}
    }

    /// Copies `from` to a new file `to` with one copy_file_range() call.
    fn copy_file(from: &Path, to: &Path) -> Result<usize> {
        use nix::errno::Errno;
        use std::os::fd::AsRawFd;

        let src = File::open(from)?;
        let dst = File::create(to)?;
        let len = src.metadata()?.len() as usize;
        // SAFETY: both fds are open, and null offsets use the file offsets.
        let copied = Errno::result(unsafe {
            libc::copy_file_range(
                src.as_raw_fd(),
                std::ptr::null_mut(),
                dst.as_raw_fd(),
                std::ptr::null_mut(),
                len,
                0,
            )
        })?;
        Ok(copied as usize)
    }

    #[test]
    #[ignore] // it depends on privileged mode to pass through /dev/fuse
    fn integration_test_vfs_copy_file_range() -> Result<()> {
        // A Vfs with two PassthroughFs mounts answers a copy between them with EXDEV, and the
        // kernel copies the data itself. A copy within one mount goes to its PassthroughFs. Both
        // copies reach the server, because the kernel stops sending COPY_FILE_RANGE only after
        // ENOSYS.
        if !running_as_root() {
            eprintln!("skipping integration_test_vfs_copy_file_range: not running as root");
            return Ok(());
        }
        if !kernel_at_least(5, 3) {
            eprintln!(
                "skipping integration_test_vfs_copy_file_range: kernel {} does not fall back on EXDEV from the server (needs >= 5.3)",
                kernel_release()
            );
            return Ok(());
        }

        let src_a = TempDir::new().unwrap();
        let src_b = TempDir::new().unwrap();
        let mnt_dir = TempDir::new().unwrap();
        let data: Vec<u8> = (0..8192u32).map(|i| i as u8).collect();
        std::fs::write(src_a.as_path().join("file"), &data).unwrap();

        let vfs = Vfs::new(VfsOptions {
            no_open: false,
            ..Default::default()
        });
        for (src, path) in [(&src_a, "/a"), (&src_b, "/b")] {
            let fs = PassthroughFs::<()>::new(Config {
                root_dir: src.as_path().to_str().unwrap().to_string(),
                do_import: false,
                ..Default::default()
            })
            .unwrap();
            fs.import().unwrap();
            vfs.mount(Box::new(fs), path).unwrap();
        }
        let server = Server::new(vfs);
        let requests = Arc::new(CopyRequests::default());
        let hook = requests.clone();

        let mut se = FuseSession::new(mnt_dir.as_path(), "vfs_copy", "", false).unwrap();
        se.mount().unwrap();
        let mut ch = se.new_channel().unwrap();
        let handle = std::thread::spawn(move || {
            while let Ok(Some((reader, writer))) = ch.get_request() {
                // Serve until the kernel ends the connection. A command that another test forks
                // can inherit a file here and FLUSH it at exec, after this test is done.
                if let Err(e) = server.handle_message(reader, writer, None, Some(&*hook)) {
                    eprintln!("request failed: {e}");
                }
            }
        });

        let mnt = mnt_dir.as_path();
        let across = copy_file(&mnt.join("a/file"), &mnt.join("b/copy"));
        let within = copy_file(&mnt.join("a/file"), &mnt.join("a/copy"));
        se.umount().unwrap();
        handle.join().unwrap();

        assert_eq!(across?, data.len());
        assert_eq!(within?, data.len());
        assert_eq!(std::fs::read(src_b.as_path().join("copy"))?, data);
        assert_eq!(std::fs::read(src_a.as_path().join("copy"))?, data);
        assert_eq!(requests.0.load(Ordering::Relaxed), 2);
        Ok(())
    }

    #[test]
    #[ignore] // it depends on privileged mode to pass through /dev/fuse
    fn integration_test_newer_opcodes_fall_back() -> Result<()> {
        use nix::errno::Errno;
        use std::os::fd::AsRawFd;

        // Newer kernels send FUSE_STATX and FUSE_COPY_FILE_RANGE_64. Both get ENOSYS, and the
        // kernel falls back to GETATTR and COPY_FILE_RANGE.
        let src_dir = TempDir::new().unwrap();
        let mnt_dir = TempDir::new().unwrap();
        let src = src_dir.as_path().to_str().unwrap().to_string();
        let mnt = mnt_dir.as_path().to_str().unwrap().to_string();
        let data: Vec<u8> = (0..8192u32).map(|i| i as u8).collect();
        std::fs::write(src_dir.as_path().join("file"), &data).unwrap();

        let mut daemon = passthroughfs::Daemon::new(&src, &mnt, 1).unwrap();
        daemon.mount().unwrap();

        let file = mnt_dir.as_path().join("file");
        let path = CString::new(file.as_os_str().as_bytes()).unwrap();
        // SAFETY: `statx` is plain old data, so all zeroes is a valid value.
        let mut stx: libc::statx = unsafe { std::mem::zeroed() };
        // SAFETY: `path` is NUL-terminated and statx() fills `stx`, both of which outlive the call.
        let res = unsafe {
            libc::statx(
                libc::AT_FDCWD,
                path.as_ptr(),
                libc::AT_STATX_FORCE_SYNC,
                libc::STATX_BASIC_STATS | libc::STATX_BTIME,
                &mut stx,
            )
        };
        assert_eq!(res, 0, "statx: {}", std::io::Error::last_os_error());
        assert_eq!(stx.stx_size, data.len() as u64);

        let src_file = std::fs::File::open(&file)?;
        let dst_file = std::fs::File::create(mnt_dir.as_path().join("copy"))?;
        // SAFETY: both fds are open, and null offsets use the file offsets.
        let copied = Errno::result(unsafe {
            libc::copy_file_range(
                src_file.as_raw_fd(),
                std::ptr::null_mut(),
                dst_file.as_raw_fd(),
                std::ptr::null_mut(),
                data.len(),
                0,
            )
        })
        .unwrap();
        assert_eq!(copied as usize, data.len());
        drop(dst_file);
        assert_eq!(std::fs::read(src_dir.as_path().join("copy"))?, data);

        daemon.umount().unwrap();
        Ok(())
    }
}
