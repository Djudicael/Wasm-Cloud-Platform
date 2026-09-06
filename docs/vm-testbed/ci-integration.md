# CI/CD Integration for MicroVM Testbed

This guide covers integrating the microVM testbed into your CI/CD pipeline for automated pre-production validation.

## Overview

The microVM testbed is designed to run as a **nightly or pre-release** validation step, complementing the fast native-process E2E tests that run on every PR.

CI automation that provisions through the shell workflow must assign a unique
state file per job and pass that exact path to provision, deploy, validation,
and teardown scripts. Never clean Firecracker processes, TAP devices, bridges,
or `/tmp` directories by broad name patterns. The Rust `vm-testbed` integration
tests manage their own fixtures; do not mix their lifecycle with an unrelated
shell-script state file.

```
┌─────────────────────────────────────────────────────────────────┐
│                        CI Pipeline                               │
├─────────────────────────────────────────────────────────────────┤
│  On Every PR:                                                    │
│    1. required workspace checks with the pinned Rust toolchain   │
│    2. targeted native and WASI application tests                 │
│    [Fast: ~5 minutes]                                            │
├─────────────────────────────────────────────────────────────────┤
│  Nightly / Pre-Release:                                          │
│    1. Build VM images                                            │
│    2. provision/deploy tests or vm-testbed integration tests     │
│    3. Run chaos test suite                                       │
│    4. Generate report                                            │
│    [Slow: ~30 minutes]                                           │
└─────────────────────────────────────────────────────────────────┘
```

## GitHub Actions

### Self-Hosted Runner (Recommended)

GitHub Actions free runners do not expose `/dev/kvm`. You need a self-hosted runner on a Linux machine with KVM.

#### Runner Setup

```bash
# On your Linux machine (bare metal or VM with nested virtualization)

# 1. Create a dedicated user
sudo useradd -m github-runner
sudo usermod -aG kvm github-runner

# 2. Install GitHub Actions runner
# Follow: https://github.com/actions/runner/blob/main/docs/start/README.md

# 3. Install dependencies
sudo apt-get update
sudo apt-get install -y build-essential bc bison flex libssl-dev libelf-dev

# 4. Install Rust (as github-runner)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env
rustup target add wasm32-wasip2

# 5. Install the checksum-pinned Firecracker version from kernel-testbed.env
sudo ./scripts/vm/install-firecracker.sh

# 6. Configure runner
cd ~/actions-runner
./config.sh --url https://github.com/YOUR_ORG/YOUR_REPO --token YOUR_TOKEN

# 7. Install as service
sudo ./svc.sh install
sudo ./svc.sh start
```

#### Workflow File

```yaml
# .github/workflows/vm-testbed.yml
name: MicroVM Testbed

on:
  schedule:
    # Run nightly at 2 AM UTC
    - cron: '0 2 * * *'
  workflow_dispatch:
    inputs:
      chaos_level:
        description: 'Chaos test level to run'
        required: true
        default: 'all'
        type: choice
        options:
          - all
          - l2
          - l3
          - l5

jobs:
  vm-testbed:
    runs-on: [self-hosted, linux, kvm]
    timeout-minutes: 60

    steps:
      - name: Checkout
        uses: actions/checkout@v4

      - name: Cache VM images
        uses: actions/cache@v4
        with:
          path: |
            ./assets/vmlinux-6.18
            ./assets/nats-rootfs.ext4
            ./assets/wasm-node-rootfs.ext4
          key: vm-images-${{ hashFiles('scripts/vm/**') }}

      - name: Build wasm-node binaries
        run: cargo build --release --bin wasm-node --bin wasm-ctl

      - name: Build test Wasm app
        run: |
          RUSTFLAGS='--cfg tokio_unstable' \
            cargo build --manifest-path apps/hello-axum/Cargo.toml \
            --target wasm32-wasip2 --release

      - name: Build VM images (if not cached)
        run: |
          if [[ ! -f ./assets/vmlinux-6.18 ]]; then
            ./scripts/vm/build-all-images.sh
          fi

      - name: Run single-node deploy test
        run: |
          sudo cargo test -p vm-testbed --test single_node_deploy -- --nocapture
        env:
          FIRECRACKER_PATH: /usr/local/bin/firecracker

      - name: Run L2 chaos test
        if: github.event.inputs.chaos_level == 'all' || github.event.inputs.chaos_level == 'l2'
        run: |
          sudo cargo test -p vm-testbed --test vm_chaos test_vm_kill_and_restart -- --nocapture

      - name: Run L5 chaos test
        if: github.event.inputs.chaos_level == 'all' || github.event.inputs.chaos_level == 'l5'
        run: |
          sudo cargo test -p vm-testbed --test vm_chaos test_vm_network_partition -- --nocapture

      - name: Upload logs on failure
        if: failure()
        uses: actions/upload-artifact@v4
        with:
          name: vm-logs
          path: |
            /tmp/vm-testbed-*/firecracker.log
            /tmp/vm-testbed-*/metrics.json

      - name: Cleanup
        if: always()
        run: |
          # The Rust integration harness owns and tears down its exact fixtures.
          # A shell-provisioned job must call destroy-testbed.sh with its own
          # recorded state file instead of using broad process/network cleanup.
          true
```

### GitHub Actions with Nested Virtualization (Alternative)

Some GitHub-hosted larger runners support nested virtualization. This is more expensive but requires no infrastructure maintenance.

```yaml
jobs:
  vm-testbed:
    runs-on: ubuntu-latest-4-cores  # Or larger
    steps:
      - name: Enable KVM
        run: |
          sudo apt-get update
          sudo apt-get install -y qemu-kvm
          sudo chmod 666 /dev/kvm
          
      - name: Rest of setup...
        # ... same as above
```

**Note**: Check GitHub's documentation for which runner sizes support nested virtualization. This changes frequently.

## GitLab CI

```yaml
# .gitlab-ci.yml
stages:
  - test
  - vm-testbed

variables:
  CARGO_HOME: $CI_PROJECT_DIR/.cargo
  RUSTFLAGS: "--cfg tokio_unstable"

cache:
  paths:
    - .cargo/
    - target/
    - assets/

unit-tests:
  stage: test
  image: rust:1.97.1
  script:
    - cargo check --workspace --all-targets --exclude http-hello-component --exclude wasi-grpc-echo

vm-testbed:
  stage: vm-testbed
  image: rust:1.97.1
  tags:
    - kvm  # Requires GitLab runner with KVM
  script:
    - apt-get update && apt-get install -y qemu-kvm iproute2 iptables
    - ./scripts/vm/install-firecracker.sh
    - cargo build --release --bin wasm-node
    - ./scripts/vm/build-all-images.sh
    - sudo cargo test -p vm-testbed -- --nocapture
  only:
    - schedules
    - tags
```

## AWS CodeBuild

```yaml
# buildspec.yml
version: 0.2

env:
  variables:
    FIRECRACKER_VERSION: "v1.16.1"

phases:
  install:
    commands:
      - apt-get update
      - apt-get install -y qemu-kvm firecracker
      - chmod 666 /dev/kvm
      - curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
      - source $HOME/.cargo/env

  pre_build:
    commands:
      - cargo build --release --bin wasm-node

  build:
    commands:
      - ./scripts/vm/build-all-images.sh
      - sudo cargo test -p vm-testbed -- --nocapture

artifacts:
  files:
    - "**/*"
  discard-paths: no
```

**Note**: Use a `c5.metal` or `m5.metal` instance type for bare-metal KVM access.

## Jenkins

```groovy
// Jenkinsfile
pipeline {
    agent {
        label 'kvm && linux'
    }
    
    options {
        timeout(time: 60, unit: 'MINUTES')
    }
    
    stages {
        stage('Build') {
            steps {
                sh 'cargo build --release --bin wasm-node'
                sh 'RUSTFLAGS="--cfg tokio_unstable" cargo build --manifest-path apps/hello-axum/Cargo.toml --target wasm32-wasip2 --release'
            }
        }
        
        stage('Build VM Images') {
            steps {
                sh './scripts/vm/build-all-images.sh'
            }
        }
        
        stage('Run VM Tests') {
            steps {
                sh 'sudo cargo test -p vm-testbed -- --nocapture'
            }
        }
    }
    
    post {
        always {
            sh '''
                # The Rust integration harness owns its exact fixtures.
                # Shell-provisioned stages must retain and destroy by state file.
                true
            '''
        }
        failure {
            archiveArtifacts artifacts: '/tmp/vm-testbed-*/**', allowEmptyArchive: true
        }
    }
}
```

## Test Reports

Generate structured test reports for tracking over time:

```bash
# Run tests with JSON output
cargo test -p vm-testbed -- --format json > test-results.json

# Or use a custom reporter (see crates/vm-testbed/src/reporter.rs)
cargo test -p vm-testbed -- --nocapture 2>&1 | tee test-output.log
```

### Example Report Format

```json
{
  "test_run_id": "vm-testbed-2024-01-15-02-00-00",
  "timestamp": "2024-01-15T02:00:00Z",
  "results": [
    {
      "test": "test_single_node_deploy",
      "status": "passed",
      "duration_ms": 45000,
      "vm_count": 2,
      "kernel_version": "6.18.48"
    },
    {
      "test": "test_vm_kill_and_restart",
      "status": "passed",
      "duration_ms": 62000,
      "ttr_seconds": 15.2,
      "failure_level": "L2"
    }
  ]
}
```

## Cost Optimization

### Caching VM Images

VM images are large (~200-500 MB). Cache them between runs:

```yaml
- name: Cache VM images
  uses: actions/cache@v4
  with:
    path: ./assets/
    key: vm-images-${{ hashFiles('scripts/vm/**', 'Cargo.lock') }}
```

### Incremental Builds

Only rebuild what changed:

```bash
# Check if kernel config changed
if git diff HEAD~1 -- scripts/vm/build-kernel.sh | grep -q .; then
    ./scripts/vm/build-kernel.sh
fi

# Check if wasm-node binary changed
if git diff HEAD~1 -- crates/node/src/ | grep -q .; then
    cargo build --release --bin wasm-node
    ./scripts/vm/build-node-rootfs.sh
fi
```

### Parallel Test Execution

Run independent tests in parallel:

```bash
# Terminal 1: L2 tests
cargo test -p vm-testbed --test vm_chaos test_l2 -- --nocapture

# Terminal 2: L5 tests
cargo test -p vm-testbed --test vm_chaos test_l5 -- --nocapture
```

## Security Considerations

### Running with sudo

The tests need `CAP_NET_ADMIN` for TAP creation. Options:

1. **sudo** (simplest, but gives full root)
2. **Capabilities** (more secure):
   ```bash
   sudo setcap cap_net_admin+eip $(which cargo)
   ```
3. **Unprivileged TAP** (if kernel supports it):
   ```bash
   # Requires CONFIG_TUN=y and proper /dev/net/tun permissions
   ```

### Isolation

Run each test in a clean environment:

```bash
# Use unique bridge names per job
export VM_BRIDGE_NAME="br-ci-$CI_JOB_ID"

# Cleanup is handled by ClusterFixture::teardown(), but add extra safety:
trap 'sudo ip link del $VM_BRIDGE_NAME 2>/dev/null || true' EXIT
```

## Monitoring

Track test metrics over time:

| Metric | How to Measure |
|--------|---------------|
| VM boot time | Firecracker metrics JSON |
| TTR (Time To Recovery) | Test code measures injection to health |
| eBPF load time | Kernel logs inside VM |
| Memory usage | Firecracker balloon stats |
| Network throughput | iperf3 between VMs |

## Troubleshooting CI Failures

### Common Issues

| Issue | Cause | Fix |
|-------|-------|-----|
| `/dev/kvm not found` | Runner doesn't have KVM | Use self-hosted runner with KVM |
| `firecracker: command not found` | Not in PATH | Set `FIRECRACKER_PATH` env var |
| `TAP device creation failed` | Missing CAP_NET_ADMIN | Run with sudo or set capabilities |
| `VM did not become healthy` | Slow boot on overloaded CI | Increase timeout |
| `Port already in use` | A recorded testbed was not torn down | Run `scripts/vm/destroy-testbed.sh --state-file PATH` with the same state file used for provisioning |

### Debug Mode

Enable verbose logging:

```bash
export RUST_LOG=debug
cargo test -p vm-testbed -- --nocapture
```

Collect all logs on failure:

```bash
# In CI post-failure step:
tar czf vm-logs.tar.gz /tmp/vm-testbed-*/
```
