# Tracing Call Stacks in the OpenBSD Source Tree

When a finding depends on how a function is reached, trace the call path with the
source tools rather than guessing. Reading a pointer field is not a dereference;
only accessing what it points to is. Confirm the actual reachability before
reporting a NULL dereference or a context (sleeping/spl/lock) violation.

## Useful entry points
- Syscalls: `kern/syscalls.master` -> generated `kern/init_sysent.c` -> the
  `sys_*` handler.
- Process/thread creation: `kern/kern_fork.c` (`fork1()`).
- Scheduling and sleep: `kern/kern_synch.c` (`mi_switch()`, `tsleep`/`wakeup`).
- VFS: `kern/vfs_syscalls.c` -> `kern/vfs_vnops.c` -> the `VOP_*` vectors.
- Sockets: `kern/uipc_syscalls.c`; mbufs in `kern/uipc_mbuf.c`.
- Network input: `net/` (`ether_input` -> `ip_input`/`ip6_input`); pf in
  `net/pf.c`.
- Device autoconf: `kern/subr_autoconf.c` (`config_found`, `config_attach`).
- Virtual memory: `uvm/uvm_map.c` (address-space ops) and `uvm/uvm_fault.c`
  (the fault path); pmap is under `arch/<arch>/<arch>/pmap.c`.
- SMR reclamation: read-side macros in `sys/smr.h` (`SMR_PTR_GET`,
  `SMR_*_FOREACH`); deferred frees run from `kern/kern_smr.c` via
  `smr_call`/`smr_barrier`.

## What to establish
- Which IPL / lock context the code runs at (interrupt handler? under
  `NET_LOCK()`? holding a driver mutex? at raised spl?). This determines whether
  a sleep or a `PR_WAITOK` allocation is legal. Read it off the IPL a handler
  was registered with (`*_intr_establish` at attach), the IPL a mutex was
  initialised with (`mtx_init`), any `splassert(9)` in the function, and the
  `spl*()` raised on the way in. `NOLOCK` in `kern/syscalls.master` and an
  `IPL_MPSAFE` handler mean the `KERNEL_LOCK` is not held.
- Whether a pointer can actually be NULL at the point of use, by finding its
  assignments and the callers.
- For driver code, whether the hardware/softc state is initialised before the
  path runs (attach vs. interrupt vs. detach ordering).

## How to look
- Prefer following `funcA() -> funcB()` chains and reading the specific
  functions to reasoning from names alone.
- The man page (`share/man/man9/*`) is the authoritative contract for kernel
  APIs; consult it when an API's locking or sleeping behaviour matters.
