# OpenBSD UVM (Virtual Memory)

Review guidance for UVM, the machine-independent virtual memory system in
`sys/uvm/`. The public interface is `uvm_extern.h`; the core objects are maps
(`uvm_map.c`), memory objects (`uvm_object.c`), pages (`uvm_page.c`), anons and
amaps (`uvm_anon.c`/`uvm_amap.c`), and the fault handler (`uvm_fault.c`). The
machine-dependent half is the pmap (`sys/arch/<arch>/include/pmap.h`,
`uvm_pmap.h`). See uvm(9), uvm_map(9), pmap(9), and uvm_fault(9). Reason from
the headers and the lock legends in them, not from VM habits from other kernels.

## Page, object, and anon locking
- Each `struct vm_page` field carries a lock tag in `uvm_page.h`: `[I]`
  immutable after creation, `[a]` atomic, `[Q]` `uvm.pageqlock`, `[F]`
  `uvm.fpageqlock`, `[o]` the owner lock (`uobject->vmobjlock` **or**
  `uanon->an_lock`). Touching an `[o]` field (`uobject`, `uanon`, `offset`,
  `wire_count`, the object tree) without the owner lock held is a race.
- `pg_flags` is `[a]`: change it **only** with `atomic_setbits_int()` /
  `atomic_clearbits_int()`, never a plain `pg->pg_flags |= ...`. A non-atomic
  RMW races with another CPU updating a different bit.
- `uvm_object::vmobjlock` is an `rwlock(9)` (may be shared between objects via
  `rw_obj_init()`); the page queue lock (`uvm_lock_pageq()` →
  `mtx_enter(&uvm.pageqlock)`) is a `mutex(9)`. Do not sleep while holding the
  page queue lock, and respect the order: map lock → object/anon lock → page
  queue lock. Inverting it is an AB/BA deadlock.
- Anons: `an_lock` protects `an_page`/swap slot; `an_ref` is the reference
  count. Amaps lock with `amap_lock(amap, RW_WRITE)` / `amap_unlock()`.

## The PG_BUSY protocol
- `PG_BUSY` means a page is owned for paging I/O. Before sleeping on or doing I/O
  to a page, set `PG_BUSY` (under the owner lock); a second actor that finds a
  busy page must set `PG_WANTED` and wait (`uvm_pagewait()`), not touch it.
- On every exit path the owner must clear `PG_BUSY`, wake `PG_WANTED` waiters,
  and handle `PG_RELEASED` (the page was freed while busy — it must be freed
  now, not reused). Dropping a busy page without waking waiters hangs them;
  using/freeing another actor's busy page is a use-after-free.
- After any sleep (page wait, amap/anon lock, allocation), the map, object,
  amap, and page state may all have changed — `uvm_fault()` re-looks-up and
  revalidates. A patch that caches a `vm_page`/`vm_anon`/entry pointer across a
  sleep and reuses it without rechecking is suspect.

## Maps, faults, and kernel memory
- A `struct vm_map` is locked for write with `vm_map_lock()`/`vm_map_unlock()`
  and for read with `vm_map_lock_read()`/`vm_map_unlock_read()`. Use
  `uvm_map_lookup_entry()` under the lock; an entry pointer is only valid while
  the map is held.
- Splitting or trimming a map entry must go through `UVM_MAP_CLIP_START()` /
  `UVM_MAP_CLIP_END()` (under the map lock) so the amap/aref offsets,
  reference counts, and entry boundaries stay consistent. Hand-editing
  `start`/`end` or duplicating an entry without adjusting the amap reference is
  a corruption/leak bug.
- Kernel VA is obtained with `km_alloc()`/`km_free()` (or `uvm_km_alloc()`); the
  size and the `kmem_va_mode`/`UVM_KMF_*` flags passed to free must match those
  passed to alloc. A mismatch corrupts the kernel map.
- Reference counts: balance `uo_refs` on objects and `an_ref` on anons across
  every path (kernel objects use the sentinel `UVM_OBJ_KERN` and are never
  freed). Balance `wire_count` for `uvm_map_pageable()`/wiring.

## pmap (machine-dependent layer)
See pmap(9). The pmap is allowed to batch or defer the actual MMU/TLB work, so
the key rule for callers is to flush at the end of a batch:
- A run of `pmap_enter()` / `pmap_remove()` / `pmap_kenter_pa()` /
  `pmap_kremove()` is only guaranteed committed (and the stale TLB entries shot
  down across CPUs) after a `pmap_update()`. pmap(9) requires the
  `pmap_update()` explicitly after `pmap_kenter_pa()`/`pmap_kremove()`. A path
  that removes a mapping and then frees or reuses the page **before**
  `pmap_update()` can leave a live stale translation — a use-after-free through
  the TLB.
- `pmap_enter()` with `PMAP_CANFAIL` may fail and return `ENOMEM` — the caller
  must check it. Without `PMAP_CANFAIL` the pmap is required to panic rather
  than fail, so passing it on a path that cannot tolerate a panic is a bug.
  `pmap_kenter_pa()`/`pmap_kremove()` are the interrupt-safe, unmanaged variants
  and are **only** for wired kernel mappings — not pageable user mappings.
- Before paging out, all mappings of a page must be removed with
  `pmap_page_protect(pg, PROT_NONE)`; modified/referenced state lives only in
  the pmap and must be collected via `pmap_is_modified()`/`pmap_clear_modify()`
  (and the reference equivalents) — reading `pg_flags` is not enough.
- Only managed pages have a `vm_page`. Convert a PA with `PHYS_TO_VM_PAGE()` and
  handle `NULL` (unmanaged / device memory). `pmap`-private bits live in
  `PG_PMAP*`; MI code must not stomp them when manipulating `pg_flags`.

## Pageout and swap
- The page daemon (`uvm_pdaemon.c`) and swap (`uvm_swap.c`) run asynchronously
  against faulting threads. A page handed to `uvm_swap_put()`/pageout must be
  `PG_BUSY` and owned for the duration; completion clears the busy state and
  handles `PG_RELEASED`. Swap slots from `uvm_swap_alloc()` must be released
  with `uvm_swap_free()` on every path, including failed `uvm_swap_get()`, or
  the slot leaks.
- Swap-backed pages may be queued for encryption (`PQ_ENCRYPT`); do not read or
  reuse page contents while that is pending.

## Userland input
- `mmap`/`munmap`/`mprotect`/`minherit` paths (`uvm_mmap.c`) take attacker-
  controlled addresses and lengths. Verify page-rounding (`round_page()` /
  `trunc_page()`) and `addr + size` arithmetic cannot overflow `vaddr_t`/
  `vsize_t`, and that ranges are bounded against `VM_MIN_ADDRESS` /
  `VM_MAXUSER_ADDRESS` before use.
