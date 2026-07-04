/*
 * sudtrace.lds — Supplementary linker script for sudtrace.
 *
 * Defines __sud_begin and __sud_end so SUD can whitelist the address range
 * of sudtrace's own code + data.  Works in combination with
 * -Wl,-Ttext-segment=0x40000000 on the command line.
 *
 * These bracket the entire loaded image so that any instruction pointer
 * inside sudtrace is allowed through SUD, while everything outside
 * (child code, ld.so, etc.) triggers SIGSYS.
 */

SECTIONS
{
    .sud_head : {
        __sud_begin = .;
    }
}
INSERT BEFORE .text;

SECTIONS
{
    /* __sud_end after the last section the binary uses */
    .sud_sentinel : {
        . = ALIGN(4096);
        __sud_end = .;
    }
}
INSERT AFTER .bss;
