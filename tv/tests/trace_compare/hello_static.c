/* hello_static.c — a statically linked test program.
 * Writes to stdout so traces can capture STDOUT events,
 * opens a file so OPEN events appear, then exits 0. */
#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
int main(void) {
    int fd = open("/dev/null", O_RDONLY);
    if (fd >= 0) close(fd);
    printf("hello from static\n");
    return 0;
}
