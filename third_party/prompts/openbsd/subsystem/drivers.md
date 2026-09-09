# OpenBSD Device Drivers

Review guidance for drivers under sys/dev (sys/dev/ic core logic, sys/dev/pci,
sys/dev/usb, sys/dev/fdt, sys/dev/acpi attachments).

## autoconf lifecycle
- Driver glue follows autoconf(9): `match` probes, `attach` initialises the
  softc and hardware, `detach`/`activate` tear down. The split is core logic in
  `dev/ic/foo.c` with bus attachments in `dev/pci/foo_pci.c` (or usb, fdt).
- At attach, hardware and softc fields must be initialised before interrupts are
  established (`*_intr_establish` / `pci_intr_establish`); otherwise the handler
  can run against uninitialised state.
- At detach, disable device interrupts and drain pending timeouts/tasks before
  freeing softc resources, or the interrupt handler/callback can touch freed
  memory.

## bus_space and bus_dma
- Register access goes through `bus_space(9)` with the correct tag, handle, and
  access width; mixing widths or wrong offsets corrupts device state.
- DMA uses `bus_dma(9)`: maps must be created, loaded, `bus_dmamap_sync(9)`'d
  with the correct direction before and after the transfer, then unloaded.
  A missing sync yields stale data on non-coherent platforms; a leaked map is a
  resource leak.
- Honour the descriptor/ring ownership protocol: only touch a descriptor the CPU
  owns, and sync before reading hardware-written fields.

## Interrupts and locking
- Interrupt handlers run at a raised IPL; do not sleep or use `PR_WAITOK`/
  `M_WAITOK` there. Use `splnet()`/`splbio()`/`splx()` (or a `mutex(9)` at the
  right IPL) to protect state shared with the handler, balanced on all paths.
- Endianness: convert on-device and descriptor data with the byte-order helpers
  (`letoh32`, `htole16`, ...).
