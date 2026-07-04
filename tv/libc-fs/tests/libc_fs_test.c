#include "libc-fs/libc.h"
#include "libc-fs/fmt.h"

void sud_rt_sigreturn_restorer(void) {}
#if defined(__i386__)
void sud_sigreturn_restorer(void) {}
#endif

static int fail(const char *msg)
{
    write(2, msg, strlen(msg));
    write(2, "\n", 1);
    return 1;
}

int main(int argc, char **argv)
{
    (void)argc; (void)argv;
    char buf[128];
    memset(buf, 'x', sizeof(buf));
    memcpy(buf, "abc", 4);
    if (strcmp(buf, "abc") != 0) return fail("strcmp/memcpy failed");
    memmove(buf + 1, buf, 4);
    if (strcmp(buf, "aabc") != 0) return fail("memmove failed");
    if (snprintf(buf, sizeof(buf), "%s:%d:%x", "ok", 42, 255) <= 0)
        return fail("snprintf failed");
    if (strcmp(buf, "ok:42:ff") != 0) return fail("snprintf content failed");
    char *p = buf;
    p = fmt_str(p, "pid=");
    p = fmt_int(p, 123);
    *p = '\0';
    if (strcmp(buf, "pid=123") != 0) return fail("fmt helpers failed");
    return 0;
}
