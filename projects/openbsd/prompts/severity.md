# Severity Levels

When identifying issues, you must assign a severity level to each finding.
Treat this task seriously. Don't unnecessarily raise the priority: critical
issues must be critical, high issues must be very damaging. Use Medium as the
default and lower or raise it based on the "Question to ask" and the examples.

## Calibrating the level (reason before you label)

State this reasoning at the start of the severity_explanation so the label is
auditable.

- Consequence, blast radius, and reversibility: what actually happens if the
  bug triggers (memory or data corruption, panic, info leak, resource leak,
  incorrect result, performance, or other), how far it reaches, and whether it
  can be undone. Irreversible damage outranks recoverable damage: a panic clears
  on reboot, corruption written to disk does not. This is the starting point for
  the level.
- Triggering path: lay out the concrete path that reaches the bug, naming the
  preconditions a caller or input must satisfy. If you cannot, because it rests
  on an unproven assumption or on an ABI, register, or convention you might be
  misreading, still report the finding and mark it speculative.
- Reachability: if the bug is reachable from untrusted, remote, or unprivileged
  input (a crafted packet, a syscall or sysctl argument, an ioctl, a mounted
  filesystem), raise the level. Do not lower a finding because you believe it is
  unreachable: reachability is hard to establish from a diff, and a wrong call
  buries a real bug. If you cannot establish reachability, leave the level on
  consequence alone.

A speculative finding is the one case where the level is capped, at Medium,
because the open question is whether the bug is real at all. Only the severity
is capped: nothing is dropped for being speculative. This is the only reason to
lower a level. Reachability never does.

## Critical
- **Definition**: Issues that cause data loss, memory corruption, or security vulnerabilities.
- **Question to ask**: Would carrying on be worse than panicking -- memory or
  data corruption, kernel memory disclosed, a mitigation defeated? Then it is
  critical. So is a panic that untrusted, remote, or unprivileged input can
  trigger. If none of that holds, it is **not** critical.
- **Examples**:
    - Security vulnerability (e.g. a defeat of pledge(2)/unveil(2), W^X, or privilege separation).
    - Data or filesystem corruption.
    - Memory corruption (buffer overflow, use-after-free, double free of an mbuf or pool item).
    - Kernel panic reachable from userspace or remotely (e.g. via a crafted packet or syscall).
    - Information leak of uninitialised kernel memory to userspace (copyout of a partially filled struct).
    - Userland-visible ABI breakage of a syscall or sysctl without a compat path.

## High
- **Definition**: Serious issues that can bring the system down or make it unusable.
- **Question to ask**: Can the system panic, hang, or become unusable on its
  own? Then it is high; if untrusted input is what reaches it, it is critical.
- **Examples**:
    - Kernel panic (NULL deref, "locking against myself", kernel diagnostic assertion).
    - Logic errors leading to incorrect functional behaviour.
    - Resource leaks (pool items, mbufs, vnode references, held rwlocks).
    - Violation of core locking rules (sleeping while holding a mutex or at raised spl, missing NET_LOCK).
    - Significant performance or scalability regression.

## Medium
- **Definition**: Recoverable issues or non-critical regressions.
- **Examples**:
    - Resource leaks on cold/error paths.
    - Inefficient or overly coarse locking.
    - Incorrect statistics or counters.
    - Meaningful mismatch between the commit message and the code.
    - Non-critical performance regressions.

## Low
- **Definition**: Naming, style, and KNF issues.
- **Question to ask**: Is there any visible real-world effect? If not, it is low.
- **Examples**:
    - KNF / style(9) violations (indentation, brace placement, function ordering).
    - Typos in comments or commit messages.
    - Confusing variable naming.
    - Negligible performance differences.
    - Unnecessary complexity or missing comments.
