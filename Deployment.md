# sectorwrap Deployment Guide

This guide describes how to build and run `sectorwrap` on an x86-64 Linux
machine with SCSI media exposed through `/dev/sg*`.

The commands in this document can destroy data. Never use the underlying
`/dev/sdX` or `/dev/sgX` device concurrently with sectorwrap.

## Requirements

### Operating system

The current ublk implementation requires:

- x86-64 Linux
- Linux kernel with ublk support enabled
- `/dev/ublk-control`
- SCSI generic support (`CONFIG_SCSI_GENERIC`)
- `io_uring` support
- root privileges, or equivalent access to the required device nodes

Check the kernel interfaces:

```sh
uname -m
uname -r
test -e /dev/ublk-control && echo "ublk control: available"
test -e /dev/sg0 && echo "SCSI generic: available"
```

If `/dev/ublk-control` does not exist, load the kernel module if your
distribution provides one:

```sh
sudo modprobe ublk_drv
```

If the module is unavailable, install or boot a kernel package that includes
ublk support. The exact package name is distribution-specific.

### Build packages

On Debian or Ubuntu:

```sh
sudo apt update
sudo apt install \
  build-essential \
  clang \
  libclang-dev \
  pkg-config \
  sg3-utils \
  e2fsprogs
```

On Fedora:

```sh
sudo dnf install \
  gcc \
  gcc-c++ \
  make \
  clang \
  clang-libs \
  libstdc++-devel \
  pkgconf-pkg-config \
  sg3_utils \
  e2fsprogs
```

Rust can be installed from the distribution or from
[rustup](https://rustup.rs/). The project requires a current stable Rust
toolchain:

```sh
rustc --version
cargo --version
```

## Build

Clone or copy the repository to the deployment machine:

```sh
git clone <repository-url> sectorwrap
cd sectorwrap
```

Build and test the safe core and manager:

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Build the Linux ublk worker:

```sh
cargo build --release --features ublk --bin sectorwrap-ublk
```

Build the manager:

```sh
cargo build --release --bin sectorwrap
```

The manager starts `sectorwrap-ublk` from the same directory as its own
executable. Install both binaries together:

```sh
sudo install -Dm755 target/release/sectorwrap \
  /usr/local/bin/sectorwrap
sudo install -Dm755 target/release/sectorwrap-ublk \
  /usr/local/bin/sectorwrap-ublk
```

Verify the installation:

```sh
sectorwrap --help
sectorwrap-ublk --help
```

## Device permissions

The simplest and safest first deployment is to run the manager as root:

```sh
sudo sectorwrap status
```

The process needs access to:

- `/dev/sg*`
- `/dev/ublk-control`
- `/run/sectorwrap`

Do not grant broad write access to `/dev/sg*` to untrusted users. If a
non-root service account is required, create a dedicated group and use
udev rules with an explicit allowlist of devices. Do not make all SCSI generic
devices world-writable.

## Discover devices

Before any write operation, inspect the devices:

```sh
sudo sg_inq /dev/sg5
sudo sg_readcap -l /dev/sg5
sudo sectorwrap status
```

For a 528-byte disk, `sg_readcap` must report:

```text
Logical block length=528 bytes
```

Also identify the corresponding block device, if one exists:

```sh
ls -l /sys/class/scsi_generic/sg5/device/block
lsblk -o NAME,PATH,SIZE,LOG-SEC,PHY-SEC,TYPE,FSTYPE,MOUNTPOINTS
```

Unmount filesystems and disable anything that may access the underlying
device before continuing:

```sh
sudo umount /dev/sdX1 2>/dev/null || true
sudo fuser -vm /dev/sdX /dev/sg5
```

The `fuser` output must not show an active filesystem, RAID device, LVM
volume, multipath device, or other consumer.

## Format a protected device

`sectorwrap format` initializes the complete physical media. It writes a zero
payload and newly generated protection data for every sector, so the operation
is destructive and can take a long time on large disks.

Interactive formatting:

```sh
sudo sectorwrap format /dev/sg5
```

Expected confirmation:

```text
! WARNING POTENTIAL DATA LOSS !
Formatting Devices
   - /dev/sg5
Confirm? (y/N)
```

For an automated deployment, use `--yes` only after independently validating
the device path:

```sh
sudo sectorwrap format --yes /dev/sg5
```

The format layout is selected from the reported physical sector size:

| Physical sector | User payload | Hidden protection |
|---:|---:|---|
| 520 bytes | 512 bytes | 4-byte LBA + 4-byte `0xff` padding |
| 528 bytes | 512 bytes | 4-byte LBA + 12-byte BCH parity |
| 4224 bytes | 4096 bytes | 4-byte LBA + 124-byte BCH parity |

Native 512-byte devices are passthrough devices and are intentionally refused
by `format`.

## Start a ublk mapping

Start one or more devices:

```sh
sudo sectorwrap start /dev/sg5
```

Multiple devices can be started in one command:

```sh
sudo sectorwrap start /dev/sg0 /dev/sg2 /dev/sg6
```

Expected output:

```text
/dev/sg0 -> /dev/ublkb0
/dev/sg2 -> /dev/ublkb1
/dev/sg6 -> /dev/ublkb2
```

The manager waits until the ublk device node exists before reporting success.
Mappings are recorded in:

```text
/run/sectorwrap/mappings
```

Check the result:

```sh
sudo sectorwrap status
lsblk -o NAME,PATH,SIZE,LOG-SEC,PHY-SEC,TYPE,FSTYPE,MOUNTPOINTS
```

Do not access `/dev/sg5` directly while its `/dev/ublkbN` mapping is active.

## Create and mount ext4

Replace `/dev/ublkb0` with the device reported by `sectorwrap start`:

```sh
sudo mkfs.ext4 -F -b 4096 /dev/ublkb0
sudo mkdir -p /mnt/sectorwrap-test
sudo mount /dev/ublkb0 /mnt/sectorwrap-test
```

Basic read/write test:

```sh
echo "sectorwrap test" | sudo tee /mnt/sectorwrap-test/test.txt
sync
sudo cat /mnt/sectorwrap-test/test.txt
sudo dmesg --color=never | tail -50
```

When testing is complete:

```sh
sudo umount /mnt/sectorwrap-test
sudo sectorwrap stop /dev/ublkb0
sudo sectorwrap status
```

`stop` requests a graceful worker shutdown and waits for it to exit before
removing the persistent mapping entry.

## Running from a checkout

For development or hardware bring-up, run the release binaries directly:

```sh
sudo ./target/release/sectorwrap status
sudo ./target/release/sectorwrap start /dev/sg5
```

Keep `sectorwrap` and `sectorwrap-ublk` in the same directory. The manager
locates the worker by replacing its own executable name with
`sectorwrap-ublk`.

## Troubleshooting

### `READ CAPACITY(16)` fails

Check the device and SCSI generic permissions:

```sh
sudo sg_inq /dev/sg5
sudo sg_readcap -l /dev/sg5
ls -l /dev/sg5
```

Some devices do not support `READ CAPACITY(16)` correctly. The current
implementation requires the 16-byte command and does not silently guess the
geometry.

### ublk device is not created

Check:

```sh
test -e /dev/ublk-control
sudo modprobe ublk_drv
sudo dmesg --color=never | tail -100
```

Run the worker manually to see its diagnostic output:

```sh
sudo /usr/local/bin/sectorwrap-ublk \
  --device /dev/sg5 \
  --sector-size 528
```

### A device is reported as already mapped

Inspect the state and worker:

```sh
sudo sectorwrap status
sudo cat /run/sectorwrap/mappings
ps -ef | grep '[s]ectorwrap-ublk'
```

Use `sectorwrap stop /dev/ublkbN` for a live mapping. Do not delete the state
file while a worker is still running.

### ext4 reports I/O errors

Stop using the filesystem immediately and inspect the worker log and kernel
messages:

```sh
sudo dmesg --color=never | tail -100
sudo sectorwrap status
```

An LBA mismatch or uncorrectable protection error is intentionally returned as
an I/O error. Do not bypass sectorwrap and mount the underlying device to
"repair" it; that would bypass the protection format.

## Operational safety checklist

Before `format`:

- Confirm the exact `/dev/sgX` path.
- Confirm the reported physical sector size.
- Confirm no `/dev/sdX`, partition, RAID, LVM, or multipath consumer is active.
- Confirm no sectorwrap mapping is using the device.
- Have a verified backup.

Before `mkfs`:

- Confirm that the target is `/dev/ublkbN`, not `/dev/sgX` or `/dev/sdX`.
- Confirm the expected logical capacity with `lsblk`.
- Confirm that the underlying device is not accessed separately.

