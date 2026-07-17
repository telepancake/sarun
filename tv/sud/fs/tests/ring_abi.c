#include "sud/fs/ring.h"

int main(void)
{
    struct sud_fs_ring_header header = {0};
    return (int)header.magic;
}
