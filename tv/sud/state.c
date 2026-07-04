#include "sud/state.h"

char       g_self_exe[PATH_MAX];
char       g_self_exe32[PATH_MAX];
char       g_self_exe64[PATH_MAX];
char       g_target_exe[PATH_MAX];
char      *g_path_env;
int        g_trace_exec_env = 1;
