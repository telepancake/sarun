/*
 * sud/raw.c — Assembly trampolines and global data for raw syscall infrastructure.
 *
 * Contains symbols with external linkage that must exist in exactly one
 * translation unit: clone trampolines, the sigreturn restorer, and the
 * signal-safe arena allocator buffer.
 */

#include "sud/libc.h"
#include "sud/raw.h"

/* ================================================================
 * rt_sigreturn restorer
 * ================================================================ */
#if defined(__x86_64__)
__asm__(
    "    .text\n"
    "    .globl sud_rt_sigreturn_restorer\n"
    "    .type sud_rt_sigreturn_restorer, @function\n"
    "sud_rt_sigreturn_restorer:\n"
    "    mov  $" STR(SYS_rt_sigreturn) ", %eax\n"
    "    syscall\n"
    "    hlt\n"
    "    .size sud_rt_sigreturn_restorer, .-sud_rt_sigreturn_restorer\n"
);
#else
__asm__(
    "    .text\n"
    "    .globl sud_rt_sigreturn_restorer\n"
    "    .type sud_rt_sigreturn_restorer, @function\n"
    "sud_rt_sigreturn_restorer:\n"
    "    mov  $" STR(SYS_rt_sigreturn) ", %eax\n"
    "    int  $0x80\n"
    "    hlt\n"
    "    .size sud_rt_sigreturn_restorer, .-sud_rt_sigreturn_restorer\n"
);

/*
 * Legacy (non-SA_SIGINFO) sigreturn restorer for i386.
 *
 * On i386 the kernel chooses the signal frame layout based on
 * SA_SIGINFO: handlers installed *with* SA_SIGINFO get a struct
 * rt_sigframe and must return through SYS_rt_sigreturn (173); handlers
 * installed *without* SA_SIGINFO get the legacy struct sigframe and
 * must return through SYS_sigreturn (119).  Calling the wrong sigreturn
 * makes the kernel parse the frame at the wrong offsets and load
 * garbage into the user registers (typically EIP=ESP=0 with random
 * segment selectors → instant SI_KERNEL SIGSEGV on iret).
 *
 * x86_64 has only the rt sigframe / rt_sigreturn flavour, so this
 * helper is i386-only.
 */
__asm__(
    "    .text\n"
    "    .globl sud_sigreturn_restorer\n"
    "    .type sud_sigreturn_restorer, @function\n"
    "sud_sigreturn_restorer:\n"
    "    pop  %eax\n"          /* discard signum the kernel pushed   */
    "    mov  $" STR(SYS_sigreturn) ", %eax\n"
    "    int  $0x80\n"
    "    hlt\n"
    "    .size sud_sigreturn_restorer, .-sud_sigreturn_restorer\n"
);
#endif

/* ================================================================
 * Clone trampolines
 * ================================================================ */
#if defined(__x86_64__)

__asm__(
    "    .text\n"
    "    .globl clone3_raw\n"
    "    .type clone3_raw, @function\n"
    "clone3_raw:\n"
    "    push %r12\n"
    "    push %r13\n"
    "    mov  %rdx, %r12\n"
    "    sub  $8, %rsp\n"
    "    movq $0, (%rsp)\n"
    "    mov  %rsp, %r13\n"
    "    mov  $435, %eax\n"
    "    syscall\n"
    "    test %rax, %rax\n"
    "    jz   .Lc3_child\n"
    "    js   .Lc3_done\n"
    ".Lc3_spin:\n"
    "    pause\n"
    "    cmpq $0, (%r13)\n"
    "    je   .Lc3_spin\n"
    ".Lc3_done:\n"
    "    add  $8, %rsp\n"
    "    pop  %r13\n"
    "    pop  %r12\n"
    "    ret\n"
    "\n"
    ".Lc3_child:\n"
    "    mov  168(%r12), %rax\n"
    "    push %rax\n"
    "    mov  136(%r12), %rax\n"
    "    push %rax\n"
    "    mov  128(%r12), %rax\n"
    "    push %rax\n"
    "    mov  120(%r12), %rax\n"
    "    push %rax\n"
    "    mov  112(%r12), %rax\n"
    "    push %rax\n"
    "    mov  104(%r12), %rax\n"
    "    push %rax\n"
    "    mov  96(%r12),  %rax\n"
    "    push %rax\n"
    "    mov  88(%r12),  %rax\n"
    "    push %rax\n"
    "    mov  80(%r12),  %rax\n"
    "    push %rax\n"
    "    mov  72(%r12),  %rax\n"
    "    push %rax\n"
    "    mov  64(%r12),  %rax\n"
    "    push %rax\n"
    "    mov  56(%r12),  %rax\n"
    "    push %rax\n"
    "    mov  48(%r12),  %rax\n"
    "    push %rax\n"
    "    mov  40(%r12),  %rax\n"
    "    push %rax\n"
    "\n"
    "    movq $1, (%r13)\n"
    "    call prepare_child_sud\n"
    "\n"
    "    pop  %r8\n"
    "    pop  %r9\n"
    "    pop  %r10\n"
    "    pop  %r11\n"
    "    pop  %r12\n"
    "    pop  %r13\n"
    "    pop  %r14\n"
    "    pop  %r15\n"
    "    pop  %rdi\n"
    "    pop  %rsi\n"
    "    pop  %rbp\n"
    "    pop  %rbx\n"
    "    pop  %rdx\n"
    "    pop  %rcx\n"
    "    xor  %eax, %eax\n"
    "    jmp  *%rcx\n"
    "    .size clone3_raw, .-clone3_raw\n"
);

__asm__(
    "    .text\n"
    "    .globl clone_raw\n"
    "    .type clone_raw, @function\n"
    "clone_raw:\n"
    "    push %r12\n"
    "    push %r13\n"
    "    mov  %r9, %r12\n"
    "    sub  $8, %rsp\n"
    "    movq $0, (%rsp)\n"
    "    mov  %rsp, %r13\n"
    "    mov  %rcx, %r10\n"
    "    mov  $56, %eax\n"
    "    syscall\n"
    "    test %rax, %rax\n"
    "    jz   .Lcl_child\n"
    "    js   .Lcl_done\n"
    ".Lcl_spin:\n"
    "    pause\n"
    "    cmpq $0, (%r13)\n"
    "    je   .Lcl_spin\n"
    ".Lcl_done:\n"
    "    add  $8, %rsp\n"
    "    pop  %r13\n"
    "    pop  %r12\n"
    "    ret\n"
    "\n"
    ".Lcl_child:\n"
    "    mov  168(%r12), %rax\n"
    "    push %rax\n"
    "    mov  136(%r12), %rax\n"
    "    push %rax\n"
    "    mov  128(%r12), %rax\n"
    "    push %rax\n"
    "    mov  120(%r12), %rax\n"
    "    push %rax\n"
    "    mov  112(%r12), %rax\n"
    "    push %rax\n"
    "    mov  104(%r12), %rax\n"
    "    push %rax\n"
    "    mov  96(%r12),  %rax\n"
    "    push %rax\n"
    "    mov  88(%r12),  %rax\n"
    "    push %rax\n"
    "    mov  80(%r12),  %rax\n"
    "    push %rax\n"
    "    mov  72(%r12),  %rax\n"
    "    push %rax\n"
    "    mov  64(%r12),  %rax\n"
    "    push %rax\n"
    "    mov  56(%r12),  %rax\n"
    "    push %rax\n"
    "    mov  48(%r12),  %rax\n"
    "    push %rax\n"
    "    mov  40(%r12),  %rax\n"
    "    push %rax\n"
    "    movq $1, (%r13)\n"
    "    call prepare_child_sud\n"
    "    pop  %r8\n"
    "    pop  %r9\n"
    "    pop  %r10\n"
    "    pop  %r11\n"
    "    pop  %r12\n"
    "    pop  %r13\n"
    "    pop  %r14\n"
    "    pop  %r15\n"
    "    pop  %rdi\n"
    "    pop  %rsi\n"
    "    pop  %rbp\n"
    "    pop  %rbx\n"
    "    pop  %rdx\n"
    "    pop  %rcx\n"
    "    xor  %eax, %eax\n"
    "    jmp  *%rcx\n"
    "    .size clone_raw, .-clone_raw\n"
);

_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_R8])  == 40,  "R8 offset");
_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_R9])  == 48,  "R9 offset");
_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_R10]) == 56,  "R10 offset");
_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_R11]) == 64,  "R11 offset");
_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_R12]) == 72,  "R12 offset");
_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_R13]) == 80,  "R13 offset");
_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_R14]) == 88,  "R14 offset");
_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_R15]) == 96,  "R15 offset");
_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_RDI]) == 104, "RDI offset");
_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_RSI]) == 112, "RSI offset");
_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_RBP]) == 120, "RBP offset");
_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_RBX]) == 128, "RBX offset");
_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_RDX]) == 136, "RDX offset");
_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_RAX]) == 144, "RAX offset");
_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_RIP]) == 168, "RIP offset");

#else /* i386 */

volatile ucontext_t *g_clone_uc_i386;
volatile int g_clone_sync_i386;
volatile int g_clone_lock_i386;

__asm__(
    "    .text\n"
    "    .globl clone_raw_impl\n"
    "    .type clone_raw_impl, @function\n"
    "clone_raw_impl:\n"
    "    push %ebp\n"
    "    mov  %esp, %ebp\n"
    "    push %edi\n"
    "    push %esi\n"
    "    push %ebx\n"
    "    mov  $120, %eax\n"
    "    mov  8(%ebp), %ebx\n"
    "    mov  12(%ebp), %ecx\n"
    "    mov  16(%ebp), %edx\n"
    "    mov  20(%ebp), %esi\n"
    "    mov  24(%ebp), %edi\n"
    "    int  $0x80\n"
    "    test %eax, %eax\n"
    "    jnz  .Lcl_i386_parent\n"
    "    mov  g_clone_uc_i386, %ebp\n"
    "    mov  76(%ebp), %eax\n"
    "    push %eax\n"
    "    mov  56(%ebp), %eax\n"
    "    push %eax\n"
    "    mov  52(%ebp), %eax\n"
    "    push %eax\n"
    "    mov  44(%ebp), %eax\n"
    "    push %eax\n"
    "    mov  40(%ebp), %eax\n"
    "    push %eax\n"
    "    mov  36(%ebp), %eax\n"
    "    push %eax\n"
    "    movl $1, g_clone_sync_i386\n"
    "    call prepare_child_sud\n"
    "    pop  %edi\n"
    "    pop  %esi\n"
    "    pop  %ebp\n"
    "    pop  %ebx\n"
    "    pop  %edx\n"
    "    pop  %ecx\n"
    "    xor  %eax, %eax\n"
    "    jmp  *%ecx\n"
    ".Lcl_i386_parent:\n"
    "    pop  %ebx\n"
    "    pop  %esi\n"
    "    pop  %edi\n"
    "    pop  %ebp\n"
    "    ret\n"
    "    .size clone_raw_impl, .-clone_raw_impl\n"
);

_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_EDI]) == 36, "EDI offset");
_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_ESI]) == 40, "ESI offset");
_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_EBP]) == 44, "EBP offset");
_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_ESP]) == 48, "ESP offset");
_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_EBX]) == 52, "EBX offset");
_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_EDX]) == 56, "EDX offset");
_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_ECX]) == 60, "ECX offset");
_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_EAX]) == 64, "EAX offset");
_Static_assert(__builtin_offsetof(ucontext_t, uc_mcontext.gregs[REG_EIP]) == 76, "EIP offset");

#endif

/* ================================================================
 * Recent-syscalls ring buffer — definition; declared in handler.h.
 *
 * Initialised so every entry's nr is -1 ("unused") so the crash
 * dumper can suppress unused slots when the program crashes very
 * early before the ring has wrapped.
 * ================================================================ */
#include "sud/handler.h"
struct sud_syslog_entry g_sud_syslog[SUD_SYSLOG_SIZE] = {
    [0 ... SUD_SYSLOG_SIZE - 1] = { -1, 0, 0, 0 }
};
volatile unsigned int g_sud_syslog_head = 0;
