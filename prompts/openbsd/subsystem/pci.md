# OpenBSD PCI Drivers

Review guidance for PCI/PCIe device drivers (sys/dev/pci, with the bus glue in
sys/dev/pci/pci.c and the per-arch chipset in `arch/<arch>/.../pci_machdep.h`).
This complements drivers.md (autoconf, bus_space, bus_dma) with PCI specifics.
See pci_intr_map(9) and pci_conf_read(9).

## Attach and matching
- `match` identifies the device, typically via `pci_matchbyid()` against a
  `struct pci_matchid` table; the vendor/product table and the `attach` code
  must agree. Read config space with `pci_conf_read()`/`pci_conf_write()`.
- Map registers with `pci_mapreg_map()` for the correct BAR, type, and size; a
  wrong BAR index or size argument maps the wrong window. Keep the returned tag,
  handle, and size for later bus_space access and for unmapping on detach.
- Use `pci_get_capability()` to find capabilities (MSI, MSI-X, PCIe, power
  management) rather than assuming a fixed config-space offset.

## Interrupts
- Establish interrupts with `pci_intr_map()` (or `pci_intr_map_msi()` /
  `pci_intr_map_msix()` / `pci_intr_map_msivec()`) followed by
  `pci_intr_establish()`; tear down with `pci_intr_disestablish()` on detach and
  on the attach error path. Failing to disestablish leaves a handler pointing at
  freed softc.
- The IPL passed to `pci_intr_establish()` must match the level the handler's
  data needs; a `mutex(9)` shared with the handler must be initialised at an IPL
  at least that high.
- MSI/MSI-X must be enabled correctly and the right number of vectors mapped;
  check the handler is installed before the device is told to raise interrupts.

## Teardown
- `detach` must disestablish the interrupt, stop DMA, drain timeouts/tasks, and
  unmap the register window (matching `pci_mapreg_map`) before freeing softc
  state. Verify the symmetry against what `attach` set up, including partial
  failure inside `attach`.
