/* Freestanding AArch32 EABI guest-process probe. */
.syntax unified
.text
.global _start
.type _start, %function
_start:
    mov r0, #1
    adr r1, message
    mov r2, #12
    mov r7, #4
    svc #0
    mov r0, #32
    mov r7, #1
    svc #0
.align 2
message:
    .ascii "compat32-ok\n"
.size _start, . - _start
