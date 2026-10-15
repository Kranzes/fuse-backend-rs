# FUSE-over-io_uring Performance Analysis

This document analyzes the performance characteristics of the FUSE-over-io_uring
transport implementation, including known regressions and upstream optimization
progress.

## Benchmark Summary

### Metadata Operations: Significant Wins

| Workload | 1-thread | 8-thread |
|----------|----------|----------|
| filecreate | +150-216% | +276-277% |
| filedelete | -15 to +8% | +41-46% |
| randread-4k | +1-3% | +63% |
| randwrite-4k | +5-16% | -22 to -24% |

The uring transport excels at metadata operations because it eliminates the
per-request syscall overhead (read/write on `/dev/fuse`). With 8 threads,
filecreate achieves **3.7x** throughput compared to sync.

### Sequential I/O: Known Regression

These numbers predate the queue ownership fix described below.

| Workload | 1-thread | 8-thread |
|----------|----------|----------|
| seqread | -44 to +5% | **-37%** |
| seqwrite | -1 to -15% | **-28 to -34%** |

Most of the seqwrite regression came from how the workers shared the queues,
which is fixed now. "Queue ownership" below explains it. The rest of the
sequential I/O gap may come from the upstream changes described in "Root
Cause Analysis".

## Queue ownership

The kernel puts each request on the queue of the CPU that sends it. With the
writeback cache, the flusher thread sends the WRITEs, from one CPU at a time.
The workers used to own whole queues, so the worker that owned that CPU's
queue served every WRITE while the others waited for requests. Over a few
seconds the flusher moves between CPUs, so the per-thread CPU time looked
evenly spread, but at any moment one worker was writing.

Profiling a 4-job 1 MiB seqwrite through the writeback cache with 4 workers
showed the uring workers spending most of their samples in
`io_cqring_wait`, waiting for requests, while the sync threads were busy in
`ext4_buffered_write_iter`. Now the entries of every queue are spread over
the workers, and the kernel hands a request to any free entry of its queue,
so a queue that gets more requests than one worker can serve reaches the
other workers too. In that profile run, seqwrite went from
3.99 GiB/s to 5.79 GiB/s, against 6.83 GiB/s for sync. The workers now
contend on the backing file's inode lock, as the sync threads do.

## Root Cause Analysis

### What It's NOT

1. **Not memcpy overhead**: The scatter Reader/Writer optimization eliminates
   all userspace copies. Both classic and uring paths use identical kernel-side
   copy functions (`fuse_copy_args`, `fuse_copy_out_args`).

2. **Not missing readahead**: Readahead is handled at the VFS layer
   (`fs/fuse/file.c:fuse_readahead`), completely independent of the transport.
   Both classic and uring paths benefit equally.

3. **Not pipeline depth**: Increasing `entries_per_queue` from 4 to 16 had
   zero impact on sequential I/O performance, when each worker owned whole
   queues.

### What It Is

The regression stems from two optimizations that were present in RFC v1/v2
but removed in later versions pending upstream subsystem changes:

#### 1. `__wake_on_current_cpu` (Scheduler Dependency)

RFC v1 had an optimization to wake the waiting thread on the **same CPU core**
where the request was processed, avoiding cross-core cache line bouncing.

From Bernd Schubert's v3 changelog:
> "Removed the `__wake_on_current_cpu` optimization (for now as that needs to
> go through another subsystem/tree), removing it means a significant
> performance drop"

This requires scheduler changes that haven't been accepted upstream yet.

#### 2. Direct Ring Submission (Locking Issues)

RFC v1/v2 submitted requests directly from the task that created them,
avoiding the `io_uring_cmd_complete_in_task()` workqueue deferral.

This was removed due to teardown race conditions and lock ordering violations.
The current implementation defers to a workqueue, adding latency.

### RFC v1 Benchmark Comparison

Bernd Schubert's RFC v1 benchmarks (2024, with optimizations present) showed:

| Workload | /dev/fuse | uring | Gain |
|----------|-----------|-------|------|
| 128K paged reads (1 job) | 1117 MB/s | 1921 MB/s | **1.72x** |
| 128K paged reads (8 jobs) | 6273 MB/s | 10855 MB/s | **1.73x** |
| 1024K DIO reads (4 jobs) | 3823 MB/s | 15022 MB/s | **3.58x** |

Current kernels (6.14-7.2) lack these optimizations, resulting in the
regression we observe.

## Upstream Optimization Progress

### Linux 7.3: Buffer Pools + Zero-Copy (Merged)

**Author**: Joanne Koong  
**Status**: Merged for Linux 7.3 (August 2026)

- **Buffer pools**: Kernel-managed shared memory pool instead of per-entry
  dedicated buffers. Reduces memory overhead significantly.

- **Zero-copy**: Server can directly access client pages (pinned user pages
  for DIO, page cache folios for buffered I/O) without intermediary copies.
  Requires `CAP_SYS_ADMIN`.

Benchmark results from Joanne's patch series (bs=1M):

| Workload | Before | After | Gain |
|----------|--------|-------|------|
| direct randreads | ~2100 MB/s | ~2600 MB/s | +20% |
| buffered randreads | ~1900 MB/s | ~2400 MB/s | +25% |
| buffered randwrites | 950 MB/s | 1050 MB/s | +10% |

### Pending: `__wake_on_current_cpu`

**Status**: Blocked on scheduler subsystem changes

This optimization will eliminate cross-core wakeup latency for sequential I/O.

### Pending: Direct Ring Submission

**Status**: Will be re-submitted after core work stabilizes

This will eliminate the workqueue deferral overhead.

## Implementation Notes

### Current State

This implementation targets the stable FUSE_URING protocol 7.42 (kernel 6.14+):

- Scatter Reader/Writer eliminates all userspace memcpy
- Per-CPU queue model with configurable entries_per_queue
- Fallback to classic `/dev/fuse` for notifications/interrupts

### Future Work

When Linux 7.3+ is available on target systems:

1. **Buffer pool registration**: Reduce memory overhead by sharing a contiguous
   pool across requests instead of per-entry buffers.

2. **Zero-copy mode**: Enable direct page access (requires CAP_SYS_ADMIN) to
   eliminate kernel-side copies entirely.

### Tuning Recommendations

- `entries_per_queue`: Default is 4. The entries of each queue are spread
  over the workers, so it also caps how many workers can serve one queue.
  With the default, a busy queue reaches at most 4 workers.

- Thread count: The per-CPU queue model means optimal performance when
  worker threads match available CPU cores.

## References

- Bernd Schubert's fuse-over-io_uring patch series:
  https://lore.kernel.org/io-uring/20241209-fuse-uring-for-6-10-rfc4-v8-0-d9f9f2642be3@ddn.com/

- Joanne Koong's buffer pools + zero-copy:
  https://lwn.net/Articles/1049130/

- Kernel documentation:
  https://docs.kernel.org/filesystems/fuse/fuse-io-uring.html
