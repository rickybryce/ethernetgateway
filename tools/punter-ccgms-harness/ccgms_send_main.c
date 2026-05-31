/* Host harness: run CCGMS's punter_xmit() (sender) over stdin/stdout so the
 * gateway's punter_receive can receive from it. All protocol logic comes from
 * ccgms_punter.c unmodified. */
#include <stdio.h>
#include <unistd.h>
#include <poll.h>
#include <string.h>

extern int punter_xmit(void *data, int data_len);

int _inbyte(unsigned short timeout_ms) {
    struct pollfd p = { .fd = 0, .events = POLLIN };
    int r = poll(&p, 1, timeout_ms ? timeout_ms : 1);
    if (r <= 0) return -1;
    unsigned char c;
    if (read(0, &c, 1) != 1) return -1;
    return c;
}

void _outbyte(int c) {
    unsigned char b = (unsigned char)c;
    write(1, &b, 1);
}

int xfer_save_data(unsigned char *data, int length) { (void)data; (void)length; return 1; }

int main(void) {
    /* 300 bytes, same generator as the download test so we can eyeball it */
    static unsigned char data[300];
    for (int i = 0; i < 300; i++) data[i] = (unsigned char)(i * 7 + 1);
    fprintf(stderr, "=== CCGMS punter_xmit start (%d bytes) ===\n", (int)sizeof(data));
    int ok = punter_xmit(data, sizeof(data));
    fprintf(stderr, "=== CCGMS punter_xmit done: ret=%d ===\n", ok);
    return ok ? 0 : 1;
}
