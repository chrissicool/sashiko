# OpenBSD Device Drivers

Review guidance for drivers under sys/dev (sys/dev/ic core logic, sys/dev/pci,
sys/dev/usb, sys/dev/fdt, sys/dev/acpi attachments).

## autoconf lifecycle
- Driver glue follows autoconf(9): `match` probes, `attach` initialises the
  softc and hardware, `detach` tears down, and `activate` carries the power
  transitions (see below). The split is core logic in
  `dev/ic/foo.c` with bus attachments in `dev/pci/foo_pci.c` (or usb, fdt).
- At attach, hardware and softc fields must be initialised before interrupts are
  established (`*_intr_establish` / `pci_intr_establish`); otherwise the handler
  can run against uninitialised state.
- At detach, disable device interrupts and drain pending timeouts/tasks before
  freeing softc resources, or the interrupt handler/callback can touch freed
  memory.

## suspend, resume and hibernate
- `activate` dispatches the power lifecycle. Four of its actions form two
  symmetric pairs around the sleep: `DVACT_QUIESCE` then `DVACT_SUSPEND` on the
  way down, `DVACT_RESUME` then `DVACT_WAKEUP` on the way back up.
- The two pairs differ in what they may do, and that decides where code belongs.
  `DVACT_QUIESCE` and `DVACT_WAKEUP` run with interrupts available: they may
  sleep, take a sleepable lock, schedule, and read the filesystem.
  `DVACT_SUSPEND` and `DVACT_RESUME` run with interrupts disabled: they may only
  touch registers, and must not sleep, wait on a completion interrupt, allocate
  with `M_WAITOK`, or load firmware.
- The resulting shape: stop the device and drain whatever sleeps at quiesce,
  save registers at suspend, restore them at resume, bring the device back into
  service at wakeup. `iwm(4)` follows it exactly -- `iwm_stop()` under a
  sleepable `rw_enter_write()` at quiesce, PCI config restore plus
  `iwm_disable_interrupts()` at resume, and the full
  `iwm_start_hw()`/`iwm_init_hw()` bring-up, which calls `loadfirmware()`, at
  wakeup.
- A `tsleep`/`msleep`, an `M_WAITOK` allocation, a `loadfirmware()` or a wait for
  a device interrupt under `DVACT_SUSPEND` or `DVACT_RESUME` is misplaced however
  correct it looks: it belongs in the quiesce or wakeup half of the pair.
  Busy-waiting with `delay(9)` is the only waiting those two stages can do.
- A parent orders itself against its children with
  `config_activate_children(self, act)`: children go down before it does and
  come back up after it, because nothing below a bus can work until the bus
  itself does. So the call comes before a parent's own suspend work and after
  its own resume work.
- Coming back up must re-establish everything the hardware lost, not only what
  going down explicitly tore down. Config space, BARs, interrupt enables, DMA
  ring contents and device state programmed at attach are all candidates; a
  change that adds setup to `attach` with no matching path at resume or wakeup
  works until the first suspend.
- `DVACT_RESUME` must not fail. `config_activate_children()` has no recovery for
  it and says so ("failing resume cannot be handled"); only `DVACT_SUSPEND` is
  rolled back, by resuming the siblings already suspended. A suspend path that
  gives up halfway therefore has to leave the device resumable.
- Re-initialise only what was running: `if (ifp->if_flags & IFF_UP) foo_init(sc)`
  on the way up, matching `if (ifp->if_flags & IFF_RUNNING) foo_stop(sc)` on the
  way down. Unconditional re-init starts hardware the user had stopped.
- `DVACT_POWERDOWN` powers off the device on platforms that perform suspend-to-idle.

- `DVACT_DEACTIVATE` is not part of that cycle: it marks the device gone in
  software, and in practice arrives through `config_detach()` when something
  like a USB device is unplugged. The hardware may already be absent, so the
  handler must not touch it -- flag the device dying, wake anything sleeping on
  it, and fail new work. It is also a veto: `config_deactivate()` clears
  `DVF_ACTIVE` and restores it if the driver returns non-zero, and the detach
  then fails because the device is still busy.

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
