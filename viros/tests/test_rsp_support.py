"""In-memory, selectable duplex byte streams for sandboxed RSP tests."""

import os


class MemoryDuplexEndpoint:
    """Socket-shaped endpoint implemented with two ordinary OS pipes."""

    def __init__(self, read_fd, write_fd):
        self._read_fd = read_fd
        self._write_fd = write_fd
        self._closed = False

    def fileno(self):
        return self._read_fd

    def sendall(self, data):
        view = memoryview(data)
        while view:
            written = os.write(self._write_fd, view)
            view = view[written:]

    def send(self, data):
        self.sendall(data)
        return len(data)

    def recv(self, length):
        return os.read(self._read_fd, length)

    def close(self):
        if self._closed:
            return
        self._closed = True
        os.close(self._read_fd)
        os.close(self._write_fd)


def memory_duplex_pair():
    a_read, b_write = os.pipe()
    b_read, a_write = os.pipe()
    return (
        MemoryDuplexEndpoint(a_read, a_write),
        MemoryDuplexEndpoint(b_read, b_write),
    )
