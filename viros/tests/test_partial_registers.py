import unittest

from inferiors.partial_registers import (
    AARCH64_USER_REGISTERS,
    Aarch64PartialRegisterLayout,
    PartialRegisterError,
)


def core_xml(*, explicit=True, bad_bits=None):
    registers = []
    for regnum, name in enumerate(AARCH64_USER_REGISTERS):
        bits = bad_bits if name == "cpsr" and bad_bits is not None else (
            32 if name == "cpsr" else 64
        )
        number = f' regnum="{regnum}"' if explicit else ""
        registers.append(
            f'<reg name="{name}" bitsize="{bits}"{number}/>'
        )
    return ("<feature>" + "".join(registers) + "</feature>").encode()


def values():
    result = {f"x{number}": number for number in range(31)}
    result.update(sp=0x0102030405060708, pc=0x1122334455667788, cpsr=0xA1B2C3D4)
    return result


CORE_G_BYTES = 31 * 8 + 8 + 8 + 4


class PartialRegisterLayoutTests(unittest.TestCase):
    def descriptions(self, *, core=None, extra=b""):
        return {
            b"target.xml": (
                b'<target xmlns:xi="http://www.w3.org/2001/XInclude">'
                b"<architecture>aarch64</architecture>"
                b'<xi:include href="features/core.xml"/>'
                + (b'<xi:include href="features/extra.xml"/>' if extra else b"")
                + b"</target>"
            ),
            b"features/core.xml": core or core_xml(),
            **({b"features/extra.xml": extra} if extra else {}),
        }

    def test_encodes_only_observed_core_prefix(self):
        extra = (
            b'<feature><reg name="fpsr" bitsize="32"/>'
            b'<reg name="fpcr" bitsize="32"/>'
            b'<reg name="tag_ctl" bitsize="64"/></feature>'
        )
        layout = Aarch64PartialRegisterLayout.from_target_descriptions(
            self.descriptions(extra=extra), byte_order="little",
            observed_g_bytes=CORE_G_BYTES,
        )
        payload = layout.encode_g_packet(values())

        expected_core = b"".join(
            values()[name]
            .to_bytes(4 if name == "cpsr" else 8, "little")
            .hex()
            .encode()
            for name in AARCH64_USER_REGISTERS
        )
        self.assertEqual(payload, expected_core)
        self.assertEqual(len(layout.registers), len(AARCH64_USER_REGISTERS))

    def test_big_endian_is_explicit_and_host_independent(self):
        layout = Aarch64PartialRegisterLayout.from_target_descriptions(
            self.descriptions(), byte_order="big",
            observed_g_bytes=CORE_G_BYTES,
        )
        payload = layout.encode_g_packet(values())
        x0_offset = 0
        sp_offset = 31 * 16
        cpsr_offset = (31 + 2) * 16
        self.assertEqual(payload[x0_offset:x0_offset + 16], b"0000000000000000")
        self.assertEqual(payload[sp_offset:sp_offset + 16], b"0102030405060708")
        self.assertEqual(payload[cpsr_offset:cpsr_offset + 8], b"a1b2c3d4")

    def test_nested_include_order_assigns_implicit_regnums_globally(self):
        descriptions = {
            "target.xml": (
                '<target xmlns:xi="http://www.w3.org/2001/XInclude">'
                "<architecture>aarch64</architecture>"
                '<xi:include href="set/first.xml"/>'
                "</target>"
            ),
            "set/first.xml": (
                '<feature xmlns:xi="http://www.w3.org/2001/XInclude">'
                '<xi:include href="../core.xml"/>'
                '<reg name="after_core" bitsize="16"/>'
                "</feature>"
            ),
            "core.xml": core_xml(explicit=False),
        }
        layout = Aarch64PartialRegisterLayout.from_target_descriptions(
            descriptions, byte_order="little", observed_g_bytes=CORE_G_BYTES,
        )
        self.assertEqual(layout.registers[-1].name, "cpsr")
        self.assertEqual(layout.registers[-1].regnum, 33)
        self.assertEqual(len(layout.encode_g_packet(values())), CORE_G_BYTES * 2)

    def test_sparse_regnums_order_described_registers_without_gap_padding(self):
        # Remote protocol positions are sorted by protocol register number;
        # absent numbers have no described size and therefore no packet bytes.
        extra = b'<feature><reg name="far_system" bitsize="24" regnum="99"/></feature>'
        layout = Aarch64PartialRegisterLayout.from_target_descriptions(
            self.descriptions(extra=extra), byte_order="little",
            observed_g_bytes=CORE_G_BYTES,
        )
        self.assertEqual(layout.registers[-1].regnum, 33)
        payload = layout.encode_g_packet(values())
        self.assertEqual(len(payload), CORE_G_BYTES * 2)

    def test_explicit_regnum_controls_packet_order_not_xml_order(self):
        descriptions = self.descriptions(
            extra=b'<feature><reg name="early_system" bitsize="16" regnum="0"/></feature>'
        )
        # Move every core register up one so that the later XML include owns
        # protocol position zero and packet sorting has visible work to do.
        shifted = []
        for number, name in enumerate(AARCH64_USER_REGISTERS, 1):
            bits = 32 if name == "cpsr" else 64
            shifted.append(
                f'<reg name="{name}" bitsize="{bits}" regnum="{number}"/>'
            )
        descriptions[b"features/core.xml"] = (
            "<feature>" + "".join(shifted) + "</feature>"
        ).encode()
        # Put the system include first in XML.  Packet order still follows
        # regnum and begins with its two unavailable bytes.
        descriptions[b"target.xml"] = (
            b'<target xmlns:xi="http://www.w3.org/2001/XInclude">'
            b"<architecture>aarch64</architecture>"
            b'<xi:include href="features/extra.xml"/>'
            b'<xi:include href="features/core.xml"/>'
            b"</target>"
        )
        layout = Aarch64PartialRegisterLayout.from_target_descriptions(
            descriptions, byte_order="little",
            observed_g_bytes=CORE_G_BYTES + 2,
        )
        self.assertEqual(layout.registers[0].name, "early_system")
        self.assertTrue(layout.encode_g_packet(values()).startswith(b"xxxx"))

    def test_qemu_unbound_xi_prefix_is_understood_like_its_dtd(self):
        descriptions = self.descriptions()
        descriptions[b"target.xml"] = (
            b'<?xml version="1.0"?><!DOCTYPE target SYSTEM "gdb-target.dtd">'
            b"<target><architecture>aarch64</architecture>"
            b'<xi:include href="features/core.xml"/></target>'
        )
        layout = Aarch64PartialRegisterLayout.from_target_descriptions(
            descriptions, byte_order="little", observed_g_bytes=CORE_G_BYTES,
        )
        self.assertEqual(len(layout.registers), 34)

    def test_rejects_duplicate_name_and_duplicate_number(self):
        duplicate_name = core_xml()[:-10] + b'<reg name="x0" bitsize="64"/></feature>'
        with self.assertRaisesRegex(PartialRegisterError, "duplicate.*name"):
            Aarch64PartialRegisterLayout.from_target_descriptions(
                self.descriptions(core=duplicate_name), byte_order="little",
                observed_g_bytes=CORE_G_BYTES,
            )

        duplicate_number = core_xml()[:-10] + b'<reg name="other" bitsize="64" regnum="0"/></feature>'
        with self.assertRaisesRegex(PartialRegisterError, "duplicate.*number"):
            Aarch64PartialRegisterLayout.from_target_descriptions(
                self.descriptions(core=duplicate_number), byte_order="little",
                observed_g_bytes=CORE_G_BYTES,
            )

    def test_rejects_missing_or_wrong_width_required_register(self):
        missing = core_xml().replace(b'<reg name="pc" bitsize="64" regnum="32"/>', b"")
        with self.assertRaisesRegex(PartialRegisterError, "lacks required.*pc"):
            Aarch64PartialRegisterLayout.from_target_descriptions(
                self.descriptions(core=missing), byte_order="little",
                observed_g_bytes=CORE_G_BYTES - 8,
            )

        with self.assertRaisesRegex(PartialRegisterError, "cpsr.*16 bits"):
            Aarch64PartialRegisterLayout.from_target_descriptions(
                self.descriptions(core=core_xml(bad_bits=16)), byte_order="little",
                observed_g_bytes=CORE_G_BYTES - 2,
            )

    def test_rejects_non_byte_sized_extra_register(self):
        extra = b'<feature><reg name="odd" bitsize="7"/></feature>'
        with self.assertRaisesRegex(PartialRegisterError, "non-byte-sized"):
            Aarch64PartialRegisterLayout.from_target_descriptions(
                self.descriptions(extra=extra), byte_order="little",
                observed_g_bytes=CORE_G_BYTES,
            )

    def test_rejects_non_boundary_short_and_oversize_observations(self):
        descriptions = self.descriptions(
            extra=b'<feature><reg name="system" bitsize="64"/></feature>'
        )
        with self.assertRaisesRegex(PartialRegisterError, "ends inside"):
            Aarch64PartialRegisterLayout.from_target_descriptions(
                descriptions, byte_order="little",
                observed_g_bytes=CORE_G_BYTES - 1,
            )
        with self.assertRaisesRegex(PartialRegisterError, "prefix lacks.*cpsr"):
            Aarch64PartialRegisterLayout.from_target_descriptions(
                descriptions, byte_order="little",
                observed_g_bytes=CORE_G_BYTES - 4,
            )
        with self.assertRaisesRegex(PartialRegisterError, "exceeds"):
            Aarch64PartialRegisterLayout.from_target_descriptions(
                descriptions, byte_order="little",
                observed_g_bytes=CORE_G_BYTES + 9,
            )
        with self.assertRaisesRegex(PartialRegisterError, "positive integer"):
            Aarch64PartialRegisterLayout.from_target_descriptions(
                descriptions, byte_order="little", observed_g_bytes=True,
            )

    def test_rejects_incomplete_or_invalid_supplied_values(self):
        layout = Aarch64PartialRegisterLayout.from_target_descriptions(
            self.descriptions(), byte_order="little",
            observed_g_bytes=CORE_G_BYTES,
        )
        incomplete = values()
        incomplete.pop("pc")
        with self.assertRaisesRegex(PartialRegisterError, "lack.*pc"):
            layout.encode_g_packet(incomplete)
        with self.assertRaisesRegex(PartialRegisterError, "unknown names.*typo"):
            layout.encode_g_packet({**values(), "typo": 1})
        with self.assertRaisesRegex(PartialRegisterError, "invalid 32-bit.*cpsr"):
            layout.encode_g_packet({**values(), "cpsr": 1 << 32})


if __name__ == "__main__":
    unittest.main()
