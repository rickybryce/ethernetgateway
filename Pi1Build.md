# Building Ethernet Gateway for the Raspberry Pi 1 / Pi Zero (ARMv6)

This guide explains how to **cross-compile** Ethernet Gateway for an original
Raspberry Pi (Pi 1 / Pi Zero / Pi Zero W / Compute Module 1) and deploy it as a
`systemd` service.

These boards use the **ARMv6** architecture (ARM1176JZF-S, VFPv2, **no NEON, no
Thumb-2**). They are too slow and RAM-starved to build this project natively —
a native build takes on the order of a day and thrashes swap — so we build on a
faster machine and copy the finished binary over.

> **This does not affect normal builds.** Everything here is driven by
> environment variables and an external toolchain. The repository, `Cargo.toml`,
> and `Cargo.lock` are untouched, so `cargo build` for x86-64, aarch64, ARMv7,
> Windows, etc. is exactly as before.

---

## Why it's not just `cargo build --target ...`

Three ARMv6-specific problems have to be solved, in this order:

1. **Rust target.** The correct target is `arm-unknown-linux-gnueabihf` (ARMv6
   hard-float). **Not** `armv7-unknown-linux-gnueabihf` — that emits ARMv7/NEON
   instructions that fault with `SIGILL` on an ARMv6 core.

2. **Crypto assembly is ARMv7-only.** The `ring` and `aws-lc-sys` crates ship
   pre-generated `.S` assembly that hard-codes `.arch armv7-a`, `.thumb`, and
   `.fpu neon`. Compiler flags cannot override those in-file directives, so the
   asm runs illegal instructions on ARMv6.
   - `aws-lc-sys` (used by `russh` → the **SSH / master-slave uplink**) has a
     portable-C fallback we can force on. **This is the important one.**
   - `ring` (used by `ureq`/`rustls` → the HTTPS features: web browser, AI chat,
     weather) has **no** ARMv6 support and no no-asm mode. Its code is left in
     the binary but is **dormant** — the gateway starts and runs fine; only the
     HTTPS-based features would fault if invoked. Telnet, all file-transfer
     protocols, the serial modem, the SSH server/gateway, and the master-slave
     uplink do **not** use `ring`.

3. **glibc / toolchain match.** A normal desktop cross-toolchain (e.g. Debian's
   `gcc-arm-linux-gnueabihf`) links against a **newer glibc** than the Pi ships
   and mixes its own C runtime startup files with the target's glibc. The result
   either refuses to load (`GLIBC_2.xx not found`) or **segfaults before `main`**
   at process startup — even for a trivial "hello world". The fix is to build
   with a self-contained toolchain whose glibc is **older than or equal to** the
   Pi's, and whose default CPU is ARMv6.

The clean solution to (1) and (3) is the **Bootlin `armv6-eabihf` glibc
toolchain**: its `gcc` defaults to ARMv6KZ (ARM1176 — exactly this hardware) and
it bundles a matching glibc 2.34 sysroot and C runtime. Problem (2) is solved by
forcing `aws-lc-sys` to build portable C.

---

## Target facts (reference)

Confirmed on the target hardware:

| Property | Value |
|----------|-------|
| CPU | `armv6l` — ARM1176JZF-S, VFPv2, **no NEON / no Thumb-2** |
| OS | Raspberry Pi OS / Raspbian Bookworm |
| glibc | 2.36 |
| systemd | 252 |

Check yours with `uname -m` (expect `armv6l`) and `ldd --version`.

---

## Prerequisites (build host)

A Linux x86-64 machine with:

- A Rust toolchain (`rustc`, `cargo`).
- The ARMv6 Rust standard library:
  ```sh
  rustup target add arm-unknown-linux-gnueabihf
  ```
- `curl`, `tar`.
- SSH access to the Pi. (`sshpass` is optional; any SSH/`scp` method works.)

---

## Step 1 — Get the Bootlin ARMv6 toolchain

Pick a Bootlin `armv6-eabihf --glibc` release whose glibc is **≤ the Pi's**
(2.36 here). We use `2021.11-1` (gcc 10.3, **glibc 2.34**, default arch
ARMv6KZ), which is a safe margin below 2.36.

```sh
mkdir -p ~/armv6-bootlin && cd ~/armv6-bootlin
curl -LO https://toolchains.bootlin.com/downloads/releases/toolchains/armv6-eabihf/tarballs/armv6-eabihf--glibc--stable-2021.11-1.tar.bz2
tar xf armv6-eabihf--glibc--stable-2021.11-1.tar.bz2
```

This yields:

```
BR=~/armv6-bootlin/armv6-eabihf--glibc--stable-2021.11-1
GCC=$BR/bin/arm-buildroot-linux-gnueabihf-gcc     # the linker + C compiler
AR=$BR/bin/arm-buildroot-linux-gnueabihf-ar
STRIP=$BR/bin/arm-buildroot-linux-gnueabihf-strip
SR=$BR/arm-buildroot-linux-gnueabihf/sysroot      # matching glibc 2.34 sysroot
```

> Newer Bootlin releases (2023+/2024+) ship glibc **newer** than 2.36 and will
> re-introduce the "GLIBC_2.xx not found" problem. Stick to an older release.

## Step 2 — Add `libudev` to the toolchain sysroot

The `serialport` crate needs `libudev`, which isn't in the Bootlin sysroot.
Copy the Pi's own `libudev` into the sysroot and provide a pkg-config file.

Grab `libudev.so.1.*` from the Pi (`/usr/lib/arm-linux-gnueabihf/libudev.so.1.*`)
by any means (scp), then:

```sh
cp libudev.so.1.7.5 "$SR/usr/lib/"          # version may differ; adjust name
ln -sf libudev.so.1.7.5 "$SR/usr/lib/libudev.so.1"
ln -sf libudev.so.1     "$SR/usr/lib/libudev.so"
mkdir -p "$SR/usr/lib/pkgconfig"
cat > "$SR/usr/lib/pkgconfig/libudev.pc" <<'EOF'
prefix=/usr
exec_prefix=/usr
libdir=/usr/lib
includedir=/usr/include
Name: libudev
Description: libudev
Version: 252
Libs: -L${libdir} -ludev
Cflags: -I${includedir}
EOF
```

## Step 3 — Cross-build (release)

From the repository root:

```sh
env \
  PKG_CONFIG_ALLOW_CROSS=1 \
  PKG_CONFIG_SYSROOT_DIR="$SR" \
  PKG_CONFIG_PATH="$SR/usr/lib/pkgconfig" \
  CARGO_TARGET_ARM_UNKNOWN_LINUX_GNUEABIHF_LINKER="$GCC" \
  CC_arm_unknown_linux_gnueabihf="$GCC" \
  AR_arm_unknown_linux_gnueabihf="$AR" \
  CARGO_PROFILE_RELEASE_OPT_LEVEL=2 \
  AWS_LC_SYS_NO_ASM=1 \
  AWS_LC_SYS_CMAKE_BUILDER=0 \
  cargo build --release --target arm-unknown-linux-gnueabihf
```

Why each aws-lc variable matters:

- `AWS_LC_SYS_NO_ASM=1` — build portable C instead of the ARMv7/Thumb-2 asm.
- `AWS_LC_SYS_CMAKE_BUILDER=0` — `NO_ASM` otherwise auto-selects the *cmake*
  builder, which only permits no-asm at `opt-level 0`. Forcing the **cc**
  builder allows no-asm at `opt-level ≤ 2`.
- `CARGO_PROFILE_RELEASE_OPT_LEVEL=2` — keeps the whole release build at
  opt-level 2 so the cc builder accepts `NO_ASM`. (Env-only; the repo's release
  profile is unchanged.)

### Verify the result

```sh
B=target/arm-unknown-linux-gnueabihf/release/ethernetgateway
# No glibc symbol newer than the Pi's 2.36 (expect nothing >= 2.35 here):
arm-linux-gnueabihf-readelf -V "$B" | grep -oE 'GLIBC_[0-9.]+' | sort -uV | tail
# No ARMv7 / Thumb-2 objects from aws-lc (expect zero):
find target/arm-unknown-linux-gnueabihf/release/build -path '*aws-lc-sys*' -name '*.o' \
  -exec sh -c 'readelf -A "$1" 2>/dev/null | grep -qE "v7|Thumb-2" && echo "V7: $1"' _ {} \;
```

## Step 4 — Strip and copy to the Pi

The built binary is named `ethernetgateway`. Because `/home/ricky/ethernetgateway`
may already exist as a source directory on the Pi, deploy the binary under a
**different** name — `ethernet-gateway` (hyphenated):

```sh
"$STRIP" -o /tmp/ethernet-gateway "$B"
scp /tmp/ethernet-gateway ricky@<PI_IP>:/home/ricky/ethernet-gateway
ssh ricky@<PI_IP> chmod +x /home/ricky/ethernet-gateway
```

*(Use your own Pi hostname/IP and login password — not stored here.)*

## Step 5 — Generate and adjust the config

Let the binary create its own `egateway.conf`, then disable the desktop GUI
(there is no display on a headless Pi):

```sh
ssh ricky@<PI_IP> 'cd /home/ricky && timeout 5 ./ethernet-gateway; \
  sed -i "s/^enable_console = true/enable_console = false/" egateway.conf'
```

`enable_console = false` stops the binary from attempting to open the egui
window on boot. (Even with it left on, the gateway falls back to headless mode,
but it logs a harmless GUI error each start.)

Enable other features (SSH server, serial ports, master-slave slave uplink,
etc.) by editing `egateway.conf` as usual.

## Step 6 — Install the systemd service

Create `/etc/systemd/system/ethernet-gateway.service`:

```ini
[Unit]
Description=Ethernet Gateway (telnet/SSH file-transfer gateway)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=ricky
Group=ricky
WorkingDirectory=/home/ricky
ExecStart=/home/ricky/ethernet-gateway
Restart=on-failure
RestartSec=5
# SIGHUP reloads config in place (see src/main.rs signal handling)
ExecReload=/bin/kill -HUP $MAINPID
KillSignal=SIGTERM

[Install]
WantedBy=multi-user.target
```

Then, on the Pi:

```sh
sudo systemctl daemon-reload
sudo systemctl enable ethernet-gateway
sudo systemctl start ethernet-gateway
systemctl status ethernet-gateway      # expect: active (running)
```

The service runs as `ricky` with the working directory `/home/ricky`, so
`egateway.conf`, the `transfer/` directory, and `ethernet_ssh_host_key` all live
in the home directory. All listening ports (telnet 2323, SSH 2222, web 8080) are
> 1024, so no root/capabilities are needed.

## Step 7 — Verify reboot auto-start

```sh
sudo reboot
# wait ~1-2 minutes, then:
ssh ricky@<PI_IP> 'systemctl is-active ethernet-gateway; ss -ltn | grep 2323'
```

You should see `active` and the telnet port listening.

---

## Runtime notes and caveats

- **Crypto performance.** With `AWS_LC_SYS_NO_ASM`, the SSH crypto uses the
  portable C implementation, which is noticeably slower than the hand-tuned
  assembly. On a single-core ARMv6 this means SSH handshakes are sluggish, but
  the master-slave serial uplink is low-bandwidth so it is fine in practice.
- **HTTPS features (`ring`).** The web browser, AI chat, and weather features
  reach the internet over TLS via `ring`, which has no ARMv6 support. They are
  compiled in but will fault if used on this hardware. Everything else —
  telnet, XMODEM/YMODEM/ZMODEM/Kermit/Punter, the serial modem emulator, the
  Gateway Shell, the SSH server/gateway, and the master-slave uplink — works.
- **Validated.** On the target hardware the service starts on boot, the telnet
  server accepts connections, and an SSH key exchange (Ed25519 host key
  generation + curve25519 ECDH + ciphers/MACs, all portable C) completes
  successfully — confirming the crypto path used by the uplink runs without
  illegal-instruction faults.
