#ifndef SUD_FS_RING_H
#define SUD_FS_RING_H

/*
 * Bounded shared transport between SUD tracees and SarunFs.
 *
 * A slot is a complete request/reply mailbox.  Clients claim FREE with a
 * compare-exchange, publish REQUEST with release ordering, and futex-wake
 * request_wake.  An engine worker claims REQUEST as PROCESSING, writes the
 * reply, then publishes RESPONSE and wakes the slot state.  No ring cursor can
 * be stranded by a process dying halfway through an enqueue: abandoned slots
 * carry their owning host tgid/tid and can be reclaimed independently.
 *
 * The payloads are ordinary FUSE protocol messages.  This file contains no
 * filesystem operation, path rule, or Sarun-specific semantic encoding.
 */

#include <stdint.h>

#define SUD_FS_RING_MAGIC       UINT32_C(0x53465247) /* "SFRG" */
#define SUD_FS_RING_VERSION     UINT32_C(1)
#define SUD_FS_RING_FD          1021
#define SUD_FS_SLOT_COUNT       32u
#define SUD_FS_SLOT_DATA        32768u

enum sud_fs_slot_state {
    SUD_FS_SLOT_FREE = 0,
    SUD_FS_SLOT_WRITING = 1,
    SUD_FS_SLOT_REQUEST = 2,
    SUD_FS_SLOT_PROCESSING = 3,
    SUD_FS_SLOT_RESPONSE = 4,
    SUD_FS_SLOT_CANCELLED = 5,
};

/* Atomics use the corresponding uint32_t object through compiler __atomic
 * builtins.  Keeping the ABI fields as integers makes the layout identical in
 * the freestanding 32-bit and 64-bit wrappers and Rust. */
struct sud_fs_ring_header {
    uint32_t magic;
    uint32_t version;
    uint32_t total_size;
    uint32_t slot_count;
    uint32_t slot_data;
    uint32_t shutdown;
    uint32_t request_wake;
    uint32_t next_id;
    uint32_t reserved[8];
};

struct __attribute__((aligned(64))) sud_fs_slot {
    uint32_t state;
    uint32_t request_len;
    uint32_t response_len;
    uint32_t flags;
    uint64_t request_id;
    int32_t owner_tgid;
    int32_t owner_tid;
    uint32_t reserved[8];
    unsigned char request[SUD_FS_SLOT_DATA];
    unsigned char response[SUD_FS_SLOT_DATA];
};

struct sud_fs_ring {
    struct sud_fs_ring_header header;
    struct sud_fs_slot slots[SUD_FS_SLOT_COUNT];
};

_Static_assert(sizeof(struct sud_fs_ring_header) == 64,
               "sud fs ring header ABI");
_Static_assert(sizeof(struct sud_fs_slot) == 65600,
               "sud fs slot ABI");
_Static_assert(sizeof(struct sud_fs_ring) == 2099264,
               "sud fs ring ABI");

#endif /* SUD_FS_RING_H */
