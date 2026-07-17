#include "libc-fs/libc.h"
#include "sud/raw.h"
#include "sud/addin.h"
#include "sud/fs/client.h"
#include "sud/fs/fuse_client.h"
#include "sud/fs/vfs.h"

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

static void finish_reply(struct sud_fs_slot *slot)
{
    __atomic_store_n(&slot->state, SUD_FS_SLOT_RESPONSE, __ATOMIC_RELEASE);
    raw_syscall6(SYS_futex, (long)&slot->state, FUTEX_WAKE, 1, 0, 0, 0);
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
    finish_reply(slot);
}

static void no_reply(struct sud_fs_slot *slot)
{
    slot->response_len = 0;
    finish_reply(slot);
}

static long fs_call(long nr, long a0, long a1, long a2,
                    long a3, long a4, long a5)
{
    char scratch[PATH_MAX * 2];
    struct sud_syscall_ctx context = {
        nr, { a0, a1, a2, a3, a4, a5 }, 0, raw_gettid(),
        scratch, sizeof(scratch), { 0 }
    };
    if (!sud_fs_addin.pre_syscall(&context)) return -ENOSYS;
    return context.ret;
}

static int child_calls(struct sud_fs_ring *ring)
{
    if (sud_fs_client_bind(ring) != 0 || sud_vfs_init("/") != 0) return 10;
    int fd = (int)fs_call(SYS_openat, AT_FDCWD, (long)"/hello",
                          O_RDWR, 0, 0, 0);
    if (fd < 0 || !sud_vfs_owns_fd(fd)) return 11;
    char data[8] = {0};
    if (fs_call(SYS_read, fd, (long)data, 5, 0, 0, 0) != 5
        || memcmp(data, "hello", 5) != 0)
        return 12;
    if (fs_call(SYS_lseek, fd, 1, SEEK_SET, 0, 0, 0) != 1
        || fs_call(SYS_write, fd, (long)"A", 1, 0, 0, 0) != 1) return 13;
    int duplicate = (int)fs_call(SYS_dup, fd, 0, 0, 0, 0, 0);
    if (duplicate < 0) return 14;
    if (fs_call(SYS_lseek, duplicate, 0, SEEK_SET, 0, 0, 0) != 0) return 15;
    memset(data, 0, sizeof(data));
    if (fs_call(SYS_read, fd, (long)data, 2, 0, 0, 0) != 2
        || memcmp(data, "hA", 2) != 0)
        return 16;
    stat_buf_t stat_buffer;
    if (fs_call(SYS_fstat, fd, (long)&stat_buffer, 0, 0, 0, 0) != 0)
        return 17;
    if (fs_call(SYS_ftruncate, fd, 3, 0, 0, 0, 0) != 0) return 18;
    if (fs_call(SYS_close, fd, 0, 0, 0, 0, 0) != 0) return 19;
    if (fs_call(SYS_close, duplicate, 0, 0, 0, 0, 0) != 0) return 20;
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
    entry.attr.size = 5;
    reply(slot, &entry, sizeof(entry));

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_OPEN || header->nodeid != 2) return 22;
    struct fuse_open_out opened = { .fh = 9 };
    reply(slot, &opened, sizeof(opened));

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    struct fuse_read_in *read = (struct fuse_read_in *)(header + 1);
    if (header->opcode != FUSE_READ || read->fh != 9
        || read->offset != 0 || read->size != 5) return 23;
    reply(slot, "hello", 5);

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    struct fuse_write_in *write = (struct fuse_write_in *)(header + 1);
    if (header->opcode != FUSE_WRITE || write->fh != 9
        || write->offset != 1 || write->size != 1
        || memcmp(write + 1, "A", 1) != 0) return 24;
    struct fuse_write_out written = { .size = 1 };
    reply(slot, &written, sizeof(written));

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    read = (struct fuse_read_in *)(header + 1);
    if (header->opcode != FUSE_READ || read->fh != 9
        || read->offset != 0 || read->size != 2) return 25;
    reply(slot, "hA", 2);

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_GETATTR || header->nodeid != 2
        || !(((struct fuse_getattr_in *)(header + 1))->getattr_flags
             & FUSE_GETATTR_FH)) return 26;
    struct fuse_attr_out attributes = {0};
    attributes.attr.ino = 2;
    attributes.attr.mode = S_IFREG | 0644;
    attributes.attr.size = 5;
    attributes.attr.blksize = 4096;
    reply(slot, &attributes, sizeof(attributes));

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    struct fuse_setattr_in *setattr = (struct fuse_setattr_in *)(header + 1);
    if (header->opcode != FUSE_SETATTR || header->nodeid != 2
        || !(setattr->valid & FATTR_SIZE) || !(setattr->valid & FATTR_FH)
        || setattr->fh != 9 || setattr->size != 3) return 27;
    attributes.attr.size = 3;
    reply(slot, &attributes, sizeof(attributes));

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_FLUSH || header->nodeid != 2) return 28;
    reply(slot, 0, 0);

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_FLUSH || header->nodeid != 2) return 29;
    reply(slot, 0, 0);

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_RELEASE || header->nodeid != 2) return 30;
    reply(slot, 0, 0);

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_FORGET || header->nodeid != 2
        || ((struct fuse_forget_in *)(header + 1))->nlookup != 1) return 31;
    no_reply(slot);
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
    const char ok[] = "sud vfs test OK\n";
    (void)write(1, ok, sizeof(ok) - 1);
    return 0;
}
