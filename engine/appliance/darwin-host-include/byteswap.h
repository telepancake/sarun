#ifndef SARUN_DARWIN_BYTESWAP_H
#define SARUN_DARWIN_BYTESWAP_H

#include <stdint.h>

#define bswap_16(value) __builtin_bswap16((uint16_t)(value))
#define bswap_32(value) __builtin_bswap32((uint32_t)(value))
#define bswap_64(value) __builtin_bswap64((uint64_t)(value))

#endif
