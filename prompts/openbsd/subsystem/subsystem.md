# Subsystem Guide Index

Load subsystem guides from the prompt directory based on what the code touches.
Each guide contains subsystem-specific invariants, API contracts, and common bug
patterns for that area of the OpenBSD source tree.

A change can match multiple rows. Load **every** matching guide, not just the
most specific one. The triggers column lists path fragments, function names, and
symbol patterns.

## Subsystem Guides

| Subsystem | Triggers | File |
|-----------|----------|------|
| Networking | sys/net/, sys/netinet/, sys/netinet6/, sys/net80211/, `pf_*`, NET_LOCK, if_input, if_enqueue, `ifq_*`, `ifiq_*`, `ether_*`, ip_input, ip6_input, m_pullup, m_freem, mbuf | networking.md |
