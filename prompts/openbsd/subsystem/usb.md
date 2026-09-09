# OpenBSD USB Drivers

Review guidance for USB device drivers (sys/dev/usb), built on the usbdi(9) API
(`sys/dev/usb/usbdi.h`). USB devices can be unplugged at any time, so detach and
the "dying" path get extra scrutiny. This complements drivers.md. See
usbd_transfer(9).

## Pipes and transfers
- Open an endpoint with `usbd_open_pipe()` / `usbd_open_pipe_intr()` and close it
  with `usbd_close_pipe()`. Allocate a transfer with `usbd_alloc_xfer()`,
  describe it with `usbd_setup_xfer()`, submit with `usbd_transfer()`, and free
  with `usbd_free_xfer()`. Every alloc/open must be matched by a free/close on
  all paths, including errors.
- A `usbd_transfer()` with the `USBD_SYNCHRONOUS` flag sleeps until completion,
  so it must run in process context — never from an interrupt handler or with a
  `mutex(9)` held. Asynchronous transfers complete in a callback; do not free
  the xfer or its buffer while it may still be in flight.
- Check the `usbd_status` return of these calls; treat `USBD_NORMAL_COMPLETION`
  as the only success and handle `USBD_CANCELLED`/`USBD_IOERROR` on teardown.

## Deferred work
- Hardware access from an interrupt callback that needs to sleep must be
  deferred to a USB task: initialise with `usb_init_task()` and schedule with
  `usb_add_task()`; cancel with `usb_rm_task()` before the softc is freed.

## Detach and the dying device
- Disconnect is asynchronous. After `usbd_deactivate()` the device is "dying";
  code should check `usbd_is_dying()` and stop issuing transfers. On detach,
  `usbd_abort_pipe()` outstanding transfers and close pipes before freeing
  buffers the controller may still write to.
- Use the reference helpers (`usbd_ref_incr()` / `usbd_ref_wait()`) so detach
  waits for in-flight users to drain; freeing softc while a transfer callback or
  task can still run is a use-after-free.
- Validate descriptor and transfer lengths returned by the device; a malicious
  or buggy device returns attacker-controlled sizes that must be bounded before
  use.
