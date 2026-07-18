import unittest

from inferiors.rsp_codec import Ack, Interrupt, InvalidPacket, Packet, RspCodec, frame_packet


class RspCodecTests(unittest.TestCase):
    def test_round_trip_escaped_binary_and_chunking(self):
        payload = b"m$#}*\x00\xff"
        framed = frame_packet(payload)
        codec = RspCodec()
        events = []
        for byte in framed:
            events.extend(codec.feed(bytes((byte,))))
        self.assertEqual(events, [Packet(payload)])

    def test_ack_interrupt_and_packet_in_one_chunk(self):
        events = RspCodec().feed(b"+-\x03" + frame_packet("qC"))
        self.assertEqual(events, [Ack(True), Ack(False), Interrupt(), Packet(b"qC")])

    def test_bad_checksum_does_not_poison_next_packet(self):
        codec = RspCodec()
        events = codec.feed(b"$qC#00" + frame_packet("?"))
        self.assertIsInstance(events[0], InvalidPacket)
        self.assertEqual(events[1], Packet(b"?"))

    def test_resynchronizes_on_nested_packet_marker(self):
        self.assertEqual(RspCodec().feed(b"$junk" + frame_packet("g")), [Packet(b"g")])


if __name__ == "__main__":
    unittest.main()
