#include "libc-fs/libc.h"
#include "sud/raw.h"
#include "sud/fs/client.h"
#include "sud/fs/fuse_client.h"

void sud_rt_sigreturn_restorer(void) {}
#if defined(__i386__)
void sud_sigreturn_restorer(void) {}
#endif

static struct sud_fs_slot *take_request(struct sud_fs_ring *ring)
{
    for (;;) {
        uint32_t observed = __atomic_load_n(&ring->header.request_wake,
                                             __ATOMIC_ACQUIRE);
        for (unsigned int i = 0; i < SUD_FS_SLOT_COUNT; i++) {
            uint32_t expected = SUD_FS_SLOT_REQUEST;
            if (__atomic_compare_exchange_n(&ring->slots[i].state, &expected,
                                             SUD_FS_SLOT_PROCESSING, 0,
                                             __ATOMIC_ACQUIRE,
                                             __ATOMIC_RELAXED))
                return &ring->slots[i];
        }
        raw_syscall6(SYS_futex, (long)&ring->header.request_wake,
                     FUTEX_WAIT, observed, 0, 0, 0);
    }
}

static void reply(struct sud_fs_slot *slot, const void *payload, size_t length)
{
    const struct fuse_in_header *input =
        (const struct fuse_in_header *)slot->request;
    struct fuse_out_header *output = (struct fuse_out_header *)slot->response;
    output->len = (uint32_t)(sizeof(*output) + length);
    output->error = 0;
    output->unique = input->unique;
    if (length) memcpy(output + 1, payload, length);
    slot->response_len = output->len;
    __atomic_store_n(&slot->state, SUD_FS_SLOT_RESPONSE, __ATOMIC_RELEASE);
    raw_syscall6(SYS_futex, (long)&slot->state, FUTEX_WAKE, 1, 0, 0, 0);
}

static int child_calls(struct sud_fs_ring *ring)
{
    if (sud_fs_client_bind(ring) != 0 || sud_fuse_init() != 0) return 10;
    struct fuse_entry_out entry;
    if (sud_fuse_lookup(FUSE_ROOT_ID, "hello", &entry) != 0
        || entry.nodeid != 2) return 11;
    struct fuse_attr_out attributes;
    if (sud_fuse_getattr(2, 0, 0, &attributes) != 0
        || attributes.attr.ino != 2) return 12;
    struct fuse_open_out opened;
    if (sud_fuse_open(2, O_RDWR, &opened) != 0 || opened.fh != 9) return 13;
    char data[8] = {0};
    if (sud_fuse_read(2, 9, 0, O_RDWR, data, sizeof(data)) != 5
        || memcmp(data, "hello", 5) != 0) return 14;
    if (sud_fuse_write(2, 9, 5, O_RDWR, "new", 3) != 3) return 15;
    struct fuse_kstatfs statistics;
    if (sud_fuse_statfs(2, &statistics) != 0
        || statistics.bsize != 4096 || statistics.namelen != 255) return 16;
    if (sud_fuse_setxattr(2, "user.test", "value", 5, 0) != 0) return 17;
    if (sud_fuse_getxattr(2, "user.test", 0, 0) != 5) return 18;
    memset(data, 0, sizeof(data));
    if (sud_fuse_getxattr(2, "user.test", data, sizeof(data)) != 5
        || memcmp(data, "value", 5) != 0) return 19;
    if (sud_fuse_listxattr(2, 0, 0) != 10) return 20;
    char names[16] = {0};
    if (sud_fuse_listxattr(2, names, sizeof(names)) != 10
        || strcmp(names, "user.test") != 0) return 21;
    if (sud_fuse_removexattr(2, "user.test") != 0) return 22;
    if (sud_fuse_fallocate(2, 9, FALLOC_FL_KEEP_SIZE, 4096, 8192) != 0)
        return 23;
    if (sud_fuse_lseek(2, 9, 0, SEEK_DATA) != 4096) return 24;
    if (sud_fuse_flush(2, 9, O_RDWR) != 0) return 25;
    if (sud_fuse_release(2, 9, O_RDWR) != 0) return 26;
    return 0;
}

static int serve_calls(struct sud_fs_ring *ring)
{
    struct sud_fs_slot *slot = take_request(ring);
    struct fuse_in_header *header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_INIT) return 20;
    struct fuse_init_out init = {0};
    init.major = FUSE_KERNEL_VERSION;
    init.minor = FUSE_KERNEL_MINOR_VERSION;
    init.max_write = SUD_FS_SLOT_DATA - 128;
    reply(slot, &init, sizeof(init));

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_LOOKUP || header->nodeid != FUSE_ROOT_ID
        || strcmp((char *)(header + 1), "hello") != 0) return 21;
    struct fuse_entry_out entry = {0};
    entry.nodeid = 2;
    entry.attr.ino = 2;
    entry.attr.mode = S_IFREG | 0644;
    reply(slot, &entry, sizeof(entry));

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_GETATTR || header->nodeid != 2) return 22;
    struct fuse_attr_out attributes = {0};
    attributes.attr.ino = 2;
    attributes.attr.mode = S_IFREG | 0644;
    reply(slot, &attributes, sizeof(attributes));

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_OPEN || header->nodeid != 2) return 23;
    struct fuse_open_out opened = { .fh = 9 };
    reply(slot, &opened, sizeof(opened));

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_READ || header->nodeid != 2
        || ((struct fuse_read_in *)(header + 1))->fh != 9) return 24;
    reply(slot, "hello", 5);

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    struct fuse_write_in *write = (struct fuse_write_in *)(header + 1);
    if (header->opcode != FUSE_WRITE || write->fh != 9 || write->size != 3
        || memcmp(write + 1, "new", 3) != 0) return 25;
    struct fuse_write_out written = { .size = 3 };
    reply(slot, &written, sizeof(written));

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_STATFS || header->nodeid != 2) return 26;
    struct fuse_kstatfs statistics = { .bsize = 4096, .namelen = 255 };
    reply(slot, &statistics, sizeof(statistics));

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    struct fuse_setxattr_in *setxattr = (struct fuse_setxattr_in *)(header + 1);
    if (header->opcode != FUSE_SETXATTR || header->nodeid != 2
        || setxattr->size != 5 || setxattr->flags != 0
        || strcmp((char *)setxattr + 8, "user.test") != 0
        || memcmp((char *)setxattr + 8 + 10, "value", 5) != 0) return 27;
    reply(slot, 0, 0);

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    struct fuse_getxattr_in *getxattr = (struct fuse_getxattr_in *)(header + 1);
    if (header->opcode != FUSE_GETXATTR || getxattr->size != 0
        || strcmp((char *)(getxattr + 1), "user.test") != 0) return 28;
    struct fuse_getxattr_out xattr_size = { .size = 5 };
    reply(slot, &xattr_size, sizeof(xattr_size));

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    getxattr = (struct fuse_getxattr_in *)(header + 1);
    if (header->opcode != FUSE_GETXATTR || getxattr->size != 8) return 29;
    reply(slot, "value", 5);

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    getxattr = (struct fuse_getxattr_in *)(header + 1);
    if (header->opcode != FUSE_LISTXATTR || getxattr->size != 0) return 30;
    xattr_size.size = 10;
    reply(slot, &xattr_size, sizeof(xattr_size));

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    getxattr = (struct fuse_getxattr_in *)(header + 1);
    if (header->opcode != FUSE_LISTXATTR || getxattr->size != 16) return 31;
    reply(slot, "user.test\0", 10);

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_REMOVEXATTR
        || strcmp((char *)(header + 1), "user.test") != 0) return 32;
    reply(slot, 0, 0);

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    struct fuse_fallocate_in *fallocate = (struct fuse_fallocate_in *)(header + 1);
    if (header->opcode != FUSE_FALLOCATE || fallocate->fh != 9
        || fallocate->mode != FALLOC_FL_KEEP_SIZE
        || fallocate->offset != 4096 || fallocate->length != 8192) return 33;
    reply(slot, 0, 0);

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    struct fuse_lseek_in *lseek = (struct fuse_lseek_in *)(header + 1);
    if (header->opcode != FUSE_LSEEK || lseek->fh != 9
        || lseek->offset != 0 || lseek->whence != SEEK_DATA) return 34;
    struct fuse_lseek_out seeked = { .offset = 4096 };
    reply(slot, &seeked, sizeof(seeked));

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_FLUSH) return 35;
    reply(slot, 0, 0);
    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_RELEASE) return 36;
    reply(slot, 0, 0);
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
    long pid = raw_syscall6(SYS_fork, 0, 0, 0, 0, 0, 0);
    if (pid < 0) return 2;
    if (pid == 0) _exit(child_calls(ring));
    int server = serve_calls(ring);
    if (server != 0) return server;
    int status = 0;
    if (raw_syscall6(SYS_wait4, pid, (long)&status, 0, 0, 0, 0) < 0)
        return 3;
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) return 4;
    const char ok[] = "sud fuse client test OK\n";
    (void)write(1, ok, sizeof(ok) - 1);
    return 0;
}
