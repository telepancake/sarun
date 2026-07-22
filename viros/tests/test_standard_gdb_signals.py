import unittest

from inferiors.gdb_signals import (
    GDB_SIGNAL_UNKNOWN,
    STANDARD_LINUX_SIGNALS,
    linux_signal_to_gdb,
    standard_linux_signal_to_gdb,
)


class StandardLinuxSignalMappingTests(unittest.TestCase):
    def test_every_non_realtime_remap_matches_gdbs_enum(self):
        remapped = {
            7: 10,   # SIGBUS
            10: 30,  # SIGUSR1
            12: 31,  # SIGUSR2
            17: 20,  # SIGCHLD
            18: 19,  # SIGCONT
            19: 17,  # SIGSTOP
            20: 18,  # SIGTSTP
            23: 16,  # SIGURG
            29: 23,  # SIGIO
            30: 32,  # SIGPWR
            31: 12,  # SIGSYS
        }
        for target_signal, gdb_signal in remapped.items():
            with self.subTest(target_signal=target_signal):
                self.assertEqual(
                    standard_linux_signal_to_gdb(target_signal), gdb_signal
                )

    def test_unchanged_non_realtime_signals_and_zero_remain_stable(self):
        unchanged = {
            0, 1, 2, 3, 4, 5, 6, 8, 9, 11, 13, 14, 15,
            21, 22, 24, 25, 26, 27, 28,
        }
        for signal in unchanged:
            with self.subTest(signal=signal):
                self.assertEqual(standard_linux_signal_to_gdb(signal), signal)

    def test_sigstkflt_has_no_gdb_signal_number(self):
        self.assertNotIn(16, STANDARD_LINUX_SIGNALS.fixed)
        self.assertEqual(
            standard_linux_signal_to_gdb(16), GDB_SIGNAL_UNKNOWN
        )

    def test_complete_realtime_32_through_64_range(self):
        expected = {32: 77, 64: 78}
        expected.update({signal: signal + 12 for signal in range(33, 64)})
        for target_signal, gdb_signal in expected.items():
            with self.subTest(target_signal=target_signal):
                self.assertEqual(
                    standard_linux_signal_to_gdb(target_signal), gdb_signal
                )

    def test_numbers_outside_layout_are_unknown_and_types_are_checked(self):
        for signal in (-1, 65, 127):
            with self.subTest(signal=signal):
                self.assertEqual(
                    linux_signal_to_gdb(signal, STANDARD_LINUX_SIGNALS),
                    GDB_SIGNAL_UNKNOWN,
                )
        for invalid in (True, 11.0, "11"):
            with self.subTest(invalid=invalid):
                with self.assertRaises(TypeError):
                    standard_linux_signal_to_gdb(invalid)


if __name__ == "__main__":
    unittest.main()
