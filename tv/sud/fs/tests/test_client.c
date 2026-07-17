#include "libc-fs/libc.h"
#include "sud/raw.h"
#include "sud/fs/client.h"

void sud_rt_sigreturn_restorer(void) {}
#if defined(__i386__)
void sud_sigreturn_restorer(void) {}
#endif

static int child_request(struct sud_fs_ring *ring)
{
    struct sud_fs_transaction tx;
    if (sud_fs_client_bind(ring) != 0) return 10;
    if (sud_fs_transaction_begin(&tx, 4) != 0) return 11;
    memcpy(tx.request, "ping", 4);
    if (sud_fs_transaction_submit(&tx) != 0) return 12;
    int ok = tx.response_len == 4 && memcmp(tx.response, "pong", 4) == 0;
    sud_fs_transaction_end(&tx);
    return ok ? 0 : 13;
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

    long pid = raw_syscall6(SYS_fork, 0, 0, 0, 0, 0, 0);
    if (pid < 0) return 2;
    if (pid == 0)
        _exit(child_request(ring));

    struct sud_fs_slot *slot = 0;
    for (;;) {
        uint32_t observed = __atomic_load_n(&ring->header.request_wake,
                                             __ATOMIC_ACQUIRE);
        for (unsigned int i = 0; i < SUD_FS_SLOT_COUNT; i++) {
            uint32_t expected = SUD_FS_SLOT_REQUEST;
            if (__atomic_compare_exchange_n(&ring->slots[i].state, &expected,
                                             SUD_FS_SLOT_PROCESSING, 0,
                                             __ATOMIC_ACQUIRE,
                                             __ATOMIC_RELAXED)) {
                slot = &ring->slots[i];
                break;
            }
        }
        if (slot) break;
        raw_syscall6(SYS_futex, (long)&ring->header.request_wake,
                     FUTEX_WAIT, observed, 0, 0, 0);
    }
    if (slot->request_len != 4 || memcmp(slot->request, "ping", 4) != 0)
        return 3;
    memcpy(slot->response, "pong", 4);
    slot->response_len = 4;
    __atomic_store_n(&slot->state, SUD_FS_SLOT_RESPONSE, __ATOMIC_RELEASE);
    raw_syscall6(SYS_futex, (long)&slot->state, FUTEX_WAKE, 1, 0, 0, 0);

    int status = 0;
    if (raw_syscall6(SYS_wait4, pid, (long)&status, 0, 0, 0, 0) < 0)
        return 4;
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0)
        return 5;
    const char ok[] = "sud fs client test OK\n";
    (void)write(1, ok, sizeof(ok) - 1);
    return 0;
}
