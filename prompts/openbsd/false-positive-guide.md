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

## API contracts and callers
- Do not report that a new or refactored internal helper could fail if a
  hypothetical caller passed it something invalid -- a NULL softc, an unaligned
  address, a zero length, an mbuf without the header it expects. Report it only
  when a caller that already exists in the tree, or one added by this series,
  actually violates the contract. Find that caller; do not posit one.
- Before claiming a helper is called against its contract, read the helper and
  its man page. Do not infer what it accepts from its name: a lookup that takes
  an address may well accept any address inside a range rather than the exact
  base of one.
- A newly added `KASSERT(9)` or `panic(9)` that states an invariant every
  in-tree caller already satisfies is not a denial of service on its own. Report
  it only when you can name a caller that reaches it with untrusted, remote, or
  unprivileged input -- in which case it is a real panic, because GENERIC builds
  with `option DIAGNOSTIC`.

## Existing vs. introduced
- If a problem already existed before this diff, say so explicitly and only
  report it when it is high or critical severity. Do not attribute pre-existing
  behaviour to the patch.
- A defect that the diff only moves, renames, or re-exposes is still
  pre-existing. A defect is new only when the diff creates its root cause. When
  the `+` lines alone do not settle that, read the file at the parent revision.
- A later patch in the same series may complete or fix the change; do not report
  an "incomplete" finding if a subsequent patch you can see resolves it. This
  covers a helper nothing calls yet, a field nothing reads yet, and wiring that
  lands one patch later: judge it against the state at the end of the series,
  not against this patch alone. Do not trust an unverifiable promise in the
  commit message, though.

## Style vs. bug
- KNF/style(9) deviations are Low severity, not correctness bugs; do not inflate
  them.
