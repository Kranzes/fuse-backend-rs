// Copyright 2020-2022 Ant Group. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! FUSE session management.
//!
//! A FUSE channel is a FUSE request handling context that takes care of handling FUSE requests
//! sequentially. A FUSE session is a connection from a FUSE mountpoint to a FUSE server daemon.
//! A FUSE session can have multiple FUSE channels so that FUSE requests are handled in parallel.

use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io;
use std::ops::Deref;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use crate::splice::SplicePipes;
use crate::{FuseChannelExt, FuseDevReaderExt, FuseSessionExt};
use fuse_backend_core::abi::fuse_abi::BackingMap;
use libc::c_uint;
use mio::{Events, Poll, Token, Waker};
use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sched::{setns, unshare, CloneFlags};
use nix::sys::epoll::{EpollEvent, EpollFlags};
use nix::sys::stat::Mode;
use nix::unistd::{getgid, getuid, read};
use nix::NixPath;

use super::{
    pagesize,
    Error::{IoError, SessionFailure},
    FuseBuf, FuseDevWriter, Reader, Result, FUSE_HEADER_SIZE, FUSE_KERN_BUF_PAGES,
};

// These follows definition from libfuse.
//
// Each channel registers exactly two tokens with its epoll instance (the exit
// waker and the fuse device fd), so a single `poll` can return at most two
// events. The event buffer is reused across requests (see `FuseChannel`), so a
// small fixed capacity is enough; 8 leaves ample headroom while keeping the
// buffer tiny instead of libfuse's 1024-entry (~12KB) allocation.
const POLL_EVENTS_CAPACITY: usize = 8;

const FUSE_DEVICE: &str = "/dev/fuse";
const FUSE_FSTYPE: &str = "fuse";
const FUSERMOUNT_BIN: &str = "fusermount3";

const EXIT_FUSE_EVENT: Token = Token(0);
const FUSE_DEV_EVENT: Token = Token(1);

/// A fuse session manager to manage the connection with the in kernel fuse driver.
pub struct FuseSession {
    mountpoint: PathBuf,
    fsname: String,
    subtype: String,
    file: Option<File>,
    // Socket to keep alive / drop for fusermount's auto_unmount.
    keep_alive: Option<UnixStream>,
    bufsize: usize,
    readonly: bool,
    wakers: Mutex<Vec<Arc<Waker>>>,
    auto_unmount: bool,
    allow_other: bool,
    target_mntns: Option<libc::pid_t>,
    // fusermount binary, default to fusermount3
    fusermount: String,
    mount_flags: Option<MsFlags>,
    splice_write: bool,
    // Set once detach() has unmounted the file system, so umount() and later detach() calls
    // don't unmount the mountpoint again. Another file system may be mounted there by then, and
    // without a mount ID, is_unmounted() misses that while the detached mount is in use.
    detached: Mutex<bool>,
    // The unique ID of the mount, on Linux 6.8+. See is_unmounted().
    mount_id: Option<u64>,
}

impl FuseSession {
    /// Create a new fuse session, without mounting/connecting to the in kernel fuse driver.
    pub fn new(
        mountpoint: &Path,
        fsname: &str,
        subtype: &str,
        readonly: bool,
    ) -> Result<FuseSession> {
        FuseSession::new_with_autounmount(mountpoint, fsname, subtype, readonly, false)
    }

    /// Create a new fuse session, without mounting/connecting to the in kernel fuse driver.
    pub fn new_with_autounmount(
        mountpoint: &Path,
        fsname: &str,
        subtype: &str,
        readonly: bool,
        auto_unmount: bool,
    ) -> Result<FuseSession> {
        let dest = mountpoint
            .canonicalize()
            .map_err(|_| SessionFailure(format!("invalid mountpoint {mountpoint:?}")))?;
        if !dest.is_dir() {
            return Err(SessionFailure(format!("{dest:?} is not a directory")));
        }

        Ok(FuseSession {
            mountpoint: dest,
            fsname: fsname.to_owned(),
            subtype: subtype.to_owned(),
            file: None,
            keep_alive: None,
            bufsize: FUSE_KERN_BUF_PAGES * pagesize() + FUSE_HEADER_SIZE,
            readonly,
            wakers: Mutex::new(Vec::new()),
            auto_unmount,
            target_mntns: None,
            fusermount: FUSERMOUNT_BIN.to_string(),
            allow_other: true,
            mount_flags: None,
            splice_write: false,
            detached: Mutex::new(false),
            mount_id: None,
        })
    }

    /// Set the target pid of mount namespace of the fuse session mount, the fuse will be mounted
    /// under the given mnt ns.
    ///
    /// A helper thread joins that namespace to mount and unmount there, so the calling thread
    /// stays in its own. If that fails, for example without the privileges, the session runs
    /// fusermount through nsenter.
    pub fn set_target_mntns(&mut self, pid: Option<libc::pid_t>) {
        self.target_mntns = pid;
    }

    /// Send READ replies with `splice(2)` when the file system takes the data
    /// from a file through `ZeroCopyWriter::write_from()`, as `PassthroughFs`
    /// does. The data then goes from the file's page cache into the FUSE
    /// request without a copy through user space. It's off by default.
    ///
    /// Channels created afterwards get two pipes each for this on their first
    /// splice. `AsyncFuseServing` splices too, with two pipes for each request
    /// buffer, see `FuseDevTask::set_splice_write()`. The io_uring queues of
    /// `UringFuseServing` don't. A channel copies the replies that don't fit
    /// in its pipes, and stops splicing if the kernel refuses to.
    ///
    /// A pipe grows to fit the largest READ it carried and keeps that size.
    /// Its slots count against the user's `/proc/sys/fs/pipe-user-pages-soft`,
    /// 16384 by default. For the READs of up to 128 KiB that the kernel sends
    /// by default, a pair takes up to 128 slots of 4 KiB. With
    /// `FsOptions::MAX_PAGES`, READs of 1 MiB grow both pipes to
    /// `pipe-max-size`, 256 slots by default, unless the daemon has
    /// `CAP_SYS_RESOURCE`. Those READs still copy, because their header
    /// doesn't fit. A sync channel has one pair, and an async worker one per
    /// request buffer, up to `max_inflight`, 16 by default. So 8 async
    /// workers with 16 READs in flight each can reach the limit, or 2 with
    /// READs of 1 MiB. From then on the user's pipes stop growing, and every
    /// new pipe of that user gets two slots.
    pub fn set_splice_write(&mut self, splice: bool) {
        self.splice_write = splice;
    }

    /// Set fusermount binary, default to fusermount3.
    pub fn set_fusermount(&mut self, bin: &str) {
        self.fusermount = bin.to_string();
    }

    /// Set the allow_other mount option. This allows other users than the one mounting the
    /// filesystem to access the filesystem. However, this option is usually restricted to the root
    /// user unless configured otherwise.
    pub fn set_allow_other(&mut self, allow_other: bool) {
        self.allow_other = allow_other;
    }

    /// Get current fusermount binary.
    pub fn get_fusermount(&self) -> &str {
        self.fusermount.as_str()
    }

    /// Expose the associated FUSE session file.
    pub fn get_fuse_file(&self) -> Option<&File> {
        self.file.as_ref()
    }

    /// Force setting the associated FUSE session file.
    pub fn set_fuse_file(&mut self, file: File) {
        self.file = Some(file);
    }

    /// Set custom mount flags for the session.
    /// If not set, default flags (MS_NOSUID | MS_NODEV | MS_NOATIME) will be
    /// used. MS_RDONLY will be added automatically if the session is readonly.
    /// Not setting MS_NOSUID and MS_NODEV will probably get ignored by
    /// fusermount3 for security reasons, so it means you need to be root to
    /// mount the FS.
    pub fn set_mount_flags(&mut self, flags: MsFlags) {
        self.mount_flags = Some(flags);
    }

    /// Get the currently configured mount flags, or None if using defaults.
    pub fn get_mount_flags(&self) -> Option<MsFlags> {
        self.mount_flags
    }

    /// Clone fuse file using ioctl FUSE_DEV_IOC_CLONE.
    pub fn clone_fuse_file(&self) -> Result<File> {
        let mut old_fd = self
            .file
            .as_ref()
            .ok_or(SessionFailure(
                "fuse session file doesn't exist".to_string(),
            ))?
            .as_raw_fd();

        let cloned_file = open_fuse_device()?;

        // define the function which invokes "ioctl FUSE_DEV_IOC_CLONE"
        // refer: https://github.com/torvalds/linux/blob/c42d9eeef8e5ba9292eda36fd8e3c11f35ee065c/include/uapi/linux/fuse.h#L1051-L1052
        // #define FUSE_DEV_IOC_MAGIC   229
        // #define FUSE_DEV_IOC_CLONE   _IOR(FUSE_DEV_IOC_MAGIC, 0, uint32_t)
        nix::ioctl_read!(clone_fuse_fd, 229, 0, i32);

        unsafe { clone_fuse_fd(cloned_file.as_raw_fd(), (&mut old_fd) as *mut i32) }
            .map_err(|e| SessionFailure(format!("failed to clone fuse file: {:?}", e)))?;

        Ok(cloned_file)
    }

    /// Get the mountpoint of the session.
    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    /// Get the file system name of the session.
    pub fn fsname(&self) -> &str {
        &self.fsname
    }

    /// Get the subtype of the session.
    pub fn subtype(&self) -> &str {
        &self.subtype
    }

    /// Get the default buffer size of the session.
    pub fn bufsize(&self) -> usize {
        self.bufsize
    }

    /// Mount the fuse mountpoint, building connection with the in kernel fuse driver.
    ///
    /// On Linux 5.2 and later, it mounts through fsopen(2), fsmount(2) and move_mount(2). If that
    /// fails, it uses mount(2). fusermount takes over where mount(2) fails with EPERM, or fails for
    /// any reason in the namespace of `set_target_mntns()`. All of them mount with the same
    /// options. With `auto_unmount`, it always runs fusermount.
    pub fn mount(&mut self) -> Result<()> {
        let mut flags = self
            .mount_flags
            .unwrap_or(MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOATIME);
        if self.readonly {
            flags |= MsFlags::MS_RDONLY;
        }
        // On older kernels, the statx after the mount may send a FUSE request that nobody serves
        // yet, so check for unique mount IDs first. The statx can't see another namespace.
        let has_mount_ids =
            self.target_mntns.is_none() && unique_mount_id(&self.mountpoint).is_some();
        let (file, socket) = fuse_kern_mount(
            &self.mountpoint,
            &self.fsname,
            &self.subtype,
            flags,
            self.auto_unmount,
            self.allow_other,
            self.target_mntns,
            &self.fusermount,
        )?;

        fcntl(&file, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))
            .map_err(|e| SessionFailure(format!("set fd nonblocking: {e}")))?;
        self.file = Some(file);
        self.keep_alive = socket;
        self.mount_id = has_mount_ids
            .then(|| unique_mount_id(&self.mountpoint))
            .flatten();
        *self
            .detached
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner) = false;

        Ok(())
    }

    /// Destroy a fuse session.
    ///
    /// On Linux 6.8+, unless the session mounted in another namespace, this unmounts the session's
    /// own mount even after an abort through `/sys/fs/fuse/connections`, and never a file system
    /// mounted at the mountpoint since.
    pub fn umount(&mut self) -> Result<()> {
        let detached = std::mem::take(
            self.detached
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner),
        );
        // Forget the ID, so it doesn't apply to a file set later with set_fuse_file().
        let mount_id = self.mount_id.take();
        // If we have a keep_alive socket, just drop it,
        // and let fusermount do the unmount.
        if let (None, Some(file)) = (self.keep_alive.take(), self.file.take()) {
            // Just close the fd if detach() or someone else unmounted it already.
            if detached || is_unmounted(&file, &self.mountpoint, mount_id) {
                return Ok(());
            }
            // Drop to close fuse session fd, otherwise synchronous umount can recurse into
            // filesystem and cause deadlock.
            drop(file);
            fuse_lazy_umount(&self.mountpoint, &self.fusermount, self.target_mntns)
        } else {
            Ok(())
        }
    }

    /// Lazily unmount the file system, like `umount -l`.
    ///
    /// Unlike [`FuseSession::umount()`], this takes `&self` and keeps the fuse fd open, so it
    /// works through [`FuseServing::session()`](crate::FuseServing::session) while another thread
    /// blocks in [`FuseServing::wait()`](crate::FuseServing::wait). Once nothing in the mount is
    /// in use, the kernel tears the connection down and the workers stop.
    ///
    /// It unmounts at most once, and a later `umount()` doesn't unmount again. So neither of them
    /// unmounts another file system mounted at the mountpoint after the detach. With
    /// `auto_unmount`, fusermount still unmounts the mountpoint once `umount()` closes the session.
    /// Like `umount()`, it does nothing once the file system is unmounted.
    pub fn detach(&self) -> Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        // Hold the lock across the unmount, so concurrent calls unmount only once.
        let mut detached = self.detached.lock().unwrap_or_else(PoisonError::into_inner);
        if *detached || is_unmounted(file, &self.mountpoint, self.mount_id) {
            return Ok(());
        }
        fuse_lazy_umount(&self.mountpoint, &self.fusermount, self.target_mntns)?;
        *detached = true;
        Ok(())
    }

    /// Create a new fuse message channel.
    pub fn new_channel(&self) -> Result<FuseChannel> {
        if let Some(file) = &self.file {
            let file = file
                .try_clone()
                .map_err(|e| SessionFailure(format!("dup fd: {e}")))?;
            let channel = FuseChannel::new(file, self.bufsize, self.splice_pipes())?;
            let waker = channel.get_waker();
            self.add_waker(waker)?;

            Ok(channel)
        } else {
            Err(SessionFailure("invalid fuse session".to_string()))
        }
    }

    /// Create a new blocking fuse message channel.
    ///
    /// The channel receives requests with plain blocking reads on a fuse
    /// device fd cloned with `FUSE_DEV_IOC_CLONE`, saving the `epoll_wait`
    /// syscall per request that [`Self::new_channel()`] pays.
    ///
    /// Note: blocking channels are not woken by [`Self::wake()`]; they exit
    /// when the fuse session is umounted (the pending read returns `ENODEV`).
    /// Requires kernel support for `FUSE_DEV_IOC_CLONE` (kernel >= 4.2).
    pub fn new_blocking_channel(&self) -> Result<BlockingFuseChannel> {
        let file = self.clone_fuse_file()?;
        Ok(BlockingFuseChannel::new(
            file,
            self.bufsize,
            self.splice_pipes(),
        ))
    }

    /// Whether `set_splice_write()` turned splicing on.
    #[cfg(feature = "async-io")]
    pub(crate) fn splice_write(&self) -> bool {
        self.splice_write
    }

    fn splice_pipes(&self) -> Option<SplicePipes> {
        self.splice_write.then(SplicePipes::new)
    }

    /// Wake channel loop and exit
    pub fn wake(&self) -> Result<()> {
        let wakers = self
            .wakers
            .lock()
            .map_err(|e| SessionFailure(format!("lock wakers: {e}")))?;
        for waker in wakers.iter() {
            waker
                .wake()
                .map_err(|e| SessionFailure(format!("wake channel: {e}")))?;
        }
        Ok(())
    }

    fn add_waker(&self, waker: Arc<Waker>) -> Result<()> {
        let mut wakers = self
            .wakers
            .lock()
            .map_err(|e| SessionFailure(format!("lock wakers: {e}")))?;
        wakers.push(waker);
        Ok(())
    }
}

impl Drop for FuseSession {
    fn drop(&mut self) {
        let _ = self.umount();
    }
}

impl FuseSessionExt for FuseSession {
    fn file(&self) -> Option<&File> {
        self.file.as_ref()
    }

    fn bufsize(&self) -> usize {
        self.bufsize
    }
}

/// A fuse channel abstraction.
///
/// Each session can hold multiple channels.
pub struct FuseChannel {
    file: File,
    poll: Poll,
    waker: Arc<Waker>,
    buf: Vec<u8>,
    events: Events,
    splice: Option<SplicePipes>,
}

impl FuseChannel {
    fn new(file: File, bufsize: usize, splice: Option<SplicePipes>) -> Result<Self> {
        let poll = Poll::new().map_err(|e| SessionFailure(format!("epoll create: {e}")))?;
        let waker = Waker::new(poll.registry(), EXIT_FUSE_EVENT)
            .map_err(|e| SessionFailure(format!("epoll register session fd: {e}")))?;
        let waker = Arc::new(waker);

        // mio default add EPOLLET to event flags, so epoll will use edge-triggered mode.
        // It may let poll miss some event, so manually register the fd with only EPOLLIN flag
        // to use level-triggered mode.
        //
        // Also request EPOLLEXCLUSIVE: a session hands out channels over the
        // *same* fuse device fd (via `try_clone`), so without it one arriving
        // request makes every channel's `epoll_wait` report readable and they
        // all race into `read()`, where all but one get `EAGAIN` and re-poll
        // (thundering herd). EPOLLEXCLUSIVE makes the kernel wake a single
        // waiting channel per event instead. It requires kernel >= 4.5; older
        // kernels reject the flag with EINVAL, so fall back to a plain EPOLLIN
        // registration there.
        // mio owns the epoll instance; the registration below goes straight
        // to `epoll_ctl(2)` on its fd because mio's registry always adds
        // EPOLLET (edge triggered) and would miss events, and nix's `Epoll`
        // type would take ownership of the fd mio still owns.
        let epoll = poll.as_raw_fd();
        let fd = file.as_raw_fd();
        let mut event = EpollEvent::new(
            EpollFlags::EPOLLIN | EpollFlags::EPOLLEXCLUSIVE,
            usize::from(FUSE_DEV_EVENT) as u64,
        );
        // SAFETY: `epoll` is mio's live epoll instance and `fd` the session's
        // live fuse device fd. `EpollEvent` is `#[repr(transparent)]` over
        // `libc::epoll_event`, so the pointer cast is sound; `event` outlives
        // the call.
        let res = unsafe {
            libc::epoll_ctl(
                epoll,
                libc::EPOLL_CTL_ADD,
                fd,
                &mut event as *mut _ as *mut libc::epoll_event,
            )
        };
        if let Err(e) = Errno::result(res) {
            if e == Errno::EINVAL {
                // Kernel < 4.5: EPOLLEXCLUSIVE is unsupported, retry without it.
                event = EpollEvent::new(EpollFlags::EPOLLIN, usize::from(FUSE_DEV_EVENT) as u64);
                // SAFETY: as above.
                let res = unsafe {
                    libc::epoll_ctl(
                        epoll,
                        libc::EPOLL_CTL_ADD,
                        fd,
                        &mut event as *mut _ as *mut libc::epoll_event,
                    )
                };
                Errno::result(res)
                    .map_err(|e| SessionFailure(format!("epoll register channel fd: {e}")))?;
            } else {
                return Err(SessionFailure(format!("epoll register channel fd: {e}")));
            }
        }

        Ok(FuseChannel {
            file,
            poll,
            waker,
            buf: vec![0x0u8; bufsize],
            events: Events::with_capacity(POLL_EVENTS_CAPACITY),
            splice,
        })
    }

    fn get_waker(&self) -> Arc<Waker> {
        self.waker.clone()
    }

    /// Get next available FUSE request from the underlying fuse device file.
    ///
    /// Returns:
    /// - Ok(None): signal has pending on the exiting event channel
    /// - Ok(Some((reader, writer))): reader to receive request and writer to send reply
    /// - Err(e): error message
    pub fn get_request(&mut self) -> Result<Option<(Reader<'_>, FuseDevWriter<'_>)>> {
        let mut need_exit = false;
        loop {
            let mut fusereq_available = false;
            match self.poll.poll(&mut self.events, None) {
                Ok(_) => {}
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(SessionFailure(format!("epoll wait: {e}"))),
            }

            for event in self.events.iter() {
                // We will handle errors when reading from the fuse device
                if event.is_readable() || event.is_error() {
                    match event.token() {
                        EXIT_FUSE_EVENT => need_exit = true,
                        FUSE_DEV_EVENT => fusereq_available = true,
                        x => {
                            error!("unexpected epoll event");
                            return Err(SessionFailure(format!("unexpected epoll event: {}", x.0)));
                        }
                    }
                } else {
                    // We should not step into this branch as other event is not registered.
                    panic!("unknown epoll result events");
                }
            }

            // Handle wake up event first. We don't read the event fd so that a LEVEL triggered
            // event can still be delivered to other threads/daemons.
            if need_exit {
                debug!("Will exit from fuse service");
                return Ok(None);
            }
            if fusereq_available {
                let fd = self.file.as_raw_fd();
                match read(&self.file, &mut self.buf) {
                    Ok(len) => {
                        // ###############################################
                        // Note: it's a heavy hack to reuse the same underlying data
                        // buffer for both Reader and Writer, in order to reduce memory
                        // consumption. Here we assume Reader won't be used anymore once
                        // we start to write to the Writer. To get rid of this hack,
                        // just allocate a dedicated data buffer for Writer.
                        let buf = unsafe {
                            std::slice::from_raw_parts_mut(self.buf.as_mut_ptr(), self.buf.len())
                        };
                        // Reader::new() and Writer::new() should always return success.
                        let reader =
                            Reader::from_fuse_buffer(FuseBuf::new(&mut self.buf[..len])).unwrap();
                        let writer = FuseDevWriter::new(fd, buf)
                            .unwrap()
                            .with_splice(self.splice.as_ref());
                        return Ok(Some((reader, writer)));
                    }
                    Err(e) => match e {
                        Errno::ENOENT => {
                            // ENOENT means the operation was interrupted, it's safe to restart
                            trace!("restart reading due to ENOENT");
                            continue;
                        }
                        Errno::EAGAIN => {
                            trace!("restart reading due to EAGAIN");
                            continue;
                        }
                        Errno::EINTR => {
                            trace!("syscall interrupted");
                            continue;
                        }
                        Errno::ENODEV => {
                            debug!("got ENODEV when reading fuse fd, assuming fuse filesystem was umounted.");
                            return Ok(None);
                        }
                        Errno::ECONNABORTED => {
                            // The kernel returns ECONNABORTED instead of ENODEV when
                            // the connection is aborted while a request is being
                            // handed over to this reader (fuse_abort_conn() sets
                            // fpq->connected = 0 between the two). Either way the
                            // session is being torn down, so treat it like ENODEV.
                            debug!("got ECONNABORTED when reading fuse fd, assuming fuse session was aborted.");
                            return Ok(None);
                        }
                        e => {
                            warn! {"read fuse dev failed on fd {}: {}", fd, e};
                            return Err(SessionFailure(format!("read new request: {e:?}")));
                        }
                    },
                }
            }
        }
    }
}

/// A fuse channel that receives requests with plain blocking reads.
///
/// Compared to [`FuseChannel`], this channel saves the `epoll_wait` syscall
/// per request by reading the fuse device fd directly, at the cost of losing
/// wakeup-based shutdown: [`FuseSession::wake()`] has no effect on blocking
/// channels. A blocking channel exits instead when the fuse connection is
/// torn down (unmount or abort), which makes the pending read fail with
/// `ENODEV` and [`Self::get_request()`] return `Ok(None)`.
///
/// Blocking channels are created from a fuse device fd cloned with
/// `FUSE_DEV_IOC_CLONE`, which has its own file description and is thus
/// unaffected by the `O_NONBLOCK` flag set on the session fd.
pub struct BlockingFuseChannel {
    file: File,
    buf: Vec<u8>,
    splice: Option<SplicePipes>,
}

impl BlockingFuseChannel {
    fn new(file: File, bufsize: usize, splice: Option<SplicePipes>) -> Self {
        BlockingFuseChannel {
            file,
            buf: vec![0x0u8; bufsize],
            splice,
        }
    }

    /// Get next available FUSE request from the underlying fuse device file.
    ///
    /// Blocks until a request arrives or the fuse connection is torn down.
    ///
    /// Returns:
    /// - Ok(None): the fuse session has been umounted or aborted
    /// - Ok(Some((reader, writer))): reader to receive request and writer to send reply
    /// - Err(e): error message
    pub fn get_request(&mut self) -> Result<Option<(Reader<'_>, FuseDevWriter<'_>)>> {
        loop {
            let fd = self.file.as_raw_fd();
            match read(&self.file, &mut self.buf) {
                Ok(len) => {
                    // ###############################################
                    // Note: it's a heavy hack to reuse the same underlying data
                    // buffer for both Reader and Writer, in order to reduce memory
                    // consumption. Here we assume Reader won't be used anymore once
                    // we start to write to the Writer. To get rid of this hack,
                    // just allocate a dedicated data buffer for Writer.
                    let buf = unsafe {
                        std::slice::from_raw_parts_mut(self.buf.as_mut_ptr(), self.buf.len())
                    };
                    // Reader::new() and Writer::new() should always return success.
                    let reader =
                        Reader::from_fuse_buffer(FuseBuf::new(&mut self.buf[..len])).unwrap();
                    let writer = FuseDevWriter::new(fd, buf)
                        .unwrap()
                        .with_splice(self.splice.as_ref());
                    return Ok(Some((reader, writer)));
                }
                Err(e) => match e {
                    Errno::ENOENT => {
                        // ENOENT means the operation was interrupted, it's safe to restart
                        trace!("restart reading due to ENOENT");
                        continue;
                    }
                    Errno::EINTR => {
                        trace!("syscall interrupted");
                        continue;
                    }
                    Errno::ENODEV => {
                        debug!("got ENODEV when reading fuse fd, assuming fuse filesystem was umounted.");
                        return Ok(None);
                    }
                    Errno::ECONNABORTED => {
                        // See the comment on the ECONNABORTED arm above: the read
                        // raced an aborting connection, so the session is gone.
                        debug!("got ECONNABORTED when reading fuse fd, assuming fuse session was aborted.");
                        return Ok(None);
                    }
                    // No EAGAIN arm on purpose: unlike `FuseChannel`, this fd is
                    // blocking (cloned via FUSE_DEV_IOC_CLONE, see the type docs),
                    // and the kernel only returns EAGAIN from a fuse device read
                    // for O_NONBLOCK file descriptions, so it cannot happen here.
                    e => {
                        warn! {"read fuse dev failed on fd {}: {}", fd, e};
                        return Err(SessionFailure(format!("read new request: {e:?}")));
                    }
                },
            }
        }
    }
}

impl FuseChannelExt for FuseChannel {
    fn next_request(&mut self) -> Result<Option<(Reader<'_>, FuseDevWriter<'_>)>> {
        FuseChannel::get_request(self)
    }
}

impl FuseChannelExt for BlockingFuseChannel {
    fn next_request(&mut self) -> Result<Option<(Reader<'_>, FuseDevWriter<'_>)>> {
        BlockingFuseChannel::get_request(self)
    }
}

fn open_fuse_device() -> Result<File> {
    OpenOptions::new()
        .create(false)
        .read(true)
        .write(true)
        .open(FUSE_DEVICE)
        .map_err(|e| SessionFailure(format!("open {FUSE_DEVICE}: {e}")))
}

/// Mount a fuse file system
#[allow(clippy::too_many_arguments)]
fn fuse_kern_mount(
    mountpoint: &Path,
    fsname: &str,
    subtype: &str,
    flags: MsFlags,
    auto_unmount: bool,
    allow_other: bool,
    target_mntns: Option<libc::pid_t>,
    fusermount: &str,
) -> Result<(File, Option<UnixStream>)> {
    let meta = mountpoint
        .metadata()
        .map_err(|e| SessionFailure(format!("stat {mountpoint:?}: {e}")))?;
    // the current implementation of fuse-backend-rs uses a fixed buffer to store the fuse response,
    // the default value of this buffer is as follows, but in fact, the kernel in the direct io path,
    // the size of the request may be larger than the length of this buffer (this is determined by
    // the max_read option to determine the maximum size of kernel requests, the default value is
    // a very large number), which leads to the buffer is not enough to fill the read content,
    // resulting in read failure. so here we limit the size of max_read to the length of our buffer,
    // so that the fuse kernel will not send requests that exceed the length of the buffer.
    // in virtiofs scene max_read can't be adjusted, his default is UINT_MAX, but we don't have to
    // worry about it, because the buffer is allocated by the kernel driver, we just use this buffer
    // to fill the response, so we don't need to do any adjustment.
    let max_read = FUSE_KERN_BUF_PAGES * pagesize() + FUSE_HEADER_SIZE;

    let mount_opts = |file: &File| {
        let mut opts = format!(
            "default_permissions,fd={},rootmode={:o},user_id={},group_id={},max_read={}",
            file.as_raw_fd(),
            meta.permissions().mode() & libc::S_IFMT,
            getuid(),
            getgid(),
            max_read
        );
        if allow_other {
            opts.push_str(",allow_other");
        }
        opts
    };

    let file = open_fuse_device()?;
    let opts = mount_opts(&file);
    // Only fusermount can auto_unmount. It unmounts once the session closes its socket.
    if auto_unmount {
        return fuse_fusermount_mount(
            mountpoint,
            fsname,
            subtype,
            opts,
            flags,
            auto_unmount,
            target_mntns,
            fusermount,
        );
    }

    match fuse_fsmount(mountpoint, fsname, subtype, &opts, flags, target_mntns) {
        Ok(()) => return Ok((file, None)),
        Err(e) => debug!("can't mount {mountpoint:?} with fsmount(), falling back: {e}"),
    }

    // The fd may belong to the failed mount's connection by now, so open another one.
    drop(file);
    let file = open_fuse_device()?;
    let opts = mount_opts(&file);
    let mut fstype = String::from(FUSE_FSTYPE);
    if !subtype.is_empty() {
        fstype.push('.');
        fstype.push_str(subtype);
    }

    if let Some(mountpoint) = mountpoint.to_str() {
        debug!(
            "mount source {} dest {} with fstype {} opts {} fd {}",
            fsname,
            mountpoint,
            fstype,
            opts,
            file.as_raw_fd(),
        );
    }

    let mount2 = || {
        mount(
            Some(fsname),
            mountpoint,
            Some(fstype.deref()),
            flags,
            Some(opts.deref()),
        )
    };
    match in_mntns(target_mntns, mount2) {
        Ok(()) => Ok((file, None)),
        // As before, fusermount mounts without the privileges for mount(2). In another mntns, it
        // takes over on any error.
        Err(e) if e == Errno::EPERM || target_mntns.is_some() => fuse_fusermount_mount(
            mountpoint,
            fsname,
            subtype,
            opts,
            flags,
            auto_unmount,
            target_mntns,
            fusermount,
        ),
        Err(e) => Err(SessionFailure(format!(
            "failed to mount {mountpoint:?}: {e}"
        ))),
    }
}

/// Take ownership of the fd that a syscall returned.
fn owned_fd(ret: libc::c_long) -> nix::Result<OwnedFd> {
    let fd = Errno::result(ret)?;
    // SAFETY: the syscall returned a new fd, which nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Mount a fuse file system like mount(2) does with `flags` and `opts`, through fsopen(),
/// fsconfig(), fsmount() and move_mount() of Linux 5.2 and later. With `target_mntns`, a helper
/// thread moves the mount into that mount namespace. Fails with EINVAL for flags it can't set.
fn fuse_fsmount(
    mountpoint: &Path,
    fsname: &str,
    subtype: &str,
    opts: &str,
    flags: MsFlags,
    target_mntns: Option<libc::pid_t>,
) -> nix::Result<()> {
    // From include/uapi/linux/mount.h, which libc covers only since 0.2.187.
    const FSOPEN_CLOEXEC: c_uint = 0x1;
    const FSCONFIG_SET_FLAG: c_uint = 0;
    const FSCONFIG_SET_STRING: c_uint = 1;
    const FSCONFIG_CMD_CREATE: c_uint = 6;
    const FSMOUNT_CLOEXEC: c_uint = 0x1;
    const MOVE_MOUNT_F_EMPTY_PATH: c_uint = 0x4;
    const MOVE_MOUNT_T_SYMLINKS: c_uint = 0x10;
    // mount(2) sets some of these flags on the superblock and the others on the mount. fsconfig()
    // takes the superblock flags by these names, and fsmount() the mount flags as these
    // MOUNT_ATTR_* bits. Both APIs default to relatime.
    const FLAGS: [(MsFlags, Option<&str>, c_uint); 10] = [
        (MsFlags::MS_RDONLY, Some("ro"), 0x1),
        (MsFlags::MS_NOSUID, None, 0x2),
        (MsFlags::MS_NODEV, None, 0x4),
        (MsFlags::MS_NOEXEC, None, 0x8),
        (MsFlags::MS_NOATIME, None, 0x10),
        (MsFlags::MS_NODIRATIME, None, 0x80),
        (MsFlags::MS_RELATIME, None, 0),
        (MsFlags::MS_SYNCHRONOUS, Some("sync"), 0),
        (MsFlags::MS_DIRSYNC, Some("dirsync"), 0),
        (MsFlags::MS_LAZYTIME, Some("lazytime"), 0),
    ];

    let mut params = Vec::new();
    let mut attrs = 0;
    let mut unset = flags;
    for (flag, key, attr) in FLAGS {
        if flags.contains(flag) {
            params.extend(key.map(|key| (key, None)));
            attrs |= attr;
            unset.remove(flag);
        }
    }
    if !unset.is_empty() {
        return Err(Errno::EINVAL);
    }
    // Pass the parameters in mount(2)'s order, and split `opts` the way the kernel splits
    // mount(2)'s data.
    if !subtype.is_empty() {
        params.push(("subtype", Some(subtype)));
    }
    params.push(("source", Some(fsname)));
    params.extend(opts.split(',').map(|opt| match opt.split_once('=') {
        Some((key, value)) => (key, Some(value)),
        None => (opt, None),
    }));
    let cstr = |s: &[u8]| CString::new(s).map_err(|_| Errno::EINVAL);
    let target = cstr(mountpoint.as_os_str().as_bytes())?;

    // SAFETY: the file system name is a C string.
    let fs =
        owned_fd(unsafe { libc::syscall(libc::SYS_fsopen, b"fuse\0".as_ptr(), FSOPEN_CLOEXEC) })?;
    let fsconfig = |cmd: c_uint, key: Option<&CStr>, value: Option<&CStr>| {
        let ptr = |s: Option<&CStr>| s.map_or(std::ptr::null(), CStr::as_ptr);
        // SAFETY: the strings outlive the call.
        Errno::result(unsafe {
            libc::syscall(
                libc::SYS_fsconfig,
                fs.as_raw_fd(),
                cmd,
                ptr(key),
                ptr(value),
                0,
            )
        })
    };
    for (key, value) in params {
        let value = value.map(|value| cstr(value.as_bytes())).transpose()?;
        let cmd = match value {
            Some(_) => FSCONFIG_SET_STRING,
            None => FSCONFIG_SET_FLAG,
        };
        fsconfig(cmd, Some(&cstr(key.as_bytes())?), value.as_deref())?;
    }
    fsconfig(FSCONFIG_CMD_CREATE, None, None)?;
    // SAFETY: the arguments are plain integers.
    let mnt = owned_fd(unsafe {
        libc::syscall(libc::SYS_fsmount, fs.as_raw_fd(), FSMOUNT_CLOEXEC, attrs)
    })?;

    // Follow a symlink at the mountpoint, like mount(2).
    in_mntns(target_mntns, || {
        // SAFETY: the paths are C strings that outlive the call.
        Errno::result(unsafe {
            libc::syscall(
                libc::SYS_move_mount,
                mnt.as_raw_fd(),
                b"\0".as_ptr(),
                libc::AT_FDCWD,
                target.as_ptr(),
                MOVE_MOUNT_F_EMPTY_PATH | MOVE_MOUNT_T_SYMLINKS,
            )
        })
        .map(drop)
    })
}

/// Run `f` on a new thread in the mount namespace of process `pid`, leaving the calling thread in
/// its own. Without `pid`, run it on the calling thread.
fn in_mntns<T: Send>(
    pid: Option<libc::pid_t>,
    f: impl FnOnce() -> nix::Result<T> + Send,
) -> nix::Result<T> {
    let Some(pid) = pid else {
        return f();
    };
    let ns = nix::fcntl::open(
        format!("/proc/{pid}/ns/mnt").as_str(),
        OFlag::O_RDONLY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?;
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .spawn_scoped(s, || {
                // setns() refuses a thread that shares its root and working directory.
                unshare(CloneFlags::CLONE_FS)?;
                setns(&ns, CloneFlags::CLONE_NEWNS)?;
                f()
            })
            .map_err(|e| Errno::from_raw(e.raw_os_error().unwrap_or(libc::EAGAIN)))?
            .join()
            .unwrap_or_else(|e| std::panic::resume_unwind(e))
    })
}

fn msflags_to_string(flags: MsFlags) -> String {
    [
        (MsFlags::MS_RDONLY, ("rw", "ro")),
        (MsFlags::MS_NOSUID, ("suid", "nosuid")),
        (MsFlags::MS_NODEV, ("dev", "nodev")),
        (MsFlags::MS_NOEXEC, ("exec", "noexec")),
        (MsFlags::MS_SYNCHRONOUS, ("async", "sync")),
        (MsFlags::MS_NOATIME, ("atime", "noatime")),
    ]
    .map(
        |(flag, (neg, pos))| {
            if flags.contains(flag) {
                pos
            } else {
                neg
            }
        },
    )
    .join(",")
}

/// Mount a fuse file system with fusermount
#[allow(clippy::too_many_arguments)]
fn fuse_fusermount_mount(
    mountpoint: &Path,
    fsname: &str,
    subtype: &str,
    opts: String,
    flags: MsFlags,
    auto_unmount: bool,
    target_mntns: Option<libc::pid_t>,
    fusermount: &str,
) -> Result<(File, Option<UnixStream>)> {
    let mut opts = vec![format!("fsname={fsname}"), opts, msflags_to_string(flags)];
    if !subtype.is_empty() {
        opts.push(format!("subtype={subtype}"));
    }
    if auto_unmount {
        opts.push("auto_unmount".to_owned());
    }
    let opts = opts.join(",");

    let (send, recv) = UnixStream::pair().unwrap();

    // Keep the sending socket around after exec to pass to fusermount.
    // When its partner recv closes, fusermount will unmount.
    // Remove the close-on-exec flag from the socket, so we can pass it to
    // fusermount.
    fcntl(&send, FcntlArg::F_SETFD(FdFlag::empty()))
        .map_err(|e| SessionFailure(format!("Failed to remove close-on-exec flag: {e}")))?;

    // Old version of fusermount doesn't support long --options, yet.
    let mut proc = fusermount_command(fusermount, target_mntns)
        .env("_FUSE_COMMFD", format!("{}", send.as_raw_fd()))
        .arg("-o")
        .arg(opts)
        .arg("--")
        .arg(mountpoint)
        .spawn()
        .map_err(IoError)?;

    if auto_unmount {
        std::thread::spawn(move || {
            let _ = proc.wait();
        });
    } else {
        match proc.wait().map_err(IoError)?.code() {
            Some(0) => {}
            exit_code => {
                return Err(SessionFailure(format!(
                    "Unexpected exit code when running fusermount: {exit_code:?}"
                )))
            }
        }
    }
    drop(send);

    match vmm_sys_util::sock_ctrl_msg::ScmSocket::recv_with_fd(&recv, &mut [0u8; 8]).map_err(
        |e| {
            SessionFailure(format!(
                "Unexpected error when receiving fuse file descriptor from fusermount: {}",
                e
            ))
        },
    )? {
        (_recv_bytes, Some(file)) => Ok((file, if auto_unmount { Some(recv) } else { None })),
        (recv_bytes, None) => Err(SessionFailure(format!(
            "fusermount did not send a file descriptor.  We received {recv_bytes} bytes."
        ))),
    }
}

/// Whether the connection is torn down, by an unmount or by an abort through
/// /sys/fs/fuse/connections/NNN/abort. Either way the fuse fd reports only POLLERR.
pub(crate) fn is_torn_down(file: &File) -> bool {
    let mut fds = [PollFd::new(file.as_fd(), PollFlags::empty())];
    poll(&mut fds, PollTimeout::from(0u16)).is_ok() && fds[0].revents() == Some(PollFlags::POLLERR)
}

/// The minor device number of the last mount at `mountpoint`, an
/// absolute path, in `mountinfo`, the format of /proc/self/mountinfo.
pub(crate) fn mount_minor(mountinfo: &str, mountpoint: &Path) -> Option<u32> {
    // mountinfo writes space, tab, newline and backslash as octal escapes.
    let mountpoint: String = mountpoint
        .to_str()?
        .chars()
        .map(|c| match c {
            ' ' | '\t' | '\n' | '\\' => format!("\\{:03o}", c as u32),
            c => c.to_string(),
        })
        .collect();
    mountinfo.lines().rev().find_map(|line| {
        // ID, parent ID, major:minor, root, mount point, ...
        let mut fields = line.split(' ');
        let dev = fields.nth(2)?;
        let point = fields.nth(1)?;
        if point != mountpoint {
            return None;
        }
        dev.split_once(':')?.1.parse().ok()
    })
}

/// The minor device number of the last mount at `mountpoint` in /proc/self/mountinfo.
#[cfg(test)]
pub(crate) fn mount_minor_at(mountpoint: &Path) -> Option<u32> {
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    mount_minor(&mountinfo, mountpoint)
}

/// Open the `abort` file of the connection of `session` in the fusectl file system.
/// Writing to it ends the connection, like `umount -f`. Once the connection is gone, writes do
/// nothing, so the file never ends a later connection that got the same number.
///
/// fusectl names connections after the minor number of the mount's device, which comes from
/// /proc/self/mountinfo, because a stat() of the mountpoint would wait for a FUSE request that
/// nobody serves yet. `FuseSession` keeps the mountpoint canonical, as mountinfo has it. A
/// session mounted in another mount namespace is looked up in the mountinfo of that namespace.
/// The file belongs to the user that mounted the file system.
pub(crate) fn open_abort_file(session: &FuseSession) -> io::Result<File> {
    let mountinfo = match session.target_mntns {
        Some(pid) => std::fs::read_to_string(format!("/proc/{pid}/mountinfo"))?,
        None => std::fs::read_to_string("/proc/self/mountinfo")?,
    };
    let minor = mount_minor(&mountinfo, session.mountpoint())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the mount isn't in mountinfo"))?;
    OpenOptions::new()
        .write(true)
        .open(format!("/sys/fs/fuse/connections/{minor}/abort"))
}

/// Whether the file system is unmounted. With the unique `mount_id` of the mount, that's whether
/// the mount at the mountpoint has another ID. The connection alone can't tell. A mount unmounted
/// from outside keeps it up while a file on it is open or another mount namespace holds a copy,
/// and an abort through /sys/fs/fuse/connections tears it down but keeps the mount.
/// Without an ID, or if the statx fails, a torn down connection counts as unmounted.
fn is_unmounted(file: &File, mountpoint: &Path, mount_id: Option<u64>) -> bool {
    match mount_id.map(|id| (id, unique_mount_id(mountpoint))) {
        Some((id, Some(current))) => current != id,
        _ => is_torn_down(file),
    }
}

/// The unique ID of the mount at `mountpoint`, on Linux 6.8+. The kernel never reuses one.
fn unique_mount_id(mountpoint: &Path) -> Option<u64> {
    // From the uapi headers, because older libc releases lack these flags, and statx() on musl.
    #[repr(C)]
    #[derive(Default)]
    struct Statx {
        stx_mask: u32,
        _blksize: u32,
        _fields: [u64; 17],
        stx_mnt_id: u64,
        _spare: [u64; 13],
    }
    const STATX_MNT_ID_UNIQUE: u32 = 0x4000;
    const AT_STATX_DONT_SYNC: libc::c_int = 0x4000;

    let mut stx = Statx::default();
    // Safe because stx has the size of a struct statx, and the kernel writes nothing else.
    let res = mountpoint
        .with_nix_path(|path| unsafe {
            libc::syscall(
                libc::SYS_statx,
                libc::AT_FDCWD,
                path.as_ptr(),
                // Don't let FUSE refresh the attributes with a GETATTR, or set off an automount.
                libc::AT_SYMLINK_NOFOLLOW | libc::AT_NO_AUTOMOUNT | AT_STATX_DONT_SYNC,
                STATX_MNT_ID_UNIQUE,
                &mut stx as *mut Statx,
            )
        })
        .ok()?;
    (res == 0 && stx.stx_mask & STATX_MNT_ID_UNIQUE != 0).then_some(stx.stx_mnt_id)
}

/// Detach a fuse file system from the mount tree, in the mount namespace of `target_mntns` if
/// set. Fall back to fusermount if umount2() fails with EPERM, or fails for any reason in that
/// namespace.
fn fuse_lazy_umount(
    mountpoint: &Path,
    fusermount: &str,
    target_mntns: Option<libc::pid_t>,
) -> Result<()> {
    // Like fusermount, don't follow a symlink that replaced the mountpoint.
    let umount = || umount2(mountpoint, MntFlags::MNT_DETACH | MntFlags::UMOUNT_NOFOLLOW);
    match in_mntns(target_mntns, umount) {
        Ok(()) => Ok(()),
        // As before, fusermount unmounts without the privileges for umount2(). In another mntns,
        // it takes over on any error.
        Err(e) if e == Errno::EPERM || target_mntns.is_some() => {
            fuse_fusermount_umount(mountpoint, fusermount, target_mntns)
        }
        Err(e) => Err(SessionFailure(format!(
            "failed to umount {}: {e}",
            mountpoint.display()
        ))),
    }
}

/// Build a fusermount command. With `target_mntns` set, it runs through nsenter in the mount
/// namespace of that process.
fn fusermount_command(
    fusermount: &str,
    target_mntns: Option<libc::pid_t>,
) -> std::process::Command {
    match target_mntns {
        Some(pid) => {
            let mut c = std::process::Command::new("nsenter");
            c.arg("-t")
                .arg(format!("{}", pid))
                .arg("-m")
                .arg(fusermount);
            c
        }
        None => std::process::Command::new(fusermount),
    }
}

/// Umount a fuse file system with the fusermount helper, in the mount namespace of
/// `target_mntns`.
fn fuse_fusermount_umount(
    mountpoint: &Path,
    fusermount: &str,
    target_mntns: Option<libc::pid_t>,
) -> Result<()> {
    match fusermount_command(fusermount, target_mntns)
        .arg("--unmount")
        .arg("--quiet")
        .arg("--lazy")
        .arg("--")
        .arg(mountpoint)
        .status()
        .map_err(IoError)?
        .code()
    {
        Some(0) => Ok(()),
        exit_code => Err(SessionFailure(format!(
            "Unexpected exit code when unmounting via running fusermount: {exit_code:?}"
        ))),
    }
}

/// Register `fd` as a backing file with ioctl FUSE_DEV_IOC_BACKING_OPEN on the FUSE device
/// `fuse_file` and return its backing id, for `open` to return with `OpenOptions::PASSTHROUGH`.
///
/// Needs `CAP_SYS_ADMIN` and `FsOptions::PASSTHROUGH` enabled at INIT. The id stays valid until
/// `fuse_backing_close()` or the end of the connection, and files opened with it keep using the
/// backing file after it is closed.
pub fn fuse_backing_open(fuse_file: &File, fd: RawFd) -> io::Result<u32> {
    // #define FUSE_DEV_IOC_BACKING_OPEN _IOW(FUSE_DEV_IOC_MAGIC, 1, struct fuse_backing_map)
    nix::ioctl_write_ptr!(backing_open, 229, 1, BackingMap);

    let map = BackingMap {
        fd,
        ..Default::default()
    };
    let id = unsafe { backing_open(fuse_file.as_raw_fd(), &map) }?;
    Ok(id as u32)
}

/// Release a backing id returned by `fuse_backing_open()` with ioctl FUSE_DEV_IOC_BACKING_CLOSE.
pub fn fuse_backing_close(fuse_file: &File, backing_id: u32) -> io::Result<()> {
    // #define FUSE_DEV_IOC_BACKING_CLOSE _IOW(FUSE_DEV_IOC_MAGIC, 2, uint32_t)
    nix::ioctl_write_ptr!(backing_close, 229, 2, u32);

    unsafe { backing_close(fuse_file.as_raw_fd(), &backing_id) }?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;
    use vmm_sys_util::tempdir::TempDir;

    /// Held while a test's child has a mount namespace of its own, and by the tests that need
    /// their mount to have no copy there. A new namespace copies every mount, and a copy keeps
    /// the connection of a lazily unmounted file system up.
    static MNTNS_COPY: Mutex<()> = Mutex::new(());

    #[test]
    fn test_new_session() {
        let se = FuseSession::new(Path::new("haha"), "foo", "bar", true);
        assert!(se.is_err());

        let dir = TempDir::new().unwrap();
        let se = FuseSession::new(dir.as_path(), "foo", "bar", false);
        assert!(se.is_ok());
    }

    #[test]
    fn test_new_channel() {
        let file = File::from(nix::unistd::dup(std::io::stdout()).unwrap());
        let _ = FuseChannel::new(file, 3, None).unwrap();
    }

    #[test]
    fn test_fusermount() {
        let dir = TempDir::new().unwrap();
        let se = FuseSession::new(dir.as_path(), "foo", "bar", true);
        assert!(se.is_ok());
        let mut se = se.unwrap();
        assert_eq!(se.get_fusermount(), FUSERMOUNT_BIN);

        se.set_fusermount("fusermount");
        assert_eq!(se.get_fusermount(), "fusermount");
    }

    #[test]
    fn test_clone_fuse_file() {
        let dir = TempDir::new().unwrap();
        let mut se = FuseSession::new(dir.as_path(), "foo", "bar", true).unwrap();
        se.mount().unwrap();

        let cloned_file = se.clone_fuse_file().unwrap();
        assert!(cloned_file.as_raw_fd() > 0);

        se.umount().unwrap();
        se.set_fuse_file(cloned_file);
        se.mount().unwrap();
    }

    #[test]
    fn test_new_blocking_channel() {
        let dir = TempDir::new().unwrap();
        let mut se = FuseSession::new(dir.as_path(), "foo", "bar", true).unwrap();
        assert!(se.new_blocking_channel().is_err());

        se.mount().unwrap();
        let ch = se.new_blocking_channel().unwrap();
        // The cloned fd has its own file description, so it stays blocking
        // even though the session fd is O_NONBLOCK.
        let flags = fcntl(&ch.file, FcntlArg::F_GETFL).unwrap();
        assert_eq!(
            OFlag::from_bits_truncate(flags) & OFlag::O_NONBLOCK,
            OFlag::empty()
        );

        se.umount().unwrap();
    }

    #[test]
    fn test_blocking_channel_exit_on_umount() {
        // The read below must see the teardown within its timeout.
        let _ns = MNTNS_COPY.lock().unwrap_or_else(PoisonError::into_inner);
        let dir = TempDir::new().unwrap();
        let mut se = FuseSession::new(dir.as_path(), "foo", "bar", true).unwrap();
        se.mount().unwrap();

        let mut ch = se.new_blocking_channel().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            // A freshly-mounted connection queues FUSE_INIT, so the first
            // get_request() returns that request instead of blocking. Drain
            // requests until the umount below tears the connection down and
            // get_request() finally returns Ok(None): that is the teardown
            // contract this test asserts, and the only way a blocking channel
            // exits (FuseSession::wake() has no effect on it).
            let torn_down = loop {
                match ch.get_request() {
                    Ok(None) => break true,
                    Ok(Some(_)) => continue,
                    Err(_) => break false,
                }
            };
            tx.send(torn_down).unwrap();
        });

        se.umount().unwrap();
        // The pending read must return (Ok(None)) once the connection is
        // torn down; the timeout guards against a hang.
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)), Ok(true));
        thread.join().unwrap();
    }

    #[test]
    fn test_mount_minor() {
        let info = "22 1 0:21 / /proc rw - proc proc rw\n\
                    95 30 0:84 / /tmp/a rw - fuse.x x rw\n\
                    96 30 0:86 / /tmp/a rw - fuse.x x rw\n";
        // The last mount at a path is the one in effect.
        assert_eq!(mount_minor(info, "/tmp/a".as_ref()), Some(86));
        assert_eq!(mount_minor(info, "/tmp/b".as_ref()), None);
        // Special characters are octal escapes.
        let info = "97 30 0:88 / /tmp/a\\040b rw - fuse.x x rw\n";
        assert_eq!(mount_minor(info, "/tmp/a b".as_ref()), Some(88));
    }

    /// A connection aborted through /sys/fs/fuse/connections keeps its mount
    /// until the session unmounts it.
    #[test]
    fn test_umount_aborted_connection() {
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("not root, skipping");
            return;
        }
        let dir = TempDir::new().unwrap();
        let mnt = dir.as_path().join("mount point");
        std::fs::create_dir(&mnt).unwrap();
        let mut se = FuseSession::new(&mnt, "abort_test", "", false).unwrap();
        se.mount().unwrap();
        if se.mount_id.is_none() {
            eprintln!("the kernel has no unique mount IDs, skipping");
            return;
        }
        let mountpoint = se.mountpoint().to_owned();
        let minor = mount_minor_at(&mountpoint).unwrap();
        let abort = format!("/sys/fs/fuse/connections/{minor}/abort");
        if std::fs::write(abort, b"1").is_err() {
            eprintln!("fusectl isn't mounted, skipping");
            return;
        }
        assert!(is_torn_down(se.get_fuse_file().unwrap()));

        drop(se);
        assert_eq!(mount_minor_at(&mountpoint), None);
    }

    /// Unmount the session from outside, mount a tmpfs in its place, and check that dropping
    /// the session leaves the tmpfs alone. With `in_use`, a file open on the detached mount keeps
    /// its connection up.
    fn check_umount_spares_later_mount(in_use: bool) {
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("not root, skipping");
            return;
        }
        let _ns = MNTNS_COPY.lock().unwrap_or_else(PoisonError::into_inner);
        let dir = TempDir::new().unwrap();
        let mut se = FuseSession::new(dir.as_path(), "spare_test", "", false).unwrap();
        se.mount().unwrap();
        if in_use && se.mount_id.is_none() {
            eprintln!("the kernel has no unique mount IDs, skipping");
            return;
        }
        let mountpoint = se.mountpoint().to_owned();
        // Nobody serves the connection, and an O_PATH open sends it no request.
        let file = in_use.then(|| {
            File::options()
                .read(true)
                .custom_flags(libc::O_PATH)
                .open(&mountpoint)
                .unwrap()
        });
        umount2(&mountpoint, MntFlags::MNT_DETACH).unwrap();
        let dev = se.get_fuse_file().unwrap();
        if in_use {
            assert!(!is_torn_down(dev));
        } else {
            // A child that another test forked during the mount keeps the connection up through its
            // copies of the fsopen() and fsmount() descriptors until it execs or exits. The poll
            // returns at the teardown.
            let mut fds = [PollFd::new(dev.as_fd(), PollFlags::empty())];
            assert_eq!(poll(&mut fds, PollTimeout::from(5000u16)), Ok(1));
            assert_eq!(fds[0].revents(), Some(PollFlags::POLLERR));
        }
        mount(
            Some("tmpfs"),
            &mountpoint,
            Some("tmpfs"),
            MsFlags::empty(),
            None::<&str>,
        )
        .unwrap();

        drop(se);
        assert!(mount_minor_at(&mountpoint).is_some());
        umount2(&mountpoint, MntFlags::MNT_DETACH).unwrap();
        drop(file);
    }

    /// Once the file system is unmounted from outside, another one may be
    /// mounted in its place, and the session leaves that one alone.
    #[test]
    fn test_umount_spares_later_mount() {
        check_umount_spares_later_mount(false);
    }

    /// The same while the detached mount is in use, so its connection is still up.
    #[test]
    fn test_umount_spares_later_mount_in_use() {
        check_umount_spares_later_mount(true);
    }

    /// Run `f` on a new thread on which `syscall` fails with `errno`.
    fn without_syscall<T: Send>(
        syscall: libc::c_long,
        errno: Errno,
        f: impl FnOnce() -> T + Send,
    ) -> T {
        let bpf = |code: u32, jf: u8, k: u32| libc::sock_filter {
            code: code as u16,
            jt: 0,
            jf,
            k,
        };
        let mut filter = [
            // Load seccomp_data.nr.
            bpf(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, 0, 0),
            bpf(
                libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
                1,
                syscall as u32,
            ),
            bpf(libc::BPF_RET, 0, libc::SECCOMP_RET_ERRNO | errno as u32),
            bpf(libc::BPF_RET, 0, libc::SECCOMP_RET_ALLOW),
        ];
        std::thread::scope(|s| {
            s.spawn(|| {
                let prog = libc::sock_fprog {
                    len: filter.len() as u16,
                    filter: filter.as_mut_ptr(),
                };
                // SAFETY: `prog` outlives the calls, and the filter binds only this thread.
                unsafe {
                    assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
                    assert_eq!(
                        libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &prog),
                        0
                    );
                }
                f()
            })
            .join()
            .unwrap()
        })
    }

    /// The last line of /proc/`pid`/mountinfo for `mountpoint`, without the fields that differ
    /// between two mounts with the same options: the mount IDs, the device number and the IDs
    /// of the peer groups.
    fn mountinfo(pid: &str, mountpoint: &Path) -> Option<String> {
        let info = std::fs::read_to_string(format!("/proc/{pid}/mountinfo")).unwrap();
        let mountpoint = mountpoint.to_str();
        let line = info
            .lines()
            .rev()
            .find(|line| line.split(' ').nth(4) == mountpoint)?;
        let fields: Vec<&str> = line
            .split(' ')
            .skip(3)
            .map(|f| f.split(':').next().unwrap())
            .collect();
        Some(fields.join(" "))
    }

    /// fsmount() mounts with the options that mount(2) mounts with.
    #[test]
    fn test_fsmount_matches_mount() {
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("not root, skipping");
            return;
        }
        let dir = TempDir::new().unwrap();
        let other_flags = MsFlags::MS_NOEXEC
            | MsFlags::MS_NODIRATIME
            | MsFlags::MS_RELATIME
            | MsFlags::MS_SYNCHRONOUS
            | MsFlags::MS_DIRSYNC
            | MsFlags::MS_LAZYTIME;
        // readonly, allow_other, subtype, mount flags
        let configs = [
            (false, true, "bar", None),
            (true, false, "", None),
            (false, true, "bar", Some(other_flags)),
        ];
        for (readonly, allow_other, subtype, flags) in configs {
            // Failing mount(2) leaves fsmount(), and failing fsopen() leaves mount(2).
            let [new, old] = [libc::SYS_mount, libc::SYS_fsopen].map(|syscall| {
                let mut se = FuseSession::new(dir.as_path(), "foo", subtype, readonly).unwrap();
                se.set_allow_other(allow_other);
                if let Some(flags) = flags {
                    se.set_mount_flags(flags);
                }
                without_syscall(syscall, Errno::ENOSYS, || se.mount()).unwrap();
                let line = mountinfo("self", se.mountpoint()).unwrap();
                se.umount().unwrap();
                line
            });
            assert_eq!(new, old);
        }
    }

    /// Where fsmount() fails, at the start or the end, mount() falls back to mount(2).
    #[test]
    fn test_fsmount_fallback() {
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("not root, skipping");
            return;
        }
        let dir = TempDir::new().unwrap();
        for (syscall, errno) in [
            (libc::SYS_fsopen, Errno::ENOSYS),
            (libc::SYS_move_mount, Errno::EPERM),
        ] {
            let mut se = FuseSession::new(dir.as_path(), "foo", "bar", false).unwrap();
            without_syscall(syscall, errno, || se.mount()).unwrap();
            assert!(mountinfo("self", se.mountpoint()).is_some());
            // The INIT request waits on the session's fd.
            let mut fds = [PollFd::new(
                se.get_fuse_file().unwrap().as_fd(),
                PollFlags::POLLIN,
            )];
            assert_eq!(poll(&mut fds, PollTimeout::from(0u16)), Ok(1));
            assert_eq!(fds[0].revents(), Some(PollFlags::POLLIN));
            se.umount().unwrap();
        }
    }

    /// With set_target_mntns(), the session mounts and unmounts in the target's mount namespace,
    /// and the calling thread stays in its own.
    #[test]
    fn test_fsmount_target_mntns() {
        use std::os::unix::process::CommandExt;

        if unsafe { libc::geteuid() } != 0 {
            eprintln!("not root, skipping");
            return;
        }
        let _ns = MNTNS_COPY.lock().unwrap_or_else(PoisonError::into_inner);
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("60");
        // SAFETY: the closure calls only unshare() and mount(), which are async-signal-safe,
        // and nix passes the short path without allocating.
        unsafe {
            cmd.pre_exec(|| {
                unshare(CloneFlags::CLONE_NEWNS)?;
                // Keep the mounts of the new namespace out of this one.
                mount(
                    None::<&str>,
                    "/",
                    None::<&str>,
                    MsFlags::MS_REC | MsFlags::MS_PRIVATE,
                    None::<&str>,
                )?;
                Ok(())
            });
        }
        let mut child = cmd.spawn().unwrap();
        let pid = child.id().to_string();

        let dir = TempDir::new().unwrap();
        // Failing mount(2) leaves fsmount(), and failing fsopen() leaves mount(2). Both run on a
        // helper thread.
        let results = [libc::SYS_mount, libc::SYS_fsopen].map(|syscall| {
            let mut se = FuseSession::new(dir.as_path(), "foo", "bar", false).unwrap();
            se.set_target_mntns(Some(child.id() as libc::pid_t));
            // Make the fallback through nsenter and fusermount fail.
            se.set_fusermount("/nonexistent");
            let (mounted, same_ns) = without_syscall(syscall, Errno::ENOSYS, || {
                let ns = || std::fs::read_link("/proc/thread-self/ns/mnt").unwrap();
                let before = ns();
                (se.mount().is_ok(), ns() == before)
            });
            let there = mountinfo(&pid, se.mountpoint());
            let here = mountinfo("self", se.mountpoint());
            let unmounted = se.umount().is_ok();
            let after = mountinfo(&pid, se.mountpoint());
            (mounted, same_ns, there, here, unmounted, after)
        });
        let _ = child.kill();
        let _ = child.wait();

        for (mounted, same_ns, there, here, unmounted, after) in results {
            assert!(mounted && same_ns);
            assert!(there.unwrap().contains(" - fuse.bar foo "));
            assert_eq!(here, None);
            assert!(unmounted);
            assert_eq!(after, None);
        }
    }
}

#[cfg(feature = "async-io")]
pub use asyncio::FuseDevTask;

#[cfg(feature = "async-io")]
pub use stop_signal::StopSignal;
// The io_uring workers take one too, but nothing public does without
// `async-io`.
#[cfg(all(feature = "uring", not(feature = "async-io")))]
pub(crate) use stop_signal::StopSignal;

#[cfg(any(feature = "async-io", feature = "uring"))]
mod stop_signal {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use vmm_sys_util::eventfd::{EventFd, EFD_CLOEXEC, EFD_NONBLOCK};

    /// A stop flag that, when it stops, wakes the `FuseDevTask`s waiting for
    /// requests on the tokio runtime. A bare `Arc<AtomicBool>` converts into
    /// a signal that wakes no one, and an idle task checks it every 100ms.
    #[derive(Clone, Debug)]
    pub struct StopSignal {
        flag: Arc<AtomicBool>,
        // Becomes readable for good when the signal stops.
        event: Option<Arc<EventFd>>,
    }

    impl StopSignal {
        /// A signal that sets `flag` when it stops. Stop it with `stop()`. An
        /// idle task with a signal has no timer, so a store to `flag` doesn't
        /// wake it.
        pub fn new(flag: Arc<AtomicBool>) -> std::io::Result<Self> {
            Ok(StopSignal {
                flag,
                event: Some(Arc::new(EventFd::new(EFD_NONBLOCK | EFD_CLOEXEC)?)),
            })
        }

        /// Set the flag and wake the tasks. The tasks finish the requests in
        /// flight and return.
        pub fn stop(&self) {
            self.flag.store(true, Ordering::Release);
            if let Some(event) = &self.event {
                let _ = event.write(1);
            }
        }

        pub(crate) fn is_stopped(&self) -> bool {
            self.flag.load(Ordering::Acquire)
        }

        /// The eventfd that stays readable once the signal stops, if the
        /// signal has one.
        pub(crate) fn event(&self) -> Option<&EventFd> {
            self.event.as_deref()
        }
    }

    impl From<Arc<AtomicBool>> for StopSignal {
        fn from(flag: Arc<AtomicBool>) -> Self {
            StopSignal { flag, event: None }
        }
    }
}

#[cfg(feature = "async-io")]
/// Task context to handle fuse request in asynchronous mode.
mod asyncio {
    use std::cell::{Cell, RefCell};
    use std::future::Future;
    use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
    use std::os::unix::io::{AsRawFd, RawFd};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use futures_util::stream::{FuturesUnordered, StreamExt};
    use nix::errno::Errno;
    use nix::fcntl::{fcntl, FcntlArg, OFlag};
    use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
    use nix::sys::time::TimeSpec;
    use nix::sys::timerfd::{ClockId, Expiration, TimerFd, TimerFlags, TimerSetTimeFlags};
    use tokio::io::unix::{AsyncFd, AsyncFdReadyGuard};
    use tokio::io::Interest;
    use vmm_sys_util::eventfd::EventFd;

    use fuse_backend_core::api::filesystem::AsyncFileSystem;
    use fuse_backend_core::api::server::Server;
    use fuse_backend_core::async_file::{File as AsyncFile, TrackedRequest};
    use fuse_backend_core::async_runtime;
    use fuse_backend_core::buffer::Reader;

    use crate::serving::WorkerActivity;
    use crate::splice::SplicePipes;
    use crate::{FuseBuf, FuseDevReaderExt, FuseDevWriter, StopSignal};

    /// Default limit on the number of concurrently processed requests.
    ///
    /// The limit bounds both the io_uring queue depth and the memory used by
    /// the per-request buffers (one request buffer per in-flight request).
    const DEFAULT_MAX_INFLIGHT: usize = 16;

    /// Pool of reusable request buffers for pipelined request processing.
    ///
    /// Every in-flight request owns one buffer for its request data and
    /// reply. Buffers are allocated lazily on first use and recycled through
    /// the free list afterwards; once `max_inflight` buffers exist, no more
    /// buffers are allocated, which also bounds the number of concurrently
    /// processed requests and the memory used by them. When the pool is
    /// exhausted, the caller serves in-flight requests first instead of
    /// reading new ones (back pressure), and unread requests queue up in
    /// the kernel, which throttles the clients on its own once its queues
    /// fill up. A buffer leaked with its request doesn't count against the
    /// limit, see `release_done()`, so the limit doesn't bound leaks.
    struct BufferPool {
        free: RefCell<Vec<RequestBuf>>,
        created: Cell<usize>,
        buf_size: usize,
        max_inflight: usize,
        /// Set if the requests splice their READ replies. Their pipes are
        /// siblings of these, so that they stop splicing together.
        splice: Option<SplicePipes>,
    }

    /// The buffer of an in-flight request, and the pipes its READ reply
    /// splices through. A request can wait between the splice of its data and
    /// the reply, so every request needs pipes of its own.
    struct RequestBuf {
        buf: Vec<u8>,
        splice: Option<SplicePipes>,
    }

    impl BufferPool {
        fn new(buf_size: usize, max_inflight: usize, splice: bool) -> Self {
            BufferPool {
                free: RefCell::new(Vec::new()),
                created: Cell::new(0),
                buf_size,
                max_inflight: max_inflight.max(1),
                splice: splice.then(SplicePipes::new),
            }
        }

        /// Get a buffer from the pool, allocating a new one lazily if the
        /// pool is empty but the limit hasn't been reached yet, or return
        /// `None` if all buffers are in use.
        fn try_acquire(&self) -> Option<RequestBuf> {
            if let Some(buf) = self.free.borrow_mut().pop() {
                return Some(buf);
            }
            if self.created.get() < self.max_inflight {
                self.created.set(self.created.get() + 1);
                return Some(RequestBuf {
                    buf: vec![0x0u8; self.buf_size],
                    splice: self.splice.as_ref().map(SplicePipes::sibling),
                });
            }
            None
        }

        /// Return a buffer to the pool for reuse.
        fn release(&self, buf: RequestBuf) {
            self.free.borrow_mut().push(buf);
        }

        /// Return the buffer of a completed request, or, if the request
        /// leaked it, let the pool allocate another one in its place.
        fn release_done(&self, buf: Option<RequestBuf>) {
            match buf {
                Some(buf) => self.release(buf),
                None => self.created.set(self.created.get() - 1),
            }
        }
    }

    /// Task context to handle fuse request in asynchronous mode.
    ///
    /// This structure provides a context to handle fuse request in asynchronous mode, including
    /// the fuse device file and a `Server` instance to serve requests.
    ///
    /// ## Examples
    /// ```text
    /// let buf_size = (fuse_backend_core::api::server::MAX_BUFFER_SIZE + 0x1000) as usize;
    /// let file = session.clone_fuse_file().unwrap();
    /// let state = Arc::new(AtomicBool::new(false));
    /// let mut task = FuseDevTask::new(buf_size, file, fs_server, state.clone());
    ///
    /// // Run the task
    /// executor.spawn(async move { task.poll_handler().await });
    ///
    /// // Stop the task. It checks `state` before reading a request. An idle task checks it
    /// // every 100ms with the tokio runtime, and with tokio-uring only once a request arrives
    /// // or the session is torn down. A `StopSignal` in place of `state` wakes an idle task on
    /// // the tokio runtime right away.
    /// state.store(true, Ordering::Relaxed);
    /// session.umount().unwrap();
    /// ```
    pub struct FuseDevTask<F: AsyncFileSystem + Sync> {
        file: AsyncFile,
        state: StopSignal,
        server: Arc<Server<F>>,
        buf_size: usize,
        max_inflight: usize,
        activity: Option<WorkerActivity>,
        splice_write: bool,
        leaked: Arc<AtomicUsize>,
        block_while_idle: bool,
    }

    impl<F: AsyncFileSystem + Sync> FuseDevTask<F> {
        /// Create a new fuse task context for asynchronous IO.
        ///
        /// The number of concurrently processed requests is limited to
        /// `DEFAULT_MAX_INFLIGHT`, use `Self::new_with_max_inflight()` to
        /// customize the limit.
        ///
        /// # Parameters
        /// - buf_size: size of buffer to receive requests from/send reply to the fuse fd.
        ///   It must be big enough to hold any request, at least
        ///   `fuse_backend_core::api::server::MAX_BUFFER_SIZE + 0x1000`, otherwise the kernel rejects
        ///   reads from the fuse device with `EINVAL` once the INIT handshake is done.
        ///   Note that requests are processed concurrently, each with its own buffer of
        ///   this size.
        /// - file: file object for the fuse device, ownership is taken by the task object
        /// - server: `Server` instance to serve requests from the fuse fd
        /// - state: shared flag to control the task object, an `Arc<AtomicBool>` or a
        ///   `StopSignal`. Once it's set, the task reads no more requests and returns after
        ///   completing the requests in flight. How soon an idle task notices depends on the
        ///   runtime, as `poll_handler()` describes.
        pub fn new(
            buf_size: usize,
            file: std::fs::File,
            server: Arc<Server<F>>,
            state: impl Into<StopSignal>,
        ) -> Self {
            Self::new_with_max_inflight(buf_size, file, server, state, DEFAULT_MAX_INFLIGHT)
        }

        /// Create a new fuse task context for asynchronous IO with a custom
        /// limit on the number of concurrently processed requests.
        ///
        /// # Parameters
        /// - buf_size, file, server, state: same as `Self::new()`.
        /// - max_inflight: maximum number of requests processed concurrently.
        ///   Each in-flight request owns one buffer of `buf_size` bytes, so
        ///   this limit also bounds the memory used by the task. Values
        ///   smaller than 1 are clamped to 1.
        pub fn new_with_max_inflight(
            buf_size: usize,
            file: std::fs::File,
            server: Arc<Server<F>>,
            state: impl Into<StopSignal>,
            max_inflight: usize,
        ) -> Self {
            // The fuse device fd is a file description without `O_NONBLOCK`
            // (both freshly opened fds and fds cloned with `FUSE_DEV_IOC_CLONE`).
            // Set it explicitly, because reads issued while no request is
            // pending must not block the runtime thread:
            // - with the io-uring runtime, io-uring submits reads inline on
            //   fds without `O_NONBLOCK`, and `fuse_dev_do_read()` only honors
            //   `O_NONBLOCK` (not `IOCB_NOWAIT`), so a blocking fd would stall
            //   the io-uring submission thread. With `O_NONBLOCK` the read
            //   fails with `EAGAIN` and io-uring waits for readiness via poll
            //   instead.
            // - with the tokio runtime, reads are issued with plain
            //   `preadv()`, which would block the whole single-threaded
            //   runtime (stalling in-flight request handling and teardown).
            //   With `O_NONBLOCK` the read fails with `EAGAIN` and
            //   `poll_handler()` waits for the fd to become readable instead.
            //
            // Note: `O_NONBLOCK` is a property of the file description, so the
            // fd must not be shared with other consumers of the same file
            // description; the task takes ownership of `file` for this reason.
            fcntl(&file, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))
                .expect("failed to set fuse device fd nonblocking");

            FuseDevTask {
                file: AsyncFile::from_std_file(file),
                server,
                state: state.into(),
                buf_size,
                max_inflight: max_inflight.max(1),
                activity: None,
                splice_write: false,
                leaked: Arc::default(),
                block_while_idle: false,
            }
        }

        /// The number of requests that this task leaked, each with a request buffer of
        /// `buf_size` bytes and a reference to the server, because IO into the buffer was in
        /// flight when the request was dropped or completed. See `async_file::TrackedRequest`.
        pub fn leaked_requests(&self) -> usize {
            self.leaked.load(Ordering::Relaxed)
        }

        /// Report whether the task serves requests to its serving layer.
        pub(crate) fn report_activity(&mut self, activity: WorkerActivity) {
            self.activity = Some(activity);
        }

        /// Send READ replies with `splice(2)`, as `FuseSession::set_splice_write()` does for
        /// the channels of a session. `AsyncFuseServing` calls this with the setting of its
        /// session. It's off by default.
        ///
        /// A READ reply splices when the file system hands the transport a file with a
        /// descriptor through `ZeroCopyWriter::write_from()`, or through
        /// `AsyncZeroCopyWriter::async_write_from()` on a runtime whose file IO blocks the
        /// thread, see `FuseDevWriter::async_write_from_at()`. The splice runs on the runtime
        /// thread. Each request buffer gets two pipes on its first splice and keeps them until
        /// `poll_handler()` returns, so a task holds up to `max_inflight` pairs. See
        /// `FuseSession::set_splice_write()` for what they count against.
        pub fn set_splice_write(&mut self, splice: bool) {
            self.splice_write = splice;
        }

        /// Wait for requests in `epoll_wait(2)` while no request is in flight, blocking the
        /// thread that polls `poll_handler()`.
        ///
        /// The task's fd joins an epoll instance of its own with `EPOLLEXCLUSIVE`, so a new
        /// request wakes one of the idle tasks that serve clones of the connection, not all of
        /// them. On both runtimes, the task then reads the fd with `read(2)` once it's readable.
        /// See `read_exclusive()` and `Exclusive`. Use this only for a task that has its runtime
        /// to itself. A task that no tokio runtime polls ignores this.
        pub(crate) fn block_while_idle(&mut self) {
            self.block_while_idle = true;
        }

        /// Handler to process fuse requests in asynchronous mode.
        ///
        /// An async fn to handle requests from the fuse fd. It works in asynchronous IO mode when:
        /// - receiving request from fuse fd
        /// - handling requests by calling Server::async_handle_message()
        /// - sending reply to fuse fd
        ///
        /// Requests are processed concurrently: each request read from the fuse device
        /// is turned into a request-handling future pushed into a `FuturesUnordered`
        /// set, which is driven in parallel with reading the next request, so multiple
        /// requests are in flight at the same time even though the runtime is
        /// single-threaded. This allows the asynchronous IO engine (io_uring) to queue
        /// and batch IO operations instead of serving them one by one. The number of
        /// in-flight requests is bounded by the request buffer pool, and replies may
        /// be sent out of order, which is fine because the kernel matches replies to
        /// requests by their unique ids.
        ///
        /// The select loop is biased to complete in-flight requests before reading new
        /// ones: unread requests just queue up in the kernel without consuming
        /// userspace resources, while completed-but-unreplied requests pin request
        /// buffers and keep clients waiting, so draining them first keeps the buffer
        /// pool circulating and tail latency low. Note that a read issued on the fuse
        /// device is never canceled once started: when the select loop turns to
        /// in-flight work while a read is pending, the read is driven to completion
        /// before the loop restarts, because the kernel may consume a request from
        /// its queue into the read buffer at any point, and canceling the read
        /// afterwards would silently lose that request. A task that blocks while
        /// idle is the exception. It reads with nonblocking `read(2)`, which
        /// leaves no read pending in the kernel, so it can drop its read when the
        /// last request in flight completes, and start over to block.
        ///
        /// It returns when it finds `state` set, or when the fuse session is torn down (EOF or
        /// `ENODEV` from the fuse device). With the tokio runtime, a `StopSignal` wakes an idle
        /// task, and an idle task with a bare flag checks it every 100ms. If it can't wait for
        /// either, it logs a warning. With tokio-uring, a read waiting in the kernel usually
        /// completes only when a request arrives or the session is torn down, and the task checks
        /// `state` then. A task that blocks while idle wakes on a `StopSignal` too, and checks a
        /// bare flag every 10ms, on both runtimes.
        ///
        /// To stop an idle task without tearing the session down, for example for a takeover
        /// with `Server::restore_init()`, drop this future. Do this only with the tokio runtime,
        /// where it's safe while the task waits for a request, because the task reads nothing
        /// until the fd is readable. Requests that the task read but didn't answer stay pending
        /// in the kernel as long as the file description they came from stays open, and
        /// `notify_resend()` makes the kernel send them again. The task owns its fd, so before
        /// `new()`, keep a duplicate from `File::try_clone()` to hand the file description to the
        /// new server.
        ///
        /// With tokio-uring, dropping this future is safe, for example in a `select!` with a
        /// shutdown signal, but it doesn't cancel the IO in the kernel. The read that waits for a
        /// request owns its buffer, and the kernel can still copy the next request into it.
        /// Nothing answers that request until another `notify_resend()`, so for a takeover, set
        /// `state` and let the task serve one more request instead. A request with IO into its
        /// buffer in flight is leaked, see `leaked_requests()` and `async_file::File`.
        ///
        /// Note: when driven by the tokio-uring runtime, io_uring submission queue entries
        /// are submitted to the kernel whenever the runtime parks, so the driving future
        /// must not busy-loop and should let the runtime park regularly (e.g. by running
        /// `poll_handler()` as a separate task). Otherwise requests may pile up in the
        /// kernel without being served.
        ///
        /// With the tokio runtime, the task waits for requests through the IO driver of the
        /// tokio runtime that polls it, and panics if that runtime has IO disabled. If no tokio
        /// runtime polls it, it retries reads in a loop. A task that keeps finding requests
        /// yields every few requests with `async_runtime::park()`, so that the runtime's
        /// timers, IO and other tasks run.
        pub async fn poll_handler(&mut self) {
            // TODO: register the request buffers as io uring buffers.
            let fd = self.file.as_raw_fd();
            // With the tokio runtime, `preadv()` fails with `EAGAIN` while no request is
            // pending, so wait for the fd to become readable. A torn down connection
            // reports only `EPOLLERR`, so wait for errors too.
            //
            // A task whose `state` is already set returns before it registers the fd.
            // Registering needs a tokio runtime and a file that epoll supports. Without them,
            // the task yields and retries reads instead.
            if self.state.is_stopped() {
                return;
            }
            let in_tokio = tokio::runtime::Handle::try_current().is_ok();
            let exclusive = if self.block_while_idle && in_tokio {
                DeviceReadiness::exclusive(fd, &self.state)
                    .map_err(|e| warn!("can't wait for requests with EPOLLEXCLUSIVE: {}", e))
                    .ok()
            } else {
                None
            };
            let blocks_while_idle = exclusive.is_some();
            let ready = exclusive.or_else(|| match &self.file {
                AsyncFile::Tokio(_) if !in_tokio => {
                    warn!("not polled by a tokio runtime, retrying fuse device reads instead of waiting for requests");
                    None
                }
                AsyncFile::Tokio(_) => {
                    // SAFETY: `fd` belongs to `self.file`, which stays open for the whole
                    // function, so it outlives the registration.
                    let registered = unsafe {
                        AsyncFd::register_with_interest(fd, Interest::READABLE | Interest::ERROR)
                    };
                    match registered {
                        Ok(device) => Some(DeviceReadiness::new(device, &self.state)),
                        Err(e) => {
                            warn!(
                                "can't wait for the fuse device fd to become readable, retrying reads instead: {}",
                                e
                            );
                            None
                        }
                    }
                }
                _ => None,
            });
            let pool = BufferPool::new(self.buf_size, self.max_inflight, self.splice_write);
            let mut inflight = FuturesUnordered::new();
            // See `PARK_AFTER_READS`.
            let mut reads_without_wait = 0;

            'serve: while !self.state.is_stopped() {
                // Get a buffer for the next request. If the pool is exhausted,
                // all buffers are held by in-flight requests, so make progress
                // on those first (back pressure).
                let mut buf = match pool.try_acquire() {
                    Some(buf) => buf,
                    None => {
                        if let Some(done) = inflight.next().await {
                            pool.release_done(done);
                            continue 'serve;
                        }
                        // Unreachable: the pool never hands out more buffers
                        // than it created, and every created buffer is either
                        // in the free list or held by an in-flight request.
                        error!("request buffer pool is empty without in-flight requests");
                        break;
                    }
                };

                // The outcome of the read once it completes. Limit the scope
                // of the read future, so that its borrow of `buf` ends before
                // the buffer is handed over to a request or the pool.
                let read_result = {
                    let park = reads_without_wait >= PARK_AFTER_READS;
                    let idle = inflight.is_empty();
                    let inner = read_request(
                        &self.file,
                        ready.as_ref(),
                        &self.state,
                        &mut buf.buf,
                        park,
                        idle,
                    );
                    tokio::pin!(inner);
                    // Whether the read returned to the runtime before it completed.
                    let mut waited = false;
                    let mut read = std::future::poll_fn(|cx| {
                        let poll = inner.as_mut().poll(cx);
                        waited |= poll.is_pending();
                        poll
                    });

                    let result = loop {
                        if inflight.is_empty() {
                            if let Some(activity) = &self.activity {
                                activity.idle();
                            }
                        }
                        tokio::select! {
                            biased;
                            // Complete in-flight requests first, see the function doc.
                            res = inflight.next(), if !inflight.is_empty() => {
                                // Safe: the set is non-empty, so next() only returns
                                // once a request future has completed.
                                pool.release_done(res.unwrap());
                                // A task that blocks while idle can drop its read, see
                                // the function doc, and start over to block.
                                if inflight.is_empty() && blocks_while_idle {
                                    break None;
                                }
                            }
                            result = &mut read => {
                                break Some(result);
                            }
                        }
                    };
                    // A task that blocks while idle ignores `park` and counts its own reads,
                    // see `Exclusive`.
                    reads_without_wait = if waited {
                        0
                    } else {
                        reads_without_wait.saturating_add(1)
                    };
                    // The read future has completed, so dropping it can't cancel
                    // an in-flight read and lose a request anymore.
                    result
                };
                let Some(read_result) = read_result else {
                    pool.release(buf);
                    continue 'serve;
                };

                match read_result {
                    Ok(None) => {
                        // `state` stopped the task while it waited for a request.
                        debug!("fuse dev task stopping: state flag set");
                        pool.release(buf);
                        break 'serve;
                    }
                    Ok(Some(0)) => {
                        // EOF, the fuse session has been torn down.
                        info!("fuse dev task stopping: fuse session torn down");
                        pool.release(buf);
                        break 'serve;
                    }
                    Ok(Some(len)) => {
                        if let Some(activity) = &self.activity {
                            activity.busy();
                        }
                        let request = handle_request(self.server.clone(), fd, buf, len);
                        // SAFETY: the request owns its buffer, which it returns, and an
                        // `Arc` of the server, so a leak keeps the file system alive too.
                        let request = unsafe { TrackedRequest::new(request, self.leaked.clone()) };
                        inflight.push(request);
                    }
                    Err(e) => {
                        pool.release(buf);
                        match e.raw_os_error() {
                            Some(libc::ENODEV)
                            | Some(libc::ENOTCONN)
                            | Some(libc::ECONNABORTED) => {
                                // The fuse device was unmounted or the connection was aborted.
                                break 'serve;
                            }
                            Some(libc::EINTR) | Some(libc::ENOENT) => {
                                // Interrupted by a signal, or the kernel dropped
                                // the request because it was interrupted. Retry
                                // the read.
                                continue 'serve;
                            }
                            Some(libc::EAGAIN) => {
                                // No request is pending, and nothing waited
                                // for the fd to become readable. This happens
                                // with io_uring, where some kernels complete
                                // the read with `EAGAIN` instead of arming
                                // poll on the fuse device. It also happens
                                // with a tokio task that couldn't register
                                // the fd. Retrying immediately would
                                // busy-loop without ever letting the runtime
                                // park (which is when io_uring submissions
                                // and reactor events are processed). Yield to
                                // the runtime before retrying instead.
                                tokio::task::yield_now().await;
                                reads_without_wait = 0;
                                continue 'serve;
                            }
                            _ => {
                                // The error isn't an OS error, for example because the
                                // runtime is shutting down, or one that retrying won't
                                // fix, such as EINVAL for a buffer smaller than the
                                // kernel's requests. Retrying would spin on it, so stop
                                // like the synchronous server does.
                                error!("failed to read request from fuse device fd, {}", e);
                                break 'serve;
                            }
                        }
                    }
                }
            }

            // Drain all in-flight requests before returning, so quiescing the
            // task doesn't drop requests without a reply.
            while let Some(done) = inflight.next().await {
                pool.release_done(done);
            }

            // TODO: unregister the request buffers as io uring buffers.
        }
    }

    /// How often a task waiting for a request through its runtime checks its `state` flag.
    const STATE_CHECK_INTERVAL: Duration = Duration::from_millis(100);

    /// The tick of a task that blocks while idle, see `FuseDevTask::block_while_idle()`. The
    /// tick ends `wait_idle()`, and the task then lets its runtime park once, see
    /// `DeviceReadiness::park_turn()`, so the runtime runs its other tasks and submits their IO
    /// at least this often.
    const IDLE_TURN_INTERVAL: Duration = Duration::from_millis(10);

    /// How long a task that blocks while idle waits through its runtime on each tick. Long
    /// enough that the runtime gets to park before the timer fires.
    const PARK_TURN: Duration = Duration::from_micros(100);

    /// How long a task that keeps finding requests without waiting reads on before it waits, see
    /// `Exclusive::spin()`.
    const HOT_SPIN: Duration = Duration::from_micros(20);

    /// How many reads in a row must have found a request without waiting before a task spins,
    /// see `Exclusive::spin()`.
    const SPIN_AFTER: u32 = 2;

    /// How many requests a task reads in a row, without its read returning to the runtime,
    /// before it lets the runtime park with `async_runtime::park()`.
    ///
    /// With tokio, a task whose fd stays readable finds the next request at once, because
    /// `AsyncFd::ready()` takes none of the task's budget and the read doesn't wait. It would
    /// never return to its runtime, and the runtime's timers, IO and other tasks wouldn't run.
    /// With tokio-uring, every read is io_uring IO that waits for the runtime to park. Parking
    /// every 8 requests cost 32 fio jobs 3% of their IOPS on tokio.
    const PARK_AFTER_READS: u32 = 64;

    /// How many requests a task that blocks while idle reads, since its runtime last parked or
    /// it last blocked, before it lets the runtime park, see `Exclusive::park_if_due()`.
    const EXCLUSIVE_PARK_AFTER_READS: u32 = 8;

    /// A timer that becomes readable every `period`.
    fn periodic_timer(period: Duration) -> std::io::Result<TimerFd> {
        let timer = TimerFd::new(
            ClockId::CLOCK_MONOTONIC,
            TimerFlags::TFD_NONBLOCK | TimerFlags::TFD_CLOEXEC,
        )?;
        timer.set(
            Expiration::Interval(TimeSpec::from_duration(period)),
            TimerSetTimeFlags::empty(),
        )?;
        Ok(timer)
    }

    /// What makes an idle task check whether it should stop.
    enum Wakeup {
        /// A copy of the `StopSignal`'s eventfd, which stays readable once
        /// the signal stops. Each task registers a copy of its own, so tasks
        /// on one runtime can share a signal.
        Signal(EventFd),
        /// A timer that fires every `STATE_CHECK_INTERVAL`, or the tick of a
        /// task that blocks while idle.
        Timer(TimerFd),
    }

    impl Wakeup {
        fn new(signal: &StopSignal) -> std::io::Result<Self> {
            if let Some(event) = signal.event() {
                return Ok(Wakeup::Signal(event.try_clone()?));
            }
            periodic_timer(STATE_CHECK_INTERVAL).map(Wakeup::Timer)
        }
    }

    impl AsRawFd for Wakeup {
        fn as_raw_fd(&self) -> RawFd {
            match self {
                Wakeup::Signal(event) => event.as_raw_fd(),
                // nix's `TimerFd` implements only `AsFd`.
                Wakeup::Timer(timer) => timer.as_fd().as_raw_fd(),
            }
        }
    }

    /// Waits for the fuse device to become readable. The task's stop signal,
    /// or for a bare flag a periodic timer, wakes the wait, so an idle task
    /// still notices when it should stop.
    ///
    /// The timer is a timerfd on tokio's IO driver, not a tokio timer. So the
    /// runtime only needs IO enabled, and a runtime shutdown makes the wait
    /// fail instead of panic.
    struct DeviceReadiness {
        device: AsyncFd<RawFd>,
        // Without a wakeup, an idle task checks `state` once a request arrives.
        wakeup: Option<AsyncFd<Wakeup>>,
        // See `FuseDevTask::block_while_idle()`. `device` is then registered on the fd of its
        // epoll instance and drops first, and `wakeup` is the instance's tick.
        exclusive: Option<Exclusive>,
    }

    /// The epoll instance of a task that blocks while idle. It holds the tick, a copy of the
    /// stop signal's eventfd, and the device with `EPOLLEXCLUSIVE` while the task waits for
    /// requests.
    ///
    /// Every new request calls into each instance that holds the device, under the lock of the
    /// connection's request queue, and wakes the runtime of an instance that its task doesn't
    /// block on. With 8 workers that kept finding requests, that cost half the IOPS of 32 fio
    /// jobs. So a task that finds a request without waiting takes the device out, and puts it
    /// back before it waits.
    struct Exclusive {
        epoll: Epoll,
        device: RawFd,
        // The copy of the stop signal's eventfd that `epoll` holds, if the signal has one.
        _stop: Option<OwnedFd>,
        // Whether `epoll` holds the device.
        armed: Cell<bool>,
        // The requests read since the runtime last parked or the task last blocked.
        reads: Cell<u32>,
        // The reads in a row that found a request without waiting.
        streak: Cell<u32>,
    }

    /// What ended `Exclusive::wait_idle()`.
    enum IdleWake {
        Device,
        Tick,
        Stop,
    }

    impl Exclusive {
        fn device(&self) -> BorrowedFd<'_> {
            // SAFETY: the task owns the device fd and keeps it open while it polls this.
            unsafe { BorrowedFd::borrow_raw(self.device) }
        }

        /// Put the device into the epoll instance before the task waits. The instance then
        /// reports a request that arrived since the last read.
        fn arm(&self) -> std::io::Result<()> {
            self.streak.set(0);
            if !self.armed.get() {
                // Edge-triggered, because a level-triggered instance polls the device again on
                // every wait, under the lock of the connection's request queue.
                self.epoll.add(
                    self.device(),
                    EpollEvent::new(
                        EpollFlags::EPOLLIN | EpollFlags::EPOLLEXCLUSIVE | EpollFlags::EPOLLET,
                        DEVICE_EVENT,
                    ),
                )?;
                self.armed.set(true);
            }
            Ok(())
        }

        /// Take the device out of the epoll instance after a read that found a request without
        /// waiting.
        ///
        /// The task has the request already, so if that fails, the device stays in.
        fn found_request(&self) {
            self.streak.set(self.streak.get() + 1);
            if self.armed.get() {
                match self.epoll.delete(self.device()) {
                    Ok(()) => self.armed.set(false),
                    Err(e) => warn!(
                        "can't take the fuse device out of the epoll instance: {}",
                        e
                    ),
                }
            }
        }

        /// Keep reading for `HOT_SPIN` after a read that found nothing, if the last
        /// `SPIN_AFTER` reads found a request without waiting. Requests that come that fast
        /// would otherwise cost a wakeup each. After a single such read, the spin cost the IOPS
        /// of one job a fifth in some runs, because each request still woke a blocked task.
        fn spin(&self, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
            if self.streak.get() < SPIN_AFTER {
                return Ok(None);
            }
            let start = Instant::now();
            loop {
                // Each read takes the lock of the request queue.
                for _ in 0..64 {
                    std::hint::spin_loop();
                }
                if let Some(len) = read_now(self.device(), buf)? {
                    return Ok(Some(len));
                }
                if start.elapsed() >= HOT_SPIN {
                    return Ok(None);
                }
            }
        }

        /// Block the thread until the epoll instance reports the device, `tick` or the stop
        /// signal, and return what ended the wait, the device before the others.
        fn wait_idle(&self, tick: Option<&TimerFd>) -> std::io::Result<IdleWake> {
            let mut events = [EpollEvent::empty(); 3];
            loop {
                match self.epoll.wait(&mut events, EpollTimeout::NONE) {
                    Ok(n) => {
                        let reported = |data| events[..n].iter().any(|e| e.data() == data);
                        let device = reported(DEVICE_EVENT);
                        // The runtime runs at least on every tick while the task blocks. A tick
                        // that comes with a request doesn't end the wait, so the runtime parks
                        // before the next read instead. The instance holds the tick
                        // level-triggered, so consume its expirations, or the next wait would
                        // end on them at once and park the runtime a second time.
                        let park = device && reported(TICK_EVENT);
                        if let Some(tick) = tick.filter(|_| park) {
                            // EAGAIN means there were none.
                            let _ = tick.wait();
                        }
                        self.reads
                            .set(if park { EXCLUSIVE_PARK_AFTER_READS } else { 0 });
                        return Ok(if device {
                            IdleWake::Device
                        } else if reported(STOP_EVENT) {
                            IdleWake::Stop
                        } else {
                            IdleWake::Tick
                        });
                    }
                    Err(Errno::EINTR) => continue,
                    Err(e) => return Err(e.into()),
                }
            }
        }

        /// Count a request that the task read.
        fn read_one(&self) {
            self.reads.set(self.reads.get() + 1);
        }

        /// Let the runtime park after `EXCLUSIVE_PARK_AFTER_READS` requests read since it last parked or
        /// the task last blocked.
        ///
        /// Otherwise a task that keeps finding requests never lets its runtime park, so its
        /// timers and IO don't run, and tokio-uring doesn't submit the IO of the requests in
        /// flight. A wait through the runtime doesn't count, because it can end on readiness
        /// that the runtime saw before without parking.
        async fn park_if_due(&self) {
            if self.reads.get() >= EXCLUSIVE_PARK_AFTER_READS {
                async_runtime::park().await;
                // Not before, because the serving loop can drop the read while it waits.
                self.reads.set(0);
            }
        }
    }

    impl DeviceReadiness {
        fn new(device: AsyncFd<RawFd>, signal: &StopSignal) -> Self {
            // SAFETY: the `AsyncFd` owns the wakeup, so its fd stays open for
            // as long as the registration.
            let wakeup = Wakeup::new(signal).and_then(|wakeup| {
                unsafe { AsyncFd::register_with_interest(wakeup, Interest::READABLE) }
                    .map_err(std::io::Error::from)
            });
            let wakeup = wakeup
                .inspect_err(|e| {
                    warn!(
                        "can't wait for the stop signal or timer of an idle task, checking its state when a request arrives instead: {}",
                        e
                    )
                })
                .ok();
            DeviceReadiness {
                device,
                wakeup,
                exclusive: None,
            }
        }

        /// Wait for the device `fd` through an epoll instance that holds it with
        /// `EPOLLEXCLUSIVE`.
        ///
        /// The instance holds the tick and a copy of the eventfd of `signal` too. Fails if it
        /// can't create the tick, which an idle task needs to let its runtime run, see
        /// `IDLE_TURN_INTERVAL`.
        fn exclusive(fd: RawFd, signal: &StopSignal) -> std::io::Result<Self> {
            let tick = periodic_timer(IDLE_TURN_INTERVAL)?;
            let stop = signal
                .event()
                // SAFETY: the signal's eventfd stays open while the signal lives, and the
                // borrow only lasts for the duplicate.
                .map(|event| {
                    unsafe { BorrowedFd::borrow_raw(event.as_raw_fd()) }.try_clone_to_owned()
                })
                .transpose()?;
            let exclusive = Exclusive {
                epoll: Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC)?,
                device: fd,
                _stop: stop,
                armed: Cell::new(false),
                reads: Cell::new(0),
                streak: Cell::new(0),
            };
            exclusive.arm()?;
            exclusive.epoll.add(
                tick.as_fd(),
                EpollEvent::new(EpollFlags::EPOLLIN, TICK_EVENT),
            )?;
            if let Some(stop) = &exclusive._stop {
                // A stopped signal stays readable, so add it edge-triggered. Then a signal
                // whose flag was reset since ends no more waits.
                exclusive.epoll.add(
                    stop,
                    EpollEvent::new(EpollFlags::EPOLLIN | EpollFlags::EPOLLET, STOP_EVENT),
                )?;
            }
            // SAFETY: the `AsyncFd` owns the tick, so its fd stays open for as long as the
            // registration.
            let tick = unsafe {
                AsyncFd::register_with_interest(Wakeup::Timer(tick), Interest::READABLE)
            }?;
            // SAFETY: `device` drops before `exclusive`, which closes the epoll fd.
            let registered = unsafe {
                AsyncFd::register_with_interest(exclusive.epoll.0.as_raw_fd(), Interest::READABLE)
            }?;
            Ok(DeviceReadiness {
                device: registered,
                wakeup: Some(tick),
                exclusive: Some(exclusive),
            })
        }

        /// The tick of a task that blocks while idle.
        fn tick(&self) -> Option<&TimerFd> {
            match self.wakeup.as_ref()?.get_ref() {
                Wakeup::Timer(timer) => Some(timer),
                Wakeup::Signal(_) => None,
            }
        }

        /// Wait until the device is readable and return its guard, or return
        /// `None` when the stop signal or the timer wakes the task.
        async fn wait(
            &self,
            state: &StopSignal,
        ) -> std::io::Result<Option<AsyncFdReadyGuard<'_, RawFd>>> {
            let device = self.device.ready(Interest::READABLE | Interest::ERROR);
            let Some(wakeup) = &self.wakeup else {
                return device.await.map(Some);
            };
            tokio::select! {
                biased;
                ready = device => ready.map(Some),
                ready = wakeup.ready(Interest::READABLE) => {
                    let mut guard = ready?;
                    match wakeup.get_ref() {
                        // Consume the expirations, so the next one makes the
                        // timer readable again. EAGAIN means there were none.
                        Wakeup::Timer(timer) => {
                            let _ = timer.wait();
                            guard.clear_ready();
                        }
                        // A stopped signal stays readable. If its flag was
                        // reset since, wait for the next stop() instead of
                        // spinning on the old one.
                        Wakeup::Signal(_) if !state.is_stopped() => guard.clear_ready(),
                        Wakeup::Signal(_) => {}
                    }
                    Ok(None)
                }
            }
        }

        /// Let the runtime park once. Arm the tick to fire `PARK_TURN` from now, and wait for it
        /// through the runtime.
        ///
        /// A runtime with a pending wakeup polls its driver without parking, and tokio-uring
        /// submits the IO that tasks start only when its runtime parks. Waiting for a tick that
        /// fired already ends without a park, so this waits for a tick it arms itself.
        async fn park_turn(&self) -> std::io::Result<()> {
            let (Some(wakeup), Some(tick)) = (&self.wakeup, self.tick()) else {
                return Ok(());
            };
            // Arming the timer drops the expirations it hasn't reported yet.
            tick.set(
                Expiration::IntervalDelayed(
                    TimeSpec::from_duration(PARK_TURN),
                    TimeSpec::from_duration(IDLE_TURN_INTERVAL),
                ),
                TimerSetTimeFlags::empty(),
            )?;
            // The runtime can still report the tick that ended `wait_idle()`, which has no
            // expirations left.
            loop {
                let mut guard = wakeup.ready(Interest::READABLE).await?;
                // Consume the expirations, so the next one makes the timer readable again.
                // EAGAIN means there were none.
                let expired = tick.wait().is_ok();
                guard.clear_ready();
                if expired {
                    return Ok(());
                }
            }
        }
    }

    /// Read one request from the fuse device into `buf`, or return `None` if
    /// `state` asks the task to stop while it waits for one.
    ///
    /// With `ready`, wait until the fd is readable before each read, and wait
    /// again if the read finds no request. With `park`, let the runtime park
    /// first, see `PARK_AFTER_READS`. A task that blocks while idle reads with
    /// `read_exclusive()` instead.
    ///
    /// The read owns the buffer until it completes. A drop of this future
    /// during the read leaves `buf` empty, and tokio-uring frees the buffer
    /// later.
    async fn read_request(
        file: &AsyncFile,
        ready: Option<&DeviceReadiness>,
        state: &StopSignal,
        buf: &mut Vec<u8>,
        park: bool,
        idle: bool,
    ) -> std::io::Result<Option<usize>> {
        if let Some(ready) = ready {
            if let Some(exclusive) = &ready.exclusive {
                return read_exclusive(ready, exclusive, state, buf, idle).await;
            }
        }
        if park {
            async_runtime::park().await;
        }
        loop {
            let guard = match ready {
                Some(ready) => loop {
                    if state.is_stopped() {
                        return Ok(None);
                    }
                    if let Some(guard) = ready.wait(state).await? {
                        break Some(guard);
                    }
                },
                None => None,
            };
            let (result, read_buf) = file.async_read_owned_at(std::mem::take(buf), 0).await;
            *buf = read_buf;
            match (result, guard) {
                (Err(e), Some(mut guard)) if e.raw_os_error() == Some(libc::EAGAIN) => {
                    guard.clear_ready();
                }
                (result, _) => return result.map(Some),
            }
        }
    }

    /// `read_request()` for a task that blocks while idle. With nothing in flight (`idle`), it
    /// blocks in `wait_idle()`, and otherwise it waits for the epoll instance through the
    /// runtime.
    async fn read_exclusive(
        ready: &DeviceReadiness,
        exclusive: &Exclusive,
        state: &StopSignal,
        buf: &mut [u8],
        idle: bool,
    ) -> std::io::Result<Option<usize>> {
        exclusive.park_if_due().await;
        // The epoll instance is edge-triggered, so it reports requests that arrive together
        // once. Read before every wait, idle or not.
        let found = match read_now(exclusive.device(), buf)? {
            None if idle => exclusive.spin(buf)?,
            found => found,
        };
        if let Some(len) = found {
            exclusive.read_one();
            exclusive.found_request();
            return Ok(Some(len));
        }
        exclusive.arm()?;
        if idle {
            // Let the tasks that the last requests woke run before the task blocks. This polls
            // the runtime's IO driver but doesn't park the runtime, so tokio-uring submits
            // their IO only on the next tick. Parking would cost a quarter to a third of the
            // IOPS of one job.
            tokio::task::yield_now().await;
        }
        loop {
            if state.is_stopped() {
                return Ok(None);
            }
            let guard = if idle {
                match exclusive.wait_idle(ready.tick())? {
                    IdleWake::Device => None,
                    IdleWake::Stop => continue,
                    IdleWake::Tick => {
                        // Let the runtime park once, then run the tasks that woke meanwhile
                        // before blocking again. Waiting for the device too would end at once
                        // on the readiness that the tick left on the epoll instance, and cost
                        // a read that finds nothing.
                        ready.park_turn().await?;
                        tokio::task::yield_now().await;
                        continue;
                    }
                }
            } else {
                match ready.wait(state).await? {
                    None => continue,
                    guard => guard,
                }
            };
            if let Some(len) = read_now(exclusive.device(), buf)? {
                exclusive.read_one();
                return Ok(Some(len));
            }
            match guard {
                Some(mut guard) => guard.clear_ready(),
                // Another task read the request that ended `wait_idle()`. A tick that came with
                // it is still due, so let the runtime park before blocking again.
                None => exclusive.park_if_due().await,
            }
        }
    }

    /// The `epoll_event` data of the device, the tick and the stop signal in the instance of
    /// `DeviceReadiness::exclusive()`.
    const DEVICE_EVENT: u64 = 0;
    const TICK_EVENT: u64 = 1;
    const STOP_EVENT: u64 = 2;

    /// Read a request from the nonblocking device `fd`, or return `None` if none is pending.
    fn read_now(fd: BorrowedFd<'_>, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
        match nix::unistd::read(fd, buf) {
            Ok(len) => Ok(Some(len)),
            Err(Errno::EAGAIN) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Serve one fuse request and send its reply, then hand the request buffer
    /// back for reuse.
    async fn handle_request<F: AsyncFileSystem + Sync>(
        server: Arc<Server<F>>,
        fd: RawFd,
        mut req: RequestBuf,
        len: usize,
    ) -> RequestBuf {
        let buf = &mut req.buf;
        // ###############################################
        // Note: it's a heavy hack to reuse the same underlying data
        // buffer for both Reader and Writer, in order to reduce memory
        // consumption. Here we assume Reader won't be used anymore once
        // we start to write to the Writer. To get rid of this hack,
        // just allocate a dedicated data buffer for Writer.
        let buf_slice = unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr(), buf.len()) };
        // Reader::from_fuse_buffer() and FuseDevWriter::new() should always
        // return success.
        let reader = Reader::<()>::from_fuse_buffer(FuseBuf::new(&mut buf[0..len])).unwrap();
        let writer = FuseDevWriter::<()>::new(fd, buf_slice)
            .unwrap()
            .with_splice(req.splice.as_ref());
        let result = unsafe {
            server
                .async_handle_message(reader, writer, None, None)
                .await
        };

        if let Err(e) = result {
            // TODO: error handling
            error!("failed to handle fuse request, {}", e);
        }

        req
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::atomic::AtomicBool;

        use fuse_backend_core::async_runtime;
        use fuse_backend_passthrough::{Config, PassthroughFs};
        use vmm_sys_util::tempfile::TempFile;

        #[test]
        fn test_fuse_dev_task_quiesce() {
            let fs = PassthroughFs::<()>::new(Config::default()).unwrap();
            let server = Arc::new(Server::new(fs));
            let file = TempFile::new().unwrap().into_file();
            // Quiesce the task before polling it, so poll_handler() returns
            // right away without touching the device.
            let state = Arc::new(AtomicBool::new(true));
            let mut task = FuseDevTask::new(0x1000, file, server, state);

            async_runtime::block_on(task.poll_handler());
        }

        #[test]
        fn test_buffer_pool_limit() {
            let pool = BufferPool::new(64, 2, false);

            // Buffers are allocated lazily up to the limit...
            let buf1 = pool.try_acquire().unwrap();
            let buf2 = pool.try_acquire().unwrap();
            assert_eq!(buf1.buf.len(), 64);
            assert_eq!(buf2.buf.len(), 64);
            // ...and no buffer is handed out once the limit is reached.
            assert!(pool.try_acquire().is_none());

            // Released buffers are recycled and handed out again.
            pool.release(buf1);
            let buf3 = pool.try_acquire().unwrap();
            assert!(pool.try_acquire().is_none());
            pool.release(buf2);
            pool.release(buf3);
        }

        #[test]
        fn test_buffer_pool_splice_pipes() {
            assert!(BufferPool::new(64, 1, false)
                .try_acquire()
                .unwrap()
                .splice
                .is_none());

            // Each buffer gets pipes of its own, and they stop splicing together.
            let pool = BufferPool::new(64, 2, true);
            let a = pool.try_acquire().unwrap().splice.unwrap();
            let b = pool.try_acquire().unwrap().splice.unwrap();
            assert!(a.enabled() && b.enabled());
            let sock = std::os::unix::net::UnixStream::pair().unwrap().0;
            assert_eq!(a.splice_in(sock.as_raw_fd(), 0, 0x1000), None);
            assert!(!a.enabled() && !b.enabled());
        }

        #[test]
        fn test_buffer_pool_leaked_buffer() {
            let pool = BufferPool::new(64, 1, false);
            let buf = pool.try_acquire().unwrap();
            assert!(pool.try_acquire().is_none());

            // A leaked buffer makes room for a new one.
            std::mem::forget(buf);
            pool.release_done(None);
            let buf = pool.try_acquire().unwrap();
            assert!(pool.try_acquire().is_none());

            // A returned buffer goes back to the free list.
            pool.release_done(Some(buf));
            assert!(pool.try_acquire().is_some());
            assert!(pool.try_acquire().is_none());
        }

        #[test]
        fn test_buffer_pool_limit_clamped() {
            // A limit of 0 is clamped to 1, so the pool stays usable.
            let pool = BufferPool::new(64, 0, false);
            assert!(pool.try_acquire().is_some());
            assert!(pool.try_acquire().is_none());
        }
    }
}
