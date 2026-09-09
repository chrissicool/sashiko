# Locking and synchronization

You are a world-class concurrency and locking expert auditing an OpenBSD kernel patch.
Carefully review the patch for ANY locking, concurrency, or synchronisation bug.
You MUST consider the following categories and report any violations:
1. Sleeping in a non-sleepable context: are there calls that can sleep (`tsleep`/`msleep`, `rw_enter` on a contended `rwlock(9)`, `pool_get`/`malloc` with `PR_WAITOK`/`M_WAITOK`) while holding a `mutex(9)`, at a raised spl(9), in an interrupt handler, or inside an SMR read-side critical section (`smr_read_enter`/`smr_read_leave`)?
2. Lock ordering and deadlocks: are locks acquired in a different order than elsewhere, creating an AB/BA deadlock? Does the code take a lock already held by a caller (for example acquiring `NET_LOCK()` when it is already held), risking "locking against myself"?
3. Mutex release order: are nested `mutex(9)` released in the reverse order they were acquired? A mutex carries an IPL, and `mtx_leave` restores the level that was in effect before its own `mtx_enter` raised it, so releasing the outer mutex first drops the IPL below the level the inner one still requires.
4. Race conditions and lockless access: are shared variables, list entries, or pointers accessed without the appropriate lock held (for example clearing a pointer concurrently while another path dereferences it)? Where lockless access is intended, are the required memory barriers (`membar_enter`, `membar_exit`, `membar_producer`, `membar_consumer`) present, and can an SMR reader observe a reused element? Is `NET_LOCK()`/`KERNEL_LOCK()` required on this path and actually held (check `NET_ASSERT_LOCKED`)? Are there TOCTOU races where state is checked outside the lock but relied upon inside?
5. Use-after-free under locks: are works/timeouts/tasks left able to run against state that is being torn down? Is a structure published (e.g. linked into a list, or an ifnet attached) before its lock or private data is fully initialised?
6. Interrupt priority level: is `splnet()`/`splbio()`/`splhigh()` raised to cover the critical section, and is the saved level restored with `splx()` on every path?
7. Unprotected state modifications: are hardware state, flags, or statistics updated without the lock or spl that protects them?
8. Are there any global state modifications that leave half-initialized state at preemption points? In particular in KERNEL_LOCK'ed sections.
