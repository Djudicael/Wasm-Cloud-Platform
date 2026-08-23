---
name: destroy-microvm-testbed
description: Safely destroy a local Wasm Cloud Platform Firecracker testbed created by the provisioning skill. Use after local deployment, smoke, integration, or chaos testing to stop recorded microVM processes, remove their TAP/bridge networking, and delete the testbed state file. Do not use against production infrastructure.
---

# Destroy the microVM testbed

Do not destroy a test environment merely because deployment checks completed. If the user wants interactive browser or API testing, leave it running until they explicitly request teardown.

1. Identify the exact state file used for provisioning. Default to `.vm-testbed-state.json`; do not guess another testbed's state path.
2. Run `bash scripts/vm/destroy-testbed.sh --state-file PATH` from the repository root.
3. If the state file is absent, report that the environment is already down. Do not kill processes or remove bridges by broad name matching.
4. If a `.services.json` companion exists, the script first validates and stops only its exact recorded HAProxy PID, then removes its generated config and log.
5. The script delegates VM teardown to `vm-testbed-cli down`, which only targets the recorded NATS, platform-node, and application-service VM PIDs and their recorded network.
6. Locally generated OIDC signing keys and credentials live on the Linux runtime filesystem rather than a `/mnt/*` checkout, because mounted Windows filesystems may not enforce requested Unix modes. The canonical script derives one exact directory from the absolute state-file path and removes only that directory.
7. Confirm that the VM and companion state files are gone and the recorded processes are no longer alive. Report any cleanup failure with retained state so an operator can retry safely.
