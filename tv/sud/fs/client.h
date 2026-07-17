#ifndef SUD_FS_CLIENT_H
#define SUD_FS_CLIENT_H

#include <stddef.h>
#include <stdint.h>

#include "sud/fs/ring.h"

#ifndef E2BIG
#define E2BIG 7
#endif
#ifndef EPIPE
#define EPIPE 32
#endif
#ifndef EPROTO
#define EPROTO 71
#endif

struct sud_fs_transaction {
    struct sud_fs_slot *slot;
    unsigned char *request;
    const unsigned char *response;
    size_t request_len;
    size_t response_len;
    uint64_t request_id;
};

/* Map and validate SUD_FS_RING_FD. Returns 0 or a negative errno. */
int sud_fs_client_init(void);

/* Claim one bounded mailbox. The caller constructs an ordinary FUSE request
 * directly in tx->request, then submits and consumes tx->response before end. */
int sud_fs_transaction_begin(struct sud_fs_transaction *tx, size_t request_len);
int sud_fs_transaction_submit(struct sud_fs_transaction *tx);
void sud_fs_transaction_end(struct sud_fs_transaction *tx);

/* Test seam and fork hook: binding is process-global but the cursor is atomic,
 * so every thread and CLONE_VM task can share one mapping. */
int sud_fs_client_bind(struct sud_fs_ring *ring);
void sud_fs_client_fork_child(void);
int sud_fs_fd_lane_begin(uint64_t *request_id);
void sud_fs_fd_lane_end(void);

#endif /* SUD_FS_CLIENT_H */
