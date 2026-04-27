/*
 * proctrace.c — Log exec/exit/open/cwd/output for tagged process subtrees.
 *
 * v13
 * ===
 * • Output is read from the /proc/proctrace/new fd — no log file.
 *   open() creates session + tags opener.  read() drains binary wire format.
 *   close() destroys session.
 *
 * • Per-session kernel ring buffer with backpressure:
 *   when buffer is full, producing workqueue workers block until
 *   the reader drains space.  This means traced processes slow
 *   down proportionally to how fast the reader consumes — no lost
 *   events, no OOM, no disk I/O.
 *
 * • TGID-based tagging (threads bundled).
 * • Binary trace format (see trace/trace.h).
 * • OPEN logs inode + device of the result.
 * • CWD event emitted on exec and chdir (replaces cwd field in EXEC).
 * • Compat 4.15–6.x.
 */

#include <linux/module.h>
#include <linux/kernel.h>
#include <linux/version.h>
#include <linux/slab.h>
#include <linux/list.h>
#include <linux/hashtable.h>
#include <linux/spinlock.h>
#include <linux/mutex.h>
#include <linux/atomic.h>
#include <linux/proc_fs.h>
#include <linux/seq_file.h>
#include <linux/uaccess.h>
#include <linux/fs.h>
#include <linux/fs_struct.h>
#include <linux/file.h>
#include <linux/fdtable.h>
#include <linux/path.h>
#include <linux/dcache.h>
#include <linux/mm.h>
#include <linux/sched.h>
#include <linux/sched/mm.h>
#include <linux/pid_namespace.h>
#include <linux/binfmts.h>
#include <linux/timekeeping.h>
#include <linux/kprobes.h>
#include <linux/workqueue.h>
#include <linux/ctype.h>
#include <linux/fcntl.h>
#include <linux/elf.h>
#include <linux/namei.h>
#include <linux/vmalloc.h>
#include <linux/wait.h>
#include <linux/poll.h>
#include <linux/circ_buf.h>

#include "wire/wire.h"
#include "trace/trace.h"

MODULE_LICENSE("GPL");
MODULE_AUTHOR("proctrace");
MODULE_DESCRIPTION("Log exec/exit/open/cwd/output for tagged process subtrees");

/* ===============================================================
 * Compat
 * =============================================================== */

#if LINUX_VERSION_CODE < KERNEL_VERSION(5, 8, 0)
  #define mmap_read_lock(mm)    down_read(&(mm)->mmap_sem)
  #define mmap_read_unlock(mm)  up_read(&(mm)->mmap_sem)
#endif

#if LINUX_VERSION_CODE < KERNEL_VERSION(5, 6, 0)
  #define PROC_OPS_TYPE             struct file_operations
  #define PROC_OPS_OWNER            .owner   = THIS_MODULE,
  #define PROC_OPS_OPEN(fn)         .open    = (fn),
  #define PROC_OPS_READ(fn)         .read    = (fn),
  #define PROC_OPS_WRITE(fn)        .write   = (fn),
  #define PROC_OPS_RELEASE(fn)      .release = (fn),
  #define PROC_OPS_LSEEK(fn)        .llseek  = (fn),
  #define PROC_OPS_POLL(fn)         .poll    = (fn),
#else
  #define PROC_OPS_TYPE             struct proc_ops
  #define PROC_OPS_OWNER
  #define PROC_OPS_OPEN(fn)         .proc_open    = (fn),
  #define PROC_OPS_READ(fn)         .proc_read    = (fn),
  #define PROC_OPS_WRITE(fn)        .proc_write   = (fn),
  #define PROC_OPS_RELEASE(fn)      .proc_release = (fn),
  #define PROC_OPS_LSEEK(fn)        .proc_lseek   = (fn),
  #define PROC_OPS_POLL(fn)         .proc_poll    = (fn),
#endif

#if LINUX_VERSION_CODE < KERNEL_VERSION(4, 16, 0)
  #ifndef strscpy
    #define strscpy(dst, src, sz) strlcpy(dst, src, sz)
  #endif
#endif

#if LINUX_VERSION_CODE >= KERNEL_VERSION(5, 10, 0)
  #define FORK_SYMBOL_PRIMARY   "kernel_clone"
  #define FORK_SYMBOL_FALLBACK  "_do_fork"
#else
  #define FORK_SYMBOL_PRIMARY   "_do_fork"
  #define FORK_SYMBOL_FALLBACK  "do_fork"
#endif

#if LINUX_VERSION_CODE >= KERNEL_VERSION(5, 9, 0)
  #define EXEC_SYMBOL_PRIMARY   "bprm_execve"
  #define EXEC_SYMBOL_FALLBACK  "exec_binprm"
#else
  #define EXEC_SYMBOL_PRIMARY   "exec_binprm"
  #define EXEC_SYMBOL_FALLBACK  "search_binary_handler"
#endif

#if LINUX_VERSION_CODE >= KERNEL_VERSION(5, 12, 0)
  #define OPEN_SYMBOL_PRIMARY   "do_sys_openat2"
  #define OPEN_SYMBOL_FALLBACK  "do_sys_open"
#else
  #define OPEN_SYMBOL_PRIMARY   "do_sys_open"
  #define OPEN_SYMBOL_FALLBACK  "do_sys_openat2"
#endif

#define UNLINK_SYMBOL_PRIMARY   "do_unlinkat"
#define UNLINK_SYMBOL_FALLBACK  "vfs_unlink"

/* ===============================================================
 * Constants
 * =============================================================== */

#define TAG_HASH_BITS       10
#define ARGV_MAX_READ       32768
#define ENV_MAX_READ        65536
#define WRITE_CAPTURE_MAX   4096
#define RING_BUF_ORDER      8       /* 2^8 pages = 1MB ring buffer */
#define RING_BUF_SIZE       (PAGE_SIZE << RING_BUF_ORDER)

/* ===============================================================
 * /dev/null inode
 * =============================================================== */

static struct inode *devnull_ino;

static void capture_devnull_inode(void)
{
    struct path p;
    if (kern_path("/dev/null", LOOKUP_FOLLOW, &p) == 0) {
        devnull_ino = p.dentry->d_inode; path_put(&p);
    }
}

/* ===============================================================
 * Ring buffer (per-session)
 * ===============================================================
 *
 * Simple byte ring buffer.  Producer (workqueue) blocks when full.
 * Consumer (read syscall) blocks when empty.
 * Protected by ring_lock spinlock; waitqueues for blocking.
 */

struct ring_buffer {
    char               *buf;
    size_t              size;       /* always power of 2 */
    size_t              head;       /* write position */
    size_t              tail;       /* read position */
    spinlock_t          lock;
    wait_queue_head_t   wq_reader;  /* reader waits here when empty */
    wait_queue_head_t   wq_writer;  /* writer waits here when full */
    bool                closed;     /* set on session teardown */
    ev_state            st;         /* delta encoder state */
    spinlock_t          emit_lock;  /* serialize ev_state + queue ordering */
};

static int ring_init(struct ring_buffer *rb, size_t size)
{
    rb->buf = vmalloc(size);
    if (!rb->buf) return -ENOMEM;
    rb->size = size; rb->head = 0; rb->tail = 0; rb->closed = false;
    spin_lock_init(&rb->lock);
    init_waitqueue_head(&rb->wq_reader);
    init_waitqueue_head(&rb->wq_writer);
    memset(&rb->st, 0, sizeof(rb->st));
    spin_lock_init(&rb->emit_lock);
    return 0;
}

static void ring_destroy(struct ring_buffer *rb)
{
    if (rb->buf) { vfree(rb->buf); rb->buf = NULL; }
}

static size_t ring_used(struct ring_buffer *rb)
{ return rb->head - rb->tail; }

static size_t ring_free(struct ring_buffer *rb)
{ return rb->size - ring_used(rb); }

/*
 * Write into ring buffer.  Blocks if full (called from workqueue context
 * where sleeping is safe).  Returns bytes written, or -EPIPE if closed.
 */
static ssize_t ring_write(struct ring_buffer *rb, const char *data, size_t len)
{
    size_t written = 0;

    while (written < len) {
        size_t avail, chunk, off;
        unsigned long flags;

        /* Wait for space. */
        if (wait_event_interruptible(rb->wq_writer,
                rb->closed || ring_free(rb) > 0))
            return written ? (ssize_t)written : -EINTR;

        if (rb->closed) return -EPIPE;

        spin_lock_irqsave(&rb->lock, flags);
        avail = ring_free(rb);
        if (avail == 0) {
            spin_unlock_irqrestore(&rb->lock, flags);
            cond_resched();
            continue;
        }
        chunk = len - written;
        if (chunk > avail) chunk = avail;

        off = rb->head & (rb->size - 1);
        if (off + chunk > rb->size) {
            size_t first = rb->size - off;
            memcpy(rb->buf + off, data + written, first);
            memcpy(rb->buf, data + written + first, chunk - first);
        } else {
            memcpy(rb->buf + off, data + written, chunk);
        }
        rb->head += chunk;
        written += chunk;
        spin_unlock_irqrestore(&rb->lock, flags);

        wake_up_interruptible(&rb->wq_reader);
    }

    return (ssize_t)written;
}

/*
 * Read from ring buffer into user buffer.  Blocks if empty.
 * Returns bytes read, 0 on closed+empty (EOF), or -errno.
 */
static ssize_t ring_read_user(struct ring_buffer *rb,
                              char __user *ubuf, size_t count)
{
    size_t avail, chunk, off, first;
    unsigned long flags;

    if (wait_event_interruptible(rb->wq_reader,
            rb->closed || ring_used(rb) > 0))
        return -EINTR;

    spin_lock_irqsave(&rb->lock, flags);
    avail = ring_used(rb);
    if (avail == 0) {
        spin_unlock_irqrestore(&rb->lock, flags);
        return 0; /* EOF — closed and drained */
    }
    chunk = count;
    if (chunk > avail) chunk = avail;

    off = rb->tail & (rb->size - 1);
    if (off + chunk > rb->size) {
        first = rb->size - off;
        spin_unlock_irqrestore(&rb->lock, flags);
        if (copy_to_user(ubuf, rb->buf + off, first)) return -EFAULT;
        if (copy_to_user(ubuf + first, rb->buf, chunk - first)) return -EFAULT;
    } else {
        spin_unlock_irqrestore(&rb->lock, flags);
        if (copy_to_user(ubuf, rb->buf + off, chunk)) return -EFAULT;
    }

    spin_lock_irqsave(&rb->lock, flags);
    rb->tail += chunk;
    spin_unlock_irqrestore(&rb->lock, flags);

    wake_up_interruptible(&rb->wq_writer);
    return (ssize_t)chunk;
}

/* ===============================================================
 * Data structures
 * =============================================================== */

struct tagged_pid {
    pid_t pid;
    struct hlist_node hnode;
    struct rcu_head rcu;
};

struct trace_session {
    unsigned int        id;
    pid_t               root_tgid;
    bool                dead;

    struct ring_buffer  ring;
    struct inode       *creator_stdout_ino;

    DECLARE_HASHTABLE(tags, TAG_HASH_BITS);
    spinlock_t          tag_lock;
    atomic_t            tag_count;

    atomic_t                  pending_work;
    wait_queue_head_t         wq_drain;
    struct workqueue_struct  *log_wq;   /* per-session ordered workqueue */

    struct list_head    list;
};

static LIST_HEAD(sessions);
static DEFINE_MUTEX(sessions_mutex);
static atomic_t next_session_id = ATOMIC_INIT(1);

static bool have_ksys_write;

/* ===============================================================
 * Deferred writes via per-session ordered workqueue
 * ===============================================================
 *
 * Probe handlers (atomic-ish context) build the wire bytes for an
 * event under the session's emit_lock, then queue a log_work item
 * onto the per-session ORDERED workqueue. The work item runs in
 * sleepable context and calls ring_write, which blocks on a full
 * ring providing backpressure to traced processes.
 *
 * The ordered workqueue (`alloc_ordered_workqueue`) guarantees FIFO
 * execution per session, preserving the byte order of delta-encoded
 * events. Order across sessions is independent.
 */

struct log_work {
    struct work_struct      work;
    struct trace_session   *session;
    unsigned int            session_id;
    size_t                  len;
    char                    data[];
};

static void log_work_fn(struct work_struct *work)
{
    struct log_work *lw = container_of(work, struct log_work, work);
    struct trace_session *s = lw->session;

    if (s && !s->dead && s->id == lw->session_id)
        ring_write(&s->ring, lw->data, lw->len);

    if (s && atomic_dec_and_test(&s->pending_work))
        wake_up(&s->wq_drain);

    kfree(lw);
}

/* Allocate an empty log_work with `cap` bytes of inline payload.
 * Caller fills lw->data and sets lw->len, then calls
 * session_log_submit() to enqueue. GFP_ATOMIC because callers run
 * with the session's emit_lock held (irqs off). */
static struct log_work *log_work_alloc(struct trace_session *s, size_t cap)
{
    struct log_work *lw;
    if (s->dead) return NULL;
    lw = kmalloc(sizeof(*lw) + cap, GFP_ATOMIC);
    if (!lw) return NULL;
    INIT_WORK(&lw->work, log_work_fn);
    lw->session = s;
    lw->session_id = s->id;
    lw->len = 0;
    return lw;
}

/* Enqueue a previously-built log_work onto the session's ordered
 * workqueue. Must be called with the session's emit_lock held so
 * that successive events for the same session enter the workqueue
 * in delta-encoded byte order. */
static void session_log_submit(struct trace_session *s, struct log_work *lw)
{
    atomic_inc(&s->pending_work);
    queue_work(s->log_wq, &lw->work);
}

/* ===============================================================
 * Wire event emission helper.
 *
 * Builds one wire event atom into a log_work and submits it to the
 * session's per-session ordered workqueue. Must be called with the
 * sessions list RCU-read-locked. Safe in atomic context (irqs are
 * disabled by the spinlock). Drops the event silently on allocation
 * failure or if the session is dying.
 * =============================================================== */
static void emit_one(struct trace_session *s,
                     int32_t type, uint64_t ts_ns,
                     pid_t pid, pid_t tgid, pid_t ppid,
                     pid_t nspid, pid_t nstgid,
                     const int64_t *extras, unsigned n_extras,
                     const void *blob, size_t blen)
{
    struct log_work *lw;
    unsigned long flags;
    uint8_t hdr[EV_HEADER_MAX];
    Dst hd, od;
    size_t hlen;

    lw = log_work_alloc(s, EV_HEADER_MAX + 2u * WIRE_PREFIX_MAX + blen);
    if (!lw) return;

    spin_lock_irqsave(&s->ring.emit_lock, flags);
    hd = wire_dst(hdr, sizeof hdr);
    /* proctrace is single-producer per session — stream_id 1. */
    ev_build_header(&s->ring.st, &hd, 1u, type, ts_ns,
                    pid, tgid, ppid, nspid, nstgid,
                    extras, n_extras);
    if (hd.p) {
        hlen = (size_t)((uint8_t *)hd.p - hdr);
        od = wire_dst(lw->data,
                      EV_HEADER_MAX + 2u * WIRE_PREFIX_MAX + blen);
        wire_put_pair(&od,
                      wire_src(hdr, hlen),
                      wire_src(blob, blen));
        if (od.p) {
            lw->len = (size_t)((uint8_t *)od.p - (uint8_t *)lw->data);
            session_log_submit(s, lw);
            lw = NULL;
        }
    }
    spin_unlock_irqrestore(&s->ring.emit_lock, flags);
    if (lw) kfree(lw);
}

/* ===============================================================
 * Session tag helpers
 * =============================================================== */

static bool session_is_tagged(struct trace_session *s, pid_t tgid)
{
    struct tagged_pid *entry;
    hash_for_each_possible_rcu(s->tags, entry, hnode, tgid) {
        if (entry->pid == tgid) return true;
    }
    return false;
}

static int session_tag_pid(struct trace_session *s, pid_t tgid)
{
    struct tagged_pid *entry, *cur; unsigned long flags;
    if (session_is_tagged(s, tgid)) return 0;
    entry = kmalloc(sizeof(*entry), GFP_ATOMIC);
    if (!entry) return -ENOMEM;
    entry->pid = tgid; INIT_HLIST_NODE(&entry->hnode);
    spin_lock_irqsave(&s->tag_lock, flags);
    /* Re-check under lock to prevent duplicate entries (TOCTOU). */
    hash_for_each_possible(s->tags, cur, hnode, tgid) {
        if (cur->pid == tgid) {
            spin_unlock_irqrestore(&s->tag_lock, flags);
            kfree(entry);
            return 0;
        }
    }
    hash_add_rcu(s->tags, &entry->hnode, tgid);
    spin_unlock_irqrestore(&s->tag_lock, flags);
    atomic_inc(&s->tag_count); return 0;
}

static void session_untag_pid(struct trace_session *s, pid_t tgid)
{
    struct tagged_pid *entry; unsigned long flags;
    spin_lock_irqsave(&s->tag_lock, flags);
    hash_for_each_possible(s->tags, entry, hnode, tgid) {
        if (entry->pid == tgid) {
            hash_del_rcu(&entry->hnode);
            spin_unlock_irqrestore(&s->tag_lock, flags);
            kfree_rcu(entry, rcu);
            atomic_dec(&s->tag_count); return;
        }
    }
    spin_unlock_irqrestore(&s->tag_lock, flags);
}

static void session_free_all_tags(struct trace_session *s)
{
    struct tagged_pid *entry; struct hlist_node *tmp;
    unsigned long flags; int bkt;
    HLIST_HEAD(free_list);
    spin_lock_irqsave(&s->tag_lock, flags);
    hash_for_each_safe(s->tags, bkt, tmp, entry, hnode) {
        hash_del(&entry->hnode);
        hlist_add_head(&entry->hnode, &free_list);
    }
    spin_unlock_irqrestore(&s->tag_lock, flags);
    hlist_for_each_entry_safe(entry, tmp, &free_list, hnode)
        kfree(entry);
    atomic_set(&s->tag_count, 0);
}

/* ===============================================================
 * fd → inode
 * =============================================================== */

static struct inode *get_fd_inode(unsigned int fd)
{
    struct file *f; struct inode *ino = NULL;
    f = fget(fd);
    if (f) { ino = file_inode(f); fput(f); }
    return ino;
}

/* ===============================================================
 * Session lifecycle
 * =============================================================== */

static struct trace_session *session_create(pid_t root_tgid)
{
    struct trace_session *s; int ret;
    uint8_t version_atom[16];
    Dst d;
    
    s = kzalloc(sizeof(*s), GFP_KERNEL);
    if (!s) return ERR_PTR(-ENOMEM);

    ret = ring_init(&s->ring, RING_BUF_SIZE);
    if (ret) { kfree(s); return ERR_PTR(ret); }

    /* Per-session ordered workqueue: serialises log_work execution
     * so that the byte order in the ring matches the order in which
     * emit_* built (and delta-encoded) the events. */
    s->log_wq = alloc_ordered_workqueue("proctrace-%u",
                                        WQ_MEM_RECLAIM,
                                        atomic_read(&next_session_id));
    if (!s->log_wq) {
        ring_destroy(&s->ring);
        kfree(s);
        return ERR_PTR(-ENOMEM);
    }

    /* Write TRACE_VERSION as first atom in the stream. */
    d = wire_dst(version_atom, sizeof(version_atom));
    wire_put_u64(&d, TRACE_VERSION);
    if (d.p)
        ring_write(&s->ring, (char *)version_atom,
                   (uint8_t *)d.p - version_atom);

    s->id = atomic_fetch_inc(&next_session_id);
    s->root_tgid = root_tgid; s->dead = false;
    s->creator_stdout_ino = get_fd_inode(1);
    atomic_set(&s->tag_count, 0);
    atomic_set(&s->pending_work, 0);
    hash_init(s->tags); spin_lock_init(&s->tag_lock);
    init_waitqueue_head(&s->wq_drain);
    INIT_LIST_HEAD(&s->list);

    session_tag_pid(s, root_tgid);

    mutex_lock(&sessions_mutex);
    list_add_tail_rcu(&s->list, &sessions);
    mutex_unlock(&sessions_mutex);

    pr_info("proctrace: session %u root_tgid=%d\n", s->id, root_tgid);
    return s;
}

static void session_destroy(struct trace_session *s)
{
    s->dead = true; smp_wmb();
    s->ring.closed = true;
    wake_up_interruptible(&s->ring.wq_writer);
    wake_up_interruptible(&s->ring.wq_reader);

    mutex_lock(&sessions_mutex);
    list_del_rcu(&s->list);
    mutex_unlock(&sessions_mutex);

    /*
     * Wait for all RCU readers (kprobe handlers) to finish so no new
     * work items referencing this session can be queued afterwards.
     */
    synchronize_rcu();

    /*
     * All RCU readers have finished, so no new work items for this
     * session will be queued.  Wait only for this session's pending
     * items to complete.  Because the ring is closed, any workers
     * blocked in ring_write() will see ->closed and return promptly.
     *
     * Unlike the old flush_workqueue() call, this does NOT wait for
     * workers belonging to other sessions — that was the source of
     * the deadlock where a full ring on session B prevented
     * session A's destroy from ever completing.
     */
    wait_event(s->wq_drain, atomic_read(&s->pending_work) == 0);

    if (s->log_wq) destroy_workqueue(s->log_wq);
    session_free_all_tags(s);
    ring_destroy(&s->ring);
    kfree(s);
}

/* ===============================================================
 * Task info helpers
 * =============================================================== */

static char *get_task_cwd(struct task_struct *task, char *buf, int buflen)
{
    struct path pwd; char *p;
    task_lock(task);
    if (!task->fs) { task_unlock(task); return NULL; }
    get_fs_pwd(task->fs, &pwd); task_unlock(task);
    p = d_path(&pwd, buf, buflen); path_put(&pwd);
    return IS_ERR(p) ? NULL : p;
}

static char *get_task_exe_path(struct task_struct *task, char *buf, int buflen)
{
    struct mm_struct *mm; struct file *exe; char *p = NULL;
    mm = get_task_mm(task); if (!mm) return NULL;
    rcu_read_lock();
    exe = rcu_dereference(mm->exe_file);
    if (exe) { get_file(exe); rcu_read_unlock();
        p = file_path(exe, buf, buflen); fput(exe);
        if (IS_ERR(p)) p = NULL;
    } else { rcu_read_unlock(); }
    mmput(mm); return p;
}

static char *read_mm_region(struct task_struct *task,
                            unsigned long start, unsigned long end,
                            size_t max, size_t *out_len)
{
    struct mm_struct *mm; unsigned long len; char *buf; int ret;
    mm = get_task_mm(task); if (!mm) return NULL;
    len = end - start;
    if (len == 0) { mmput(mm); return NULL; }
    if (len > max) len = max;
    buf = kmalloc(len, GFP_ATOMIC);
    if (!buf) { mmput(mm); return NULL; }
    ret = access_process_vm(task, start, buf, len, 0);
    mmput(mm);
    if (ret <= 0) { kfree(buf); return NULL; }
    *out_len = (size_t)ret; return buf;
}

static char *read_task_argv_raw(struct task_struct *task, size_t *out_len)
{
    struct mm_struct *mm; unsigned long s, e;
    mm = get_task_mm(task); if (!mm) return NULL;
    mmap_read_lock(mm); s=mm->arg_start; e=mm->arg_end; mmap_read_unlock(mm);
    mmput(mm); return read_mm_region(task, s, e, ARGV_MAX_READ, out_len);
}

static char *read_task_env_raw(struct task_struct *task, size_t *out_len)
{
    struct mm_struct *mm; unsigned long s, e;
    mm = get_task_mm(task); if (!mm) return NULL;
    mmap_read_lock(mm); s=mm->env_start; e=mm->env_end; mmap_read_unlock(mm);
    mmput(mm); return read_mm_region(task, s, e, ENV_MAX_READ, out_len);
}

static pid_t get_task_ns_pid(struct task_struct *t)
{ pid_t p; rcu_read_lock(); p=task_pid_nr_ns(t,task_active_pid_ns(t)); rcu_read_unlock(); return p; }

static pid_t get_task_ns_tgid(struct task_struct *t)
{ pid_t p; rcu_read_lock(); p=task_tgid_nr_ns(t,task_active_pid_ns(t)); rcu_read_unlock(); return p; }

/* ===============================================================
 * Fast tgid check
 * =============================================================== */

static bool tgid_in_any_session(pid_t tgid)
{
    struct trace_session *s; bool f=false;
    rcu_read_lock();
    list_for_each_entry_rcu(s, &sessions, list) {
        if(!s->dead && session_is_tagged(s, tgid)){f=true;break;}
    }
    rcu_read_unlock(); return f;
}

/* ===============================================================
 * Emit CWD event
 * =============================================================== */

/* ===============================================================
 * Emit CWD event
 * =============================================================== */

static void emit_cwd_event(struct task_struct *task)
{
    struct trace_session *s;
    pid_t tgid = task->tgid;
    struct timespec64 ts;
    char *cwd_buf, *cwd;
    uint64_t ts_ns;

    cwd_buf = kmalloc(PATH_MAX, GFP_ATOMIC);
    if (!cwd_buf) return;
    cwd = get_task_cwd(task, cwd_buf, PATH_MAX);
    if (!cwd) { kfree(cwd_buf); return; }

    ktime_get_real_ts64(&ts);
    ts_ns = (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;

    rcu_read_lock();
    list_for_each_entry_rcu(s, &sessions, list) {
        if (!s->dead && session_is_tagged(s, tgid))
            emit_one(s, EV_CWD, ts_ns,
                     task->pid, task->tgid, task_ppid_nr(task),
                     get_task_ns_pid(task), get_task_ns_tgid(task),
                     NULL, 0, cwd, strlen(cwd));
    }
    rcu_read_unlock();
    kfree(cwd_buf);
}

/* ===============================================================
 * Emit write log
 * =============================================================== */

static void emit_write_log(struct task_struct *task, const char *stream,
                           unsigned long ubuf_ptr, size_t count)
{
    struct trace_session *s;
    pid_t tgid = task->tgid;
    struct timespec64 ts;
    char *ubuf = NULL;
    size_t to_read;
    unsigned long left;
    uint64_t ts_ns;
    int32_t ev;

    ktime_get_real_ts64(&ts);
    ts_ns = (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;

    to_read = count; if (to_read > WRITE_CAPTURE_MAX) to_read = WRITE_CAPTURE_MAX;
    ubuf = kmalloc(to_read, GFP_ATOMIC); if (!ubuf) return;
    left = copy_from_user(ubuf, (const void __user *)ubuf_ptr, to_read);
    if (left == to_read) goto out;
    to_read -= left;

    ev = (stream[0] == 'S' && stream[3] == 'E') ? EV_STDERR : EV_STDOUT;

    rcu_read_lock();
    list_for_each_entry_rcu(s, &sessions, list) {
        if (!s->dead && session_is_tagged(s, tgid))
            emit_one(s, ev, ts_ns,
                     task->pid, task->tgid, task_ppid_nr(task),
                     get_task_ns_pid(task), get_task_ns_tgid(task),
                     NULL, 0, ubuf, to_read);
    }
    rcu_read_unlock();
out:
    kfree(ubuf);
}

/* ===============================================================
 * kprobe hooks
 * =============================================================== */

/* ---- 1. Fork ---- */

static int fork_entry(struct kretprobe_instance *ri, struct pt_regs *regs)
{ return 0; }

static int fork_ret(struct kretprobe_instance *ri, struct pt_regs *regs)
{
    struct trace_session *s;
    pid_t parent_tgid = current->tgid;
    pid_t child_pid = (pid_t)regs_return_value(regs);
    if (child_pid <= 0) return 0;
    rcu_read_lock();
    list_for_each_entry_rcu(s, &sessions, list) {
        if (!s->dead && session_is_tagged(s, parent_tgid))
            session_tag_pid(s, child_pid);
    }
    rcu_read_unlock();
    return 0;
}

static struct kretprobe fork_kretprobe = {
    .handler=fork_ret, .entry_handler=fork_entry, .maxactive=64,
};

/* ===============================================================
 * Emit fake OPEN events for inherited fds
 * ===============================================================
 *
 * On exec, the new program inherits the fd table of its predecessor
 * (minus O_CLOEXEC ones, which the kernel has already closed by the
 * time our kretprobe fires).  Emit one "inherited":true OPEN event
 * per surviving fd, so the trace records which files/pipes/sockets
 * a freshly-exec'd process started with — even if it never calls
 * open() itself (e.g. `sort` in `cat a | sort > b`).
 *
 * Both ends of a pipe share the same pipefs inode, so matching
 * (dev,ino) across two inherited OPEN events unambiguously pairs
 * the two ends of a shell pipe.
 */

static void emit_inherited_open_for_fd(struct task_struct *task,
                                       struct timespec64 *ts,
                                       unsigned int fd, struct file *file)
{
    struct trace_session *s;
    pid_t tgid = task->tgid;
    struct inode *ino;
    unsigned long ino_nr = 0;
    dev_t dev = 0;
    char *path_buf = NULL, *path = NULL;
    uint64_t ts_ns;
    int64_t extras[7];

    ino = file_inode(file);
    if (ino) { ino_nr = ino->i_ino; dev = ino->i_sb->s_dev; }

    path_buf = kmalloc(PATH_MAX, GFP_ATOMIC);
    if (!path_buf) return;
    path = file_path(file, path_buf, PATH_MAX);
    if (IS_ERR(path)) path = NULL;

    ts_ns = (uint64_t)ts->tv_sec * 1000000000ull + (uint64_t)ts->tv_nsec;

    extras[0] = (int64_t)file->f_flags;
    extras[1] = (int64_t)fd;
    extras[2] = (int64_t)ino_nr;
    extras[3] = (int64_t)MAJOR(dev);
    extras[4] = (int64_t)MINOR(dev);
    extras[5] = 0;   /* err */
    extras[6] = 1;   /* inherited */

    rcu_read_lock();
    list_for_each_entry_rcu(s, &sessions, list) {
        if (!s->dead && session_is_tagged(s, tgid))
            emit_one(s, EV_OPEN, ts_ns,
                     task->pid, task->tgid, task_ppid_nr(task),
                     get_task_ns_pid(task), get_task_ns_tgid(task),
                     extras, 7,
                     path ? path : "", path ? strlen(path) : 0);
    }
    rcu_read_unlock();

    kfree(path_buf);
}

struct inherited_open_ctx {
    struct task_struct *task;
    struct timespec64   ts;
};

static int inherited_open_iter(const void *p, struct file *file, unsigned fd)
{
    struct inherited_open_ctx *ctx = (struct inherited_open_ctx *)p;
    emit_inherited_open_for_fd(ctx->task, &ctx->ts, fd, file);
    return 0; /* 0 = keep iterating */
}

static void emit_inherited_open_events(struct task_struct *task)
{
    struct inherited_open_ctx ctx;
    struct files_struct *files = task->files; /* safe: task == current */
    if (!files) return;
    ctx.task = task;
    ktime_get_real_ts64(&ctx.ts);
    iterate_fd(files, 0, inherited_open_iter, &ctx);
}

/* ---- 2. Exec ---- */

static int exec_entry(struct kretprobe_instance *ri, struct pt_regs *regs)
{ return 0; }

static int exec_ret(struct kretprobe_instance *ri, struct pt_regs *regs)
{
    struct trace_session *s;
    struct task_struct *task = current;
    pid_t tgid = task->tgid;
    struct timespec64 ts;
    char *argv_raw=NULL, *env_raw=NULL;
    char *exe_buf=NULL, *exe=NULL;
    size_t argv_len=0, env_len=0;
    uint64_t ts_ns;
    struct mm_struct *mm = NULL;

    if ((int)regs_return_value(regs) != 0) return 0;
    if (!tgid_in_any_session(tgid)) return 0;

    /* Emit CWD event first. */
    emit_cwd_event(task);

    ktime_get_real_ts64(&ts);
    ts_ns = (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;

    exe_buf = kmalloc(PATH_MAX, GFP_ATOMIC);
    if (exe_buf) exe = get_task_exe_path(task, exe_buf, PATH_MAX);
    argv_raw = read_task_argv_raw(task, &argv_len);
    env_raw = read_task_env_raw(task, &env_len);

    /* Snapshot AUXV once. */
    mm = get_task_mm(task);

    rcu_read_lock();
    list_for_each_entry_rcu(s, &sessions, list) {
        pid_t pid_, tgid_, ppid_, nspid_, nstgid_;
        if (s->dead || !session_is_tagged(s, tgid)) continue;

        pid_   = task->pid;
        tgid_  = task->tgid;
        ppid_  = task_ppid_nr(task);
        nspid_ = get_task_ns_pid(task);
        nstgid_= get_task_ns_tgid(task);

        /* 1. EV_EXEC — exe path */
        emit_one(s, EV_EXEC, ts_ns, pid_, tgid_, ppid_, nspid_, nstgid_,
                 NULL, 0, exe ? exe : "", exe ? strlen(exe) : 0);

        /* 2. EV_ARGV — raw NUL-separated argv */
        if (argv_raw && argv_len > 0)
            emit_one(s, EV_ARGV, ts_ns, pid_, tgid_, ppid_, nspid_, nstgid_,
                     NULL, 0, argv_raw, argv_len);

        /* 3. EV_ENV — raw NUL-separated env */
        if (env_raw && env_len > 0)
            emit_one(s, EV_ENV, ts_ns, pid_, tgid_, ppid_, nspid_, nstgid_,
                     NULL, 0, env_raw, env_len);

        /* 4. EV_AUXV — raw saved_auxv bytes */
        if (mm)
            emit_one(s, EV_AUXV, ts_ns, pid_, tgid_, ppid_, nspid_, nstgid_,
                     NULL, 0, mm->saved_auxv,
                     AT_VECTOR_SIZE * 2 * sizeof(unsigned long));
    }
    rcu_read_unlock();

    if (mm) mmput(mm);

    /* Emit fake OPEN events for inherited fds (pipes, redirects, etc). */
    emit_inherited_open_events(task);

    kfree(env_raw); kfree(argv_raw); kfree(exe_buf);
    return 0;
}

static struct kretprobe exec_kretprobe = {
    .handler=exec_ret, .entry_handler=exec_entry, .maxactive=64,
};

/* ---- 3. Exit ---- */

static int exit_pre(struct kprobe *p, struct pt_regs *regs)
{
    struct trace_session *s;
    struct task_struct *task = current;
    pid_t tgid = task->tgid;
    long code;
    struct timespec64 ts;
    uint64_t ts_ns;
    int64_t extras[4];
    int exit_sig, exit_val;
    bool core_dumped;

    if (task->pid != task->tgid) return 0;
    if (!tgid_in_any_session(tgid)) goto untag;

#if defined(CONFIG_X86_64)
    code=(long)regs->di;
#elif defined(CONFIG_X86)
    code=(long)regs->ax;
#elif defined(CONFIG_ARM64)
    code=(long)regs->regs[0];
#elif defined(CONFIG_ARM)
    code=(long)regs->ARM_r0;
#elif defined(CONFIG_S390)
    code=(long)regs->gprs[2];
#elif defined(CONFIG_PPC)
    code=(long)regs->gpr[3];
#else
    code=0;
#endif

    ktime_get_real_ts64(&ts);
    ts_ns = (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;

    exit_sig = code & 0x7f;
    core_dumped = !!(code & 0x80);
    exit_val = (code >> 8) & 0xff;

    if (exit_sig == 0) {
        extras[0] = EV_EXIT_EXITED;
        extras[1] = exit_val;
        extras[2] = 0;
        extras[3] = code;
    } else {
        extras[0] = EV_EXIT_SIGNALED;
        extras[1] = exit_sig;
        extras[2] = core_dumped ? 1 : 0;
        extras[3] = code;
    }

    rcu_read_lock();
    list_for_each_entry_rcu(s, &sessions, list) {
        if (!s->dead && session_is_tagged(s, tgid))
            emit_one(s, EV_EXIT, ts_ns,
                     task->pid, task->tgid, task_ppid_nr(task),
                     get_task_ns_pid(task), get_task_ns_tgid(task),
                     extras, 4, NULL, 0);
    }
    rcu_read_unlock();

untag:
    rcu_read_lock();
    list_for_each_entry_rcu(s, &sessions, list) {
        if (!s->dead && session_is_tagged(s, tgid)) session_untag_pid(s, tgid);
    }
    rcu_read_unlock();
    return 0;
}

static struct kprobe exit_kprobe = { .pre_handler=exit_pre, .symbol_name="do_exit" };

/* ---- 4. Open ---- */

struct open_probe_data { unsigned long filename_ptr; int flags; };

static int open_entry(struct kretprobe_instance *ri, struct pt_regs *regs)
{
    struct open_probe_data *d=(void*)ri->data;
    if(!tgid_in_any_session(current->tgid)){d->filename_ptr=0;return 1;}
#if defined(CONFIG_X86_64)
    d->filename_ptr=regs->si; d->flags=(int)regs->dx;
#elif defined(CONFIG_ARM64)
    d->filename_ptr=regs->regs[1]; d->flags=(int)regs->regs[2];
#else
    d->filename_ptr=0;
#endif
    return 0;
}

static int open_ret(struct kretprobe_instance *ri, struct pt_regs *regs)
{
    struct open_probe_data *d=(void*)ri->data;
    struct trace_session *s;
    struct task_struct *task=current;
    pid_t tgid=task->tgid;
    struct timespec64 ts;
    long fd_or_err=(long)regs_return_value(regs);
    char *upath=NULL;
    long n;
    struct inode *ino=NULL;
    unsigned long ino_nr=0;
    dev_t dev=0;
    uint64_t ts_ns;
    int64_t extras[7];

    if(!d->filename_ptr||!tgid_in_any_session(tgid)) return 0;
    ktime_get_real_ts64(&ts);
    ts_ns = (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;

    upath=kmalloc(PATH_MAX,GFP_ATOMIC); if(!upath) return 0;
    n=strncpy_from_user(upath,(const char __user*)d->filename_ptr,PATH_MAX);
    if(n<=0) goto out;

    /* Get inode of result fd. */
    if(fd_or_err>=0){
        struct file *rf=fget((unsigned int)fd_or_err);
        if(rf){
            ino=file_inode(rf);
            if(ino){ino_nr=ino->i_ino; dev=ino->i_sb->s_dev;}
            fput(rf);
        }
    }

    extras[0] = (int64_t)d->flags;
    extras[1] = (int64_t)(fd_or_err >= 0 ? fd_or_err : -1);
    extras[2] = (int64_t)ino_nr;
    extras[3] = (int64_t)MAJOR(dev);
    extras[4] = (int64_t)MINOR(dev);
    extras[5] = (int64_t)(fd_or_err >= 0 ? 0 : fd_or_err);  /* err */
    extras[6] = 0;  /* inherited=false */

    rcu_read_lock();
    list_for_each_entry_rcu(s,&sessions,list){
        if(!s->dead&&session_is_tagged(s,tgid))
            emit_one(s, EV_OPEN, ts_ns,
                     task->pid, task->tgid, task_ppid_nr(task),
                     get_task_ns_pid(task), get_task_ns_tgid(task),
                     extras, 7, upath, n);
    }
    rcu_read_unlock();
out:
    kfree(upath);
    return 0;
}

static struct kretprobe open_kretprobe = {
    .handler=open_ret,.entry_handler=open_entry,
    .data_size=sizeof(struct open_probe_data),.maxactive=128,
};

/* ---- 5. Chdir ---- */

static int chdir_entry(struct kretprobe_instance *ri, struct pt_regs *regs)
{
    if (!tgid_in_any_session(current->tgid)) return 1;
    return 0;
}

static int chdir_ret(struct kretprobe_instance *ri, struct pt_regs *regs)
{
    if ((int)regs_return_value(regs) != 0) return 0;
    emit_cwd_event(current);
    return 0;
}

static struct kretprobe chdir_kretprobe = {
    .handler=chdir_ret,.entry_handler=chdir_entry,.maxactive=32,
};

/* ---- 5b. Unlink ---- */

struct unlink_probe_data { unsigned long filename_ptr; };

static int unlink_entry(struct kretprobe_instance *ri, struct pt_regs *regs)
{
    struct unlink_probe_data *d=(void*)ri->data;
    if(!tgid_in_any_session(current->tgid)){d->filename_ptr=0;return 1;}
#if defined(CONFIG_X86_64)
    d->filename_ptr=regs->si;  /* do_unlinkat(dfd, pathname) — pathname is arg1 */
#elif defined(CONFIG_ARM64)
    d->filename_ptr=regs->regs[1];
#else
    d->filename_ptr=0;
#endif
    return 0;
}

static int unlink_ret(struct kretprobe_instance *ri, struct pt_regs *regs)
{
    /* UNLINK is not part of the wire format. Drop it entirely. */
    return 0;
}

static struct kretprobe unlink_kretprobe = {
    .handler=unlink_ret,.entry_handler=unlink_entry,
    .data_size=sizeof(struct unlink_probe_data),.maxactive=64,
};

/* ---- 6a. ksys_write (fd-based) ---- */

struct ksys_write_data { unsigned int fd; unsigned long buf_ptr; size_t count; };

static int ksys_write_entry(struct kretprobe_instance *ri, struct pt_regs *regs)
{
    struct ksys_write_data *d=(void*)ri->data; unsigned int fd;
#if defined(CONFIG_X86_64)
    fd=(unsigned int)regs->di;
#elif defined(CONFIG_ARM64)
    fd=(unsigned int)regs->regs[0];
#else
    return 1;
#endif
    if(fd!=1&&fd!=2) return 1;
    if(!tgid_in_any_session(current->tgid)) return 1;
    d->fd=fd;
#if defined(CONFIG_X86_64)
    d->buf_ptr=regs->si; d->count=(size_t)regs->dx;
#elif defined(CONFIG_ARM64)
    d->buf_ptr=regs->regs[1]; d->count=(size_t)regs->regs[2];
#endif
    return 0;
}

static int ksys_write_ret(struct kretprobe_instance *ri, struct pt_regs *regs)
{
    struct ksys_write_data *d=(void*)ri->data;
    struct trace_session *s;
    long ret_val=(long)regs_return_value(regs);
    struct inode *write_ino; bool any;
    if(ret_val<=0) return 0;
    if(d->fd==2){emit_write_log(current,"STDERR",d->buf_ptr,(size_t)ret_val);return 0;}
    write_ino=get_fd_inode(1); if(!write_ino) return 0;
    any=false;
    rcu_read_lock();
    list_for_each_entry_rcu(s,&sessions,list){
        if(!s->dead&&session_is_tagged(s,current->tgid)&&
           write_ino==s->creator_stdout_ino){any=true;break;}
    }
    rcu_read_unlock();
    if(any) emit_write_log(current,"STDOUT",d->buf_ptr,(size_t)ret_val);
    return 0;
}

static struct kretprobe ksys_write_kretprobe = {
    .handler=ksys_write_ret,.entry_handler=ksys_write_entry,
    .data_size=sizeof(struct ksys_write_data),.maxactive=128,
};

/* ---- 6b. vfs_write (fallback) ---- */

struct vfs_write_data { struct file *filp; unsigned long buf_ptr; size_t count; };

static int vfs_write_entry(struct kretprobe_instance *ri, struct pt_regs *regs)
{
    struct vfs_write_data *d=(void*)ri->data;
    if(!tgid_in_any_session(current->tgid)) return 1;
#if defined(CONFIG_X86_64)
    d->filp=(struct file*)regs->di; d->buf_ptr=regs->si; d->count=(size_t)regs->dx;
#elif defined(CONFIG_ARM64)
    d->filp=(struct file*)regs->regs[0]; d->buf_ptr=regs->regs[1]; d->count=(size_t)regs->regs[2];
#else
    return 1;
#endif
    if(!d->filp||!d->buf_ptr||d->count==0) return 1;
    return 0;
}

static int vfs_write_ret(struct kretprobe_instance *ri, struct pt_regs *regs)
{
    struct vfs_write_data *d=(void*)ri->data;
    struct trace_session *s;
    long ret_val=(long)regs_return_value(regs);
    struct inode *write_ino,*stderr_ino;
    const char *stream=NULL; bool any=false;
    if(ret_val<=0) return 0;
    write_ino=file_inode(d->filp); if(!write_ino) return 0;
    stderr_ino=get_fd_inode(2);
    if(write_ino==stderr_ino){
        if(devnull_ino&&write_ino==devnull_ino) return 0;
        stream="STDERR"; any=true;
    }
    if(!any){
        if(devnull_ino&&write_ino==devnull_ino) return 0;
        rcu_read_lock();
        list_for_each_entry_rcu(s,&sessions,list){
            if(!s->dead&&session_is_tagged(s,current->tgid)&&
               write_ino==s->creator_stdout_ino){any=true;break;}
        }
        rcu_read_unlock();
        if(any) stream="STDOUT";
    }
    if(any&&stream) emit_write_log(current,stream,d->buf_ptr,(size_t)ret_val);
    return 0;
}

static struct kretprobe vfs_write_kretprobe = {
    .handler=vfs_write_ret,.entry_handler=vfs_write_entry,
    .data_size=sizeof(struct vfs_write_data),.maxactive=128,
};

/* ===============================================================
 * /proc interface
 * =============================================================== */

static struct proc_dir_entry *proc_dir;

static int proc_new_open(struct inode *inode, struct file *filp)
{
    struct trace_session *s = session_create(current->tgid);
    if (IS_ERR(s)) return PTR_ERR(s);
    filp->private_data = s;
    return 0;
}

static ssize_t proc_new_read(struct file *filp, char __user *ubuf,
                              size_t count, loff_t *off)
{
    struct trace_session *s = filp->private_data;
    if (!s) return -EINVAL;
    return ring_read_user(&s->ring, ubuf, count);
}

static __poll_t proc_new_poll(struct file *filp, struct poll_table_struct *wait)
{
    struct trace_session *s = filp->private_data;
    __poll_t mask = 0;
    if (!s) return POLLERR;
    poll_wait(filp, &s->ring.wq_reader, wait);
    if (ring_used(&s->ring) > 0) mask |= POLLIN | POLLRDNORM;
    if (s->ring.closed) mask |= POLLHUP;
    return mask;
}

static int proc_new_release(struct inode *inode, struct file *filp)
{
    struct trace_session *s = filp->private_data;
    if (!s) return 0;
    filp->private_data = NULL;
    session_destroy(s);
    return 0;
}

static const PROC_OPS_TYPE proc_new_ops = {
    PROC_OPS_OWNER
    PROC_OPS_OPEN(proc_new_open)
    PROC_OPS_READ(proc_new_read)
    PROC_OPS_POLL(proc_new_poll)
    PROC_OPS_RELEASE(proc_new_release)
};

static int proc_sessions_show(struct seq_file *m, void *v)
{
    struct trace_session *s;
    seq_printf(m,"%-6s %-10s %-8s %-10s %s\n","ID","ROOT_TGID","#TAGS","BUF_USED","STATUS");
    rcu_read_lock();
    list_for_each_entry_rcu(s,&sessions,list)
        seq_printf(m,"%-6u %-10d %-8d %-10zu %s\n",
            s->id,s->root_tgid,atomic_read(&s->tag_count),
            ring_used(&s->ring), s->dead?"dead":"active");
    rcu_read_unlock();
    return 0;
}

static int proc_sessions_open(struct inode *inode, struct file *file)
{ return single_open(file, proc_sessions_show, NULL); }

static const PROC_OPS_TYPE proc_sessions_ops = {
    PROC_OPS_OWNER
    PROC_OPS_OPEN(proc_sessions_open) PROC_OPS_READ(seq_read)
    PROC_OPS_LSEEK(seq_lseek) PROC_OPS_RELEASE(single_release)
};

/* ===============================================================
 * Symbol resolution
 * =============================================================== */

static bool symbol_exists(const char *name)
{
    struct kprobe kp={}; kp.symbol_name=name;
    if(register_kprobe(&kp)<0) return false;
    unregister_kprobe(&kp); return true;
}

/* ===============================================================
 * Module init / exit
 * =============================================================== */

static const char *write_hook_name;
static const char *chdir_sym;
static const char *unlink_sym;

static int __init proctrace_init(void)
{
    int ret;
    const char *fork_sym, *exec_sym, *open_sym;

    capture_devnull_inode();

    proc_dir = proc_mkdir("proctrace", NULL);
    if (!proc_dir) { ret=-ENOMEM; goto err_root; }
    if (!proc_create("new",0666,proc_dir,&proc_new_ops)) goto err_proc;
    if (!proc_create("sessions",0444,proc_dir,&proc_sessions_ops)) goto err_proc;

    fork_sym = symbol_exists(FORK_SYMBOL_PRIMARY)?FORK_SYMBOL_PRIMARY:
               symbol_exists(FORK_SYMBOL_FALLBACK)?FORK_SYMBOL_FALLBACK:NULL;
    if(!fork_sym){pr_err("proctrace: no fork\n");ret=-ENOENT;goto err_proc;}
    fork_kretprobe.kp.symbol_name=fork_sym;

    exec_sym = symbol_exists(EXEC_SYMBOL_PRIMARY)?EXEC_SYMBOL_PRIMARY:
               symbol_exists(EXEC_SYMBOL_FALLBACK)?EXEC_SYMBOL_FALLBACK:NULL;
    if(!exec_sym){pr_err("proctrace: no exec\n");ret=-ENOENT;goto err_proc;}
    exec_kretprobe.kp.symbol_name=exec_sym;

    open_sym = symbol_exists(OPEN_SYMBOL_PRIMARY)?OPEN_SYMBOL_PRIMARY:
               symbol_exists(OPEN_SYMBOL_FALLBACK)?OPEN_SYMBOL_FALLBACK:NULL;
    if(open_sym) open_kretprobe.kp.symbol_name=open_sym;

    /* chdir */
    chdir_sym = symbol_exists("__x64_sys_chdir")?"__x64_sys_chdir":
                symbol_exists("ksys_chdir")?"ksys_chdir":
                symbol_exists("sys_chdir")?"sys_chdir":NULL;
    if(chdir_sym) chdir_kretprobe.kp.symbol_name=chdir_sym;

    /* unlink */
    unlink_sym = symbol_exists(UNLINK_SYMBOL_PRIMARY)?UNLINK_SYMBOL_PRIMARY:
                 symbol_exists(UNLINK_SYMBOL_FALLBACK)?UNLINK_SYMBOL_FALLBACK:NULL;
    if(unlink_sym) unlink_kretprobe.kp.symbol_name=unlink_sym;

    have_ksys_write=false; write_hook_name=NULL;
    if(symbol_exists("ksys_write")){
        ksys_write_kretprobe.kp.symbol_name="ksys_write";
        have_ksys_write=true; write_hook_name="ksys_write";
    } else if(symbol_exists("vfs_write")){
        vfs_write_kretprobe.kp.symbol_name="vfs_write";
        write_hook_name="vfs_write";
    }

    ret=register_kretprobe(&fork_kretprobe);
    if(ret){pr_err("proctrace: fork: %d\n",ret);goto err_proc;}
    ret=register_kretprobe(&exec_kretprobe);
    if(ret){pr_err("proctrace: exec: %d\n",ret);goto err_fork;}
    ret=register_kprobe(&exit_kprobe);
    if(ret){pr_err("proctrace: exit: %d\n",ret);goto err_exec;}

    if(open_sym){ret=register_kretprobe(&open_kretprobe);
        if(ret){pr_warn("proctrace: open: %d\n",ret);open_sym=NULL;}}
    if(chdir_sym){ret=register_kretprobe(&chdir_kretprobe);
        if(ret){pr_warn("proctrace: chdir: %d\n",ret);chdir_sym=NULL;}}
    if(unlink_sym){ret=register_kretprobe(&unlink_kretprobe);
        if(ret){pr_warn("proctrace: unlink: %d\n",ret);unlink_sym=NULL;}}
    if(write_hook_name){
        ret=have_ksys_write?register_kretprobe(&ksys_write_kretprobe)
                           :register_kretprobe(&vfs_write_kretprobe);
        if(ret){pr_warn("proctrace: write: %d\n",ret);write_hook_name=NULL;have_ksys_write=false;}
    }

    pr_info("proctrace: loaded (fork=%s exec=%s exit=do_exit open=%s chdir=%s unlink=%s write=%s ring=%zuKB)\n",
            fork_sym,exec_sym,open_sym?open_sym:"off",chdir_sym?chdir_sym:"off",
            unlink_sym?unlink_sym:"off",
            write_hook_name?write_hook_name:"off", RING_BUF_SIZE/1024);
    return 0;

err_exec: unregister_kretprobe(&exec_kretprobe);
err_fork: unregister_kretprobe(&fork_kretprobe);
err_proc: proc_remove(proc_dir);
err_root:
    return ret?ret:-ENOMEM;
}

static void __exit proctrace_exit(void)
{
    struct trace_session *s, *tmp;
    if(write_hook_name){
        if(have_ksys_write)unregister_kretprobe(&ksys_write_kretprobe);
        else unregister_kretprobe(&vfs_write_kretprobe);
    }
    if(unlink_sym) unregister_kretprobe(&unlink_kretprobe);
    if(chdir_sym) unregister_kretprobe(&chdir_kretprobe);
    if(open_kretprobe.kp.symbol_name) unregister_kretprobe(&open_kretprobe);
    unregister_kprobe(&exit_kprobe);
    unregister_kretprobe(&exec_kretprobe);
    unregister_kretprobe(&fork_kretprobe);
    proc_remove(proc_dir);
    /* After kprobes are unregistered, no new events can fire.
     * Wait for any in-flight RCU readers, then drain the workqueue. */
    synchronize_rcu();
    /* Close all session rings so blocked workers wake up and exit.
     * This MUST happen before flush_workqueue — otherwise workers
     * stuck in ring_write() waiting for ring space will never return
     * and the flush will hang forever. */
    mutex_lock(&sessions_mutex);
    list_for_each_entry(s, &sessions, list) {
        s->dead = true; smp_wmb();
        s->ring.closed = true;
        wake_up_interruptible(&s->ring.wq_writer);
        wake_up_interruptible(&s->ring.wq_reader);
    }
    mutex_unlock(&sessions_mutex);
    /* Each session destroys its own per-session workqueue from
     * session_destroy(), so there's no global workqueue to flush
     * here — just process the cleanup list. */
    mutex_lock(&sessions_mutex);
    list_for_each_entry_safe(s,tmp,&sessions,list){
        list_del(&s->list);
        session_free_all_tags(s); ring_destroy(&s->ring); kfree(s);
    }
    mutex_unlock(&sessions_mutex);
    pr_info("proctrace: unloaded\n");
}

module_init(proctrace_init);
module_exit(proctrace_exit);
