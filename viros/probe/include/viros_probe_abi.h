/* SPDX-License-Identifier: MIT */
#ifndef VIROS_PROBE_ABI_H
#define VIROS_PROBE_ABI_H

/*
 * This header is shared with a host-side debugger.  Keep the ABI in fixed
 * width integers, naturally aligned, and in the target's native byte order.
 * The response advertises byte order and pointer width before any target
 * addresses are interpreted.
 */
#ifdef __KERNEL__
#include <linux/types.h>
typedef __u8  vp_u8;
typedef __u16 vp_u16;
typedef __u32 vp_u32;
typedef __s32 vp_s32;
typedef __u64 vp_u64;
#else
#include <stdint.h>
typedef uint8_t  vp_u8;
typedef uint16_t vp_u16;
typedef uint32_t vp_u32;
typedef int32_t  vp_s32;
typedef uint64_t vp_u64;
#endif

#define VIROS_PROBE_REQUEST_MAGIC  0x56505251U /* VPRQ */
#define VIROS_PROBE_RESPONSE_MAGIC 0x56505253U /* VPRS */
#define VIROS_PROBE_ABI_MAJOR 1U
#define VIROS_PROBE_ABI_MINOR 2U

#define VIROS_PROBE_REQUEST_SIZE  64U
#define VIROS_PROBE_RESPONSE_SIZE 64U
#define VIROS_PROBE_TASK_V1_SIZE  192U
#define VIROS_PROBE_TRANSLATION_V1_SIZE 64U
#define VIROS_PROBE_SAVED_REGS_V1_SIZE 304U
#define VIROS_PROBE_COMM_SIZE 16U
#define VIROS_PROBE_AUX_COUNT 10U

enum viros_probe_opcode {
	VIROS_PROBE_OP_SNAPSHOT = 1,
	VIROS_PROBE_OP_TRANSLATE_VA = 2,
	VIROS_PROBE_OP_SAVED_REGS = 3,
};

enum viros_probe_status {
	VIROS_PROBE_OK = 0,
	VIROS_PROBE_BAD_REQUEST = -1,
	VIROS_PROBE_SHORT_BUFFER = -2,
	VIROS_PROBE_CORRUPT_LIST = -3,
	VIROS_PROBE_UNSUPPORTED = -4,
	VIROS_PROBE_STALE_TASK = -5,
	VIROS_PROBE_NOT_PRESENT = -6,
	VIROS_PROBE_TASK_RUNNING = -7,
	VIROS_PROBE_INVALID_REGS = -8,
	VIROS_PROBE_COMPAT_TASK = -9,
};

enum viros_probe_arch {
	VIROS_PROBE_ARCH_UNKNOWN = 0,
	VIROS_PROBE_ARCH_AARCH64 = 1,
	VIROS_PROBE_ARCH_ARM = 2,
	VIROS_PROBE_ARCH_MIPS = 3,
};

enum viros_probe_endian {
	VIROS_PROBE_ENDIAN_LITTLE = 1,
	VIROS_PROBE_ENDIAN_BIG = 2,
};

enum viros_probe_response_flags {
	VIROS_PROBE_RESP_MORE = 1U << 0,
};

enum viros_probe_task_flags {
	VIROS_PROBE_TASK_HAS_MM = 1U << 0,
	VIROS_PROBE_TASK_GROUP_LEADER = 1U << 1,
	VIROS_PROBE_TASK_ON_CPU = 1U << 2,
	VIROS_PROBE_TASK_AUX_VALID = 1U << 3,
};

enum viros_probe_translation_flags {
	VIROS_PROBE_XLATE_PRESENT = 1U << 0,
	VIROS_PROBE_XLATE_USER = 1U << 1,
	VIROS_PROBE_XLATE_WRITABLE = 1U << 2,
	VIROS_PROBE_XLATE_EXECUTABLE = 1U << 3,
	VIROS_PROBE_XLATE_BLOCK = 1U << 4,
	VIROS_PROBE_XLATE_SPECIAL = 1U << 5,
	/* Host physical reads are permitted only when this conservative bit is set. */
	VIROS_PROBE_XLATE_SAFE_READ = 1U << 6,
};

enum viros_probe_saved_regs_flags {
	VIROS_PROBE_REGS_VALID = 1U << 0,
	VIROS_PROBE_REGS_USER = 1U << 1,
	VIROS_PROBE_REGS_AARCH64_64 = 1U << 2,
};

/* Indices in task_v1.auxv and bits in task_v1.auxv_valid. */
enum viros_probe_aux_index {
	VIROS_PROBE_AUX_PHDR = 0,
	VIROS_PROBE_AUX_PHENT = 1,
	VIROS_PROBE_AUX_PHNUM = 2,
	VIROS_PROBE_AUX_PAGESZ = 3,
	VIROS_PROBE_AUX_BASE = 4,
	VIROS_PROBE_AUX_ENTRY = 5,
	VIROS_PROBE_AUX_RANDOM = 6,
	VIROS_PROBE_AUX_SYSINFO_EHDR = 7,
	VIROS_PROBE_AUX_EXECFN = 8,
	VIROS_PROBE_AUX_SECURE = 9,
};

struct viros_probe_request_v1 {
	vp_u32 magic;
	vp_u16 abi_major;
	vp_u16 abi_minor;
	vp_u16 size;
	vp_u16 opcode;
	vp_u32 flags;
	vp_u64 init_task;
	vp_u64 cursor_task;
	vp_u32 max_records;
	vp_u32 reserved0;
	vp_u64 reserved1;
	vp_u64 reserved2;
	vp_u64 reserved3;
};

struct viros_probe_response_v1 {
	vp_u32 magic;
	vp_u16 abi_major;
	vp_u16 abi_minor;
	vp_u16 header_size;
	vp_u16 record_size;
	vp_u16 arch;
	vp_u8 endian;
	vp_u8 pointer_bits;
	vp_s32 status;
	vp_u32 flags;
	vp_u32 record_count;
	vp_u32 bytes_written;
	vp_u64 next_cursor;
	vp_u64 snapshot_root;
	vp_u32 page_shift;
	vp_u32 reserved0;
	vp_u64 reserved1;
};

struct viros_probe_task_v1 {
	vp_u16 record_size;
	vp_u16 record_version;
	vp_u32 probe_flags;
	vp_u64 task;
	vp_u64 group_leader;
	vp_u64 real_parent;
	vp_u64 mm;
	/* Kernel virtual address of mm->pgd; this is not a physical address. */
	vp_u64 pgd;
	vp_u64 start_cookie;
	vp_u64 state;
	vp_u64 task_flags;
	vp_u32 pid;
	vp_u32 tgid;
	vp_u32 ppid;
	vp_u32 cpu;
	vp_u32 exit_state;
	vp_u16 abi_bits;
	vp_u16 auxv_valid;
	vp_u8 comm[VIROS_PROBE_COMM_SIZE];
	vp_u64 auxv[VIROS_PROBE_AUX_COUNT];
};

/*
 * TRANSLATE_VA request field assignment (keeps request_v1 fixed at 64 bytes):
 *   init_task=task pointer, cursor_task=expected mm pointer,
 *   reserved1=start_cookie, reserved2=user virtual address,
 *   reserved3=the exact kernel linear-map VA-minus-PA offset, derived by the
 *   host by asking QEMU to translate this frozen task's mm->pgd kernel VA.
 * All other request payload fields and flags must be zero.  The public memory
 * API never accepts this value from its caller: it is
 * derived internally by translating the frozen mm->pgd through QEMU.  Passing
 * that bound value avoids any linked reference to an exact kernel's
 * physvirt_offset.
 */
struct viros_probe_translation_v1 {
	vp_u16 record_size;
	vp_u16 record_version;
	vp_u32 translation_flags;
	vp_u64 task;
	vp_u64 mm;
	vp_u64 virtual_address;
	vp_u64 physical_address;
	vp_u64 contiguous_bytes;
	vp_u64 mapping_bytes;
	vp_u32 page_shift;
	vp_u16 level;
	vp_u16 reserved0;
};

/*
 * SAVED_REGS request field assignment (ABI 1.2 and later):
 *   init_task=task pointer, cursor_task=expected mm pointer,
 *   reserved1=start_cookie.  Every other payload field and flags are zero.
 * The identity triple must come from one frozen snapshot.  A successful
 * record is available only for a 64-bit AArch64 userspace task which is not
 * current or on a CPU; it describes the task's saved EL0 exception frame.
 */
struct viros_probe_saved_regs_v1 {
	vp_u16 record_size;
	vp_u16 record_version;
	vp_u32 saved_regs_flags;
	vp_u64 task;
	vp_u64 mm;
	vp_u64 start_cookie;
	vp_u64 x[31];
	vp_u64 sp;
	vp_u64 pc;
	vp_u64 pstate;
};

#ifdef __KERNEL__
static_assert(sizeof(struct viros_probe_request_v1) == VIROS_PROBE_REQUEST_SIZE);
static_assert(sizeof(struct viros_probe_response_v1) == VIROS_PROBE_RESPONSE_SIZE);
static_assert(sizeof(struct viros_probe_task_v1) == VIROS_PROBE_TASK_V1_SIZE);
static_assert(sizeof(struct viros_probe_translation_v1) ==
	      VIROS_PROBE_TRANSLATION_V1_SIZE);
static_assert(sizeof(struct viros_probe_saved_regs_v1) ==
	      VIROS_PROBE_SAVED_REGS_V1_SIZE);
#if defined(CONFIG_MIPS)
/* viros_probe.c advertises MIPS as the o32, 32-bit-pointer snapshot target. */
static_assert(sizeof(void *) * 8 == 32);
#endif
#endif

#endif /* VIROS_PROBE_ABI_H */
