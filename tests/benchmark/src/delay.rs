// Copyright 2026 Alibaba Cloud. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! A file system that forwards to the benchmark's `Vfs` and waits before
//! each LOOKUP and READ, to stand in for a backend that waits on a remote
//! store. The synchronous handlers block their thread for the delay, and
//! the asynchronous ones sleep without blocking it, so a transport that
//! keeps several requests in flight overlaps the waits.
//!
//! It forwards only the requests the benchmark workloads send. The others
//! keep the `ENOSYS` defaults.

use std::ffi::CStr;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use fuse_backend_rs::abi::fuse_abi::{
    stat64, statvfs64, CreateIn, FsOptions, OpenOptions, SetattrValid,
};
use fuse_backend_rs::api::filesystem::{
    AsyncFileSystem, AsyncZeroCopyReader, AsyncZeroCopyWriter, Context, DirEntry, Entry,
    FileSystem, ZeroCopyReader, ZeroCopyWriter,
};
use fuse_backend_rs::api::Vfs;

pub struct DelayFs {
    inner: Arc<Vfs>,
    delay: Duration,
}

impl DelayFs {
    pub fn new(inner: Arc<Vfs>, delay: Duration) -> Self {
        DelayFs { inner, delay }
    }

    fn wait(&self) {
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
    }

    async fn async_wait(&self) {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
    }
}

type Inode = <Vfs as FileSystem>::Inode;
type Handle = <Vfs as FileSystem>::Handle;

impl FileSystem for DelayFs {
    type Inode = Inode;
    type Handle = Handle;

    fn init(&self, capable: FsOptions) -> io::Result<FsOptions> {
        self.inner.init(capable)
    }

    fn destroy(&self) {
        self.inner.destroy()
    }

    fn lookup(&self, ctx: &Context, parent: Inode, name: &CStr) -> io::Result<Entry> {
        self.wait();
        self.inner.lookup(ctx, parent, name)
    }

    fn forget(&self, ctx: &Context, inode: Inode, count: u64) {
        self.inner.forget(ctx, inode, count)
    }

    fn batch_forget(&self, ctx: &Context, requests: Vec<(Inode, u64)>) {
        self.inner.batch_forget(ctx, requests)
    }

    fn getattr(
        &self,
        ctx: &Context,
        inode: Inode,
        handle: Option<Handle>,
    ) -> io::Result<(stat64, Duration)> {
        self.inner.getattr(ctx, inode, handle)
    }

    fn setattr(
        &self,
        ctx: &Context,
        inode: Inode,
        attr: stat64,
        handle: Option<Handle>,
        valid: SetattrValid,
    ) -> io::Result<(stat64, Duration)> {
        self.inner.setattr(ctx, inode, attr, handle, valid)
    }

    fn open(
        &self,
        ctx: &Context,
        inode: Inode,
        flags: u32,
        fuse_flags: u32,
    ) -> io::Result<(Option<Handle>, OpenOptions, Option<u32>)> {
        self.inner.open(ctx, inode, flags, fuse_flags)
    }

    #[allow(clippy::too_many_arguments)]
    fn release(
        &self,
        ctx: &Context,
        inode: Inode,
        flags: u32,
        handle: Handle,
        flush: bool,
        flock_release: bool,
        lock_owner: Option<u64>,
    ) -> io::Result<()> {
        self.inner
            .release(ctx, inode, flags, handle, flush, flock_release, lock_owner)
    }

    fn create(
        &self,
        ctx: &Context,
        parent: Inode,
        name: &CStr,
        args: CreateIn,
    ) -> io::Result<(Entry, Option<Handle>, OpenOptions, Option<u32>)> {
        self.inner.create(ctx, parent, name, args)
    }

    #[allow(clippy::too_many_arguments)]
    fn read(
        &self,
        ctx: &Context,
        inode: Inode,
        handle: Handle,
        w: &mut dyn ZeroCopyWriter,
        size: u32,
        offset: u64,
        lock_owner: Option<u64>,
        flags: u32,
    ) -> io::Result<usize> {
        self.wait();
        self.inner
            .read(ctx, inode, handle, w, size, offset, lock_owner, flags)
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        ctx: &Context,
        inode: Inode,
        handle: Handle,
        r: &mut dyn ZeroCopyReader,
        size: u32,
        offset: u64,
        lock_owner: Option<u64>,
        delayed_write: bool,
        flags: u32,
        fuse_flags: u32,
    ) -> io::Result<usize> {
        self.inner.write(
            ctx,
            inode,
            handle,
            r,
            size,
            offset,
            lock_owner,
            delayed_write,
            flags,
            fuse_flags,
        )
    }

    fn flush(
        &self,
        ctx: &Context,
        inode: Inode,
        handle: Handle,
        lock_owner: u64,
    ) -> io::Result<()> {
        self.inner.flush(ctx, inode, handle, lock_owner)
    }

    fn fsync(&self, ctx: &Context, inode: Inode, datasync: bool, handle: Handle) -> io::Result<()> {
        self.inner.fsync(ctx, inode, datasync, handle)
    }

    fn fallocate(
        &self,
        ctx: &Context,
        inode: Inode,
        handle: Handle,
        mode: u32,
        offset: u64,
        length: u64,
    ) -> io::Result<()> {
        self.inner
            .fallocate(ctx, inode, handle, mode, offset, length)
    }

    fn statfs(&self, ctx: &Context, inode: Inode) -> io::Result<statvfs64> {
        self.inner.statfs(ctx, inode)
    }

    fn unlink(&self, ctx: &Context, parent: Inode, name: &CStr) -> io::Result<()> {
        self.inner.unlink(ctx, parent, name)
    }

    fn access(&self, ctx: &Context, inode: Inode, mask: u32) -> io::Result<()> {
        self.inner.access(ctx, inode, mask)
    }

    fn opendir(
        &self,
        ctx: &Context,
        inode: Inode,
        flags: u32,
    ) -> io::Result<(Option<Handle>, OpenOptions)> {
        self.inner.opendir(ctx, inode, flags)
    }

    fn readdir(
        &self,
        ctx: &Context,
        inode: Inode,
        handle: Handle,
        size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(DirEntry) -> io::Result<usize>,
    ) -> io::Result<()> {
        self.inner
            .readdir(ctx, inode, handle, size, offset, add_entry)
    }

    fn readdirplus(
        &self,
        ctx: &Context,
        inode: Inode,
        handle: Handle,
        size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(DirEntry, Entry) -> io::Result<usize>,
    ) -> io::Result<()> {
        self.inner
            .readdirplus(ctx, inode, handle, size, offset, add_entry)
    }

    fn releasedir(
        &self,
        ctx: &Context,
        inode: Inode,
        flags: u32,
        handle: Handle,
    ) -> io::Result<()> {
        self.inner.releasedir(ctx, inode, flags, handle)
    }

    fn fsyncdir(
        &self,
        ctx: &Context,
        inode: Inode,
        datasync: bool,
        handle: Handle,
    ) -> io::Result<()> {
        self.inner.fsyncdir(ctx, inode, datasync, handle)
    }
}

#[async_trait]
impl AsyncFileSystem for DelayFs {
    async fn async_lookup(&self, ctx: &Context, parent: Inode, name: &CStr) -> io::Result<Entry> {
        self.async_wait().await;
        self.inner.async_lookup(ctx, parent, name).await
    }

    async fn async_getattr(
        &self,
        ctx: &Context,
        inode: Inode,
        handle: Option<Handle>,
    ) -> io::Result<(stat64, Duration)> {
        self.inner.async_getattr(ctx, inode, handle).await
    }

    async fn async_setattr(
        &self,
        ctx: &Context,
        inode: Inode,
        attr: stat64,
        handle: Option<Handle>,
        valid: SetattrValid,
    ) -> io::Result<(stat64, Duration)> {
        self.inner
            .async_setattr(ctx, inode, attr, handle, valid)
            .await
    }

    async fn async_open(
        &self,
        ctx: &Context,
        inode: Inode,
        flags: u32,
        fuse_flags: u32,
    ) -> io::Result<(Option<Handle>, OpenOptions)> {
        self.inner.async_open(ctx, inode, flags, fuse_flags).await
    }

    async fn async_create(
        &self,
        ctx: &Context,
        parent: Inode,
        name: &CStr,
        args: CreateIn,
    ) -> io::Result<(Entry, Option<Handle>, OpenOptions)> {
        self.inner.async_create(ctx, parent, name, args).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn async_read(
        &self,
        ctx: &Context,
        inode: Inode,
        handle: Handle,
        w: &mut (dyn AsyncZeroCopyWriter + Send),
        size: u32,
        offset: u64,
        lock_owner: Option<u64>,
        flags: u32,
    ) -> io::Result<usize> {
        self.async_wait().await;
        self.inner
            .async_read(ctx, inode, handle, w, size, offset, lock_owner, flags)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn async_write(
        &self,
        ctx: &Context,
        inode: Inode,
        handle: Handle,
        r: &mut (dyn AsyncZeroCopyReader + Send),
        size: u32,
        offset: u64,
        lock_owner: Option<u64>,
        delayed_write: bool,
        flags: u32,
        fuse_flags: u32,
    ) -> io::Result<usize> {
        self.inner
            .async_write(
                ctx,
                inode,
                handle,
                r,
                size,
                offset,
                lock_owner,
                delayed_write,
                flags,
                fuse_flags,
            )
            .await
    }

    async fn async_fsync(
        &self,
        ctx: &Context,
        inode: Inode,
        datasync: bool,
        handle: Handle,
    ) -> io::Result<()> {
        self.inner.async_fsync(ctx, inode, datasync, handle).await
    }

    async fn async_fallocate(
        &self,
        ctx: &Context,
        inode: Inode,
        handle: Handle,
        mode: u32,
        offset: u64,
        length: u64,
    ) -> io::Result<()> {
        self.inner
            .async_fallocate(ctx, inode, handle, mode, offset, length)
            .await
    }

    async fn async_fsyncdir(
        &self,
        ctx: &Context,
        inode: Inode,
        datasync: bool,
        handle: Handle,
    ) -> io::Result<()> {
        self.inner
            .async_fsyncdir(ctx, inode, datasync, handle)
            .await
    }
}
