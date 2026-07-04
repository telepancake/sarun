#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#include "wire/wire.h"
#include "trace/trace.h"

enum { OUTPUT_BUFFER_SIZE = 1 << 20 };

static void usage(FILE *f, const char *prog)
{
    fprintf(f, "Usage: %s [-o FILE] -- command [args...]\n", prog);
}

static int write_trace_version(FILE *out)
{
    uint8_t buf[16];
    Dst d = wire_dst(buf, sizeof(buf));

    wire_put_u64(&d, TRACE_VERSION);
    if (!d.p)
        return -1;
    size_t n = (size_t)((uint8_t *)d.p - buf);
    return fwrite(buf, 1, n, out) == n ? 0 : -1;
}

static int drain_fd(int fd, FILE *out)
{
    char buf[8192];
    ssize_t n;

    do {
        n = read(fd, buf, sizeof(buf));
    } while (n < 0 && errno == EINTR);

    if (n == 0)
        return 0;
    if (n < 0)
        return -1;
    if (out && fwrite(buf, 1, (size_t)n, out) != (size_t)n)
        return -1;
    return 1;
}

static int child_exit_code(int status)
{
    if (WIFEXITED(status))
        return WEXITSTATUS(status);
    if (WIFSIGNALED(status))
        return 128 + WTERMSIG(status);
    return 1;
}

int main(int argc, char **argv)
{
    const char *outfile = NULL;
    int cmd_start = -1;
    FILE *out = NULL;
    int trace_fd = -1;
    int trace_out_fd = -1;
    int sink_out[2] = {-1, -1};
    int sink_err[2] = {-1, -1};
    int saved_stdout = -1;
    pid_t child = -1;
    int rc = 1;

    for (int i = 1; i < argc; i++) {
        if (strcmp(argv[i], "--") == 0) {
            cmd_start = i + 1;
            break;
        }
        if (strcmp(argv[i], "-o") == 0 && i + 1 < argc) {
            outfile = argv[++i];
        } else if (strcmp(argv[i], "-h") == 0 || strcmp(argv[i], "--help") == 0) {
            usage(stdout, argv[0]);
            return 0;
        } else {
            usage(stderr, argv[0]);
            return 1;
        }
    }

    if (cmd_start < 0 || cmd_start >= argc) {
        usage(stderr, argv[0]);
        return 1;
    }

    if (outfile) {
        out = fopen(outfile, "wb");
        if (!out) {
            perror("fopen");
            goto done;
        }
    } else {
        trace_out_fd = dup(STDOUT_FILENO);
        if (trace_out_fd < 0) {
            perror("dup");
            goto done;
        }
        out = fdopen(trace_out_fd, "wb");
        if (!out) {
            perror("fdopen");
            goto done;
        }
        trace_out_fd = -1;
    }
    setvbuf(out, NULL, _IOFBF, OUTPUT_BUFFER_SIZE);
    if (write_trace_version(out) != 0) {
        perror("write");
        goto done;
    }

    if (pipe(sink_out) < 0 || pipe(sink_err) < 0) {
        perror("pipe");
        goto done;
    }

    saved_stdout = dup(STDOUT_FILENO);
    if (saved_stdout < 0) {
        perror("dup");
        goto done;
    }
    if (dup2(sink_out[1], STDOUT_FILENO) < 0) {
        perror("dup2");
        goto done;
    }
    trace_fd = open("/proc/proctrace/new", O_RDONLY);
    if (dup2(saved_stdout, STDOUT_FILENO) < 0) {
        perror("dup2");
        goto done;
    }
    close(saved_stdout);
    saved_stdout = -1;
    if (trace_fd < 0) {
        perror("open /proc/proctrace/new");
        goto done;
    }

    child = fork();
    if (child < 0) {
        perror("fork");
        goto done;
    }
    if (child == 0) {
        if (dup2(sink_out[1], STDOUT_FILENO) < 0)
            _exit(127);
        if (dup2(sink_err[1], STDERR_FILENO) < 0)
            _exit(127);
        close(sink_out[0]);
        close(sink_out[1]);
        close(sink_err[0]);
        close(sink_err[1]);
        close(trace_fd);
        execvp(argv[cmd_start], argv + cmd_start);
        perror(argv[cmd_start]);
        _exit(127);
    }

    close(sink_out[1]);
    sink_out[1] = -1;
    close(sink_err[1]);
    sink_err[1] = -1;

    for (;;) {
        struct pollfd pfds[3];
        nfds_t nfds = 0;
        int trace_idx = -1, out_idx = -1, err_idx = -1;

        if (trace_fd >= 0) {
            trace_idx = (int)nfds;
            pfds[nfds].fd = trace_fd;
            pfds[nfds].events = POLLIN | POLLHUP;
            pfds[nfds].revents = 0;
            nfds++;
        }
        if (sink_out[0] >= 0) {
            out_idx = (int)nfds;
            pfds[nfds].fd = sink_out[0];
            pfds[nfds].events = POLLIN | POLLHUP;
            pfds[nfds].revents = 0;
            nfds++;
        }
        if (sink_err[0] >= 0) {
            err_idx = (int)nfds;
            pfds[nfds].fd = sink_err[0];
            pfds[nfds].events = POLLIN | POLLHUP;
            pfds[nfds].revents = 0;
            nfds++;
        }
        if (nfds == 0)
            break;

        if (poll(pfds, nfds, -1) < 0) {
            if (errno == EINTR)
                continue;
            perror("poll");
            goto done;
        }

        if (trace_idx >= 0 && (pfds[trace_idx].revents & (POLLIN | POLLHUP | POLLERR))) {
            int ret = drain_fd(trace_fd, out);
            if (ret < 0) {
                perror("read/write");
                goto done;
            }
            if (ret == 0) {
                close(trace_fd);
                trace_fd = -1;
            }
        }
        if (out_idx >= 0 && (pfds[out_idx].revents & (POLLIN | POLLHUP | POLLERR))) {
            int ret = drain_fd(sink_out[0], NULL);
            if (ret < 0) {
                perror("read");
                goto done;
            }
            if (ret == 0) {
                close(sink_out[0]);
                sink_out[0] = -1;
            }
        }
        if (err_idx >= 0 && (pfds[err_idx].revents & (POLLIN | POLLHUP | POLLERR))) {
            int ret = drain_fd(sink_err[0], NULL);
            if (ret < 0) {
                perror("read");
                goto done;
            }
            if (ret == 0) {
                close(sink_err[0]);
                sink_err[0] = -1;
            }
        }
    }

    rc = 0;

done:
    if (trace_fd >= 0)
        close(trace_fd);
    if (sink_out[0] >= 0)
        close(sink_out[0]);
    if (sink_out[1] >= 0)
        close(sink_out[1]);
    if (sink_err[0] >= 0)
        close(sink_err[0]);
    if (sink_err[1] >= 0)
        close(sink_err[1]);
    if (saved_stdout >= 0) {
        (void)dup2(saved_stdout, STDOUT_FILENO);
        close(saved_stdout);
    }
    if (child > 0) {
        int status;
        while (waitpid(child, &status, 0) < 0 && errno == EINTR) {}
        if (rc == 0)
            rc = child_exit_code(status);
    }
    if (out) {
        if (fflush(out) != 0)
            rc = 1;
        if (fclose(out) != 0)
            rc = 1;
    } else if (trace_out_fd >= 0) {
        close(trace_out_fd);
    }
    return rc;
}
