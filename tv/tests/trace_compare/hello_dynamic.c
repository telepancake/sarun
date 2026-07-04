/* hello_dynamic.c — a dynamically linked test program.
 * Writes to stdout, opens a file, then exits 0. */
#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
int main(void) {
    int fd = open("/dev/null", O_RDONLY);
    if (fd >= 0) close(fd);
    printf("hello from dynamic\n");
    return 0;
}
