# OpenBSD Networking

Review guidance for the OpenBSD network stack (sys/net, sys/netinet,
sys/netinet6, sys/net80211) and pf.

## Locking
- Most of the network stack runs under `NET_LOCK()` (a rwlock). Check whether a
  function requires it and asserts it with `NET_ASSERT_LOCKED()`; new call paths
  must hold it where the data they touch expects it.
- Some paths run without the NET_LOCK under the per-CPU/SMR model; do not assume
  a lock is held without confirming. Watch for data touched from both the
  forwarding path and ioctl/sysctl paths.
- Interface input/output queues use `ifq`/`ifiq` (see ifq(9)); use the provided
  enqueue/dequeue primitives rather than touching the queue directly.

## mbufs
- After `m_freem(9)`/`m_free(9)` the mbuf must not be used again. A function that
  consumes an mbuf (enqueues or frees it) must not let the caller free it too —
  this double-free pattern is common in error paths.
- `m_pullup(9)` may free the original mbuf and return a new one (or NULL). Always
  use the return value and re-check for NULL; never keep using the old pointer.
- Validate on-wire lengths before reading; do not trust header length fields.
  Ensure enough contiguous data with `m_pullup` before dereferencing headers.
- Account for mbuf vs. cluster storage and the `M_PKTHDR` flag on the leading
  mbuf only.

## input/output paths
- `ether_input`, `ip_input`/`ip6_input`, and protocol `*_input` routines run in
  soft-interrupt context; do not sleep or call `PR_WAITOK`/`M_WAITOK`
  allocations there.
- `if_enqueue`/`ifq_enqueue` hands the mbuf to the driver; on failure the mbuf is
  freed by the queue layer — do not free it again.

## pf
- pf (sys/net/pf*.c) has its own locking and state/rule lifecycle; see pf.md for
  detailed guidance when the change touches the packet filter.
