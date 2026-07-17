#include "libc-fs/libc.h"
#include "sud/raw.h"
#include "sud/fs/client.h"
#include "sud/fs/fd_lane.h"

#define SUD_SOL_SOCKET 1
#define SUD_SCM_RIGHTS 1
#define SUD_MSG_CMSG_CLOEXEC 0x40000000

struct lane_iovec { void *base; size_t length; };
struct lane_msghdr {
    void *name;
    unsigned int name_length;
    struct lane_iovec *iov;
    size_t iov_length;
    void *control;
    size_t control_length;
    unsigned int flags;
};
struct lane_cmsghdr { size_t length; int level; int type; };

#define LANE_ALIGN(n) (((n) + sizeof(size_t) - 1) & ~(sizeof(size_t) - 1))
#define LANE_DATA(c) ((unsigned char *)(c) + LANE_ALIGN(sizeof(struct lane_cmsghdr)))
#define LANE_LEN(n) (LANE_ALIGN(sizeof(struct lane_cmsghdr)) + (n))
#define LANE_SPACE(n) (LANE_ALIGN(sizeof(struct lane_cmsghdr)) + LANE_ALIGN(n))

int sud_fs_export_fd(uint64_t handle, uint32_t flags)
{
    uint64_t request_id;
    int result = sud_fs_fd_lane_begin(&request_id);
    if (result != 0) return result;
    struct sud_fs_fd_request request;
    memset(&request, 0, sizeof(request));
    request.magic = SUD_FS_FD_MAGIC;
    request.version = SUD_FS_FD_VERSION;
    request.operation = SUD_FS_FD_EXPORT;
    request.request_id = request_id;
    request.handle = handle;
    request.flags = flags;
    request.caller_pid = (uint32_t)raw_gettid();
    struct lane_iovec send_iov = { &request, sizeof(request) };
    struct lane_msghdr send_message;
    memset(&send_message, 0, sizeof(send_message));
    send_message.iov = &send_iov;
    send_message.iov_length = 1;
    long sent = raw_syscall6(SYS_sendmsg, SUD_FS_FD_LANE_FD,
                             (long)&send_message, 0, 0, 0, 0);
    if (sent != sizeof(request)) {
        sud_fs_fd_lane_end();
        return sent < 0 ? (int)sent : -EIO;
    }

    struct sud_fs_fd_response response;
    struct lane_iovec receive_iov = { &response, sizeof(response) };
    unsigned char control[LANE_SPACE(sizeof(int))];
    struct lane_msghdr receive_message;
    memset(&receive_message, 0, sizeof(receive_message));
    receive_message.iov = &receive_iov;
    receive_message.iov_length = 1;
    receive_message.control = control;
    receive_message.control_length = sizeof(control);
    long received = raw_syscall6(SYS_recvmsg, SUD_FS_FD_LANE_FD,
                                 (long)&receive_message,
                                 SUD_MSG_CMSG_CLOEXEC, 0, 0, 0);
    int exported = -EIO;
    if (received == sizeof(response)
        && response.magic == SUD_FS_FD_MAGIC
        && response.version == SUD_FS_FD_VERSION
        && response.operation == SUD_FS_FD_EXPORT
        && response.request_id == request_id) {
        if (response.error != 0) exported = response.error;
        else if (receive_message.control_length >= LANE_LEN(sizeof(int))) {
            struct lane_cmsghdr *header = (struct lane_cmsghdr *)control;
            if (header->level == SUD_SOL_SOCKET
                && header->type == SUD_SCM_RIGHTS
                && header->length >= LANE_LEN(sizeof(int)))
                memcpy(&exported, LANE_DATA(header), sizeof(exported));
        }
    } else if (received < 0) {
        exported = (int)received;
    }
    sud_fs_fd_lane_end();
    return exported;
}
