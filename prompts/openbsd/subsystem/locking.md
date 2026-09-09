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

## Assertions
- Prefer the assertion macros to prove intent: `MUTEX_ASSERT_LOCKED`,
  `rw_assert_wrlock`/`rw_assert_rdlock`, `NET_ASSERT_LOCKED`, `splassert`. A
  new entry point that touches lock-protected state but asserts nothing is a
  red flag worth questioning.
