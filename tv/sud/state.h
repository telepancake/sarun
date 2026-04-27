#ifndef SUD_STATE_H
#define SUD_STATE_H

#include "libc-fs/libc.h"

extern char       g_self_exe[PATH_MAX];
extern char       g_self_exe32[PATH_MAX];
extern char       g_self_exe64[PATH_MAX];
extern char       g_target_exe[PATH_MAX];
extern char      *g_path_env;
extern int        g_trace_exec_env;

#endif /* SUD_STATE_H */
