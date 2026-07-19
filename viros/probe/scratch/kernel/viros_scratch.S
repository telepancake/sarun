/* SPDX-License-Identifier: GPL-2.0 */
/*
 * Passive scratch storage for the viros reversible debugger call gate.
 *
 * Nothing references or executes this object during normal kernel operation.
 * The executable page is deliberately filled with BRK instructions so that an
 * accidental branch into any aligned instruction slot stops immediately.
 */

#if defined(CONFIG_ARM64)

#if defined(CONFIG_ARM64_4K_PAGES)
#define VIROS_SCRATCH_PAGE_SIZE 4096
#elif defined(CONFIG_ARM64_16K_PAGES)
#define VIROS_SCRATCH_PAGE_SIZE 16384
#elif defined(CONFIG_ARM64_64K_PAGES)
#define VIROS_SCRATCH_PAGE_SIZE 65536
#else
#error "viros scratch requires a supported AArch64 kernel page size"
#endif

	.pushsection .text.viros_scratch,"ax"
	.balign VIROS_SCRATCH_PAGE_SIZE
	.globl __viros_scratch_code_start
	.type __viros_scratch_code_start, %object
__viros_scratch_code_start:
	.rept VIROS_SCRATCH_PAGE_SIZE / 4
	brk #0x5653
	.endr
	.globl __viros_scratch_code_end
__viros_scratch_code_end:
	.size __viros_scratch_code_start, \
		__viros_scratch_code_end - __viros_scratch_code_start
	.popsection

	.pushsection .bss.viros_scratch_data,"aw",%nobits
	.balign VIROS_SCRATCH_PAGE_SIZE
	.globl __viros_scratch_data_start
	.type __viros_scratch_data_start, %object
__viros_scratch_data_start:
	.skip VIROS_SCRATCH_PAGE_SIZE
	.globl __viros_scratch_data_end
__viros_scratch_data_end:
	.size __viros_scratch_data_start, \
		__viros_scratch_data_end - __viros_scratch_data_start
	.popsection

	.pushsection .bss.viros_scratch_stack,"aw",%nobits
	.balign VIROS_SCRATCH_PAGE_SIZE
	.globl __viros_scratch_stack_start
	.type __viros_scratch_stack_start, %object
__viros_scratch_stack_start:
	.skip VIROS_SCRATCH_PAGE_SIZE
	.globl __viros_scratch_stack_end
__viros_scratch_stack_end:
	.size __viros_scratch_stack_start, \
		__viros_scratch_stack_end - __viros_scratch_stack_start
	.popsection

	.section .note.GNU-stack,"",%progbits

#elif defined(CONFIG_MIPS)

#if defined(CONFIG_PAGE_SIZE_4KB)
#define VIROS_SCRATCH_PAGE_SIZE 4096
#elif defined(CONFIG_PAGE_SIZE_16KB)
#define VIROS_SCRATCH_PAGE_SIZE 16384
#elif defined(CONFIG_PAGE_SIZE_64KB)
#define VIROS_SCRATCH_PAGE_SIZE 65536
#else
#error "viros scratch requires a supported MIPS kernel page size"
#endif

	.set push
	.set noreorder
	.set nomips16
	.set nomicromips
	.set mips32

	.pushsection .text.viros_scratch,"ax"
	.balign VIROS_SCRATCH_PAGE_SIZE
	.globl __viros_scratch_code_start
	.type __viros_scratch_code_start, %object
__viros_scratch_code_start:
	.rept VIROS_SCRATCH_PAGE_SIZE / 4
	.word 0x0015940d /* classic MIPS32 break 0x5650 */
	.endr
	.globl __viros_scratch_code_end
__viros_scratch_code_end:
	.size __viros_scratch_code_start, \
		__viros_scratch_code_end - __viros_scratch_code_start
	.popsection

	.pushsection .bss.viros_scratch_data,"aw",%nobits
	.balign VIROS_SCRATCH_PAGE_SIZE
	.globl __viros_scratch_data_start
	.type __viros_scratch_data_start, %object
__viros_scratch_data_start:
	.skip VIROS_SCRATCH_PAGE_SIZE
	.globl __viros_scratch_data_end
__viros_scratch_data_end:
	.size __viros_scratch_data_start, \
		__viros_scratch_data_end - __viros_scratch_data_start
	.popsection

	.pushsection .bss.viros_scratch_stack,"aw",%nobits
	.balign VIROS_SCRATCH_PAGE_SIZE
	.globl __viros_scratch_stack_start
	.type __viros_scratch_stack_start, %object
__viros_scratch_stack_start:
	.skip VIROS_SCRATCH_PAGE_SIZE
	.globl __viros_scratch_stack_end
__viros_scratch_stack_end:
	.size __viros_scratch_stack_start, \
		__viros_scratch_stack_end - __viros_scratch_stack_start
	.popsection

	.set pop
	.section .note.GNU-stack,"",%progbits

#else
#error "viros scratch requires an AArch64 or MIPS kernel"
#endif
