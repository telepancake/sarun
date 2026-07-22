"""Translate target Linux signal numbers to GDB remote signal numbers.

GDB's remote protocol uses ``enum gdb_signal`` values, not necessarily the
numeric signal value used by the target kernel.  Keeping the target layout as
data makes the translation usable by each architecture-specific event
presenter without involving the host Python signal module.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Mapping


GDB_SIGNAL_UNKNOWN = 143


@dataclass(frozen=True)
class LinuxSignalLayout:
    """Architecture-specific Linux signal numbering relevant to GDB."""

    fixed: Mapping[int, int]
    realtime_min: int
    realtime_max: int
    realtime_name_min: int = 32


def _gdb_realtime_signal(signal_name_number: int) -> int:
    """Return GDB's non-contiguous enum value for a SIG32..SIG127 name."""

    if signal_name_number == 32:
        return 77
    if 33 <= signal_name_number <= 63:
        return signal_name_number + 12
    if 64 <= signal_name_number <= 127:
        return signal_name_number + 14
    return GDB_SIGNAL_UNKNOWN


def linux_signal_to_gdb(signal: int, layout: LinuxSignalLayout) -> int:
    """Translate one target signal according to ``layout``.

    Unknown numeric signals deliberately become GDB_SIGNAL_UNKNOWN instead of
    being passed through.  A pass-through would silently mislabel signals on
    targets such as MIPS, whose numbering differs above signal 15.
    """

    if isinstance(signal, bool) or not isinstance(signal, int):
        raise TypeError("Linux signal number must be an integer")

    fixed = layout.fixed.get(signal)
    if fixed is not None:
        return fixed
    if layout.realtime_min <= signal <= layout.realtime_max:
        name_number = layout.realtime_name_min + signal - layout.realtime_min
        return _gdb_realtime_signal(name_number)
    return GDB_SIGNAL_UNKNOWN


# Linux's asm-generic signal numbering, also used by x86, ARM, and AArch64.
# SIGSTKFLT (target number 16) has no GDB enum counterpart and is deliberately
# absent, so it is presented as GDB_SIGNAL_UNKNOWN rather than mislabeled.
STANDARD_LINUX_SIGNALS = LinuxSignalLayout(
    fixed={
        0: 0,
        1: 1,   # SIGHUP
        2: 2,   # SIGINT
        3: 3,   # SIGQUIT
        4: 4,   # SIGILL
        5: 5,   # SIGTRAP
        6: 6,   # SIGABRT
        7: 10,  # SIGBUS
        8: 8,   # SIGFPE
        9: 9,   # SIGKILL
        10: 30, # SIGUSR1
        11: 11, # SIGSEGV
        12: 31, # SIGUSR2
        13: 13, # SIGPIPE
        14: 14, # SIGALRM
        15: 15, # SIGTERM
        17: 20, # SIGCHLD
        18: 19, # SIGCONT
        19: 17, # SIGSTOP
        20: 18, # SIGTSTP
        21: 21, # SIGTTIN
        22: 22, # SIGTTOU
        23: 16, # SIGURG
        24: 24, # SIGXCPU
        25: 25, # SIGXFSZ
        26: 26, # SIGVTALRM
        27: 27, # SIGPROF
        28: 28, # SIGWINCH
        29: 23, # SIGIO/SIGPOLL; GDB chooses SIGIO
        30: 32, # SIGPWR
        31: 12, # SIGSYS
    },
    realtime_min=32,
    realtime_max=64,
)


def standard_linux_signal_to_gdb(signal: int) -> int:
    """Translate an x86/ARM/AArch64 Linux signal for an RSP stop reply."""

    return linux_signal_to_gdb(signal, STANDARD_LINUX_SIGNALS)


# Linux 5.6.3 arch/mips/include/uapi/asm/signal.h numbering, translated to
# GDB 17's enum gdb_signal values.  Signals 1..15 have matching values; the
# remainder require the explicit architecture table below.
MIPS_LINUX_SIGNALS = LinuxSignalLayout(
    fixed={
        **{signal: signal for signal in range(0, 16)},
        16: 30,  # SIGUSR1
        17: 31,  # SIGUSR2
        18: 20,  # SIGCHLD
        19: 32,  # SIGPWR
        20: 28,  # SIGWINCH
        21: 16,  # SIGURG
        22: 23,  # SIGIO/SIGPOLL; GDB chooses SIGIO
        23: 17,  # SIGSTOP
        24: 18,  # SIGTSTP
        25: 19,  # SIGCONT
        26: 21,  # SIGTTIN
        27: 22,  # SIGTTOU
        28: 26,  # SIGVTALRM
        29: 27,  # SIGPROF
        30: 24,  # SIGXCPU
        31: 25,  # SIGXFSZ
    },
    realtime_min=32,
    realtime_max=127,
)


def mips_linux_signal_to_gdb(signal: int) -> int:
    """Translate a Linux MIPS signal to the value used in an RSP stop reply."""

    return linux_signal_to_gdb(signal, MIPS_LINUX_SIGNALS)
