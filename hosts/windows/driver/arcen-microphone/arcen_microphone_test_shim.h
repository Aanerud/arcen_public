#pragma once

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <mutex>

using ULONG = std::uint32_t;
using LONG = std::int32_t;
using UCHAR = std::uint8_t;
using SHORT = std::int16_t;
using BOOLEAN = std::uint8_t;
using NTSTATUS = std::int32_t;
using VOID = void;
using KIRQL = int;
using KSPIN_LOCK = std::mutex;
using PSID = void*;

struct SID {
    UCHAR Revision;
    UCHAR SubAuthorityCount;
    UCHAR IdentifierAuthority[6];
    ULONG SubAuthority[1];
};

constexpr BOOLEAN FALSE = 0;
constexpr BOOLEAN TRUE = 1;
constexpr NTSTATUS STATUS_SUCCESS = 0;
constexpr NTSTATUS STATUS_INVALID_PARAMETER =
    static_cast<NTSTATUS>(0xC000000Du);
constexpr NTSTATUS STATUS_ACCESS_DENIED =
    static_cast<NTSTATUS>(0xC0000022u);
constexpr NTSTATUS STATUS_DEVICE_NOT_READY =
    static_cast<NTSTATUS>(0xC00000A3u);

#define FILE_DEVICE_UNKNOWN 0x00000022u
#define FILE_WRITE_DATA 0x0002u
#define FILE_READ_DATA 0x0001u
#define METHOD_BUFFERED 0u
#define CTL_CODE(DeviceType, Function, Method, Access) \
    (((DeviceType) << 16) | ((Access) << 14) | ((Function) << 2) | (Method))

#ifndef _IRQL_requires_max_
#define _IRQL_requires_max_(...)
#endif
#ifndef _IRQL_requires_
#define _IRQL_requires_(...)
#endif
#ifndef _In_
#define _In_
#endif
#ifndef _Inout_
#define _Inout_
#endif
#ifndef _Out_
#define _Out_
#endif
#ifndef _In_reads_bytes_
#define _In_reads_bytes_(...)
#endif
#ifndef _Out_writes_
#define _Out_writes_(...)
#endif

inline void RtlSecureZeroMemory(void* target, std::size_t bytes) {
    volatile UCHAR* cursor = static_cast<volatile UCHAR*>(target);
    while (bytes-- != 0) {
        *cursor++ = 0;
    }
}

inline void RtlZeroMemory(void* target, std::size_t bytes) {
    std::memset(target, 0, bytes);
}

inline void RtlCopyMemory(void* target, const void* source, std::size_t bytes) {
    std::memcpy(target, source, bytes);
}

inline BOOLEAN RtlEqualMemory(
    const void* left,
    const void* right,
    std::size_t bytes) {
    return std::memcmp(left, right, bytes) == 0 ? TRUE : FALSE;
}

inline BOOLEAN RtlValidSid(PSID value) {
    const auto* sid = static_cast<const SID*>(value);
    return sid != nullptr && sid->Revision == 1 &&
                   sid->SubAuthorityCount <= 15
               ? TRUE
               : FALSE;
}

inline ULONG RtlLengthSid(PSID value) {
    const auto* sid = static_cast<const SID*>(value);
    return 8u + 4u * sid->SubAuthorityCount;
}

inline void KeInitializeSpinLock(KSPIN_LOCK*) {}

inline void KeAcquireSpinLock(KSPIN_LOCK* lock, KIRQL* oldIrql) {
    *oldIrql = 0;
    lock->lock();
}

inline void KeReleaseSpinLock(KSPIN_LOCK* lock, KIRQL) {
    lock->unlock();
}
