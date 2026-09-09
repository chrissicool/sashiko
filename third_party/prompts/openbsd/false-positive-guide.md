# Avoiding False Positives in OpenBSD Review

Before reporting a finding, rule out these common mistakes. To discard a
concern you must find concrete evidence in the code that invalidates it; if you
cannot, keep the finding. Do not invent a safety guarantee that you cannot point
to in the source.

## Locking and context
- Do not claim a missing lock without checking for an enclosing `NET_LOCK()`,
  `KERNEL_LOCK()`, or an assertion (`NET_ASSERT_LOCKED`, `MUTEX_ASSERT_LOCKED`,
  `rw_assert_wrlock`) that shows the lock is already held by the caller.
- Only system calls marked as `NOLOCK` in `sys/kern/syscalls.master` do not
  run with `KERNEL_LOCK` held.
- Only interrupt routines flagged as `IPL_MPSAFE` do not run with `KERNEL_LOCK`
  held. There are similar `*MPSAFE` flags for other subsystems.
- Do not flag a `PR_WAITOK`/`M_WAITOK` allocation as "sleeping in atomic
  context" unless you have established the call actually runs at raised spl, in
  an interrupt handler, or while holding a mutex.
- `splx()` restoring a saved IPL inside a helper that was entered at a known IPL
  is not an imbalance; trace the matching `splnet()`/`splx()` pair.
- Interrupt routines and other critical sections are allowed to call `free(9)`
  and `pool_put(9)` when they run below `IPL_VM`.
- Interrupt routines and other critical sections are allowed to call `wakeup(9)`
  when they run below `IPL_SCHED`.

## Memory and mbufs
- `m_pullup()` returning the same or a new mbuf is expected; only flag use of the
  pre-pullup pointer if the code actually keeps using it.
- A `free(9)` with a size argument of 0 is valid (size is then ignored); do not
  report it as a size mismatch. But mark it as style bug in new code.
- Reference-counted objects (`refcnt_rele`) are freed only when the count hits
  zero; a `_rele` that is not the last reference is not a use-after-free, when
  the caller has checked the result.

## Existing vs. introduced
- If a problem already existed before this diff, say so explicitly and only
  report it when it is high or critical severity. Do not attribute pre-existing
  behaviour to the patch.
- A later patch in the same series may complete or fix the change; do not report
  an "incomplete" finding if a subsequent patch you can see resolves it. Do not
  trust an unverifiable promise in the commit message, though.

## Style vs. bug
- KNF/style(9) deviations are Low severity, not correctness bugs; do not inflate
  them.
