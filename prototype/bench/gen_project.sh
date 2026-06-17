#!/usr/bin/env bash
# Generate a synthetic autoconf project whose ./configure does a large number of
# the probes a real configure does: each AC_CHECK_* forks a shell and (often)
# compiles+links+runs a tiny conftest with the compiler. That fork/exec/read
# storm over the toolchain (sh, gcc, cc1, ld, libc, headers) is exactly the
# workload that makes a slow filesystem layer visible.
#
# Usage: gen_project.sh <dest-dir>
set -euo pipefail
DEST="${1:?usage: gen_project.sh <dest-dir>}"
rm -rf "$DEST"
mkdir -p "$DEST"
cd "$DEST"

# A long list of headers/functions/types to probe. Real configure scripts check
# hundreds of these; we list a realistic spread so the run takes a few seconds.
HEADERS="stdio.h stdlib.h string.h unistd.h fcntl.h errno.h time.h sys/types.h
 sys/stat.h sys/time.h sys/wait.h sys/socket.h netinet/in.h arpa/inet.h
 dirent.h pwd.h grp.h signal.h termios.h locale.h limits.h stdint.h inttypes.h
 stdbool.h ctype.h math.h pthread.h dlfcn.h sys/mman.h sys/resource.h
 sys/ioctl.h netdb.h poll.h sys/select.h sys/un.h sys/uio.h utime.h wchar.h
 wctype.h langinfo.h nl_types.h iconv.h getopt.h sysexits.h syslog.h"

FUNCS="strdup strndup strchr strrchr memmove memset memcpy strtol strtoul
 getcwd gethostname gettimeofday clock_gettime nanosleep mmap munmap mprotect
 getpwnam getpwuid getgrnam realpath canonicalize_file_name fchdir openat
 fstatat unlinkat mkdirat symlinkat readlinkat faccessat dup2 dup3 pipe pipe2
 fcntl ioctl poll select kqueue epoll_create eventfd signalfd timerfd_create
 setenv unsetenv putenv getline getdelim asprintf vasprintf snprintf vsnprintf
 strtok_r localtime_r gmtime_r ctime_r asctime_r rand_r strerror_r"

TYPES="size_t ssize_t off_t pid_t uid_t gid_t mode_t ino_t dev_t nlink_t
 blksize_t blkcnt_t time_t suseconds_t intptr_t uintptr_t int8_t int16_t
 int32_t int64_t uint8_t uint16_t uint32_t uint64_t wchar_t"

{
  echo 'AC_INIT([slowbench], [1.0])'
  echo 'AC_CONFIG_SRCDIR([main.c])'
  echo 'AC_PROG_CC'
  echo 'AC_PROG_CPP'
  for h in $HEADERS; do echo "AC_CHECK_HEADERS([$h])"; done
  for f in $FUNCS;   do echo "AC_CHECK_FUNCS([$f])"; done
  for t in $TYPES;   do echo "AC_CHECK_TYPES([$t])"; done
  echo 'AC_CHECK_SIZEOF([int])'
  echo 'AC_CHECK_SIZEOF([long])'
  echo 'AC_CHECK_SIZEOF([void *])'
  echo 'AC_CHECK_SIZEOF([size_t])'
  echo 'AC_CONFIG_HEADERS([config.h])'
  echo 'AC_CONFIG_FILES([Makefile])'
  echo 'AC_OUTPUT'
} > configure.ac

echo 'int main(void){return 0;}' > main.c
echo 'all:' > Makefile.in
printf '\t$(CC) -o main main.c\n' >> Makefile.in

autoreconf -i >/dev/null 2>&1
echo "generated $DEST ($(grep -c AC_CHECK configure.ac) probes)"
