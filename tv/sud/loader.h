#ifndef SUD_LOADER_H
#define SUD_LOADER_H

void load_and_run_elf(const char *path, int argc, char **argv, int drop_count)
    __attribute__((noreturn));

#endif /* SUD_LOADER_H */
