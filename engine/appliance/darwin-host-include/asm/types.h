/* SPDX-License-Identifier: GPL-2.0 WITH Linux-syscall-note */
#ifndef SARUN_DARWIN_ASM_TYPES_H
#define SARUN_DARWIN_ASM_TYPES_H

/*
 * Minimal Linux UAPI integer types for kernel host utilities.  macOS has no
 * <asm/types.h>, while tools such as x86's vdso2c only need these fixed-width
 * definitions in order to read Linux ELF files.
 */
typedef signed char __s8;
typedef unsigned char __u8;
typedef signed short __s16;
typedef unsigned short __u16;
typedef signed int __s32;
typedef unsigned int __u32;
typedef signed long long __s64;
typedef unsigned long long __u64;

#endif
