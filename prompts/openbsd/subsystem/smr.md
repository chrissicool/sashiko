# OpenBSD SMR (Safe Memory Reclamation)

Review guidance for SMR, OpenBSD's mechanism for reclaiming shared objects that
readers access without locking (the rough analogue of RCU). The implementation
is `sys/kern/kern_smr.c`; the read-side macros and lists are in `sys/smr.h`.
See smr_call(9). Reason about it from the actual headers, not from RCU habits.

## Read side
- A reader works inside an SMR read-side critical section, entered with
  `smr_read_enter()` and left with `smr_read_leave()`. These never block, and
  **sleeping inside one is forbidden**: flag `tsleep`/`msleep`, `rw_enter`, or a
  `PR_WAITOK`/`M_WAITOK` allocation between enter and leave.
- Read an SMR-protected pointer with `SMR_PTR_GET()` (a `READ_ONCE`), never a
  bare dereference. The `SMR_*_FOREACH()` list iterators (for `SMR_SLIST`,
  `SMR_LIST`, `SMR_TAILQ`) are the read-side traversals and must run inside a
  read section.

## Write side
- Writers serialise against each other with their own lock (not SMR). With that
  lock held, use the `_LOCKED` variants: `SMR_PTR_GET_LOCKED`,
  `SMR_PTR_SET_LOCKED`, and `SMR_*_FOREACH_LOCKED` / `_SAFE_LOCKED`. Using a
  plain read-side macro on the write side, or a `_LOCKED` macro without the
  serialising lock held, is a bug.
- After unlinking an object so that no new reader can reach it, the memory must
  not be freed until a grace period passes. Freeing it directly is a
  use-after-free against readers still inside a critical section.

## Reclamation
- Defer the free with `smr_call(entry, func, arg)` (initialise the entry with
  `smr_init()` first), which invokes `func` later in process context with no
  locks held; or block for a grace period with `smr_barrier()` before freeing
  synchronously. `smr_flush()` forces immediate processing and is discouraged
  (heavy system impact).
- The `smr_barrier()`/`smr_flush()` caller must not hold any lock that a pending
  SMR callback needs to acquire, or it deadlocks.
- `smr_call()` callbacks must be rate-limited by the writer. An unbounded rate
  of deferred frees can exhaust memory because reclamation is asynchronous.
