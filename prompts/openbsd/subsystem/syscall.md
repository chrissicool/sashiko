# OpenBSD System Calls

Review guidance for system-call entry points: the table in
`sys/kern/syscalls.master` (which generates `init_sysent.c` and the
`syscallargs.h` argument structs) and the `sys_*` handlers across `sys/kern`.
Handlers read arguments with `SCARG(uap, name)`. The userland-boundary notes in
technical-patterns.md and the stage-6 security checks also apply.

## ABI compatibility
- Changing a syscall's signature — adding, removing, or reordering arguments, or
  changing the meaning or width of an existing one — is an ABI change. OpenBSD
  does not extend a live syscall's argument list in place: the old signature is
  kept as a `COMPAT_*` entry in `syscalls.master` (so existing binaries keep
  working) and the new signature is given a new syscall number. A change that
  alters a live syscall's arguments without a compat entry breaks old binaries,
  which still pass the old arguments and leave any new slot unset.
- A syscall marked `NOLOCK` in `syscalls.master` runs without `KERNEL_LOCK`;
  confirm the handler is genuinely MP-safe before relying on that.

## Parameter trust boundaries
Syscall parameters come from user-controlled registers or stack slots. A
parameter that is only meaningful when a specific flag is set may contain
arbitrary garbage when that flag is absent — userspace is not required to
zero-fill unused arguments. When syscall args are copied into a kernel struct,
each field inherits the trust boundary of its source argument and stays garbage
outside the flag gate even though it looks initialised in C. When a refactor
moves a check across a flag gate, verify every variable the check uses is valid
in the broader scope.

Report as a bug any validation, arithmetic, or comparison that uses a
flag-gated syscall parameter outside the scope of its flag gate.

## copyin / copyout
- `copyin(9)`, `copyinstr(9)`, and `copyout(9)` return an error that must be
  checked; a faulting user address is a normal outcome, not success.
- Validate a user-supplied length against the actual remaining space in the
  kernel buffer before transferring. A transfer size computed as `min(len, ...)`
  where `len` is the whole buffer rather than the remaining (`total - consumed`)
  space reads or writes past the buffer end.
- Zero-fill any structure copied out so padding and unset fields do not leak
  kernel memory; prefer `sizeof(variable)` over `sizeof(type)` so the size
  tracks the object actually being transferred.
