#include "libc-fs/libc.h"
#include "sud/raw.h"
#include "sud/fs/client.h"

static struct sud_fs_ring *g_ring;
static uint32_t g_cursor;

static long fs_futex(volatile uint32_t *word, int op, uint32_t value)
{
#ifdef SYS_futex
    return raw_syscall6(SYS_futex, (long)word, op, value, 0, 0, 0);
#else
    (void)word; (void)op; (void)value;
    return -ENOSYS;
#endif
}

static void wake_requests(void)
{
    __atomic_fetch_add(&g_ring->header.request_wake, 1u, __ATOMIC_RELEASE);
    (void)fs_futex(&g_ring->header.request_wake, FUTEX_WAKE, 1);
}

int sud_fs_client_bind(struct sud_fs_ring *ring)
{
    if (!ring || ring->header.magic != SUD_FS_RING_MAGIC
        || ring->header.version != SUD_FS_RING_VERSION
        || ring->header.total_size != sizeof(struct sud_fs_ring)
        || ring->header.slot_count != SUD_FS_SLOT_COUNT
        || ring->header.slot_data != SUD_FS_SLOT_DATA)
        return -EPROTO;
    g_ring = ring;
    __atomic_store_n(&g_cursor, 0u, __ATOMIC_RELAXED);
    return 0;
}

int sud_fs_client_init(void)
{
    void *mapping = raw_mmap(0, sizeof(struct sud_fs_ring),
                             PROT_READ | PROT_WRITE, MAP_SHARED,
                             SUD_FS_RING_FD, 0);
    if ((unsigned long)mapping >= (unsigned long)-4095)
        return (int)(long)mapping;
    int result = sud_fs_client_bind((struct sud_fs_ring *)mapping);
    if (result != 0)
        (void)raw_syscall6(SYS_munmap, (long)mapping,
                           sizeof(struct sud_fs_ring), 0, 0, 0, 0);
    return result;
}

void sud_fs_client_fork_child(void)
{
    /* The MAP_SHARED mapping and fd survive fork. Only the scan hint is local
     * state; resetting it prevents every freshly-forked child inheriting the
     * same stale starting position from being a correctness dependency. */
    __atomic_store_n(&g_cursor, 0u, __ATOMIC_RELAXED);
}

int sud_fs_transaction_begin(struct sud_fs_transaction *tx, size_t request_len)
{
    if (!tx || !g_ring) return -ENODEV;
    if (request_len > SUD_FS_SLOT_DATA) return -E2BIG;
    memset(tx, 0, sizeof(*tx));

    for (;;) {
        if (__atomic_load_n(&g_ring->header.shutdown, __ATOMIC_ACQUIRE))
            return -EPIPE;
        uint32_t observed = __atomic_load_n(&g_ring->header.request_wake,
                                             __ATOMIC_ACQUIRE);
        uint32_t start = __atomic_fetch_add(&g_cursor, 1u, __ATOMIC_RELAXED);
        for (uint32_t offset = 0; offset < SUD_FS_SLOT_COUNT; offset++) {
            uint32_t index = (start + offset) % SUD_FS_SLOT_COUNT;
            struct sud_fs_slot *slot = &g_ring->slots[index];
            uint32_t expected = SUD_FS_SLOT_FREE;
            if (!__atomic_compare_exchange_n(&slot->state, &expected,
                                             SUD_FS_SLOT_WRITING, 0,
                                             __ATOMIC_ACQUIRE,
                                             __ATOMIC_RELAXED))
                continue;
            slot->request_len = (uint32_t)request_len;
            slot->response_len = 0;
            slot->flags = 0;
            slot->request_id = (uint64_t)
                (__atomic_fetch_add(&g_ring->header.next_id, 1u,
                                    __ATOMIC_RELAXED) + 1u);
            slot->owner_tgid = raw_getpid();
            slot->owner_tid = raw_gettid();
            tx->slot = slot;
            tx->request = slot->request;
            tx->request_len = request_len;
            tx->request_id = slot->request_id;
            return 0;
        }
        (void)fs_futex(&g_ring->header.request_wake, FUTEX_WAIT, observed);
    }
}

int sud_fs_transaction_submit(struct sud_fs_transaction *tx)
{
    if (!tx || !tx->slot || !g_ring) return -EINVAL;
    struct sud_fs_slot *slot = tx->slot;
    __atomic_store_n(&slot->state, SUD_FS_SLOT_REQUEST, __ATOMIC_RELEASE);
    wake_requests();

    for (;;) {
        uint32_t state = __atomic_load_n(&slot->state, __ATOMIC_ACQUIRE);
        if (state == SUD_FS_SLOT_RESPONSE) {
            if (slot->response_len > SUD_FS_SLOT_DATA) return -EPROTO;
            tx->response = slot->response;
            tx->response_len = slot->response_len;
            return 0;
        }
        if (state == SUD_FS_SLOT_CANCELLED) return -EINTR;
        if (__atomic_load_n(&g_ring->header.shutdown, __ATOMIC_ACQUIRE)) {
            uint32_t expected = state;
            (void)__atomic_compare_exchange_n(&slot->state, &expected,
                                              SUD_FS_SLOT_CANCELLED, 0,
                                              __ATOMIC_ACQ_REL,
                                              __ATOMIC_ACQUIRE);
            return -EPIPE;
        }
        (void)fs_futex(&slot->state, FUTEX_WAIT, state);
    }
}

void sud_fs_transaction_end(struct sud_fs_transaction *tx)
{
    if (!tx || !tx->slot || !g_ring) return;
    __atomic_store_n(&tx->slot->state, SUD_FS_SLOT_FREE, __ATOMIC_RELEASE);
    wake_requests();
    memset(tx, 0, sizeof(*tx));
}
