/* Small static Linux guest probe for host-independent QEMU integration tests.
 *
 * The macOS host tree contains Mach-O tools, so a Linux appliance cannot use
 * host curl/getent merely because virtio-fs makes those paths visible.  This
 * fixture is cross-compiled with Zig/musl for the selected guest architecture.
 */
#include <arpa/inet.h>
#include <errno.h>
#include <netdb.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <unistd.h>

static int interface_state(const char *expected) {
    struct stat status;
    int present = stat("/sys/class/net/eth0", &status) == 0;
    int want_present = strcmp(expected, "present") == 0;
    if (present != want_present) {
        fprintf(stderr, "eth0 is %s, expected %s\n",
                present ? "present" : "absent", expected);
        return 1;
    }
    puts(present ? "ETH0_PRESENT" : "ETH0_ABSENT");
    return 0;
}

static int resolve_name(const char *name) {
    struct addrinfo hints = {0};
    struct addrinfo *addresses = NULL;
    char text[INET6_ADDRSTRLEN];
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    int result = getaddrinfo(name, NULL, &hints, &addresses);
    if (result != 0) {
        fprintf(stderr, "resolve %s: %s\n", name, gai_strerror(result));
        return 1;
    }
    struct sockaddr_in *address = (struct sockaddr_in *)addresses->ai_addr;
    if (inet_ntop(AF_INET, &address->sin_addr, text, sizeof(text)) == NULL) {
        perror("inet_ntop");
        freeaddrinfo(addresses);
        return 1;
    }
    printf("%s %s\n", text, name);
    freeaddrinfo(addresses);
    return 0;
}

static int udp_send(const char *host, const char *port) {
    struct addrinfo hints = {0};
    struct addrinfo *addresses = NULL;
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_DGRAM;
    int result = getaddrinfo(host, port, &hints, &addresses);
    if (result != 0) {
        fprintf(stderr, "resolve %s:%s: %s\n", host, port, gai_strerror(result));
        return 1;
    }
    int fd = socket(addresses->ai_family, addresses->ai_socktype, addresses->ai_protocol);
    static const char payload[] = "SARUN_UDP_PROBE";
    ssize_t sent = fd < 0 ? -1 : sendto(fd, payload, sizeof(payload) - 1, 0,
                                        addresses->ai_addr, addresses->ai_addrlen);
    if (sent != (ssize_t)(sizeof(payload) - 1)) {
        perror(fd < 0 ? "socket" : "sendto");
        freeaddrinfo(addresses);
        if (fd >= 0) close(fd);
        return 1;
    }
    freeaddrinfo(addresses);
    close(fd);
    printf("UDP_SENT %s %s\n", host, port);
    return 0;
}

static int http_get(const char *host, const char *port, const char *marker) {
    struct addrinfo hints = {0};
    struct addrinfo *addresses = NULL;
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    int result = getaddrinfo(host, port, &hints, &addresses);
    if (result != 0) {
        fprintf(stderr, "resolve %s:%s: %s\n", host, port, gai_strerror(result));
        return 1;
    }
    int fd = socket(addresses->ai_family, addresses->ai_socktype, addresses->ai_protocol);
    if (fd < 0 || connect(fd, addresses->ai_addr, addresses->ai_addrlen) != 0) {
        perror("connect");
        freeaddrinfo(addresses);
        if (fd >= 0) close(fd);
        return 1;
    }
    freeaddrinfo(addresses);
    const char request[] = "GET / HTTP/1.0\r\nHost: appliance.test\r\n\r\n";
    if (write(fd, request, sizeof(request) - 1) != (ssize_t)(sizeof(request) - 1)) {
        perror("write");
        close(fd);
        return 1;
    }
    char response[16384];
    size_t used = 0;
    for (;;) {
        ssize_t count = read(fd, response + used, sizeof(response) - 1 - used);
        if (count < 0) {
            if (errno == EINTR) continue;
            perror("read");
            close(fd);
            return 1;
        }
        if (count == 0 || used + (size_t)count == sizeof(response) - 1) {
            used += count > 0 ? (size_t)count : 0;
            break;
        }
        used += (size_t)count;
    }
    close(fd);
    response[used] = '\0';
    if (strstr(response, marker) == NULL) {
        fprintf(stderr, "HTTP response did not contain %s\n", marker);
        return 1;
    }
    printf("HTTP_OK %s\n", marker);
    return 0;
}

static int read_file(const char *path) {
    printf("READ_BEGIN %s\n", path);
    fflush(stdout);
    FILE *file = fopen(path, "rb");
    if (file == NULL) {
        perror(path);
        return 1;
    }
    char buffer[4096];
    for (;;) {
        size_t count = fread(buffer, 1, sizeof(buffer), file);
        if (count != 0 && fwrite(buffer, 1, count, stdout) != count) {
            perror("stdout");
            fclose(file);
            return 1;
        }
        if (count != sizeof(buffer)) {
            if (ferror(file)) {
                perror(path);
                fclose(file);
                return 1;
            }
            break;
        }
    }
    return fclose(file) == 0 ? 0 : 1;
}

static int write_file(const char *path, const char *contents) {
    FILE *file = fopen(path, "wb");
    if (file == NULL) {
        perror(path);
        return 1;
    }
    size_t length = strlen(contents);
    if (fwrite(contents, 1, length, file) != length) {
        perror(path);
        fclose(file);
        return 1;
    }
    if (fclose(file) != 0) {
        perror(path);
        return 1;
    }
    printf("WROTE %s %zu\n", path, length);
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 3 && strcmp(argv[1], "interface") == 0)
        return interface_state(argv[2]);
    if (argc == 3 && strcmp(argv[1], "resolve") == 0)
        return resolve_name(argv[2]);
    if (argc == 4 && strcmp(argv[1], "udp") == 0)
        return udp_send(argv[2], argv[3]);
    if (argc == 5 && strcmp(argv[1], "http") == 0)
        return http_get(argv[2], argv[3], argv[4]);
    if (argc == 3 && strcmp(argv[1], "read") == 0)
        return read_file(argv[2]);
    if (argc == 4 && strcmp(argv[1], "write") == 0)
        return write_file(argv[2], argv[3]);
    if (argc == 3 && strcmp(argv[1], "env") == 0) {
        const char *value = getenv(argv[2]);
        if (value == NULL) {
            fprintf(stderr, "%s is unset\n", argv[2]);
            return 1;
        }
        printf("%s=%s\n", argv[2], value);
        return 0;
    }
    if (argc == 2 && strcmp(argv[1], "cwd") == 0) {
        char cwd[4096];
        if (getcwd(cwd, sizeof(cwd)) == NULL) {
            perror("getcwd");
            return 1;
        }
        puts(cwd);
        return 0;
    }
    if (argc == 2 && strcmp(argv[1], "identity") == 0) {
        printf("UID=%lu GID=%lu\n", (unsigned long)getuid(), (unsigned long)getgid());
        return 0;
    }
    if (argc == 2 && strcmp(argv[1], "tty") == 0) {
        printf("TTY %d %d %d\n", isatty(0), isatty(1), isatty(2));
        return isatty(0) && isatty(1) && isatty(2) ? 0 : 1;
    }
    if (argc == 2 && strcmp(argv[1], "sigterm") == 0) {
        raise(SIGTERM);
        return 99;
    }
    if (argc == 3 && strcmp(argv[1], "sleep") == 0) {
        sleep((unsigned int)strtoul(argv[2], NULL, 10));
        puts("SLEEP_DONE");
        return 0;
    }
    fprintf(stderr,
            "usage: %s interface present|absent | resolve NAME | "
            "udp HOST PORT | http HOST PORT MARKER | read PATH | "
            "write PATH CONTENT | env NAME | cwd | identity | tty | sigterm | "
            "sleep SECONDS\n", argv[0]);
    return 2;
}
