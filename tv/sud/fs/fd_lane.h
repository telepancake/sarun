#ifndef SUD_FS_FD_LANE_H
#define SUD_FS_FD_LANE_H

#include <stdint.h>

#define SUD_FS_FD_LANE_FD 1020
#define SUD_FS_FD_MAGIC UINT32_C(0x53464644)
#define SUD_FS_FD_VERSION 1u
#define SUD_FS_FD_EXPORT 1u
#define SUD_FS_FD_EXPORT_WRITE 1u

struct sud_fs_fd_request {
    uint32_t magic;
    uint16_t version;
    uint16_t operation;
    uint64_t request_id;
    uint64_t handle;
    uint32_t flags;
    uint32_t caller_pid;
};

struct sud_fs_fd_response {
    uint32_t magic;
    uint16_t version;
    uint16_t operation;
    uint64_t request_id;
    int32_t error;
    uint32_t reserved;
};

_Static_assert(sizeof(struct sud_fs_fd_request) == 32, "fd request ABI");
_Static_assert(sizeof(struct sud_fs_fd_response) == 24, "fd response ABI");

int sud_fs_export_fd(uint64_t handle, uint32_t flags);

#endif
