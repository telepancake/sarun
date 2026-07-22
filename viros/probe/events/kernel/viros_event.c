// SPDX-License-Identifier: GPL-2.0
/*
 * Exact-kernel userspace event observer for ARMv7, AArch64, MIPS32, and
 * x86-64 kernels (including IA32 tasks).
 *
 * signal_deliver runs with the current task's signal lock held, and DIE_OOPS
 * may run in a constrained exception context.  Both callbacks therefore use
 * one preallocated record per CPU, perform bounded copies, and call no
 * allocating or logging facility.  A debugger breakpoint on viros_event_stop
 * sees a complete record through the first ABI argument.
 */
#include <linux/compiler.h>
#include <linux/init.h>
#include <linux/kdebug.h>
#include <linux/notifier.h>
#include <linux/percpu.h>
#include <linux/ptrace.h>
#include <linux/sched.h>
#include <linux/sched/signal.h>
#include <linux/sched/task_stack.h>
#include <linux/signal.h>
#include <linux/smp.h>
#include <trace/events/signal.h>
#include <asm/ptrace.h>

#include "viros_event_abi.h"

#if !defined(CONFIG_ARM) && !defined(CONFIG_ARM64) && \
	!defined(CONFIG_MIPS) && !defined(CONFIG_X86_64)
#error "viros_event supports only ARMv7, AArch64, MIPS32, and x86-64"
#endif

#if defined(CONFIG_MIPS) && !defined(CONFIG_32BIT)
#error "viros_event supports only 32-bit MIPS register frames"
#endif

struct viros_event_storage {
	struct viros_event_v1 record;
	ve_u64 sequence;
};

static DEFINE_PER_CPU(struct viros_event_storage, viros_event_storage);

/*
 * Retained no-op boundary.  The memory operand and clobber keep all record
 * stores visible before the call, and the global noinline symbol gives the
 * debugger a stable breakpoint location in vmlinux.
 */
void __used __visible noinline notrace
viros_event_stop(const struct viros_event_v1 *record)
{
	asm volatile("" : : "r" (record) : "memory");
}

static __always_inline ve_u16 viros_event_endian(void)
{
#if defined(__BYTE_ORDER__) && (__BYTE_ORDER__ == __ORDER_BIG_ENDIAN__)
	return VIROS_EVENT_ENDIAN_BIG;
#elif defined(CONFIG_CPU_BIG_ENDIAN)
	return VIROS_EVENT_ENDIAN_BIG;
#else
	return VIROS_EVENT_ENDIAN_LITTLE;
#endif
}

static __always_inline bool viros_event_has_address(int sig,
					     const struct kernel_siginfo *info)
{
	if (!info || info->si_code <= 0)
		return false;

	switch (sig) {
	case SIGILL:
	case SIGFPE:
	case SIGSEGV:
	case SIGBUS:
	case SIGTRAP:
		return true;
	default:
		return false;
	}
}

static __always_inline bool viros_event_default_fatal(
	int sig, const struct k_sigaction *ka)
{
	if (sig < 1 || sig > 64 || ka->sa.sa_handler != SIG_DFL)
		return false;
	if (sig_kernel_ignore(sig) || sig_kernel_stop(sig))
		return false;
	if ((current->signal->flags & SIGNAL_UNKILLABLE) &&
	    !sig_kernel_only(sig))
		return false;
	return true;
}

static __always_inline int viros_event_reported_signal(
	int delivered_sig, const struct k_sigaction *ka)
{
	int group_sig;

	/*
	 * get_signal() reports SIGKILL after a group exit has begun.  Preserve
	 * the original fatal signal from group_exit_code.  A zero low field is
	 * a normal exit_group() and is not a signal-delivery event.
	 */
	if (signal_group_exit(current->signal)) {
		group_sig = current->signal->group_exit_code & 0x7f;
		if (group_sig < 1 || group_sig > 64)
			return 0;
		return group_sig;
	}

	return viros_event_default_fatal(delivered_sig, ka) ? delivered_sig : 0;
}

static __always_inline void viros_event_copy_comm(struct viros_event_v1 *record)
{
	unsigned int i;
	bool ended = false;

	for (i = 0; i < VIROS_EVENT_COMM_SIZE; ++i) {
		unsigned char byte;

		if (ended) {
			record->comm[i] = 0;
			continue;
		}
		byte = READ_ONCE(current->comm[i]);
		if (!byte) {
			record->comm[i] = 0;
			ended = true;
		} else if (byte < 0x20 || byte >= 0x7f) {
			record->comm[i] = '?';
		} else {
			record->comm[i] = byte;
		}
	}
	/* TASK_COMM_LEN tasks are normally nonempty; retain ABI validity if not. */
	if (!record->comm[0]) {
		record->comm[0] = '?';
		if (VIROS_EVENT_COMM_SIZE > 1)
			record->comm[1] = 0;
	}
}

#if defined(CONFIG_ARM)
static __always_inline void viros_event_copy_registers(
	struct viros_event_v1 *record, const struct pt_regs *regs, bool compat)
{
	unsigned int i;

	(void)compat;
	for (i = 0; i < VIROS_EVENT_ARM_REGISTER_COUNT; ++i)
		record->registers[i] = (ve_u32)regs->uregs[i];
	record->register_count = VIROS_EVENT_ARM_REGISTER_COUNT;
	record->register_valid_mask =
		(1ULL << VIROS_EVENT_ARM_REGISTER_COUNT) - 1;
}
#elif defined(CONFIG_ARM64)
static __always_inline void viros_event_copy_registers(
	struct viros_event_v1 *record, const struct pt_regs *regs, bool compat)
{
	unsigned int i;

	(void)compat;
	for (i = VIROS_EVENT_AARCH64_X0; i <= VIROS_EVENT_AARCH64_X30; ++i)
		record->registers[i] = regs->regs[i];
	record->registers[VIROS_EVENT_AARCH64_SP] = regs->sp;
	record->registers[VIROS_EVENT_AARCH64_PC] = regs->pc;
	record->registers[VIROS_EVENT_AARCH64_PSTATE] = regs->pstate;
	record->register_count = VIROS_EVENT_AARCH64_REGISTER_COUNT;
	record->register_valid_mask =
		(1ULL << VIROS_EVENT_AARCH64_REGISTER_COUNT) - 1;
}
#elif defined(CONFIG_MIPS)
static __always_inline void viros_event_copy_registers(
	struct viros_event_v1 *record, const struct pt_regs *regs, bool compat)
{
	unsigned int i;

	(void)compat;
	for (i = VIROS_EVENT_MIPS_R0; i <= VIROS_EVENT_MIPS_R31; ++i)
		record->registers[i] = (ve_u32)regs->regs[i];
	/* r0 is architectural zero; the exception entry does not save k0/k1. */
	record->registers[VIROS_EVENT_MIPS_R0] = 0;
	record->registers[26] = 0;
	record->registers[27] = 0;
	record->registers[VIROS_EVENT_MIPS_STATUS] =
		(ve_u32)regs->cp0_status;
	record->registers[VIROS_EVENT_MIPS_LO] = (ve_u32)regs->lo;
	record->registers[VIROS_EVENT_MIPS_HI] = (ve_u32)regs->hi;
	record->registers[VIROS_EVENT_MIPS_BADVADDR] =
		(ve_u32)regs->cp0_badvaddr;
	record->registers[VIROS_EVENT_MIPS_CAUSE] =
		(ve_u32)regs->cp0_cause;
	record->registers[VIROS_EVENT_MIPS_PC] = (ve_u32)regs->cp0_epc;
	record->register_count = VIROS_EVENT_MIPS_REGISTER_COUNT;
	record->register_valid_mask =
		((1ULL << VIROS_EVENT_MIPS_REGISTER_COUNT) - 1) &
		~((1ULL << 26) | (1ULL << 27));
}
#elif defined(CONFIG_X86_64)
static __always_inline ve_u64 viros_x86_value(unsigned long value, bool compat)
{
	return compat ? (ve_u32)value : (ve_u64)value;
}

static __always_inline void viros_event_copy_registers(
	struct viros_event_v1 *record, const struct pt_regs *regs, bool compat)
{
	record->registers[VIROS_EVENT_X86_R15] =
		viros_x86_value(regs->r15, compat);
	record->registers[VIROS_EVENT_X86_R14] =
		viros_x86_value(regs->r14, compat);
	record->registers[VIROS_EVENT_X86_R13] =
		viros_x86_value(regs->r13, compat);
	record->registers[VIROS_EVENT_X86_R12] =
		viros_x86_value(regs->r12, compat);
	record->registers[VIROS_EVENT_X86_RBP] =
		viros_x86_value(regs->bp, compat);
	record->registers[VIROS_EVENT_X86_RBX] =
		viros_x86_value(regs->bx, compat);
	record->registers[VIROS_EVENT_X86_R11] =
		viros_x86_value(regs->r11, compat);
	record->registers[VIROS_EVENT_X86_R10] =
		viros_x86_value(regs->r10, compat);
	record->registers[VIROS_EVENT_X86_R9] =
		viros_x86_value(regs->r9, compat);
	record->registers[VIROS_EVENT_X86_R8] =
		viros_x86_value(regs->r8, compat);
	record->registers[VIROS_EVENT_X86_RAX] =
		viros_x86_value(regs->ax, compat);
	record->registers[VIROS_EVENT_X86_RCX] =
		viros_x86_value(regs->cx, compat);
	record->registers[VIROS_EVENT_X86_RDX] =
		viros_x86_value(regs->dx, compat);
	record->registers[VIROS_EVENT_X86_RSI] =
		viros_x86_value(regs->si, compat);
	record->registers[VIROS_EVENT_X86_RDI] =
		viros_x86_value(regs->di, compat);
	record->registers[VIROS_EVENT_X86_ORIG_RAX] =
		viros_x86_value(regs->orig_ax, compat);
	record->registers[VIROS_EVENT_X86_RIP] =
		viros_x86_value(regs->ip, compat);
	record->registers[VIROS_EVENT_X86_CS] =
		viros_x86_value(regs->cs, compat);
	record->registers[VIROS_EVENT_X86_EFLAGS] =
		viros_x86_value(regs->flags, compat);
	record->registers[VIROS_EVENT_X86_RSP] =
		viros_x86_value(regs->sp, compat);
	record->registers[VIROS_EVENT_X86_SS] =
		viros_x86_value(regs->ss, compat);
	record->register_count = VIROS_EVENT_X86_REGISTER_COUNT;
	record->register_valid_mask =
		(1ULL << VIROS_EVENT_X86_REGISTER_COUNT) - 1;
	if (compat) {
		record->registers[VIROS_EVENT_X86_R15] = 0;
		record->registers[VIROS_EVENT_X86_R14] = 0;
		record->registers[VIROS_EVENT_X86_R13] = 0;
		record->registers[VIROS_EVENT_X86_R12] = 0;
		record->registers[VIROS_EVENT_X86_R11] = 0;
		record->registers[VIROS_EVENT_X86_R10] = 0;
		record->registers[VIROS_EVENT_X86_R9] = 0;
		record->registers[VIROS_EVENT_X86_R8] = 0;
		record->register_valid_mask &=
			~((1ULL << VIROS_EVENT_X86_R15) |
			  (1ULL << VIROS_EVENT_X86_R14) |
			  (1ULL << VIROS_EVENT_X86_R13) |
			  (1ULL << VIROS_EVENT_X86_R12) |
			  (1ULL << VIROS_EVENT_X86_R11) |
			  (1ULL << VIROS_EVENT_X86_R10) |
			  (1ULL << VIROS_EVENT_X86_R9) |
			  (1ULL << VIROS_EVENT_X86_R8));
	}
}
#endif

static __always_inline ve_u16 viros_event_arch(void)
{
#if defined(CONFIG_ARM)
	return VIROS_EVENT_ARCH_ARM;
#elif defined(CONFIG_ARM64)
	return VIROS_EVENT_ARCH_AARCH64;
#elif defined(CONFIG_MIPS)
	return VIROS_EVENT_ARCH_MIPS;
#else
	return VIROS_EVENT_ARCH_X86;
#endif
}

static notrace void viros_event_publish(struct pt_regs *regs, ve_u16 kind,
					ve_u32 signal, ve_s32 code,
					unsigned long address,
					bool has_address, bool user,
					bool compat)
{
	struct viros_event_storage *storage;
	struct viros_event_v1 *record;

	storage = this_cpu_ptr(&viros_event_storage);
	if (!++storage->sequence)
		++storage->sequence;
	record = &storage->record;

	record->magic = VIROS_EVENT_MAGIC;
	record->abi_major = VIROS_EVENT_ABI_MAJOR;
	record->abi_minor = VIROS_EVENT_ABI_MINOR;
	record->arch = viros_event_arch();
	record->endian = viros_event_endian();
	record->pointer_bits = sizeof(void *) * 8;
	record->kind = kind;
	record->signal = signal;
	record->code = code;
	record->flags = VIROS_EVENT_REGS_VALID;
	if (user)
		record->flags |= VIROS_EVENT_REGS_USER;
	if (compat)
		record->flags |= VIROS_EVENT_REGS_COMPAT;
	if (has_address)
		record->flags |= VIROS_EVENT_ADDRESS_VALID;
	record->cpu = raw_smp_processor_id();
	record->tgid = current->tgid;
	record->tid = current->pid;
	record->sequence = storage->sequence;
	record->task = (ve_u64)(unsigned long)current;
	record->mm = (ve_u64)(unsigned long)current->mm;
	record->start_cookie = (ve_u64)current->start_time;
	record->signal_struct = (ve_u64)(unsigned long)current->signal;
	record->address = has_address ? (ve_u64)address : 0;
	record->reserved0 = 0;
	viros_event_copy_comm(record);
	viros_event_copy_registers(record, regs, compat);
	record->record_size = VIROS_EVENT_HEADER_SIZE +
		record->register_count * sizeof(record->registers[0]);

	viros_event_stop(record);
}

static notrace void viros_signal_deliver(void *unused, int delivered_sig,
					 struct kernel_siginfo *info,
					 struct k_sigaction *ka)
{
	struct pt_regs *regs;
	int sig;
	bool has_address;
	bool compat = false;

	(void)unused;
	if (!current->mm)
		return;
	regs = task_pt_regs(current);
	if (!regs || !user_mode(regs))
		return;
#if defined(CONFIG_ARM64)
	/* AArch32 frames require an ARM target description, not this one. */
	if (compat_user_mode(regs))
		return;
#elif defined(CONFIG_X86_64)
	compat = !user_64bit_mode(regs);
#endif
	sig = viros_event_reported_signal(delivered_sig, ka);
	if (!sig)
		return;

	has_address = viros_event_has_address(sig, info);
	viros_event_publish(regs, VIROS_EVENT_USER_SIGNAL, sig,
		info ? info->si_code : 0,
		has_address ? (unsigned long)info->si_addr : 0,
		has_address, true, compat);
}

static notrace int viros_die_notify(struct notifier_block *notifier,
				    unsigned long reason, void *data)
{
	struct die_args *args = data;
	int signal;

	(void)notifier;
	if (reason != DIE_OOPS || !args || !args->regs || user_mode(args->regs))
		return NOTIFY_DONE;
	signal = args->signr;
	if (signal < 1 || signal > 64)
		signal = SIGSEGV;
	viros_event_publish(args->regs, VIROS_EVENT_KERNEL_DIE, signal,
		args->trapnr, instruction_pointer(args->regs), true, false, false);
	return NOTIFY_DONE;
}

static struct notifier_block viros_die_notifier = {
	.notifier_call = viros_die_notify,
};

static int __init viros_event_init(void)
{
	int ret;

	ret = register_die_notifier(&viros_die_notifier);
	if (ret)
		return ret;
	ret = register_trace_signal_deliver(viros_signal_deliver, NULL);
	if (ret)
		unregister_die_notifier(&viros_die_notifier);
	return ret;
}
core_initcall(viros_event_init);
