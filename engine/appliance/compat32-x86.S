/* Freestanding i386 guest-process probe. */
.text
.global _start
.type _start, @function
_start:
    mov $4, %eax
    mov $1, %ebx
    mov $message, %ecx
    mov $12, %edx
    int $0x80
    mov $1, %eax
    mov $32, %ebx
    int $0x80
message:
    .ascii "compat32-ok\n"
.size _start, . - _start
