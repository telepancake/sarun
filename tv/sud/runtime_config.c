#include "sud/runtime_config.h"

extern void *malloc(size_t);

struct sud_runtime_config g_sud_runtime_config;
int g_sud_runtime_config_present;

static int streq(const char *a, const char *b)
{
    if (!a || !b) return 0;
    while (*a && *a == *b) { a++; b++; }
    return *a == *b;
}

static int parse_nonnegative(const char *text, int *parsed)
{
    if (!text || !*text || !parsed) return -1;
    int value = 0;
    while (*text >= '0' && *text <= '9') {
        int digit = *text++ - '0';
        if (value > (2147483647 - digit) / 10) return -1;
        value = value * 10 + digit;
    }
    if (*text) return -1;
    *parsed = value;
    return 0;
}

static char *copy_string(const char *text)
{
    if (!text) return 0;
    size_t length = 0;
    while (text[length]) length++;
    char *copy = malloc(length + 1);
    if (!copy) return 0;
    for (size_t i = 0; i <= length; i++) copy[i] = text[i];
    return copy;
}

void sud_runtime_config_clear(struct sud_runtime_config *cfg)
{
    if (!cfg) return;
    cfg->no_env = 0;
    cfg->drop_count = 0;
    cfg->cwd = 0;
    cfg->trace_outfile = 0;
}

int sud_runtime_config_parse(int argc, char **argv, int *argi,
                             struct sud_runtime_config *cfg)
{
    if (!cfg || !argi) return -1;
    int i = *argi;
    while (i < argc && argv[i]) {
        if (streq(argv[i], "--no-env")) {
            cfg->no_env = 1;
            i++;
        } else if (streq(argv[i], "--drop-argv")) {
            if (i + 1 >= argc || !argv[i + 1]) return -1;
            if (parse_nonnegative(argv[i + 1], &cfg->drop_count) != 0)
                return -1;
            i += 2;
        } else if (streq(argv[i], "--cwd")) {
            if (i + 1 >= argc || !argv[i + 1]) return -1;
            cfg->cwd = argv[i + 1];
            i += 2;
        } else if (streq(argv[i], "--trace-outfile")) {
            if (i + 1 >= argc || !argv[i + 1]) return -1;
            cfg->trace_outfile = argv[i + 1];
            i += 2;
        } else {
            break;
        }
    }
    *argi = i;
    return 0;
}

static int format_nonnegative(char *out, int size, int value)
{
    if (!out || size < 2 || value < 0) return -1;
    char reverse[16];
    int length = 0;
    unsigned int remaining = (unsigned int)value;
    do {
        reverse[length++] = (char)('0' + remaining % 10u);
        remaining /= 10u;
    } while (remaining && length < (int)sizeof(reverse));
    if (length + 1 > size) return -1;
    for (int i = 0; i < length; i++) out[i] = reverse[length - 1 - i];
    out[length] = '\0';
    return 0;
}

int sud_runtime_config_emit(const struct sud_runtime_config *cfg,
                            const char **out, int max,
                            char *int_scratch, int int_scratch_size)
{
    if (!cfg || !out || max < 0) return -1;
    int count = 0;
#define EMIT(value) do { if (count >= max) return -1; out[count++] = (value); } while (0)
    if (cfg->no_env) EMIT("--no-env");
    if (cfg->drop_count > 0) {
        if (format_nonnegative(int_scratch, int_scratch_size,
                               cfg->drop_count) != 0) return -1;
        EMIT("--drop-argv");
        EMIT(int_scratch);
    }
    if (cfg->cwd && cfg->cwd[0]) {
        EMIT("--cwd");
        EMIT(cfg->cwd);
    }
    if (cfg->trace_outfile && cfg->trace_outfile[0]) {
        EMIT("--trace-outfile");
        EMIT(cfg->trace_outfile);
    }
#undef EMIT
    return count;
}

void sud_runtime_config_intern(struct sud_runtime_config *cfg)
{
    if (!cfg) return;
    if (cfg->cwd && cfg->cwd[0]) cfg->cwd = copy_string(cfg->cwd);
    if (cfg->trace_outfile && cfg->trace_outfile[0])
        cfg->trace_outfile = copy_string(cfg->trace_outfile);
}

void sud_runtime_config_set_cwd(struct sud_runtime_config *cfg,
                                const char *new_cwd)
{
    if (!cfg) return;
    cfg->cwd = new_cwd && new_cwd[0] ? copy_string(new_cwd) : 0;
}
