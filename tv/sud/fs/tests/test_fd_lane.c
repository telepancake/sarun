#include "libc-fs/libc.h"
#include "sud/raw.h"
#include "sud/fs/client.h"
#include "sud/fs/fd_lane.h"

#define TEST_AF_UNIX 1
#define TEST_SOCK_SEQPACKET 5
#define TEST_SOCK_CLOEXEC 02000000
#define TEST_SOL_SOCKET 1
#define TEST_SCM_RIGHTS 1

struct test_iovec { void *base; size_t length; };
struct test_msghdr {
    void *name;
    unsigned int name_length;
    struct test_iovec *iov;
    size_t iov_length;
    void *control;
    size_t control_length;
    unsigned int flags;
};
struct test_cmsghdr { size_t length; int level; int type; };

#define TEST_ALIGN(n) (((n) + sizeof(size_t) - 1) & ~(sizeof(size_t) - 1))
#define TEST_DATA(c) ((unsigned char *)(c) + TEST_ALIGN(sizeof(struct test_cmsghdr)))
#define TEST_LEN(n) (TEST_ALIGN(sizeof(struct test_cmsghdr)) + (n))
#define TEST_SPACE(n) (TEST_ALIGN(sizeof(struct test_cmsghdr)) + TEST_ALIGN(n))

void sud_rt_sigreturn_restorer(void) {}
#if defined(__i386__)
void sud_sigreturn_restorer(void) {}
#endif

static int send_response(int socket, uint64_t id, int error, int exported)
{
    struct sud_fs_fd_response response = {
        SUD_FS_FD_MAGIC, SUD_FS_FD_VERSION, SUD_FS_FD_EXPORT, id, error, 0
    };
    struct test_iovec iov = { &response, sizeof(response) };
    unsigned char control[TEST_SPACE(sizeof(int))];
    struct test_msghdr message;
    memset(&message, 0, sizeof(message));
    message.iov = &iov;
    message.iov_length = 1;
    if (exported >= 0) {
        message.control = control;
        message.control_length = sizeof(control);
        struct test_cmsghdr *header = (struct test_cmsghdr *)control;
        header->length = TEST_LEN(sizeof(int));
        header->level = TEST_SOL_SOCKET;
        header->type = TEST_SCM_RIGHTS;
        memcpy(TEST_DATA(header), &exported, sizeof(exported));
    }
    long sent = raw_syscall6(SYS_sendmsg, socket, (long)&message, 0, 0, 0, 0);
    return sent == sizeof(response) ? 0 : -1;
}

static int receive_request(int socket, struct sud_fs_fd_request *request)
{
    struct test_iovec iov = { request, sizeof(*request) };
    struct test_msghdr message;
    memset(&message, 0, sizeof(message));
    message.iov = &iov;
    message.iov_length = 1;
    long received = raw_syscall6(SYS_recvmsg, socket, (long)&message, 0, 0, 0, 0);
    return received == sizeof(*request) ? 0 : -1;
}

static int child_run(struct sud_fs_ring *ring, int lane)
{
    if (raw_syscall6(SYS_dup2, lane, SUD_FS_FD_LANE_FD, 0, 0, 0, 0) < 0)
        return 10;
    raw_close(lane);
    if (sud_fs_client_bind(ring) != 0) return 11;
    int exported = sud_fs_export_fd(UINT64_C(0x123456789abcdef0), 0);
    if (exported < 0) return 12;
    char payload[16] = {0};
    if (raw_read(exported, payload, sizeof(payload)) != 9
        || memcmp(payload, "lane-data", 9) != 0)
        return 13;
#ifdef SYS_fcntl
    if ((raw_syscall6(SYS_fcntl, exported, F_GETFD, 0, 0, 0, 0)
         & FD_CLOEXEC) == 0)
        return 14;
#endif
    raw_close(exported);
    if (sud_fs_export_fd(99, SUD_FS_FD_EXPORT_WRITE) != -ENOENT)
        return 15;
    return 0;
}

int main(int argc, char **argv)
{
    (void)argc; (void)argv;
    struct sud_fs_ring *ring = raw_mmap(
        0, sizeof(*ring), PROT_READ | PROT_WRITE,
        MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if ((unsigned long)ring >= (unsigned long)-4095) return 1;
    memset(ring, 0, sizeof(*ring));
    ring->header.magic = SUD_FS_RING_MAGIC;
    ring->header.version = SUD_FS_RING_VERSION;
    ring->header.total_size = sizeof(*ring);
    ring->header.slot_count = SUD_FS_SLOT_COUNT;
    ring->header.slot_data = SUD_FS_SLOT_DATA;

    int sockets[2];
    if (raw_syscall6(SYS_socketpair, TEST_AF_UNIX,
                     TEST_SOCK_SEQPACKET | TEST_SOCK_CLOEXEC, 0,
                     (long)sockets, 0, 0) < 0)
        return 2;
    long pid = raw_syscall6(SYS_fork, 0, 0, 0, 0, 0, 0);
    if (pid < 0) return 3;
    if (pid == 0) {
        raw_close(sockets[0]);
        _exit(child_run(ring, sockets[1]));
    }
    raw_close(sockets[1]);

    struct sud_fs_fd_request request;
    if (receive_request(sockets[0], &request) != 0
        || request.magic != SUD_FS_FD_MAGIC
        || request.version != SUD_FS_FD_VERSION
        || request.operation != SUD_FS_FD_EXPORT
        || request.request_id != 1
        || request.handle != UINT64_C(0x123456789abcdef0)
        || request.flags != 0
        || request.caller_pid == 0)
        return 4;
    int data = (int)raw_syscall6(SYS_memfd_create, (long)"fd-lane-test",
                                 MFD_CLOEXEC, 0, 0, 0, 0);
    if (data < 0 || raw_write(data, "lane-data", 9) != 9) return 5;
    if (raw_syscall6(SYS_lseek, data, 0, SEEK_SET, 0, 0, 0) != 0) return 5;
    if (send_response(sockets[0], request.request_id, 0, data) != 0) return 6;
    raw_close(data);

    if (receive_request(sockets[0], &request) != 0
        || request.request_id != 2 || request.handle != 99
        || request.flags != SUD_FS_FD_EXPORT_WRITE)
        return 7;
    if (send_response(sockets[0], request.request_id, -ENOENT, -1) != 0)
        return 8;
    raw_close(sockets[0]);

    int status = 0;
    if (raw_syscall6(SYS_wait4, pid, (long)&status, 0, 0, 0, 0) < 0
        || !WIFEXITED(status) || WEXITSTATUS(status) != 0)
        return 9;
    const char ok[] = "sud fd lane test OK\n";
    (void)write(1, ok, sizeof(ok) - 1);
    return 0;
}
