# OpenBSD VFS and Filesystems

Review guidance for the virtual filesystem layer (sys/kern/vfs_*.c) and
individual filesystems (sys/ufs, sys/nfs, sys/isofs, sys/miscfs, ...). The
syscall entry is `kern/vfs_syscalls.c`, the vnode glue is `kern/vfs_vnops.c`
and `kern/vfs_subr.c`. See vnode(9), vref(9), and namei(9).

## Vnode references
- A vnode reference is taken with `vref()` and released with `vrele()`; a held,
  locked vnode is released with `vput()` (which is `VOP_UNLOCK` + `vrele()`, so
  never call both). `vget()` activates a vnode found on a list and may fail.
  Every reference taken on a path must be released on every exit, including
  error paths.
- A reference (`v_usecount`) keeps the vnode alive; the lock is separate. Do not
  confuse holding a reference with holding the vnode lock.

## Vnode locking
- Lock a vnode with `vn_lock(vp, LK_EXCLUSIVE | LK_RETRY)` and unlock with
  `VOP_UNLOCK()`. Most `VOP_*` operations require the vnode locked on entry;
  check each op's contract. Watch lock order between two vnodes (parent before
  child in directory operations) to avoid deadlock.
- A `VOP_*` call is an indirect call through the filesystem's `struct vops`
  vector (installed via `getnewvnode()`). When adding a vop, verify the vector
  is complete and that defaults (`eopnotsupp`/`nullop`/`spec_*`) are correct for
  ops the filesystem does not implement.

## Lifecycle and teardown
- `VOP_INACTIVE` runs when the last reference is dropped; `VOP_RECLAIM` runs when
  the vnode is recycled and must release all filesystem-private data hung off
  `v_data`. After reclaim the vnode no longer belongs to the filesystem; using a
  stale vnode pointer past reclaim is a use-after-free.
- Check that buffers (`bread`/`bwrite`/`brelse`/`bdwrite`) are balanced and that
  a `bp` is not used after `brelse()`.

## Paths and userland input
- Pathname lookup goes through `namei()`; mind `VOP_LOOKUP` locking and the
  `.`/`..` and mount-crossing cases.
- Validate lengths and offsets from userland (read/write/ioctl) before use, and
  zero-fill any structure copied back out (stat, dirent) so padding does not
  leak kernel memory.
