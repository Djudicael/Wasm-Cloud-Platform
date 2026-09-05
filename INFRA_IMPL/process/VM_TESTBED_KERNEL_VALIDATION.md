# VM-testbed guest-kernel validation

Status: test infrastructure policy  
Scope: Firecracker guests used to exercise the Wasm Cloud Platform in local or
CI environments resembling a small VPS deployment

The platform does not ship, install, or own a production host or guest kernel.
The kernel built by `scripts/vm/build-kernel.sh` exists only to make the
Firecracker testbed reproducible and capable of exercising networking, cgroups,
BTF, and the platform's eBPF functionality.

## Testbed compatibility unit

The machine-readable testbed pins are in `scripts/vm/kernel-testbed.env`. The
current x86_64 unit combines a checksum-pinned Linux LTS source archive,
checksum-pinned Firecracker release, Firecracker guest configuration pinned to
an immutable commit, and `scripts/vm/kernel/testbed-x86_64.fragment`.

The builder resolves Kconfig and verifies the boot, virtio, cgroup, tracing,
BTF, and eBPF features needed by the tests. These checks prevent a misleading
test pass caused by a broken or incomplete test kernel. They do not establish a
kernel baseline for a production VPS and are not part of the platform release.

## Where the checks run

- Developers may run them in Linux or WSL2 when rebuilding testbed images.
- `.github/workflows/vm-testbed-nightly.yml` may exercise the resulting topology
  on a dedicated self-hosted Linux/KVM runner.
- The platform release workflow does not build, package, attest, or publish the
  Firecracker binary, guest kernel, rootfs images, or service microVMs.

The patch-status and static/runtime audit scripts under `scripts/vm/` are
testbed maintenance and diagnostic helpers. They keep the emulated environment
credible and record what the selected host virtualization layer exposes.

## Production-host boundary

A production operator chooses and maintains the VPS, bare-metal host, VM image,
kernel, firmware, microcode, and virtualization layer. Before deploying
`wasm-node`, verify the actual host against the platform requirements:

- supported CPU architecture and Linux userspace;
- required ports, DNS, filesystem and clock behavior;
- cgroup v2 and resource controls used by the deployment;
- BTF, tracefs/perf facilities and narrowly granted capabilities when eBPF is
  enabled;
- sufficient disk, memory, file descriptors and process limits;
- the operator's OS patching and CPU-vulnerability policy.

KVM and Firecracker compatibility are required only when the operator chooses
to run the platform inside Firecracker. A native VPS installation does not need
KVM merely because this repository uses Firecracker for integration testing.

Runtime CPU-vulnerability output observed through WSL or the local microVM is
useful evidence about that test machine only. It is neither a platform defect
nor proof about a different VPS CPU class.
