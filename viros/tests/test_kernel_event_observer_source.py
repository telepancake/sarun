from pathlib import Path
import shutil
import subprocess
import textwrap
import unittest


ROOT = Path(__file__).resolve().parents[1]
HEADER = ROOT / "probe/events/include/viros_event_abi.h"
SOURCE = ROOT / "probe/events/kernel/viros_event.c"
KBUILD = ROOT / "probe/events/kernel/Kbuild"


class KernelEventObserverSourceTests(unittest.TestCase):
    def test_abi_header_has_the_frozen_native_layout(self):
        compiler = shutil.which("cc")
        if compiler is None:
            self.skipTest("a C compiler is not available")
        source = textwrap.dedent(
            """
            #include <stddef.h>
            #include "viros_event_abi.h"
            _Static_assert(VIROS_EVENT_MAGIC == 0x56455231U, "magic");
            _Static_assert(VIROS_EVENT_ABI_MAJOR == 1, "major");
            _Static_assert(VIROS_EVENT_ABI_MINOR == 0, "minor");
            _Static_assert(VIROS_EVENT_ENDIAN_LITTLE == 1, "little endian");
            _Static_assert(VIROS_EVENT_ENDIAN_BIG == 2, "big endian");
            _Static_assert(VIROS_EVENT_ARCH_AARCH64 == 1, "aarch64 arch");
            _Static_assert(VIROS_EVENT_ARCH_ARM == 2, "arm arch");
            _Static_assert(VIROS_EVENT_ARCH_MIPS == 3, "mips arch");
            _Static_assert(VIROS_EVENT_ARCH_X86 == 4, "x86 arch");
            _Static_assert(VIROS_EVENT_USER_SIGNAL == 1, "signal kind");
            _Static_assert(VIROS_EVENT_KERNEL_DIE == 2, "kernel die kind");
            _Static_assert(VIROS_EVENT_REGS_VALID == 1, "valid flag");
            _Static_assert(VIROS_EVENT_REGS_USER == 2, "user flag");
            _Static_assert(VIROS_EVENT_REGS_COMPAT == 4, "compat flag");
            _Static_assert(VIROS_EVENT_ADDRESS_VALID == 8, "address flag");
            _Static_assert(offsetof(struct viros_event_v1, arch) == 8,
                           "arch offset");
            _Static_assert(offsetof(struct viros_event_v1, record_size) == 16,
                           "record size offset");
            _Static_assert(offsetof(struct viros_event_v1, code) == 24,
                           "code offset");
            _Static_assert(offsetof(struct viros_event_v1, flags) == 28,
                           "flags offset");
            _Static_assert(offsetof(struct viros_event_v1, sequence) == 48,
                           "sequence offset");
            _Static_assert(offsetof(struct viros_event_v1, comm) == 112,
                           "comm offset");
            _Static_assert(offsetof(struct viros_event_v1, registers) == 128,
                           "header size");
            _Static_assert(sizeof(struct viros_event_v1) == 640, "max size");
            _Static_assert(VIROS_EVENT_ARM_REGISTER_COUNT == 17, "arm regs");
            _Static_assert(VIROS_EVENT_AARCH64_REGISTER_COUNT == 34,
                           "aarch64 regs");
            _Static_assert(VIROS_EVENT_MIPS_R0 == 0, "mips r0");
            _Static_assert(VIROS_EVENT_MIPS_R31 == 31, "mips r31");
            _Static_assert(VIROS_EVENT_MIPS_STATUS == 32, "mips status");
            _Static_assert(VIROS_EVENT_MIPS_LO == 33, "mips lo");
            _Static_assert(VIROS_EVENT_MIPS_HI == 34, "mips hi");
            _Static_assert(VIROS_EVENT_MIPS_BADVADDR == 35, "mips badvaddr");
            _Static_assert(VIROS_EVENT_MIPS_CAUSE == 36, "mips cause");
            _Static_assert(VIROS_EVENT_MIPS_PC == 37, "mips pc");
            _Static_assert(VIROS_EVENT_MIPS_REGISTER_COUNT == 38,
                           "mips regs");
            _Static_assert(VIROS_EVENT_X86_R15 == 0, "x86 r15");
            _Static_assert(VIROS_EVENT_X86_RAX == 10, "x86 rax");
            _Static_assert(VIROS_EVENT_X86_ORIG_RAX == 15,
                           "x86 orig_rax");
            _Static_assert(VIROS_EVENT_X86_RIP == 16, "x86 rip");
            _Static_assert(VIROS_EVENT_X86_CS == 17, "x86 cs");
            _Static_assert(VIROS_EVENT_X86_EFLAGS == 18, "x86 eflags");
            _Static_assert(VIROS_EVENT_X86_RSP == 19, "x86 rsp");
            _Static_assert(VIROS_EVENT_X86_SS == 20, "x86 ss");
            _Static_assert(VIROS_EVENT_X86_REGISTER_COUNT == 21,
                           "x86 regs");
            int main(void) { return 0; }
            """
        )
        subprocess.run(
            [
                compiler,
                "-std=c11",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-fsyntax-only",
                "-I",
                str(HEADER.parent),
                "-x",
                "c",
                "-",
            ],
            input=source,
            text=True,
            check=True,
            capture_output=True,
        )

    def test_observer_uses_the_exact_signal_delivery_boundary(self):
        source = SOURCE.read_text(encoding="utf-8")
        self.assertIn(
            "register_trace_signal_deliver(viros_signal_deliver, NULL)", source
        )
        self.assertIn("core_initcall(viros_event_init);", source)
        self.assertIn("viros_event_stop(record);", source)
        self.assertIn("noinline notrace", source)
        self.assertIn('asm volatile("" : : "r" (record) : "memory")', source)

    def test_observer_selects_default_fatal_userspace_events(self):
        source = SOURCE.read_text(encoding="utf-8")
        for condition in (
            "ka->sa.sa_handler != SIG_DFL",
            "sig_kernel_ignore(sig)",
            "sig_kernel_stop(sig)",
            "SIGNAL_UNKILLABLE",
            "sig_kernel_only(sig)",
            "signal_group_exit(current->signal)",
            "current->signal->group_exit_code & 0x7f",
            "!current->mm",
            "!user_mode(regs)",
        ):
            with self.subTest(condition=condition):
                self.assertIn(condition, source)

    def test_register_frames_match_gdb_core_order(self):
        source = SOURCE.read_text(encoding="utf-8")
        self.assertIn("record->registers[i] = (ve_u32)regs->uregs[i];", source)
        self.assertIn("VIROS_EVENT_ARM_REGISTER_COUNT", source)
        self.assertIn("record->registers[i] = regs->regs[i];", source)
        self.assertIn("[VIROS_EVENT_AARCH64_SP] = regs->sp;", source)
        self.assertIn("[VIROS_EVENT_AARCH64_PC] = regs->pc;", source)
        self.assertIn("[VIROS_EVENT_AARCH64_PSTATE] = regs->pstate;", source)
        self.assertIn("if (compat_user_mode(regs))", source)

    def test_mips32_frame_uses_saved_user_registers_and_marks_k0_k1_unknown(self):
        source = SOURCE.read_text(encoding="utf-8")
        self.assertIn("defined(CONFIG_MIPS) && !defined(CONFIG_32BIT)", source)
        self.assertIn("regs = task_pt_regs(current);", source)
        self.assertIn(
            "record->registers[i] = (ve_u32)regs->regs[i];", source
        )
        for field in (
            "cp0_status",
            "regs->lo",
            "regs->hi",
            "cp0_badvaddr",
            "cp0_cause",
            "cp0_epc",
        ):
            with self.subTest(field=field):
                self.assertIn(field, source)
        self.assertIn("record->registers[26] = 0;", source)
        self.assertIn("record->registers[27] = 0;", source)
        self.assertIn("~((1ULL << 26) | (1ULL << 27))", source)
        self.assertIn("return VIROS_EVENT_ARCH_MIPS;", source)

    def test_x86_frame_preserves_native_and_ia32_pt_regs_contract(self):
        source = SOURCE.read_text(encoding="utf-8")
        for field in (
            "regs->r15",
            "regs->bp",
            "regs->ax",
            "regs->orig_ax",
            "regs->ip",
            "regs->cs",
            "regs->flags",
            "regs->sp",
            "regs->ss",
        ):
            with self.subTest(field=field):
                self.assertIn(field, source)
        self.assertIn("compat = !user_64bit_mode(regs);", source)
        self.assertIn("record->flags |= VIROS_EVENT_REGS_COMPAT;", source)
        self.assertIn("return compat ? (ve_u32)value : (ve_u64)value;", source)
        self.assertIn("record->registers[VIROS_EVENT_X86_R15] = 0;", source)
        self.assertIn("return VIROS_EVENT_ARCH_X86;", source)

    def test_kernel_die_notifier_preserves_the_architecture_exception_frame(self):
        source = SOURCE.read_text(encoding="utf-8")
        self.assertIn("register_die_notifier(&viros_die_notifier)", source)
        self.assertIn("reason != DIE_OOPS", source)
        self.assertIn("user_mode(args->regs)", source)
        self.assertIn(
            "viros_event_publish(args->regs, VIROS_EVENT_KERNEL_DIE", source
        )
        self.assertIn("instruction_pointer(args->regs)", source)
        self.assertIn("return NOTIFY_DONE;", source)
        self.assertNotIn("panic_notifier_list", source)

    def test_callback_path_is_preallocated_and_quiet(self):
        source = SOURCE.read_text(encoding="utf-8")
        self.assertIn("DEFINE_PER_CPU(struct viros_event_storage", source)
        self.assertIn("this_cpu_ptr(&viros_event_storage)", source)
        for forbidden in (
            "kmalloc(",
            "kzalloc(",
            "vmalloc(",
            "printk(",
            "pr_info(",
            "schedule(",
            "mutex_lock(",
            "spin_lock(",
        ):
            with self.subTest(forbidden=forbidden):
                self.assertNotIn(forbidden, source)

    def test_kbuild_makes_a_builtin_uninstrumented_object(self):
        kbuild = KBUILD.read_text(encoding="utf-8")
        self.assertIn("obj-y += viros_event.o", kbuild)
        self.assertNotIn("obj-m", kbuild)
        for setting in (
            "KASAN_SANITIZE_viros_event.o := n",
            "KCSAN_SANITIZE_viros_event.o := n",
            "KCOV_INSTRUMENT_viros_event.o := n",
            "GCOV_PROFILE_viros_event.o := n",
            "UBSAN_SANITIZE_viros_event.o := n",
        ):
            with self.subTest(setting=setting):
                self.assertIn(setting, kbuild)


if __name__ == "__main__":
    unittest.main()
