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
| Crypto | sys/crypto/, `crypto_*`, `swcr_*`, `CRYPTO_*`, aes, chacha, poly1305, hmac, sys/dev/ic/*crypto* | crypto.md |
| Drivers | sys/dev/, config_found, config_attach, cfattach, cfdriver, bus_space_, `bus_dmamap_*`, `.*_intr_establish`, spl | drivers.md |
| Networking | sys/net/, sys/netinet/, sys/netinet6/, sys/net80211/, `pf_*`, NET_LOCK, if_input, if_enqueue, `ifq_*`, `ifiq_*`, `ether_*`, ip_input, ip6_input, m_pullup, m_freem, mbuf | networking.md |
| VMM/VMD | sys/arch/amd64/amd64/vmm*, sys/dev/vmm/, vmd, vcpu, vm_run, vmx_*, svm_*, ept, vmread, vmwrite | vmm.md |
