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
| Audio | sys/dev/audio*, `audio_*`, sys/dev/midi*, `mixer_*`, `*_trigger_output`, `*_trigger_input`, audio_if | audio.md |
| Crypto | sys/crypto/, `crypto_*`, `swcr_*`, `CRYPTO_*`, aes, chacha, poly1305, hmac, sys/dev/ic/*crypto* | crypto.md |
| Drivers | sys/dev/, config_found, config_attach, cfattach, cfdriver, bus_space_, `bus_dmamap_*`, `.*_intr_establish`, spl | drivers.md |
| Networking | sys/net/, sys/netinet/, sys/netinet6/, sys/net80211/, `pf_*`, NET_LOCK, if_input, if_enqueue, `ifq_*`, `ifiq_*`, `ether_*`, ip_input, ip6_input, m_pullup, m_freem, mbuf | networking.md |
| PCI | sys/dev/pci/, `pci_intr_*`, `pci_mapreg_map`, `pci_conf_*`, `pci_matchbyid`, `pci_get_capability`, pci_attach_args, pci_matchid | pci.md |
| pf | sys/net/pf*, `pf_*`, PF_LOCK, `PF_STATE_*`, PF_ASSERT_LOCKED, pf_state, pf_rule, pf_test, pfsync | pf.md |
| SMR | `smr_*`, `SMR_PTR_*`, `SMR_*_FOREACH*`, smr_read_enter, smr_call, smr_barrier, sys/smr.h | smr.md |
| Syscalls | `sys_*`, `copyin*`, `copyout*`, SCARG, any change to syscall parameter validation | syscall.md |
| USB | sys/dev/usb/, `usbd_*`, `usb_add_task`, `usb_init_task`, usbd_xfer, usbd_pipe, usbd_status, USBD_ | usb.md |
| VFS | sys/kern/vfs_*, sys/ufs/, sys/nfs/, sys/miscfs/, sys/isofs/, `VOP_*`, vref, vrele, vput, vget, vn_lock, getnewvnode, namei, struct vops | vfs.md |
| VMM/VMD | sys/arch/amd64/amd64/vmm*, sys/dev/vmm/, vmd, vcpu, vm_run, vmx_*, svm_*, ept, vmread, vmwrite | vmm.md |
