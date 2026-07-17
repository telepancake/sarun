#include "libc-fs/libc.h"
#include "sud/raw.h"
#include "sud/addin.h"
#include "sud/fs/client.h"
#include "sud/fs/fuse_client.h"
#include "sud/fs/fd_lane.h"
#include "sud/fs/vfs.h"

void sud_rt_sigreturn_restorer(void) {}
#if defined(__i386__)
void sud_sigreturn_restorer(void) {}
#endif

#define TEST_AF_UNIX 1
#define TEST_SOCK_SEQPACKET 5
#define TEST_SOCK_CLOEXEC 02000000
#define TEST_SOL_SOCKET 1
#define TEST_SCM_RIGHTS 1

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

static int lane_request(int socket, struct sud_fs_fd_request *request)
{
    struct lane_iovec iov = { request, sizeof(*request) };
    struct lane_msghdr message;
    memset(&message, 0, sizeof(message));
    message.iov = &iov;
    message.iov_length = 1;
    return raw_syscall6(SYS_recvmsg, socket, (long)&message, 0, 0, 0, 0)
        == sizeof(*request) ? 0 : -1;
}

static int lane_reply(int socket, uint64_t id, int exported)
{
    struct sud_fs_fd_response response = {
        SUD_FS_FD_MAGIC, SUD_FS_FD_VERSION, SUD_FS_FD_EXPORT, id, 0, 0
    };
    struct lane_iovec iov = { &response, sizeof(response) };
    unsigned char control[LANE_SPACE(sizeof(int))];
    struct lane_msghdr message;
    memset(&message, 0, sizeof(message));
    message.iov = &iov;
    message.iov_length = 1;
    message.control = control;
    message.control_length = sizeof(control);
    struct lane_cmsghdr *header = (struct lane_cmsghdr *)control;
    header->length = LANE_LEN(sizeof(int));
    header->level = TEST_SOL_SOCKET;
    header->type = TEST_SCM_RIGHTS;
    memcpy(LANE_DATA(header), &exported, sizeof(exported));
    return raw_syscall6(SYS_sendmsg, socket, (long)&message, 0, 0, 0, 0)
        == sizeof(response) ? 0 : -1;
}

static int serve_lane(int socket)
{
    struct sud_fs_fd_request request;
    int shared = -1;
    for (int i = 0; i < 2; i++) {
        if (lane_request(socket, &request) != 0
            || request.magic != SUD_FS_FD_MAGIC
            || request.version != SUD_FS_FD_VERSION
            || request.operation != SUD_FS_FD_EXPORT
            || request.handle != 9 || request.request_id != (uint64_t)i + 1
            || request.flags != (i ? SUD_FS_FD_EXPORT_WRITE : 0))
            return 70 + i;
        int data = (int)raw_syscall6(SYS_memfd_create,
                                     (long)"vfs-mmap-test",
                                     MFD_CLOEXEC, 0, 0, 0, 0);
        if (data < 0 || raw_write(data, i ? "shared" : "mapped", 6) != 6)
            return 72 + i;
        if (lane_reply(socket, request.request_id, data) != 0)
            return 74 + i;
        if (i) shared = data;
        else raw_close(data);
    }
    char byte;
    while (raw_read(socket, &byte, 1) > 0) {}
    char changed = 0;
    if (raw_pread(shared, &changed, 1, 0) != 1 || changed != 'S') return 76;
    raw_close(shared);
    raw_close(socket);
    return 0;
}

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

static int root_getattr(struct sud_fs_ring *ring)
{
    struct sud_fs_slot *slot = take_request(ring);
    struct fuse_in_header *header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_GETATTR || header->nodeid != FUSE_ROOT_ID)
        return -1;
    struct fuse_attr_out attributes = {0};
    attributes.attr.ino = FUSE_ROOT_ID;
    attributes.attr.mode = S_IFDIR | 0755;
    reply(slot, &attributes, sizeof(attributes));
    return 0;
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

struct test_flock {
    short type;
    short whence;
    long start;
    long length;
    int pid;
};

static int child_calls(struct sud_fs_ring *ring, int lane)
{
    if (raw_syscall6(SYS_dup2, lane, SUD_FS_FD_LANE_FD, 0, 0, 0, 0) < 0)
        return 9;
    raw_close(lane);
    if (sud_fs_client_bind(ring) != 0 || sud_vfs_init("/") != 0) return 10;
    char absolute[PATH_MAX];
    if (sud_vfs_absolutize(AT_FDCWD, "relative", absolute,
                           sizeof(absolute)) != 0
        || strcmp(absolute, "/relative") != 0) return 10;
    int fd = (int)fs_call(SYS_openat, AT_FDCWD, (long)"/hello",
                          O_RDWR, 0, 0, 0);
    if (fd < 0 || !sud_vfs_owns_fd(fd)) return 11;
#if defined(__x86_64__)
    long private_result = fs_call(SYS_mmap, 0, 4096, PROT_READ,
                                  MAP_PRIVATE, fd, 0);
#else
    long private_result = fs_call(SYS_mmap2, 0, 4096, PROT_READ,
                                  MAP_PRIVATE, fd, 0);
#endif
    void *private_map = (void *)private_result;
    if ((unsigned long)private_map >= (unsigned long)-4095
        || memcmp(private_map, "mapped", 6) != 0) return 11;
#if defined(__x86_64__)
    long shared_result = fs_call(SYS_mmap, 0, 4096, PROT_READ | PROT_WRITE,
                                 MAP_SHARED, fd, 0);
#else
    long shared_result = fs_call(SYS_mmap2, 0, 4096, PROT_READ | PROT_WRITE,
                                 MAP_SHARED, fd, 0);
#endif
    void *shared_map = (void *)shared_result;
    if ((unsigned long)shared_map >= (unsigned long)-4095
        || memcmp(shared_map, "shared", 6) != 0) return 11;
    *(char *)shared_map = 'S';
    raw_syscall6(SYS_munmap, (long)private_map, 4096, 0, 0, 0, 0);
    raw_syscall6(SYS_munmap, (long)shared_map, 4096, 0, 0, 0, 0);
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
    if (fs_call(SYS_fsync, fd, 0, 0, 0, 0, 0) != 0
        || fs_call(SYS_fchmod, fd, 0600, 0, 0, 0, 0) != 0) return 19;
    struct test_flock lock = {
        .type = F_WRLCK, .whence = SEEK_SET, .start = 2, .length = 3,
    };
#ifdef SYS_fcntl
    long lock_syscall = SYS_fcntl;
#else
    long lock_syscall = SYS_fcntl64;
#endif
    if (fs_call(lock_syscall, fd, F_SETLK, (long)&lock, 0, 0, 0) != 0)
        return 19;
    lock.type = F_WRLCK;
    if (fs_call(lock_syscall, fd, F_GETLK, (long)&lock, 0, 0, 0) != 0
        || lock.type != F_UNLCK) return 19;
    if (fs_call(SYS_flock, fd, LOCK_EX | LOCK_NB, 0, 0, 0, 0) != 0
        || fs_call(SYS_flock, fd, LOCK_UN, 0, 0, 0, 0) != 0) return 19;
    struct timespec times[2] = { { 123, 456 }, { 789, 12 } };
    if (fs_call(SYS_utimensat, AT_FDCWD, (long)"/hello", (long)times,
                0, 0, 0) != 0) return 19;
    stat_buf_t path_stat;
#if defined(__x86_64__)
    if (fs_call(SYS_newfstatat, AT_FDCWD, (long)"/hello",
                (long)&path_stat, 0, 0, 0) != 0) return 20;
#else
    if (fs_call(SYS_fstatat64, AT_FDCWD, (long)"/hello",
                (long)&path_stat, 0, 0, 0) != 0) return 20;
#endif
    if (fs_call(SYS_faccessat, AT_FDCWD, (long)"/hello", 4, 0, 0, 0) != 0)
        return 21;
    if (fs_call(SYS_close, fd, 0, 0, 0, 0, 0) != 0) return 22;
    if (fs_call(SYS_close, duplicate, 0, 0, 0, 0, 0) != 0) return 23;
    int directory = (int)fs_call(SYS_openat, AT_FDCWD, (long)"/dir",
                                 O_RDONLY | O_DIRECTORY, 0, 0, 0);
    if (directory < 0) return 21;
    unsigned char entries[256];
    long entry_bytes = fs_call(SYS_getdents64, directory, (long)entries,
                               sizeof(entries), 0, 0, 0);
    struct linux_dirent64 *directory_entry =
        (struct linux_dirent64 *)entries;
    if (entry_bytes <= 0 || directory_entry->d_ino != 4
        || directory_entry->d_type != DT_REG
        || strcmp(directory_entry->d_name, "x") != 0) return 22;
    if (fs_call(SYS_close, directory, 0, 0, 0, 0, 0) != 0) return 23;
    char link_target[16] = {0};
    if (fs_call(SYS_readlinkat, AT_FDCWD, (long)"/link",
                (long)link_target, sizeof(link_target), 0, 0) != 5
        || memcmp(link_target, "hello", 5) != 0) return 24;
    int linked = (int)fs_call(SYS_openat, AT_FDCWD, (long)"/link",
                              O_RDONLY, 0, 0, 0);
    if (linked < 0) return 25;
    if (fs_call(SYS_close, linked, 0, 0, 0, 0, 0) != 0) return 26;
    if (fs_call(SYS_mkdirat, AT_FDCWD, (long)"/made", 0750, 0, 0, 0) != 0)
        return 27;
    if (fs_call(SYS_renameat2, AT_FDCWD, (long)"/hello", AT_FDCWD,
                (long)"/moved", 0, 0) != 0) return 28;
    if (fs_call(SYS_linkat, AT_FDCWD, (long)"/moved", AT_FDCWD,
                (long)"/hard", 0, 0) != 0) return 29;
    if (fs_call(SYS_symlinkat, (long)"moved", AT_FDCWD,
                (long)"/soft", 0, 0, 0) != 0) return 30;
    if (fs_call(SYS_unlinkat, AT_FDCWD, (long)"/hard", 0, 0, 0, 0) != 0
        || fs_call(SYS_unlinkat, AT_FDCWD, (long)"/soft", 0, 0, 0, 0) != 0
        || fs_call(SYS_unlinkat, AT_FDCWD, (long)"/moved", 0, 0, 0, 0) != 0
        || fs_call(SYS_unlinkat, AT_FDCWD, (long)"/made", AT_REMOVEDIR,
                   0, 0, 0) != 0) return 31;
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
    if (header->opcode != FUSE_FSYNC || header->nodeid != 2
        || ((struct fuse_fsync_in *)(header + 1))->fh != 9) return 28;
    reply(slot, 0, 0);

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    setattr = (struct fuse_setattr_in *)(header + 1);
    if (header->opcode != FUSE_SETATTR || header->nodeid != 2
        || !(setattr->valid & FATTR_MODE) || !(setattr->valid & FATTR_FH)
        || setattr->mode != 0600 || setattr->fh != 9) return 29;
    attributes.attr.mode = S_IFREG | 0600;
    reply(slot, &attributes, sizeof(attributes));

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    struct fuse_lk_in *lock_input = (struct fuse_lk_in *)(header + 1);
    if (header->opcode != FUSE_SETLK || lock_input->fh != 9
        || lock_input->owner == 0 || lock_input->lk.start != 2
        || lock_input->lk.end != 4 || lock_input->lk.type != F_WRLCK) return 90;
    reply(slot, 0, 0);

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    lock_input = (struct fuse_lk_in *)(header + 1);
    if (header->opcode != FUSE_GETLK || lock_input->lk.start != 2
        || lock_input->lk.end != 4) return 91;
    struct fuse_lk_out no_lock = { .lk = lock_input->lk };
    no_lock.lk.type = F_UNLCK;
    reply(slot, &no_lock, sizeof(no_lock));

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    lock_input = (struct fuse_lk_in *)(header + 1);
    if (header->opcode != FUSE_SETLK || !(lock_input->lk_flags & FUSE_LK_FLOCK)
        || lock_input->lk.type != F_WRLCK || lock_input->lk.start != 0
        || lock_input->lk.end != UINT64_MAX) return 92;
    reply(slot, 0, 0);

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    lock_input = (struct fuse_lk_in *)(header + 1);
    if (header->opcode != FUSE_SETLK || !(lock_input->lk_flags & FUSE_LK_FLOCK)
        || lock_input->lk.type != F_UNLCK) return 93;
    reply(slot, 0, 0);

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_LOOKUP || header->nodeid != FUSE_ROOT_ID
        || strcmp((char *)(header + 1), "hello") != 0) return 29;
    memset(&entry, 0, sizeof(entry));
    entry.nodeid = 2;
    entry.attr.ino = 2;
    entry.attr.mode = S_IFREG | 0600;
    reply(slot, &entry, sizeof(entry));
    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    setattr = (struct fuse_setattr_in *)(header + 1);
    if (header->opcode != FUSE_SETATTR || header->nodeid != 2
        || !(setattr->valid & FATTR_ATIME) || !(setattr->valid & FATTR_MTIME)
        || setattr->atime != 123 || setattr->atimensec != 456
        || setattr->mtime != 789 || setattr->mtimensec != 12) return 29;
    attributes.attr.atime = 123;
    attributes.attr.atimensec = 456;
    attributes.attr.mtime = 789;
    attributes.attr.mtimensec = 12;
    reply(slot, &attributes, sizeof(attributes));
    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_FORGET || header->nodeid != 2) return 29;
    no_reply(slot);

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_LOOKUP || header->nodeid != FUSE_ROOT_ID
        || strcmp((char *)(header + 1), "hello") != 0) return 30;
    memset(&entry, 0, sizeof(entry));
    entry.nodeid = 2;
    entry.attr.ino = 2;
    entry.attr.mode = S_IFREG | 0600;
    reply(slot, &entry, sizeof(entry));
    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_FORGET || header->nodeid != 2) return 31;
    no_reply(slot);

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_LOOKUP || header->nodeid != FUSE_ROOT_ID
        || strcmp((char *)(header + 1), "hello") != 0) return 32;
    reply(slot, &entry, sizeof(entry));
    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_ACCESS || header->nodeid != 2
        || ((struct fuse_access_in *)(header + 1))->mask != 4) return 33;
    reply(slot, 0, 0);
    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_FORGET || header->nodeid != 2) return 34;
    no_reply(slot);

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_FLUSH || header->nodeid != 2) return 35;
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

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_LOOKUP || header->nodeid != FUSE_ROOT_ID
        || strcmp((char *)(header + 1), "dir") != 0) return 32;
    memset(&entry, 0, sizeof(entry));
    entry.nodeid = 3;
    entry.attr.ino = 3;
    entry.attr.mode = S_IFDIR | 0755;
    reply(slot, &entry, sizeof(entry));

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_OPENDIR || header->nodeid != 3) return 33;
    opened.fh = 10;
    reply(slot, &opened, sizeof(opened));

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    read = (struct fuse_read_in *)(header + 1);
    if (header->opcode != FUSE_READDIR || header->nodeid != 3
        || read->fh != 10 || read->offset != 0) return 34;
    unsigned char directory_data[64] = {0};
    struct fuse_dirent *directory_entry = (struct fuse_dirent *)directory_data;
    directory_entry->ino = 4;
    directory_entry->off = 1;
    directory_entry->namelen = 1;
    directory_entry->type = DT_REG;
    directory_entry->name[0] = 'x';
    size_t directory_length = FUSE_DIRENT_ALIGN(FUSE_NAME_OFFSET + 1);
    reply(slot, directory_data, directory_length);

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_RELEASEDIR || header->nodeid != 3) return 35;
    reply(slot, 0, 0);

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_FORGET || header->nodeid != 3) return 36;
    no_reply(slot);

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_LOOKUP || header->nodeid != FUSE_ROOT_ID
        || strcmp((char *)(header + 1), "link") != 0) return 37;
    memset(&entry, 0, sizeof(entry));
    entry.nodeid = 5;
    entry.attr.ino = 5;
    entry.attr.mode = S_IFLNK | 0777;
    reply(slot, &entry, sizeof(entry));
    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_READLINK || header->nodeid != 5) return 38;
    reply(slot, "hello", 5);
    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_FORGET || header->nodeid != 5) return 39;
    no_reply(slot);

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_LOOKUP || header->nodeid != FUSE_ROOT_ID
        || strcmp((char *)(header + 1), "link") != 0) return 40;
    reply(slot, &entry, sizeof(entry));
    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_READLINK || header->nodeid != 5) return 41;
    reply(slot, "hello", 5);
    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_FORGET || header->nodeid != 5) return 42;
    no_reply(slot);
    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_LOOKUP || header->nodeid != FUSE_ROOT_ID
        || strcmp((char *)(header + 1), "hello") != 0) return 43;
    memset(&entry, 0, sizeof(entry));
    entry.nodeid = 2;
    entry.attr.ino = 2;
    entry.attr.mode = S_IFREG | 0644;
    reply(slot, &entry, sizeof(entry));
    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_OPEN || header->nodeid != 2) return 44;
    opened.fh = 11;
    reply(slot, &opened, sizeof(opened));
    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_FLUSH || header->nodeid != 2) return 45;
    reply(slot, 0, 0);
    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_RELEASE || header->nodeid != 2) return 46;
    reply(slot, 0, 0);
    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_FORGET || header->nodeid != 2) return 47;
    no_reply(slot);

    if (root_getattr(ring) != 0) return 48;
    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    struct fuse_mkdir_in *mkdir_input = (struct fuse_mkdir_in *)(header + 1);
    if (header->opcode != FUSE_MKDIR || header->nodeid != FUSE_ROOT_ID
        || mkdir_input->mode != (S_IFDIR | 0750)
        || strcmp((char *)(mkdir_input + 1), "made") != 0) return 49;
    memset(&entry, 0, sizeof(entry));
    entry.nodeid = 6;
    entry.attr.ino = 6;
    entry.attr.mode = S_IFDIR | 0750;
    reply(slot, &entry, sizeof(entry));
    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_FORGET || header->nodeid != 6) return 50;
    no_reply(slot);

    if (root_getattr(ring) != 0 || root_getattr(ring) != 0) return 51;
    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    struct fuse_rename2_in *rename_input =
        (struct fuse_rename2_in *)(header + 1);
    char *rename_names = (char *)(rename_input + 1);
    if (header->opcode != FUSE_RENAME2 || header->nodeid != FUSE_ROOT_ID
        || rename_input->newdir != FUSE_ROOT_ID
        || strcmp(rename_names, "hello") != 0
        || strcmp(rename_names + strlen(rename_names) + 1, "moved") != 0)
        return 52;
    reply(slot, 0, 0);

    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_LOOKUP || header->nodeid != FUSE_ROOT_ID
        || strcmp((char *)(header + 1), "moved") != 0) return 53;
    memset(&entry, 0, sizeof(entry));
    entry.nodeid = 2;
    entry.attr.ino = 2;
    entry.attr.mode = S_IFREG | 0644;
    reply(slot, &entry, sizeof(entry));
    if (root_getattr(ring) != 0) return 54;
    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    struct fuse_link_in *link_input = (struct fuse_link_in *)(header + 1);
    if (header->opcode != FUSE_LINK || link_input->oldnodeid != 2
        || strcmp((char *)(link_input + 1), "hard") != 0) return 55;
    reply(slot, &entry, sizeof(entry));
    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_FORGET || header->nodeid != 2) return 56;
    no_reply(slot);
    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_FORGET || header->nodeid != 2) return 57;
    no_reply(slot);

    if (root_getattr(ring) != 0) return 58;
    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    char *symlink_names = (char *)(header + 1);
    if (header->opcode != FUSE_SYMLINK
        || strcmp(symlink_names, "soft") != 0
        || strcmp(symlink_names + strlen(symlink_names) + 1, "moved") != 0)
        return 59;
    memset(&entry, 0, sizeof(entry));
    entry.nodeid = 7;
    entry.attr.ino = 7;
    entry.attr.mode = S_IFLNK | 0777;
    reply(slot, &entry, sizeof(entry));
    slot = take_request(ring); header = (struct fuse_in_header *)slot->request;
    if (header->opcode != FUSE_FORGET || header->nodeid != 7) return 60;
    no_reply(slot);

    const char *removed[] = { "hard", "soft", "moved", "made" };
    for (int i = 0; i < 4; i++) {
        if (root_getattr(ring) != 0) return 61 + i;
        slot = take_request(ring);
        header = (struct fuse_in_header *)slot->request;
        uint32_t expected_opcode = i == 3 ? FUSE_RMDIR : FUSE_UNLINK;
        if (header->opcode != expected_opcode
            || strcmp((char *)(header + 1), removed[i]) != 0) return 65 + i;
        reply(slot, 0, 0);
    }
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
                     (long)sockets, 0, 0) < 0) return 2;
    long lane_pid = raw_syscall6(SYS_fork, 0, 0, 0, 0, 0, 0);
    if (lane_pid < 0) return 2;
    if (lane_pid == 0) {
        raw_close(sockets[1]);
        _exit(serve_lane(sockets[0]));
    }
    raw_close(sockets[0]);
    long pid = raw_syscall6(SYS_fork, 0, 0, 0, 0, 0, 0);
    if (pid < 0) return 2;
    if (pid == 0) _exit(child_calls(ring, sockets[1]));
    raw_close(sockets[1]);
    int server = serve_calls(ring);
    if (server != 0) return server;
    int status = 0;
    if (raw_syscall6(SYS_wait4, pid, (long)&status, 0, 0, 0, 0) < 0)
        return 3;
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) return 4;
    if (raw_syscall6(SYS_wait4, lane_pid, (long)&status, 0, 0, 0, 0) < 0)
        return 5;
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) return 6;
    const char ok[] = "sud vfs test OK\n";
    (void)write(1, ok, sizeof(ok) - 1);
    return 0;
}
