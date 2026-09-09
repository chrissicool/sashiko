# OpenBSD Audio

Review guidance for the OpenBSD audio subsystem: the MI audio layer
(sys/dev/audio.c) and hardware drivers implementing the `audio_if` interface,
plus midi.

## Interface contract
- Hardware drivers implement the `struct audio_hw_if` callbacks. When adding or
  changing a driver, verify every required callback is present and that optional
  ones are handled as optional by the MI layer.
- The MI layer serialises access with its own lock and runs the trigger
  callbacks; `*_trigger_output`/`*_trigger_input` start DMA and must not block.
  Interrupt callbacks run at a raised IPL — no sleeping or PR_WAITOK there.

## Common pitfalls
- Ring-buffer/block accounting between the MI layer and the driver: off-by-one
  or wrong block-size math causes over/underruns or out-of-bounds DMA. Verify
  the buffer is sized and synced (bus_dmamap_sync) correctly.
- Parameter negotiation (`set_params`): validate encoding, precision, channels,
  and sample rate against what the hardware supports; do not trust requested
  values blindly.
- Start/stop/close ordering: ensure DMA is stopped and interrupts quiesced
  before buffers are freed, and that open/close reference counting is balanced.
- Mixer ioctls: bound-check indices and values from userland.
