# OpenBSD VMM / VMD

Review guidance for the OpenBSD hypervisor: the in-kernel `vmm(4)` driver and
its userland counterpart vmd. The driver is split into a
machine-independent layer (`sys/dev/vmm/vmm.c`, `vmm.h` — VM/vcpu objects, the
ioctl interface, and `vm_run`) and a machine-dependent backend
(`sys/arch/amd64/amd64/vmm_machdep.c`, with the Intel VMX and AMD SVM logic).
vmm is amd64-only (`sys/arch/amd64/conf/files.amd64`).

## Architecture
- vmm provides the virtualisation primitives in the kernel; vmd (userland) owns
  device emulation and VM lifecycle and talks to vmm through ioctls and
  `vm_run`. Keep the trust boundary in mind: vmd is less trusted than the
  kernel, and guest input is untrusted.
- A vcpu runs in a tight enter/exit loop. Review the VM-exit handling: every
  exit reason must be handled or safely rejected; an unhandled or mis-decoded
  exit can leak host state or hang the cpu.

## Common pitfalls
- Reading guest-controlled values (exit qualification, GPRs, MSRs, port I/O) and
  using them without validation. Treat them as fully untrusted input.
- Copying structures across the vmm/vmd ioctl boundary without zeroing padding,
  leaking host kernel memory to a less-trusted process.
- Incorrect VMCS/VMCB field access (`vmread`/`vmwrite`), or accessing per-vcpu
  state on the wrong cpu; vcpu state is tied to the cpu it is loaded on.
- Missing checks on guest physical addresses / EPT or nested page-table walks
  leading to out-of-bounds host access.
- Reference counting and teardown of vm and vcpu objects: ensure a vcpu is not
  running when its VM is torn down.
