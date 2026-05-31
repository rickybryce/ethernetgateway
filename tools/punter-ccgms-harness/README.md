# Punter CCGMS interop harness

Runs the gateway's Punter (C1) implementation against the **real CCGMS receiver
and sender logic**, over a clean byte pipe (the child's stdin/stdout), so we can
verify interop without a C64 in the loop.

CCGMS's Punter code is `test/punter.c` from
[`mist64/ccgmsterm`](https://github.com/mist64/ccgmsterm) (BSD-2-Clause, by Per
Olofsson, modified by Michael Steil to match the CCGMS variant). It is **not**
vendored here — fetch it at build time. The two `*_main.c` files in this
directory are the only local glue: they provide `_inbyte`/`_outbyte`/
`xfer_save_data`/`main` over stdio.

## Build

```sh
cd tools/punter-ccgms-harness
curl -fsSLO https://raw.githubusercontent.com/mist64/ccgmsterm/main/test/punter.c
gcc -O0 -g punter.c ccgms_recv_main.c -o ccgms_recv   # CCGMS receiver
gcc -O0 -g punter.c ccgms_send_main.c -o ccgms_send   # CCGMS sender
```

## Run

The interop tests in `src/punter.rs` are env-gated (skipped unless the binary
path is set):

```sh
# download direction: gateway punter_send -> CCGMS receiver
CCGMS_RECV_BIN=tools/punter-ccgms-harness/ccgms_recv \
  cargo test ccgms_real_receiver_interop -- --nocapture

# upload direction: CCGMS sender -> gateway punter_receive
CCGMS_SEND_BIN=tools/punter-ccgms-harness/ccgms_send \
  cargo test ccgms_real_sender_interop -- --nocapture
```

Both should complete a 300-byte transfer (type SEQ).

## Caveat

`test/punter.c`'s `punter_recv_string(NULL, ...)` dereferences a NULL `sendstring`
if a read **times out**, so the harness only stays alive while the peer responds
promptly (which a correct gateway does). It models the C1 wire protocol, not the
C64's bit-banged-serial timing or its disk-side file open — those need real
hardware.
