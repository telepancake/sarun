// SPDX-License-Identifier: GPL-2.0
/*
 * Frozen-kernel, read-only task probe.
 *
 * This is deliberately not a module.  It is compiled by the exact kernel's
 * Kbuild, linked into scratch memory by the debugger, called once, and then
 * removed.  It must remain self-contained: probe_tool.py rejects undefined
 * symbols and instrumentation sections.
 */
#include <linux/auxvec.h>
#include <linux/compiler.h>
#include <linux/mm_types.h>
#include <linux/sched.h>
#include <linux/sched/signal.h>
#include <linux/sched/task.h>
#include <linux/sched/task_stack.h>
#include <linux/version.h>
#include <asm/page.h>
#include <asm/pgtable.h>
#include <asm/ptrace.h>
#include <asm/thread_info.h>

#include "viros_probe_abi.h"

/* Retained completion boundary used by the architecture-specific call gate. */
#if defined(CONFIG_ARM64)
asm(".pushsection .text.viros_probe_complete,\"ax\"\n"
    ".global viros_probe_complete\n"
    ".type viros_probe_complete, %function\n"
    "viros_probe_complete:\n"
    "brk #0x5650\n"
    ".size viros_probe_complete, .-viros_probe_complete\n"
    ".popsection\n");
#elif defined(CONFIG_X86_64)
/*
 * x86 has no link register.  Enter through a retained wrapper which makes a
 * normal SysV call on the dedicated, 16-byte-aligned stack.  Returning from
 * the C helper reaches the adjacent one-byte completion boundary.
 */
asm(".pushsection .text.viros_probe_entry,\"ax\"\n"
    ".balign 16\n"
    ".global viros_probe_entry\n"
    ".type viros_probe_entry, @function\n"
    "viros_probe_entry:\n"
    "call viros_probe_main\n"
    ".size viros_probe_entry, .-viros_probe_entry\n"
    ".global viros_probe_complete\n"
    ".type viros_probe_complete, @function\n"
    "viros_probe_complete:\n"
    "int3\n"
    ".size viros_probe_complete, .-viros_probe_complete\n"
    ".popsection\n");
#elif defined(CONFIG_ARM)
/*
 * The published ARM kernel is built in the four-byte ARM instruction set.
 * The call gate installs a hardware breakpoint on this symbol, so UDF is a
 * deterministic fallback rather than the normal completion mechanism.
 */
asm(".pushsection .text.viros_probe_complete,\"ax\"\n"
    ".arm\n"
    ".balign 4\n"
    ".global viros_probe_complete\n"
    ".type viros_probe_complete, %function\n"
    "viros_probe_complete:\n"
    ".word 0xe7f565f0\n" /* ARM UDF #0x5650 */
    ".size viros_probe_complete, .-viros_probe_complete\n"
    ".popsection\n");
#elif defined(CONFIG_MIPS)
/*
 * The call gate installs a hardware breakpoint on this symbol, so the
 * instruction normally is not executed.  Spell it as one fixed word anyway:
 * an o32 return address must name a four-byte classic-MIPS instruction, never
 * a MIPS16 or microMIPS encoding selected by assembler state.
 */
asm(".pushsection .text.viros_probe_complete,\"ax\"\n"
    ".set push\n"
    ".set noreorder\n"
    ".set nomips16\n"
    ".set nomicromips\n"
    ".global viros_probe_complete\n"
    ".type viros_probe_complete, %function\n"
    "viros_probe_complete:\n"
    ".word 0x0015940d\n" /* classic MIPS break 0x5650 */
    ".size viros_probe_complete, .-viros_probe_complete\n"
    ".set pop\n"
    ".popsection\n");
#endif

#ifndef AT_SYSINFO_EHDR
#define AT_SYSINFO_EHDR 33
#endif
#ifndef AT_EXECFN
#define AT_EXECFN 31
#endif
#ifndef AT_SECURE
#define AT_SECURE 23
#endif

static __always_inline vp_u16 viros_task_abi_bits(struct task_struct *task)
{
#if defined(CONFIG_ARM64) && defined(CONFIG_COMPAT) && defined(TIF_32BIT)
	if (test_tsk_thread_flag(task, TIF_32BIT))
		return 32;
#elif defined(CONFIG_X86_64) && defined(TIF_ADDR32)
	if (test_tsk_thread_flag(task, TIF_ADDR32))
		return 32;
#endif
	return sizeof(void *) * 8;
}

static __always_inline vp_u64 viros_task_state(struct task_struct *task)
{
#if LINUX_VERSION_CODE >= KERNEL_VERSION(5, 14, 0)
	return (vp_u64)task->__state;
#else
	return (vp_u64)task->state;
#endif
}

static __always_inline void viros_put_aux(
	volatile struct viros_probe_task_v1 *record, vp_u64 key, vp_u64 value)
{
	vp_u16 index;

	switch (key) {
	case AT_PHDR: index = VIROS_PROBE_AUX_PHDR; break;
	case AT_PHENT: index = VIROS_PROBE_AUX_PHENT; break;
	case AT_PHNUM: index = VIROS_PROBE_AUX_PHNUM; break;
	case AT_PAGESZ: index = VIROS_PROBE_AUX_PAGESZ; break;
	case AT_BASE: index = VIROS_PROBE_AUX_BASE; break;
	case AT_ENTRY: index = VIROS_PROBE_AUX_ENTRY; break;
	case AT_RANDOM: index = VIROS_PROBE_AUX_RANDOM; break;
	case AT_SYSINFO_EHDR: index = VIROS_PROBE_AUX_SYSINFO_EHDR; break;
	case AT_EXECFN: index = VIROS_PROBE_AUX_EXECFN; break;
	case AT_SECURE: index = VIROS_PROBE_AUX_SECURE; break;
	default: return;
	}
	record->auxv[index] = value;
	record->auxv_valid |= (vp_u16)(1U << index);
}

static __always_inline void viros_copy_auxv(
	struct task_struct *task, volatile struct viros_probe_task_v1 *record)
{
	struct mm_struct *mm = task->mm;
	unsigned int i, pairs;

	if (!mm)
		return;

	if (record->abi_bits == 32 && sizeof(unsigned long) == 8) {
		const vp_u32 *aux = (const vp_u32 *)mm->saved_auxv;
		pairs = sizeof(mm->saved_auxv) / (2 * sizeof(*aux));
		for (i = 0; i < pairs; ++i) {
			vp_u32 key = aux[i * 2];
			if (!key)
				break;
			viros_put_aux(record, key, aux[i * 2 + 1]);
		}
	} else {
		const unsigned long *aux = mm->saved_auxv;
		pairs = sizeof(mm->saved_auxv) / (2 * sizeof(*aux));
		for (i = 0; i < pairs; ++i) {
			unsigned long key = aux[i * 2];
			if (!key)
				break;
			viros_put_aux(record, key, aux[i * 2 + 1]);
		}
	}
	if (record->auxv_valid)
		record->probe_flags |= VIROS_PROBE_TASK_AUX_VALID;
}

static __always_inline void viros_fill_task(
	struct task_struct *task, volatile struct viros_probe_task_v1 *record)
{
	unsigned int i;
	struct mm_struct *mm = task->mm;

	record->record_size = VIROS_PROBE_TASK_V1_SIZE;
	record->record_version = 1;
	record->probe_flags = 0;
	record->task = (vp_u64)(unsigned long)task;
	record->group_leader = (vp_u64)(unsigned long)task->group_leader;
	record->real_parent = (vp_u64)(unsigned long)task->real_parent;
	record->mm = (vp_u64)(unsigned long)mm;
	record->pgd = mm ? (vp_u64)(unsigned long)mm->pgd : 0;
	record->start_cookie = (vp_u64)task->start_time;
	record->state = viros_task_state(task);
	record->task_flags = (vp_u64)task->flags;
	record->pid = (vp_u32)task->pid;
	record->tgid = (vp_u32)task->tgid;
	record->ppid = task->real_parent ? (vp_u32)task->real_parent->tgid : 0;
	record->cpu = (vp_u32)task_cpu(task);
	record->exit_state = (vp_u32)task->exit_state;
	record->abi_bits = viros_task_abi_bits(task);
	record->auxv_valid = 0;
	for (i = 0; i < VIROS_PROBE_COMM_SIZE; ++i)
		record->comm[i] = task->comm[i];
	record->auxv[0] = 0; record->auxv[1] = 0;
	record->auxv[2] = 0; record->auxv[3] = 0;
	record->auxv[4] = 0; record->auxv[5] = 0;
	record->auxv[6] = 0; record->auxv[7] = 0;
	record->auxv[8] = 0; record->auxv[9] = 0;

	if (mm)
		record->probe_flags |= VIROS_PROBE_TASK_HAS_MM;
	if (task == task->group_leader)
		record->probe_flags |= VIROS_PROBE_TASK_GROUP_LEADER;
#ifdef CONFIG_SMP
	if (task->on_cpu)
		record->probe_flags |= VIROS_PROBE_TASK_ON_CPU;
#endif
	viros_copy_auxv(task, record);
}

static __always_inline void viros_init_response(
	volatile struct viros_probe_response_v1 *response, vp_u16 record_size,
	vp_u16 abi_minor)
{
	response->magic = VIROS_PROBE_RESPONSE_MAGIC;
	response->abi_major = VIROS_PROBE_ABI_MAJOR;
	response->abi_minor = abi_minor;
	response->header_size = VIROS_PROBE_RESPONSE_SIZE;
	response->record_size = record_size;
#if defined(CONFIG_ARM64)
	response->arch = VIROS_PROBE_ARCH_AARCH64;
#elif defined(CONFIG_ARM)
	response->arch = VIROS_PROBE_ARCH_ARM;
#elif defined(CONFIG_MIPS)
	response->arch = VIROS_PROBE_ARCH_MIPS;
#elif defined(CONFIG_X86_64)
	response->arch = VIROS_PROBE_ARCH_X86;
#else
	response->arch = VIROS_PROBE_ARCH_UNKNOWN;
#endif
#if defined(CONFIG_MIPS) && defined(CONFIG_CPU_LITTLE_ENDIAN)
	response->endian = VIROS_PROBE_ENDIAN_LITTLE;
#elif defined(CONFIG_MIPS) && defined(CONFIG_CPU_BIG_ENDIAN)
	response->endian = VIROS_PROBE_ENDIAN_BIG;
#elif defined(__BYTE_ORDER__) && (__BYTE_ORDER__ == __ORDER_LITTLE_ENDIAN__)
	response->endian = VIROS_PROBE_ENDIAN_LITTLE;
#else
	response->endian = VIROS_PROBE_ENDIAN_BIG;
#endif
#if defined(CONFIG_MIPS)
	/* This leaf is deliberately compiled with the o32 ABI. */
	response->pointer_bits = 32;
#else
	response->pointer_bits = sizeof(void *) * 8;
#endif
	response->status = VIROS_PROBE_OK;
	response->flags = 0;
	response->record_count = 0;
	response->bytes_written = VIROS_PROBE_RESPONSE_SIZE;
	response->next_cursor = 0;
	response->snapshot_root = 0;
	response->page_shift = PAGE_SHIFT;
	response->reserved0 = 0;
	response->reserved1 = 0;
}

#if defined(CONFIG_ARM64)
static __always_inline vp_u32 viros_translation_flags(vp_u64 raw, int block)
{
	vp_u32 flags = VIROS_PROBE_XLATE_PRESENT;

	if (raw & PTE_USER)
		flags |= VIROS_PROBE_XLATE_USER;
	if (raw & PTE_WRITE)
		flags |= VIROS_PROBE_XLATE_WRITABLE;
	if (!(raw & PTE_UXN))
		flags |= VIROS_PROBE_XLATE_EXECUTABLE;
	if (block)
		flags |= VIROS_PROBE_XLATE_BLOCK;
#ifdef PTE_SPECIAL
	if (raw & PTE_SPECIAL)
		flags |= VIROS_PROBE_XLATE_SPECIAL;
#endif
#ifdef PTE_DEVMAP
	if (raw & PTE_DEVMAP)
		flags |= VIROS_PROBE_XLATE_SPECIAL;
#endif
	/* Never label kernel-only or special/device mappings safe for host reads. */
	if ((flags & (VIROS_PROBE_XLATE_USER | VIROS_PROBE_XLATE_SPECIAL)) ==
	    VIROS_PROBE_XLATE_USER)
		flags |= VIROS_PROBE_XLATE_SAFE_READ;
	return flags;
}

static __always_inline long viros_finish_translation(
	volatile struct viros_probe_response_v1 *response,
	volatile struct viros_probe_translation_v1 *record,
	struct task_struct *task, struct mm_struct *mm, unsigned long address,
	vp_u64 pfn, vp_u32 shift, vp_u16 level, vp_u64 raw, int block)
{
	vp_u64 mapping = (vp_u64)1 << shift;
	vp_u64 offset = (vp_u64)address & (mapping - 1);

	record->record_size = VIROS_PROBE_TRANSLATION_V1_SIZE;
	record->record_version = 1;
	record->translation_flags = viros_translation_flags(raw, block);
	record->task = (vp_u64)(unsigned long)task;
	record->mm = (vp_u64)(unsigned long)mm;
	record->virtual_address = address;
	record->physical_address = (pfn << PAGE_SHIFT) + offset;
	record->contiguous_bytes = mapping - offset;
	record->mapping_bytes = mapping;
	record->page_shift = shift;
	record->level = level;
	record->reserved0 = 0;
	response->record_count = 1;
	response->bytes_written += VIROS_PROBE_TRANSLATION_V1_SIZE;
	return VIROS_PROBE_OK;
}

/*
 * Read-only AArch64 walk using the exact target Kbuild's pgtable types/macros.
 * The p4d layer is intentionally retained: it folds away on older/four-level
 * arm64 configurations and remains correct for five-level capable headers.
 * No helper which allocates, faults, locks, or sleeps is used.
 */
static __always_inline long viros_translate_va(
	const struct viros_probe_request_v1 *request,
	volatile struct viros_probe_response_v1 *response, vp_u32 output_bytes)
{
	volatile struct viros_probe_translation_v1 *record;
	struct task_struct *task;
	struct mm_struct *mm;
	unsigned long address;
	pgd_t *pgd;
	p4d_t *p4d;
	pud_t *pud;
	pmd_t *pmd;
	pte_t *pte;
	vp_u64 linear_offset;

	if (output_bytes < VIROS_PROBE_RESPONSE_SIZE +
	    VIROS_PROBE_TRANSLATION_V1_SIZE || !request->init_task ||
	    !request->cursor_task || request->flags || request->max_records ||
	    request->abi_minor < 1 ||
	    request->abi_minor > VIROS_PROBE_ABI_MINOR ||
	    request->reserved0 || !request->reserved3 ||
	    (request->reserved3 & (PAGE_SIZE - 1))) {
		response->status = VIROS_PROBE_BAD_REQUEST;
		return VIROS_PROBE_BAD_REQUEST;
	}
	task = (struct task_struct *)(unsigned long)request->init_task;
	mm = (struct mm_struct *)(unsigned long)request->cursor_task;
	address = (unsigned long)request->reserved2;
	linear_offset = request->reserved3;
	/* The host only emits pointers from a frozen snapshot.  These checks make
	 * task reuse, exec() mm replacement, and cross-snapshot requests explicit. */
	if ((vp_u64)task->start_time != request->reserved1 || task->mm != mm) {
		response->status = VIROS_PROBE_STALE_TASK;
		return VIROS_PROBE_STALE_TASK;
	}
	if (!mm || (vp_u64)address != request->reserved2 ||
	    ((viros_task_abi_bits(task) == 32 && request->reserved2 >= (1ULL << 32)) ||
	     request->reserved2 >= (1ULL << 63))) {
		response->status = VIROS_PROBE_BAD_REQUEST;
		return VIROS_PROBE_BAD_REQUEST;
	}
	response->snapshot_root = request->init_task;
	record = (volatile struct viros_probe_translation_v1 *)
		((volatile vp_u8 *)response + VIROS_PROBE_RESPONSE_SIZE);

	pgd = pgd_offset(mm, address);
	if (pgd_none(*pgd) || pgd_bad(*pgd))
		goto not_present;
#if CONFIG_PGTABLE_LEVELS > 4
	p4d = (p4d_t *)(unsigned long)
		(__pte_to_phys(__pte(pgd_val(*pgd))) + linear_offset);
	p4d += p4d_index(address);
#else
	p4d = (p4d_t *)pgd;
#endif
	if (p4d_none(*p4d) || p4d_bad(*p4d))
		goto not_present;
#if CONFIG_PGTABLE_LEVELS > 3
	pud = (pud_t *)(unsigned long)
		(__pte_to_phys(__pte(p4d_val(*p4d))) + linear_offset);
	pud += pud_index(address);
#else
	pud = (pud_t *)p4d;
#endif
	if (pud_none(*pud))
		goto not_present;
	if (pud_sect(*pud))
		return viros_finish_translation(response, record, task, mm,
			address, pud_pfn(*pud), PUD_SHIFT, 2, pud_val(*pud), 1);
	if (pud_bad(*pud))
		goto not_present;
#if CONFIG_PGTABLE_LEVELS > 2
	pmd = (pmd_t *)(unsigned long)
		(__pte_to_phys(__pte(pud_val(*pud))) + linear_offset);
	pmd += pmd_index(address);
#else
	pmd = (pmd_t *)pud;
#endif
	if (pmd_none(*pmd))
		goto not_present;
	if (pmd_sect(*pmd))
		return viros_finish_translation(response, record, task, mm,
			address, pmd_pfn(*pmd), PMD_SHIFT, 3, pmd_val(*pmd), 1);
	if (pmd_bad(*pmd))
		goto not_present;
	pte = (pte_t *)(unsigned long)
		(__pte_to_phys(__pte(pmd_val(*pmd))) + linear_offset);
	pte += pte_index(address);
	if (pte_none(*pte) || !pte_valid(*pte))
		goto not_present;
	return viros_finish_translation(response, record, task, mm, address,
		pte_pfn(*pte), PAGE_SHIFT, 4, pte_val(*pte), 0);

not_present:
	response->status = VIROS_PROBE_NOT_PRESENT;
	return VIROS_PROBE_NOT_PRESENT;
}

static __always_inline long viros_saved_regs(
	const struct viros_probe_request_v1 *request,
	volatile struct viros_probe_response_v1 *response, vp_u32 output_bytes)
{
	volatile struct viros_probe_saved_regs_v1 *record;
	struct task_struct *task;
	struct mm_struct *mm;
	struct pt_regs *regs;
	unsigned long stack, regs_address;
	unsigned int i;

	if (output_bytes < VIROS_PROBE_RESPONSE_SIZE +
	    VIROS_PROBE_SAVED_REGS_V1_SIZE || request->abi_minor < 2 ||
	    !request->init_task || !request->cursor_task || request->flags ||
	    request->max_records || request->reserved0 || request->reserved2 ||
	    request->reserved3) {
		response->status = VIROS_PROBE_BAD_REQUEST;
		return VIROS_PROBE_BAD_REQUEST;
	}
	task = (struct task_struct *)(unsigned long)request->init_task;
	mm = (struct mm_struct *)(unsigned long)request->cursor_task;
	if ((vp_u64)task->start_time != request->reserved1 || task->mm != mm) {
		response->status = VIROS_PROBE_STALE_TASK;
		return VIROS_PROBE_STALE_TASK;
	}
	if (!mm || (task->flags & PF_KTHREAD)) {
		response->status = VIROS_PROBE_UNSUPPORTED;
		return VIROS_PROBE_UNSUPPORTED;
	}
	if (viros_task_abi_bits(task) != 64) {
		response->status = VIROS_PROBE_COMPAT_TASK;
		return VIROS_PROBE_COMPAT_TASK;
	}
	/* A task executing on any CPU has no authoritative saved EL0 frame. */
	if (task == current
#ifdef CONFIG_SMP
	    || task->on_cpu
#endif
	) {
		response->status = VIROS_PROBE_TASK_RUNNING;
		return VIROS_PROBE_TASK_RUNNING;
	}

	stack = (unsigned long)task_stack_page(task);
	regs = task_pt_regs(task);
	regs_address = (unsigned long)regs;
	if (!stack || stack + THREAD_SIZE < stack || regs_address < stack ||
	    regs_address + sizeof(*regs) < regs_address ||
	    regs_address + sizeof(*regs) > stack + THREAD_SIZE ||
	    (regs_address & (sizeof(vp_u64) - 1)) || !user_mode(regs) ||
	    (regs->pstate & PSR_MODE_MASK) != PSR_MODE_EL0t ||
	    (vp_u64)regs->pstate >= (1ULL << 32) ||
	    (vp_u64)regs->sp >= (1ULL << 63) ||
	    (vp_u64)regs->pc >= (1ULL << 63)) {
		response->status = VIROS_PROBE_INVALID_REGS;
		return VIROS_PROBE_INVALID_REGS;
	}

	record = (volatile struct viros_probe_saved_regs_v1 *)
		((volatile vp_u8 *)response + VIROS_PROBE_RESPONSE_SIZE);
	record->record_size = VIROS_PROBE_SAVED_REGS_V1_SIZE;
	record->record_version = 1;
	record->saved_regs_flags = VIROS_PROBE_REGS_VALID |
		VIROS_PROBE_REGS_USER | VIROS_PROBE_REGS_AARCH64_64;
	record->task = (vp_u64)(unsigned long)task;
	record->mm = (vp_u64)(unsigned long)mm;
	record->start_cookie = (vp_u64)task->start_time;
	for (i = 0; i < 31; ++i)
		record->x[i] = (vp_u64)regs->regs[i];
	record->sp = (vp_u64)regs->sp;
	record->pc = (vp_u64)regs->pc;
	record->pstate = (vp_u64)regs->pstate;
	response->record_count = 1;
	response->bytes_written += VIROS_PROBE_SAVED_REGS_V1_SIZE;
	response->snapshot_root = request->init_task;
	return VIROS_PROBE_OK;
}
#endif

/*
 * The launcher supplies a dedicated stack and an architecture-specific return
 * path to the retained completion boundary.  The volatile output prevents the
 * compiler from replacing record stores with out-of-object memcpy/memset
 * calls.
 */
#if defined(CONFIG_X86_64)
#define VIROS_PROBE_C_ENTRY viros_probe_main
#else
#define VIROS_PROBE_C_ENTRY viros_probe_entry
#endif

noinline __used notrace __no_sanitize_address
long VIROS_PROBE_C_ENTRY(const struct viros_probe_request_v1 *request,
		       void *output, vp_u32 output_bytes)
{
	volatile struct viros_probe_response_v1 *response = output;
	struct task_struct *root, *task, *next;
	vp_u32 capacity, limit;

	if (!output || output_bytes < VIROS_PROBE_RESPONSE_SIZE)
		return VIROS_PROBE_SHORT_BUFFER;
	viros_init_response(response,
		request && request->opcode == VIROS_PROBE_OP_TRANSLATE_VA ?
		VIROS_PROBE_TRANSLATION_V1_SIZE :
		request && request->opcode == VIROS_PROBE_OP_SAVED_REGS ?
		VIROS_PROBE_SAVED_REGS_V1_SIZE : VIROS_PROBE_TASK_V1_SIZE,
		request && request->abi_minor <= VIROS_PROBE_ABI_MINOR ?
		request->abi_minor : VIROS_PROBE_ABI_MINOR);

	if (!request || request->magic != VIROS_PROBE_REQUEST_MAGIC ||
	    request->abi_major != VIROS_PROBE_ABI_MAJOR ||
	    request->abi_minor > VIROS_PROBE_ABI_MINOR ||
	    request->size < VIROS_PROBE_REQUEST_SIZE) {
		response->status = VIROS_PROBE_BAD_REQUEST;
		return VIROS_PROBE_BAD_REQUEST;
	}
	if (request->opcode == VIROS_PROBE_OP_TRANSLATE_VA) {
#if defined(CONFIG_ARM64)
		return viros_translate_va(request, response, output_bytes);
#else
		response->status = VIROS_PROBE_UNSUPPORTED;
		return VIROS_PROBE_UNSUPPORTED;
#endif
	}
	if (request->opcode == VIROS_PROBE_OP_SAVED_REGS) {
#if defined(CONFIG_ARM64)
		return viros_saved_regs(request, response, output_bytes);
#else
		response->status = VIROS_PROBE_UNSUPPORTED;
		return VIROS_PROBE_UNSUPPORTED;
#endif
	}
	if (request->opcode != VIROS_PROBE_OP_SNAPSHOT || !request->init_task ||
	    request->flags || request->reserved0 || request->reserved1 ||
	    request->reserved2 || request->reserved3) {
		response->status = VIROS_PROBE_BAD_REQUEST;
		return VIROS_PROBE_BAD_REQUEST;
	}

	root = (struct task_struct *)(unsigned long)request->init_task;
	task = request->cursor_task ?
		(struct task_struct *)(unsigned long)request->cursor_task : root;
	response->snapshot_root = request->init_task;
	capacity = (output_bytes - VIROS_PROBE_RESPONSE_SIZE) /
		VIROS_PROBE_TASK_V1_SIZE;
	limit = request->max_records && request->max_records < capacity ?
		request->max_records : capacity;
	if (!limit) {
		response->status = VIROS_PROBE_SHORT_BUFFER;
		return VIROS_PROBE_SHORT_BUFFER;
	}

	while (response->record_count < limit) {
		volatile struct viros_probe_task_v1 *record;
		struct list_head *thread_node = &task->thread_group;
		record = (volatile struct viros_probe_task_v1 *)
			((volatile vp_u8 *)output + VIROS_PROBE_RESPONSE_SIZE +
			 response->record_count * VIROS_PROBE_TASK_V1_SIZE);
		viros_fill_task(task, record);
		response->record_count++;
		response->bytes_written += VIROS_PROBE_TASK_V1_SIZE;

		/* `tasks` contains leaders; walk each leader's thread ring first. */
		if (!thread_node->next || !thread_node->prev ||
		    thread_node->next->prev != thread_node ||
		    thread_node->prev->next != thread_node) {
			response->status = VIROS_PROBE_CORRUPT_LIST;
			return VIROS_PROBE_CORRUPT_LIST;
		}
		next = next_thread(task);
		if (next != task->group_leader) {
			if (!next || next->group_leader != task->group_leader) {
				response->status = VIROS_PROBE_CORRUPT_LIST;
				return VIROS_PROBE_CORRUPT_LIST;
			}
			task = next;
		} else {
			struct task_struct *leader = task->group_leader;
			struct list_head *node = &leader->tasks;
			struct list_head *next_node = node->next;

			if (!next_node || !node->prev || next_node->prev != node ||
			    node->prev->next != node) {
				response->status = VIROS_PROBE_CORRUPT_LIST;
				return VIROS_PROBE_CORRUPT_LIST;
			}
			if (next_node == &root->tasks)
				return VIROS_PROBE_OK;
			task = list_entry(next_node, struct task_struct, tasks);
		}
	}

	response->flags |= VIROS_PROBE_RESP_MORE;
	response->next_cursor = (vp_u64)(unsigned long)task;
	return VIROS_PROBE_OK;
}
