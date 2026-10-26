# OpenBSD Locking and Synchronization

Review guidance for the OpenBSD synchronisation primitives: spl(9), mutex(9),
rwlock(9), the kernel and network locks, and SMR. See also technical-patterns.md
and, for the lock-free read side, smr.md.

## Choosing the right primitive
- `spl(9)` (`splnet`/`splbio`/`splhigh`/.../`splx`) blocks interrupts on the
  local CPU only; it does not provide mutual exclusion against other CPUs. Code
  shared between an interrupt handler and a thread on an MP kernel needs a
  `mutex(9)` (or `KERNEL_LOCK`), not just spl.
- `mutex(9)` is a spinning lock that cannot sleep and must not be held across a
  sleeping call. It encodes an IPL (`mtx_init(&mtx, IPL_xxx)`): that IPL must be
  at least as high as the highest IPL from which the mutex is taken, or an
  interrupt can deadlock against a holder.
- `rwlock(9)` (`rw_enter_read`/`rw_enter_write`/`rw_exit`) is a sleeping lock;
  never take one while holding a `mutex(9)` or at raised spl, or from an
  interrupt handler.

## Common rules to check
- spl level raised on entry must be restored with `splx()` on every path,
  including error `goto`s and early returns. `splx()` restores the saved level,
  so save it (`s = splnet();`) and pass it back.
- Multiple mutexes must be released in the reverse order they were acquired
  (`mtx_enter(a); mtx_enter(b); mtx_leave(b); mtx_leave(a);`). Because a mutex
  encodes an IPL, leaving out of order makes the unwound `splx()` restore the
  wrong level.
- A lock acquired in a different order than elsewhere is an AB/BA deadlock.
  Honour the documented order (e.g. `KERNEL_LOCK` -> `NET_LOCK` -> subsystem
  locks); do not invert it. `witness(4)` (`option WITNESS`) detects order
  violations at runtime — a patch that trips witness is buggy.
- `NET_LOCK()` is a single global rwlock (write-mostly). Do not take it when a
  caller already holds it ("locking against myself"); check for
  `NET_ASSERT_LOCKED()` to learn what the path expects.
- `KERNEL_LOCK()` is the recursive big lock. A path flagged MP-safe (e.g. an
  `IPL_MPSAFE` interrupt, a `NOLOCK` syscall) does not hold it, so it cannot
  rely on the big lock for mutual exclusion.

## Object lifetime and use-after-free
At every point where a path releases a lock or drops a reference, ask: could
another path still hold a raw pointer to this object?

```
thread A (lookup)                   thread B (remove)
-----------------                   -----------------
rw_enter_read -> find item -> rw_exit
                                    rw_enter_write -> remove item -> rw_exit
                                    free(item)
use item        <- use-after-free
```

The fix is to take a reference while the lock is still held (`refcnt_take(9)`)
and drop it after the last use (`refcnt_rele(9)`), with the teardown side
waiting for the last one (`refcnt_finalize(9)`) before it frees the object.

**Build the reference budget before reporting a use-after-free.** When the
other side of the suspected use-after-free is asynchronous -- a `timeout(9)`
or `task(9)` handler, an interrupt handler, an `smr_call(9)` callback -- list
what keeps the object alive on each side. A pin is a reference, a held lock, an
open SMR read-side section (`smr_read_enter`/`smr_read_leave`), or a barrier the
freeing side has already completed. Name every acquire that pins the shared
pointer, who owns it, and every release, up to the claimed free point. A held
`KERNEL_LOCK()` is not a pin across a sleep: it is released while the thread
sleeps.

Report only if the free can happen while the using side still holds a live
pointer with no pin. It is not a use-after-free when:

- the stored pointer is itself counted. Its holder took a reference when it
  saved the pointer, so the free is only deferred until that reference is
  dropped.
- the freeing side waits for the user to finish before it frees:
  `timeout_del_barrier(9)`, `taskq_barrier(9)` (after `task_del(9)`),
  `intr_barrier(9)`, `smr_barrier(9)` or `refcnt_finalize(9)`. `timeout_del(9)`
  and `task_del(9)` alone only remove a pending entry; they do not wait for a
  handler that is already running on another CPU.

**Returning storage to a pool is a free.** `pool_put(9)` returns the item to
its pool, and `m_freem(9)` returns mbufs and clusters to theirs. The memory
stays mapped, so a stale access does not fault, but the next `pool_get(9)` may
hand the same storage to someone else: that is a logical free, so do not
dismiss the report because the storage is still addressable. Distinguish a read
from a write after the logical free. A read is reportable only when the stale
value drives a decision. A write is always reportable, because it corrupts the
next owner's use of the same storage.

Record the result as one line:

```
budget: acquires=[...], releases=[...], refs_live_at_free=..., free_reachable_without_pin=[proof | no]; logical_free=[storage-free | pool-return], use=[read-driving-decision | write]
```

## Assertions
- Prefer the assertion macros to prove intent: `MUTEX_ASSERT_LOCKED`,
  `rw_assert_wrlock`/`rw_assert_rdlock`, `NET_ASSERT_LOCKED`, `splassert`. A
  new entry point that touches lock-protected state but asserts nothing is a
  red flag worth questioning.
