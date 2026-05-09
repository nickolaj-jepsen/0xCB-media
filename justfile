set shell := ["bash", "-cu"]

elf    := "target/thumbv6m-none-eabi/release/firmware"
uf2    := "target/firmware.uf2"
serial := "/dev/ttyACM0"
mount  := "/run/media/" + env_var('USER') + "/RPI-RP2"

default:
    @just --list

# Format the entire workspace (firmware included).
fmt:
    cargo fmt --all

# Same as `cargo fmt --all -- --check` in CI; non-zero exit on diff.
fmt-check:
    cargo fmt --all -- --check

# Mirror CI: clippy on host+proto, then on firmware (different target).
clippy:
    cargo clippy -p host -p proto --all-targets -- -D warnings
    cd firmware && cargo clippy --all-targets -- -D warnings

# Mirror CI: tests on host+proto only (firmware is no_std, `test = false`).
test:
    cargo test -p host -p proto --all-targets

# Build the firmware ELF (release).
build:
    cd firmware && cargo build --release

# Convert the firmware ELF to a UF2 image.
img: build
    elf2uf2-rs {{elf}} {{uf2}}

# Wait for RPI-RP2 BOOTSEL to mount (hold encoder + plug USB-C).
wait:
    # Require the path to actually be a mountpoint *and* writable —
    # udisks creates the dir before the FAT mount lands, and copying in
    # that window writes into the root-owned underlying dir.
    @echo "Waiting for {{mount}} ..."
    @until mountpoint -q {{mount}} 2>/dev/null && [ -w {{mount}} ]; do sleep 0.5; done
    @echo "RPI-RP2 mounted."

# Copy the UF2 onto the mounted RPI-RP2; board reboots into the new firmware.
copy:
    cp {{uf2}} {{mount}}/
    @echo "Flashed. Board should reboot into the new firmware."

# build → img → wait → copy → chmod-serial
flash: img wait copy chmod-serial

# Wait for the CDC ACM serial device to re-enumerate, then grant rw access.
chmod-serial:
    @echo "Waiting for {{serial}} ..."
    @until [ -e {{serial}} ]; do sleep 0.5; done
    sudo chmod 666 {{serial}}

# Run the host daemon in the foreground with debug logging.
host: chmod-serial
    RUST_LOG=debug cargo run --release -p host --bin 0xcb-media-host
