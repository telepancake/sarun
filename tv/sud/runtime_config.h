#ifndef SUD_RUNTIME_CONFIG_H
#define SUD_RUNTIME_CONFIG_H

#include <stddef.h>

#define SUD_RC_MAX_EMIT_ARGS 8

struct sud_runtime_config {
    int no_env;
    int drop_count;
    const char *cwd;
    const char *trace_outfile;
};

void sud_runtime_config_clear(struct sud_runtime_config *cfg);
int sud_runtime_config_parse(int argc, char **argv, int *argi,
                             struct sud_runtime_config *cfg);
int sud_runtime_config_emit(const struct sud_runtime_config *cfg,
                            const char **out, int max,
                            char *int_scratch, int int_scratch_size);
void sud_runtime_config_intern(struct sud_runtime_config *cfg);
void sud_runtime_config_set_cwd(struct sud_runtime_config *cfg,
                                const char *new_cwd);

extern struct sud_runtime_config g_sud_runtime_config;
extern int g_sud_runtime_config_present;

#endif
