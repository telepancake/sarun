/* SPDX-License-Identifier: MIT */
#ifndef VIROS_EVENT_ABI_H
#define VIROS_EVENT_ABI_H

/*
 * Native-endian event record shared by an exact Linux kernel and the host
 * debugger.  All register slots are normalized to 64 bits; a 32-bit producer
 * must zero-extend every value.  Only record_size bytes, rather than the full
 * capacity of struct viros_event_v1, are part of an individual record.
 */
#ifdef __KERNEL__
#include <linux/stddef.h>
#include <linux/types.h>
typedef __u8  ve_u8;
typedef __u16 ve_u16;
typedef __u32 ve_u32;
typedef __s32 ve_s32;
typedef __u64 ve_u64;
#else
#include <stddef.h>
#include <stdint.h>
typedef uint8_t  ve_u8;
typedef uint16_t ve_u16;
typedef uint32_t ve_u32;
typedef int32_t  ve_s32;
typedef uint64_t ve_u64;
#endif

#define VIROS_EVENT_MAGIC 0x56455231U /* VER1 */
#define VIROS_EVENT_ABI_MAJOR 1U
#define VIROS_EVENT_ABI_MINOR 0U

#define VIROS_EVENT_HEADER_SIZE 128U
#define VIROS_EVENT_MAX_REGISTERS 64U
#define VIROS_EVENT_MAX_SIZE \
	(VIROS_EVENT_HEADER_SIZE + VIROS_EVENT_MAX_REGISTERS * sizeof(ve_u64))
#define VIROS_EVENT_COMM_SIZE 16U

enum viros_event_endian {
	VIROS_EVENT_ENDIAN_LITTLE = 1,
	VIROS_EVENT_ENDIAN_BIG = 2,
};

enum viros_event_arch {
	VIROS_EVENT_ARCH_AARCH64 = 1,
	VIROS_EVENT_ARCH_ARM = 2,
	VIROS_EVENT_ARCH_MIPS = 3,
	VIROS_EVENT_ARCH_X86 = 4,
};

enum viros_event_kind {
	VIROS_EVENT_USER_SIGNAL = 1,
	VIROS_EVENT_KERNEL_DIE = 2,
};

enum viros_event_flags {
	VIROS_EVENT_REGS_VALID = 1U << 0,
	VIROS_EVENT_REGS_USER = 1U << 1,
	VIROS_EVENT_REGS_COMPAT = 1U << 2,
	VIROS_EVENT_ADDRESS_VALID = 1U << 3,
};

/* ARM slots follow the GDB org.gnu.gdb.arm.core feature order. */
enum viros_event_arm_register {
	VIROS_EVENT_ARM_R0 = 0,
	VIROS_EVENT_ARM_R1,
	VIROS_EVENT_ARM_R2,
	VIROS_EVENT_ARM_R3,
	VIROS_EVENT_ARM_R4,
	VIROS_EVENT_ARM_R5,
	VIROS_EVENT_ARM_R6,
	VIROS_EVENT_ARM_R7,
	VIROS_EVENT_ARM_R8,
	VIROS_EVENT_ARM_R9,
	VIROS_EVENT_ARM_R10,
	VIROS_EVENT_ARM_R11,
	VIROS_EVENT_ARM_R12,
	VIROS_EVENT_ARM_SP,
	VIROS_EVENT_ARM_LR,
	VIROS_EVENT_ARM_PC,
	VIROS_EVENT_ARM_CPSR,
	VIROS_EVENT_ARM_REGISTER_COUNT,
};

/* AArch64 x0..x30 occupy their numeric slots. */
enum viros_event_aarch64_register {
	VIROS_EVENT_AARCH64_X0 = 0,
	VIROS_EVENT_AARCH64_X30 = 30,
	VIROS_EVENT_AARCH64_SP = 31,
	VIROS_EVENT_AARCH64_PC = 32,
	VIROS_EVENT_AARCH64_PSTATE = 33,
	VIROS_EVENT_AARCH64_REGISTER_COUNT = 34,
};

/* MIPS32 follows QEMU/GDB's legacy core order; r26/r27 are unavailable. */
enum viros_event_mips_register {
	VIROS_EVENT_MIPS_R0 = 0,
	VIROS_EVENT_MIPS_R31 = 31,
	VIROS_EVENT_MIPS_STATUS = 32,
	VIROS_EVENT_MIPS_LO = 33,
	VIROS_EVENT_MIPS_HI = 34,
	VIROS_EVENT_MIPS_BADVADDR = 35,
	VIROS_EVENT_MIPS_CAUSE = 36,
	VIROS_EVENT_MIPS_PC = 37,
	VIROS_EVENT_MIPS_REGISTER_COUNT = 38,
};

/* Linux 5.6 x86-64 struct pt_regs order, also used for IA32 tasks. */
enum viros_event_x86_register {
	VIROS_EVENT_X86_R15 = 0,
	VIROS_EVENT_X86_R14,
	VIROS_EVENT_X86_R13,
	VIROS_EVENT_X86_R12,
	VIROS_EVENT_X86_RBP,
	VIROS_EVENT_X86_RBX,
	VIROS_EVENT_X86_R11,
	VIROS_EVENT_X86_R10,
	VIROS_EVENT_X86_R9,
	VIROS_EVENT_X86_R8,
	VIROS_EVENT_X86_RAX,
	VIROS_EVENT_X86_RCX,
	VIROS_EVENT_X86_RDX,
	VIROS_EVENT_X86_RSI,
	VIROS_EVENT_X86_RDI,
	VIROS_EVENT_X86_ORIG_RAX,
	VIROS_EVENT_X86_RIP,
	VIROS_EVENT_X86_CS,
	VIROS_EVENT_X86_EFLAGS,
	VIROS_EVENT_X86_RSP,
	VIROS_EVENT_X86_SS,
	VIROS_EVENT_X86_REGISTER_COUNT,
};

struct viros_event_v1 {
	ve_u32 magic;
	ve_u16 abi_major;
	ve_u16 abi_minor;
	ve_u16 arch;
	ve_u16 endian;
	ve_u16 pointer_bits;
	ve_u16 kind;
	ve_u32 record_size;
	ve_u32 signal;
	ve_s32 code;
	ve_u32 flags;
	ve_u32 register_count;
	ve_u32 cpu;
	ve_u32 tgid;
	ve_u32 tid;
	ve_u64 sequence;
	ve_u64 task;
	ve_u64 mm;
	ve_u64 start_cookie;
	ve_u64 signal_struct;
	ve_u64 address;
	ve_u64 register_valid_mask;
	ve_u64 reserved0;
	ve_u8 comm[VIROS_EVENT_COMM_SIZE];
	ve_u64 registers[VIROS_EVENT_MAX_REGISTERS];
};

#ifdef __KERNEL__
static_assert(offsetof(struct viros_event_v1, registers) ==
	      VIROS_EVENT_HEADER_SIZE);
static_assert(sizeof(struct viros_event_v1) == VIROS_EVENT_MAX_SIZE);
static_assert(VIROS_EVENT_ARM_REGISTER_COUNT <= VIROS_EVENT_MAX_REGISTERS);
static_assert(VIROS_EVENT_AARCH64_REGISTER_COUNT <=
	      VIROS_EVENT_MAX_REGISTERS);
static_assert(VIROS_EVENT_MIPS_REGISTER_COUNT <= VIROS_EVENT_MAX_REGISTERS);
static_assert(VIROS_EVENT_X86_REGISTER_COUNT <= VIROS_EVENT_MAX_REGISTERS);
#endif

#endif /* VIROS_EVENT_ABI_H */
