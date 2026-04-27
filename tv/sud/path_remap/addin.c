#include "sud/addin.h"

#define SUD_REMAP_MAX 32
struct remap_rule { const char *src; size_t src_len; const char *dst; size_t dst_len; };
static struct remap_rule g_rules[SUD_REMAP_MAX];
static int g_rule_count;

static int is_path_boundary(char c)
{
    return c == '\0' || c == '/';
}

static char *copy_range(char *dst, const char *start, size_t n)
{
    for (size_t i = 0; i < n; i++) dst[i] = start[i];
    dst[n] = '\0';
    return dst;
}

static void path_remap_init(void)
{
    const char *env = getenv("SUD_REMAP");
    if (!env || !env[0]) return;
    while (*env && g_rule_count < SUD_REMAP_MAX) {
        const char *src = env;
        while (*env && *env != '=' && *env != ':') env++;
        if (*env != '=') {
            while (*env && *env != ':') env++;
            if (*env == ':') env++;
            continue;
        }
        size_t src_len = (size_t)(env - src);
        env++;
        const char *dst = env;
        while (*env && *env != ':') env++;
        size_t dst_len = (size_t)(env - dst);
        if (src_len && dst_len) {
            char *src_copy = malloc(src_len + 1);
            char *dst_copy = malloc(dst_len + 1);
            if (!src_copy || !dst_copy) return;
            copy_range(src_copy, src, src_len);
            copy_range(dst_copy, dst, dst_len);
            g_rules[g_rule_count].src = src_copy;
            g_rules[g_rule_count].src_len = src_len;
            g_rules[g_rule_count].dst = dst_copy;
            g_rules[g_rule_count].dst_len = dst_len;
            g_rule_count++;
        }
        if (*env == ':') env++;
    }
}

static char *remap_path(struct sud_syscall_ctx *ctx, const char *path)
{
    if (!path || !path[0] || !ctx->scratch || ctx->scratch_size == 0) return 0;
    for (int i = 0; i < g_rule_count; i++) {
        const struct remap_rule *r = &g_rules[i];
        if (strncmp(path, r->src, r->src_len) != 0) continue;
        if (!is_path_boundary(path[r->src_len])) continue;
        size_t tail_len = strlen(path + r->src_len);
        if (r->dst_len + tail_len + 1 > ctx->scratch_size) return 0;
        memcpy(ctx->scratch, r->dst, r->dst_len);
        memcpy(ctx->scratch + r->dst_len, path + r->src_len, tail_len + 1);
        return ctx->scratch;
    }
    return 0;
}

static void remap_arg(struct sud_syscall_ctx *ctx, int idx)
{
    char *p = remap_path(ctx, (const char *)ctx->args[idx]);
    if (p) ctx->args[idx] = (long)p;
}

static int path_remap_pre_syscall(struct sud_syscall_ctx *ctx)
{
    if (!g_rule_count) return 0;
#ifdef SYS_open
    if (ctx->nr == SYS_open) remap_arg(ctx, 0);
#endif
#ifdef SYS_openat
    if (ctx->nr == SYS_openat) remap_arg(ctx, 1);
#endif
#ifdef SYS_readlink
    if (ctx->nr == SYS_readlink) remap_arg(ctx, 0);
#endif
#ifdef SYS_readlinkat
    if (ctx->nr == SYS_readlinkat) remap_arg(ctx, 1);
#endif
#ifdef SYS_unlink
    if (ctx->nr == SYS_unlink) remap_arg(ctx, 0);
#endif
#ifdef SYS_unlinkat
    if (ctx->nr == SYS_unlinkat) remap_arg(ctx, 1);
#endif
#ifdef SYS_chdir
    if (ctx->nr == SYS_chdir) remap_arg(ctx, 0);
#endif
#ifdef SYS_execve
    if (ctx->nr == SYS_execve) remap_arg(ctx, 0);
#endif
#ifdef SYS_execveat
    if (ctx->nr == SYS_execveat) remap_arg(ctx, 1);
#endif
#ifdef SYS_newfstatat
    if (ctx->nr == SYS_newfstatat) remap_arg(ctx, 1);
#endif
#ifdef SYS_fstatat64
    if (ctx->nr == SYS_fstatat64) remap_arg(ctx, 1);
#endif
    return 0;
}

const struct sud_addin sud_path_remap_addin = {
    "path_remap",
    path_remap_init,
    0,
    0,
    path_remap_pre_syscall,
    0,
};
