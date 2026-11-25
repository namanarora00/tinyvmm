// Guest-side fork trigger. Maps tinyvmm's control page through /dev/mem,
// writes "fork", then reads back which VM it is now running in:
// 0 = the original, i = child i. Prints that number.
// One process doing both keeps the measured gap tight: the read is the first
// thing the guest does after the fork.
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/mman.h>
#include <unistd.h>

#define TINYVMM_CTRL 0x09050000UL

int main(void) {
    int fd = open("/dev/mem", O_RDWR | O_SYNC);
    if (fd < 0) { perror("open /dev/mem"); return 1; }
    volatile uint32_t *ctrl = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, TINYVMM_CTRL);
    if (ctrl == MAP_FAILED) { perror("mmap"); return 1; }
    ctrl[0] = 1;              // fork request: an MMIO exit to the VMM
    uint32_t role = ctrl[1];  // who am I now?
    printf("%u\n", role);
    return 0;
}
