# OpenBSD pf (Packet Filter)

Review guidance for pf (sys/net/pf.c, pf_ioctl.c, pf_norm.c, pf_lb.c and the
state/table code), the OpenBSD firewall. pf runs on the packet-forwarding path,
so correctness and locking bugs here are remotely reachable. See pf(4) and the
lock-ordering comment in sys/net/pfvar_priv.h.

## Locking
- pf has several locks with a strict order (from pfvar_priv.h):
  `KERNEL_LOCK` -> `NET_LOCK` -> `pf_state_list.pfs_rwl` -> `PF_LOCK` ->
  `PF_STATE_LOCK` -> `pf_state_list.pfs_mtx`. Acquiring them out of this order
  is a deadlock; verify any new lock acquisition respects it.
- The packet path runs under `NET_LOCK()`; rule and table changes come in
  through ioctl. Use `PF_ASSERT_LOCKED()` / `PF_STATE_ENTER_WRITE()` /
  `PF_STATE_ENTER_READ()` as the code does — do not touch shared pf state
  without the lock the surrounding code uses.
- Fragment reassembly has its own lock and does not require `PF_LOCK`; do not
  assume PF_LOCK protects it.

## States and rules
- States and rules are reference counted. A state inserted with
  `pf_state_list_insert()` must be removed with `pf_state_list_remove()`
  (holding `pfs_rwl`) before being freed; a state must not be used after it is
  unlinked/purged. Leaking or double-freeing a state corrupts the state table.
- Watch refcounting on `struct pf_rule`/anchors across ruleset swaps: a rule
  freed while a state still references it is a use-after-free.

## Packet handling
- pf manipulates mbufs (`m_pullup`, checksum fixups, NAT rewrites). Re-check the
  mbuf pointer after `m_pullup()` and validate header lengths before reading;
  malformed packets are attacker-controlled.
- NAT/redirection rewrites addresses and ports and must update checksums
  consistently; verify the incremental checksum update matches the bytes
  changed.
- pfsync exports state to peers: changes to the on-wire state layout must keep
  the two ends in sync and must not leak uninitialised memory in exported state.
