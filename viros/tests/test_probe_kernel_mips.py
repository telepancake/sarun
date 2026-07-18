from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[1]


class ProbeKernelMipsTests(unittest.TestCase):
    def test_mips_object_is_classic_o32_code(self):
        kbuild = (ROOT / "probe/kernel/Kbuild").read_text(encoding="utf-8")
        self.assertIn("ifeq ($(CONFIG_MIPS),y)", kbuild)
        self.assertIn("-mabi=32", kbuild)
        self.assertIn("-mno-mips16", kbuild)
        self.assertIn("-mno-micromips", kbuild)

    def test_mips_completion_symbol_is_exactly_one_fixed_instruction(self):
        source = (ROOT / "probe/kernel/viros_probe.c").read_text(
            encoding="utf-8"
        )
        mips = source.split("#elif defined(CONFIG_MIPS)", 1)[1]
        completion = mips.split("#endif", 1)[0]
        self.assertIn('".set nomips16\\n"', completion)
        self.assertIn('".set nomicromips\\n"', completion)
        break_5650 = (0x5650 << 6) | 0x0D
        self.assertEqual(break_5650, 0x0015940D)
        self.assertIn('".word 0x0015940d\\n"', completion)
        self.assertIn(
            '".size viros_probe_complete, .-viros_probe_complete\\n"',
            completion,
        )

    def test_mips_response_advertises_target_snapshot_metadata(self):
        source = (ROOT / "probe/kernel/viros_probe.c").read_text(
            encoding="utf-8"
        )
        header = (ROOT / "probe/include/viros_probe_abi.h").read_text(
            encoding="utf-8"
        )
        self.assertIn("response->arch = VIROS_PROBE_ARCH_MIPS;", source)
        self.assertIn(
            "defined(CONFIG_MIPS) && defined(CONFIG_CPU_LITTLE_ENDIAN)",
            source,
        )
        self.assertIn(
            "defined(CONFIG_MIPS) && defined(CONFIG_CPU_BIG_ENDIAN)", source
        )
        self.assertIn("response->pointer_bits = 32;", source)
        self.assertIn("static_assert(sizeof(void *) * 8 == 32);", header)

    def test_non_snapshot_operations_stay_arch64_only(self):
        source = (ROOT / "probe/kernel/viros_probe.c").read_text(
            encoding="utf-8"
        )
        self.assertEqual(source.count("return viros_translate_va("), 1)
        self.assertEqual(source.count("return viros_saved_regs("), 1)
        self.assertGreaterEqual(
            source.count("response->status = VIROS_PROBE_UNSUPPORTED;"), 2
        )


if __name__ == "__main__":
    unittest.main()
