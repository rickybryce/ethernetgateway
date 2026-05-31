/* Host harness: run CCGMS's punter_recv() over stdin/stdout so it can talk to
 * the gateway's punter_send. All protocol logic comes from ccgms_punter.c
 * (unmodified). stderr carries CCGMS's own handshake trace. */
#include <stdio.h>
#include <unistd.h>
#include <poll.h>
#include <stdlib.h>

extern int punter_recv(void);

static long saved = 0;

int _inbyte(unsigned short timeout_ms) {
    struct pollfd p = { .fd = 0, .events = POLLIN };
    int r = poll(&p, 1, timeout_ms ? timeout_ms : 1);
    if (r <= 0) return -1;              /* timeout / error */
    unsigned char c;
    ssize_t n = read(0, &c, 1);
    if (n != 1) return -1;              /* EOF */
    return c;
}

void _outbyte(int c) {
    unsigned char b = (unsigned char)c;
    write(1, &b, 1);
}

int xfer_save_data(unsigned char *data, int length) {
    (void)data;
    saved += length;
    fprintf(stderr, "xfer_save_data: +%d (total %ld)\n", length, saved);
    return 1;                          /* success */
}

int main(void) {
    fprintf(stderr, "=== CCGMS punter_recv start ===\n");
    int ok = punter_recv();
    fprintf(stderr, "=== CCGMS punter_recv done: ret=%d, %ld bytes saved ===\n", ok, saved);
    return ok ? 0 : 1;
}
