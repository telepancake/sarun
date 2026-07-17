#include "libc-fs/libc.h"
#include "sud/raw.h"
#include "sud/fs/client.h"
#include "sud/fs/fuse_client.h"

static uint32_t g_fuse_initialized;

struct fuse_call {
    struct sud_fs_transaction transaction;
    struct fuse_in_header *input;
    const struct fuse_out_header *output;
    const unsigned char *payload;
    size_t payload_len;
};

static int call_begin(struct fuse_call *call, uint32_t opcode,
                      uint64_t inode, size_t payload_len)
{
    size_t length = sizeof(struct fuse_in_header) + payload_len;
    int result = sud_fs_transaction_begin(&call->transaction, length);
    if (result != 0) return result;
    call->input = (struct fuse_in_header *)call->transaction.request;
    memset(call->input, 0, sizeof(*call->input));
    call->input->len = (uint32_t)length;
    call->input->opcode = opcode;
    call->input->unique = call->transaction.request_id;
    call->input->nodeid = inode;
    call->input->uid = (uint32_t)raw_syscall6(SYS_geteuid, 0, 0, 0, 0, 0, 0);
    call->input->gid = (uint32_t)raw_syscall6(SYS_getegid, 0, 0, 0, 0, 0, 0);
    call->input->pid = (uint32_t)raw_getpid();
    return 0;
}

static void *call_input_payload(struct fuse_call *call)
{
    return (unsigned char *)call->input + sizeof(*call->input);
}

static int call_submit(struct fuse_call *call)
{
    int result = sud_fs_transaction_submit(&call->transaction);
    if (result != 0) return result;
    if (call->transaction.response_len == 0) {
        call->output = 0;
        call->payload = 0;
        call->payload_len = 0;
        return 0;
    }
    if (call->transaction.response_len < sizeof(struct fuse_out_header))
        return -EPROTO;
    call->output = (const struct fuse_out_header *)call->transaction.response;
    if (call->output->len != call->transaction.response_len
        || call->output->unique != call->transaction.request_id)
        return -EPROTO;
    if (call->output->error != 0) return call->output->error;
    call->payload = call->transaction.response + sizeof(*call->output);
    call->payload_len = call->transaction.response_len - sizeof(*call->output);
    return 0;
}

static void call_end(struct fuse_call *call)
{
    sud_fs_transaction_end(&call->transaction);
}

static int copy_fixed_reply(struct fuse_call *call, void *output, size_t size)
{
    int result = call_submit(call);
    if (result == 0) {
        if (call->payload_len < size) result = -EPROTO;
        else memcpy(output, call->payload, size);
    }
    call_end(call);
    return result;
}

int sud_fuse_init(void)
{
    if (__atomic_load_n(&g_fuse_initialized, __ATOMIC_ACQUIRE)) return 0;
    struct fuse_call call;
    int result = call_begin(&call, FUSE_INIT, 0, sizeof(struct fuse_init_in));
    if (result != 0) return result;
    struct fuse_init_in *input = call_input_payload(&call);
    memset(input, 0, sizeof(*input));
    input->major = FUSE_KERNEL_VERSION;
    input->minor = FUSE_KERNEL_MINOR_VERSION;
    struct fuse_init_out output;
    result = copy_fixed_reply(&call, &output, sizeof(output));
    if (result != 0) return result;
    if (output.major != FUSE_KERNEL_VERSION) return -EPROTO;
    __atomic_store_n(&g_fuse_initialized, 1u, __ATOMIC_RELEASE);
    return 0;
}

int sud_fuse_lookup(uint64_t parent, const char *name,
                    struct fuse_entry_out *entry)
{
    size_t name_len = strlen(name) + 1;
    if (name_len <= 1 || name_len > 256) return -ENAMETOOLONG;
    struct fuse_call call;
    int result = call_begin(&call, FUSE_LOOKUP, parent, name_len);
    if (result != 0) return result;
    memcpy(call_input_payload(&call), name, name_len);
    return copy_fixed_reply(&call, entry, sizeof(*entry));
}

long sud_fuse_readlink(uint64_t inode, char *buffer, size_t size)
{
    struct fuse_call call;
    int result = call_begin(&call, FUSE_READLINK, inode, 0);
    if (result != 0) return result;
    result = call_submit(&call);
    long count = result;
    if (result == 0) {
        if (call.payload_len > size) count = -ERANGE;
        else {
            memcpy(buffer, call.payload, call.payload_len);
            count = (long)call.payload_len;
        }
    }
    call_end(&call);
    return count;
}

int sud_fuse_forget(uint64_t inode, uint64_t count)
{
    struct fuse_call call;
    int result = call_begin(&call, FUSE_FORGET, inode, sizeof(struct fuse_forget_in));
    if (result != 0) return result;
    struct fuse_forget_in *input = call_input_payload(&call);
    input->nlookup = count;
    result = call_submit(&call);
    call_end(&call);
    return result;
}

int sud_fuse_getattr(uint64_t inode, uint64_t handle, int has_handle,
                     struct fuse_attr_out *attributes)
{
    struct fuse_call call;
    int result = call_begin(&call, FUSE_GETATTR, inode, sizeof(struct fuse_getattr_in));
    if (result != 0) return result;
    struct fuse_getattr_in *input = call_input_payload(&call);
    memset(input, 0, sizeof(*input));
    if (has_handle) {
        input->getattr_flags = FUSE_GETATTR_FH;
        input->fh = handle;
    }
    return copy_fixed_reply(&call, attributes, sizeof(*attributes));
}

int sud_fuse_open(uint64_t inode, uint32_t flags, struct fuse_open_out *opened)
{
    struct fuse_call call;
    int result = call_begin(&call, FUSE_OPEN, inode, sizeof(struct fuse_open_in));
    if (result != 0) return result;
    struct fuse_open_in *input = call_input_payload(&call);
    input->flags = flags;
    input->open_flags = 0;
    return copy_fixed_reply(&call, opened, sizeof(*opened));
}

int sud_fuse_opendir(uint64_t inode, uint32_t flags,
                     struct fuse_open_out *opened)
{
    struct fuse_call call;
    int result = call_begin(&call, FUSE_OPENDIR, inode,
                            sizeof(struct fuse_open_in));
    if (result != 0) return result;
    struct fuse_open_in *input = call_input_payload(&call);
    input->flags = flags;
    input->open_flags = 0;
    return copy_fixed_reply(&call, opened, sizeof(*opened));
}

int sud_fuse_create(uint64_t parent, const char *name, uint32_t flags,
                    uint32_t mode, uint32_t umask,
                    struct fuse_entry_out *entry,
                    struct fuse_open_out *opened)
{
    size_t name_len = strlen(name) + 1;
    if (name_len <= 1 || name_len > 256) return -ENAMETOOLONG;
    struct fuse_call call;
    int result = call_begin(&call, FUSE_CREATE, parent,
                            sizeof(struct fuse_create_in) + name_len);
    if (result != 0) return result;
    struct fuse_create_in *input = call_input_payload(&call);
    input->flags = flags;
    input->mode = mode;
    input->umask = umask;
    input->open_flags = 0;
    memcpy(input + 1, name, name_len);
    result = call_submit(&call);
    if (result == 0) {
        size_t needed = sizeof(*entry) + sizeof(*opened);
        if (call.payload_len < needed) result = -EPROTO;
        else {
            memcpy(entry, call.payload, sizeof(*entry));
            memcpy(opened, call.payload + sizeof(*entry), sizeof(*opened));
        }
    }
    call_end(&call);
    return result;
}

static int entry_name_call(uint32_t opcode, uint64_t parent,
                           const void *prefix, size_t prefix_len,
                           const char *name, struct fuse_entry_out *entry)
{
    size_t name_len = strlen(name) + 1;
    if (name_len <= 1 || name_len > 256) return -ENAMETOOLONG;
    struct fuse_call call;
    int result = call_begin(&call, opcode, parent, prefix_len + name_len);
    if (result != 0) return result;
    if (prefix_len) memcpy(call_input_payload(&call), prefix, prefix_len);
    memcpy((unsigned char *)call_input_payload(&call) + prefix_len,
           name, name_len);
    return copy_fixed_reply(&call, entry, sizeof(*entry));
}

int sud_fuse_mkdir(uint64_t parent, const char *name, uint32_t mode,
                   uint32_t umask, struct fuse_entry_out *entry)
{
    struct fuse_mkdir_in input = { .mode = mode, .umask = umask };
    return entry_name_call(FUSE_MKDIR, parent, &input, sizeof(input),
                           name, entry);
}

int sud_fuse_mknod(uint64_t parent, const char *name, uint32_t mode,
                   uint32_t rdev, uint32_t umask,
                   struct fuse_entry_out *entry)
{
    struct fuse_mknod_in input;
    memset(&input, 0, sizeof(input));
    input.mode = mode;
    input.rdev = rdev;
    input.umask = umask;
    return entry_name_call(FUSE_MKNOD, parent, &input, sizeof(input),
                           name, entry);
}

int sud_fuse_symlink(uint64_t parent, const char *name, const char *target,
                     struct fuse_entry_out *entry)
{
    size_t name_len = strlen(name) + 1;
    size_t target_len = strlen(target) + 1;
    if (name_len <= 1 || name_len > 256) return -ENAMETOOLONG;
    if (name_len + target_len > SUD_FS_SLOT_DATA - sizeof(struct fuse_in_header))
        return -ENAMETOOLONG;
    struct fuse_call call;
    int result = call_begin(&call, FUSE_SYMLINK, parent,
                            name_len + target_len);
    if (result != 0) return result;
    unsigned char *payload = call_input_payload(&call);
    memcpy(payload, name, name_len);
    memcpy(payload + name_len, target, target_len);
    return copy_fixed_reply(&call, entry, sizeof(*entry));
}

int sud_fuse_unlink(uint64_t parent, const char *name, int directory)
{
    size_t name_len = strlen(name) + 1;
    if (name_len <= 1 || name_len > 256) return -ENAMETOOLONG;
    struct fuse_call call;
    int result = call_begin(&call, directory ? FUSE_RMDIR : FUSE_UNLINK,
                            parent, name_len);
    if (result != 0) return result;
    memcpy(call_input_payload(&call), name, name_len);
    result = call_submit(&call);
    call_end(&call);
    return result;
}

int sud_fuse_rename(uint64_t old_parent, const char *old_name,
                    uint64_t new_parent, const char *new_name,
                    uint32_t flags)
{
    size_t old_len = strlen(old_name) + 1;
    size_t new_len = strlen(new_name) + 1;
    if (old_len <= 1 || old_len > 256 || new_len <= 1 || new_len > 256)
        return -ENAMETOOLONG;
    struct fuse_call call;
    int result = call_begin(&call, FUSE_RENAME2, old_parent,
                            sizeof(struct fuse_rename2_in) + old_len + new_len);
    if (result != 0) return result;
    struct fuse_rename2_in *input = call_input_payload(&call);
    memset(input, 0, sizeof(*input));
    input->newdir = new_parent;
    input->flags = flags;
    unsigned char *names = (unsigned char *)(input + 1);
    memcpy(names, old_name, old_len);
    memcpy(names + old_len, new_name, new_len);
    result = call_submit(&call);
    call_end(&call);
    return result;
}

int sud_fuse_link(uint64_t inode, uint64_t new_parent, const char *new_name,
                  struct fuse_entry_out *entry)
{
    struct fuse_link_in input = { .oldnodeid = inode };
    return entry_name_call(FUSE_LINK, new_parent, &input, sizeof(input),
                           new_name, entry);
}

size_t sud_fuse_max_read(void)
{
    return SUD_FS_SLOT_DATA - sizeof(struct fuse_out_header);
}

size_t sud_fuse_max_write(void)
{
    return SUD_FS_SLOT_DATA - sizeof(struct fuse_in_header)
        - sizeof(struct fuse_write_in);
}

long sud_fuse_read(uint64_t inode, uint64_t handle, uint64_t offset,
                   uint32_t flags, void *buffer, size_t size)
{
    if (size > sud_fuse_max_read()) size = sud_fuse_max_read();
    struct fuse_call call;
    int result = call_begin(&call, FUSE_READ, inode, sizeof(struct fuse_read_in));
    if (result != 0) return result;
    struct fuse_read_in *input = call_input_payload(&call);
    memset(input, 0, sizeof(*input));
    input->fh = handle;
    input->offset = offset;
    input->size = (uint32_t)size;
    input->flags = flags;
    result = call_submit(&call);
    long count = result;
    if (result == 0) {
        if (call.payload_len > size) count = -EPROTO;
        else {
            memcpy(buffer, call.payload, call.payload_len);
            count = (long)call.payload_len;
        }
    }
    call_end(&call);
    return count;
}

long sud_fuse_write(uint64_t inode, uint64_t handle, uint64_t offset,
                    uint32_t flags, const void *buffer, size_t size)
{
    if (size > sud_fuse_max_write()) size = sud_fuse_max_write();
    struct fuse_call call;
    int result = call_begin(&call, FUSE_WRITE, inode,
                            sizeof(struct fuse_write_in) + size);
    if (result != 0) return result;
    struct fuse_write_in *input = call_input_payload(&call);
    memset(input, 0, sizeof(*input));
    input->fh = handle;
    input->offset = offset;
    input->size = (uint32_t)size;
    input->flags = flags;
    memcpy(input + 1, buffer, size);
    struct fuse_write_out output;
    result = copy_fixed_reply(&call, &output, sizeof(output));
    return result == 0 ? (long)output.size : result;
}

int sud_fuse_flush(uint64_t inode, uint64_t handle, uint32_t flags)
{
    struct fuse_call call;
    int result = call_begin(&call, FUSE_FLUSH, inode, sizeof(struct fuse_flush_in));
    if (result != 0) return result;
    struct fuse_flush_in *input = call_input_payload(&call);
    memset(input, 0, sizeof(*input));
    input->fh = handle;
    input->lock_owner = (uint64_t)raw_getpid();
    (void)flags;
    result = call_submit(&call);
    call_end(&call);
    return result;
}

int sud_fuse_release(uint64_t inode, uint64_t handle, uint32_t flags)
{
    struct fuse_call call;
    int result = call_begin(&call, FUSE_RELEASE, inode, sizeof(struct fuse_release_in));
    if (result != 0) return result;
    struct fuse_release_in *input = call_input_payload(&call);
    memset(input, 0, sizeof(*input));
    input->fh = handle;
    input->flags = flags;
    input->lock_owner = (uint64_t)raw_getpid();
    result = call_submit(&call);
    call_end(&call);
    return result;
}

long sud_fuse_readdir(uint64_t inode, uint64_t handle, uint64_t offset,
                      void *buffer, size_t size)
{
    if (size > sud_fuse_max_read()) size = sud_fuse_max_read();
    struct fuse_call call;
    int result = call_begin(&call, FUSE_READDIR, inode,
                            sizeof(struct fuse_read_in));
    if (result != 0) return result;
    struct fuse_read_in *input = call_input_payload(&call);
    memset(input, 0, sizeof(*input));
    input->fh = handle;
    input->offset = offset;
    input->size = (uint32_t)size;
    result = call_submit(&call);
    long count = result;
    if (result == 0) {
        if (call.payload_len > size) count = -EPROTO;
        else {
            memcpy(buffer, call.payload, call.payload_len);
            count = (long)call.payload_len;
        }
    }
    call_end(&call);
    return count;
}

int sud_fuse_releasedir(uint64_t inode, uint64_t handle, uint32_t flags)
{
    struct fuse_call call;
    int result = call_begin(&call, FUSE_RELEASEDIR, inode,
                            sizeof(struct fuse_release_in));
    if (result != 0) return result;
    struct fuse_release_in *input = call_input_payload(&call);
    memset(input, 0, sizeof(*input));
    input->fh = handle;
    input->flags = flags;
    result = call_submit(&call);
    call_end(&call);
    return result;
}

int sud_fuse_setattr(uint64_t inode, const struct fuse_setattr_in *request,
                     struct fuse_attr_out *attributes)
{
    struct fuse_call call;
    int result = call_begin(&call, FUSE_SETATTR, inode, sizeof(*request));
    if (result != 0) return result;
    memcpy(call_input_payload(&call), request, sizeof(*request));
    return copy_fixed_reply(&call, attributes, sizeof(*attributes));
}

int sud_fuse_access(uint64_t inode, uint32_t mask)
{
    struct fuse_call call;
    int result = call_begin(&call, FUSE_ACCESS, inode,
                            sizeof(struct fuse_access_in));
    if (result != 0) return result;
    struct fuse_access_in *input = call_input_payload(&call);
    input->mask = mask;
    input->padding = 0;
    result = call_submit(&call);
    call_end(&call);
    return result;
}

int sud_fuse_fsync(uint64_t inode, uint64_t handle, int directory,
                   int datasync)
{
    struct fuse_call call;
    int result = call_begin(&call, directory ? FUSE_FSYNCDIR : FUSE_FSYNC,
                            inode, sizeof(struct fuse_fsync_in));
    if (result != 0) return result;
    struct fuse_fsync_in *input = call_input_payload(&call);
    input->fh = handle;
    input->fsync_flags = datasync ? 1u : 0u;
    input->padding = 0;
    result = call_submit(&call);
    call_end(&call);
    return result;
}
