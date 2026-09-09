# OpenBSD Technical Patterns and Anti-Patterns

Use these as concrete things to check for. They are common sources of real bugs
in OpenBSD kernel changes. None of them is automatically a bug; verify against
the actual code path before reporting.

## Allocation
- `pool_get(9)` / `malloc(9)` with `PR_WAITOK`/`M_WAITOK` can sleep. They must
  not be called while holding a `mutex(9)`, at raised `spl`, or in an interrupt
  context. Use `PR_NOWAIT`/`M_NOWAIT` there and check for a NULL return.
- Every `pool_get`/`malloc` needs a matching `pool_put`/`free` on every path,
  including error paths. `free(9)` requires the same size argument that was
  allocated (or 0).
- A `PR_NOWAIT`/`M_NOWAIT` allocation can fail; dereferencing the result without
  a NULL check is a bug.

## Locking and interrupts
- `splnet()`/`splbio()`/etc. must be balanced by `splx()` on every path,
  including early returns and error gotos. Saving the old IPL and restoring it
  is mandatory.
- Do not sleep (tsleep/msleep, pool_get with PR_WAITOK, rw_enter on a contended
  lock) while holding a `mutex(9)` or at raised spl.
- Network stack code generally runs under `NET_LOCK()`. Check whether a function
  asserts or requires it (`NET_ASSERT_LOCKED()`), and whether new call paths
  honour it.
- Watch for lock-order inversions between subsystem locks (e.g. `NET_LOCK` vs a
  driver mutex) that can deadlock.

## mbufs and the network stack
- After `m_freem(9)`/`m_free(9)` the mbuf must not be touched again; returning a
  freed mbuf to a caller that frees it again is a double free.
- `m_pullup(9)` may free and reallocate; the old pointer is invalid afterwards
  and the return value must be re-checked for NULL.
- Validate lengths before reading packet data; do not trust on-wire lengths.

## Deferred work and teardown
- A `timeout_add(9)` must be cancelled with `timeout_del(9)` (and barrier where
  needed) before the memory it references is freed; likewise `task_add(9)` work.
- On detach, ensure interrupts are disabled and pending callbacks are drained
  before freeing softc state.
- A `task_add(9)` callback runs on a shared taskq thread and must not yield the
  CPU with `sched_pause(yield)` (or otherwise block for long) to spread out
  long-running work: doing so stalls every other task queued on the same taskq.
  A callback with more work to do should return and re-enqueue itself with
  `task_add(9)` so other tasks on the queue can make progress between runs.

## SMR (safe memory reclamation)
- An object unlinked from an SMR-protected list/pointer is still reachable by
  readers until a grace period passes. Freeing it directly is a use-after-free;
  the free must be deferred with `smr_call(9)` or gated behind `smr_barrier(9)`.
- SMR read-side critical sections (`smr_read_enter`/`smr_read_leave`) must not
  sleep or block; flag a sleeping call (tsleep/msleep, `rw_enter`, `PR_WAITOK`)
  inside one.
- The `smr_barrier()`/`smr_flush()` caller must not hold a lock that a pending
  SMR callback needs, or it deadlocks. Reads of an SMR-protected pointer must go
  through `SMR_PTR_GET()` (or a write-side lock + `SMR_PTR_GET_LOCKED()`), not a
  bare dereference.

## Userland boundary
- `copyin(9)`/`copyout(9)`/`copyinstr(9)` return errors that must be checked.
- Zero-fill structures copied out to userspace so padding does not leak kernel
  memory.
- Integer overflow in size/length math before an allocation or copy is a
  classic vulnerability; check for it.

## Byte order and device access
- Use the byte-order helpers (`letoh32`, `htole16`, `betoh32`, ...) for
  on-device and on-wire data; a missing conversion is a bug on the other endian.
- `bus_dmamap_sync(9)` must be called with the correct direction before/after
  DMA; missing syncs cause stale data on non-coherent platforms.
