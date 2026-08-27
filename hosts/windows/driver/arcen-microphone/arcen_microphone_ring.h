#pragma once

#include "arcen_microphone_contract.h"

typedef struct _ARCEN_MICROPHONE_BINDING {
    ULONG WtsSessionId;
    ULONG Generation;
    ULONG SidLength;
    UCHAR Sid[ARCEN_MICROPHONE_MAX_SID_BYTES];
    BOOLEAN Active;
} ARCEN_MICROPHONE_BINDING, *PARCEN_MICROPHONE_BINDING;

typedef struct _ARCEN_MICROPHONE_RING {
    KSPIN_LOCK Lock;
    ARCEN_MICROPHONE_BINDING Binding;
    SHORT Frames[ARCEN_MICROPHONE_RING_FRAMES]
                [ARCEN_MICROPHONE_FRAME_SAMPLES];
    ULONG ReadIndex;
    ULONG Length;
    ULONG Overruns;
    ULONG Underruns;
} ARCEN_MICROPHONE_RING, *PARCEN_MICROPHONE_RING;

_IRQL_requires_max_(DISPATCH_LEVEL)
VOID ArcenMicrophoneRingInitialize(_Out_ PARCEN_MICROPHONE_RING Ring);

_IRQL_requires_(PASSIVE_LEVEL)
NTSTATUS ArcenMicrophoneRingBind(
    _Inout_ PARCEN_MICROPHONE_RING Ring,
    _In_ const ARCEN_MICROPHONE_BIND_REQUEST* Request);

_IRQL_requires_max_(DISPATCH_LEVEL)
NTSTATUS ArcenMicrophoneRingWrite(
    _Inout_ PARCEN_MICROPHONE_RING Ring,
    _In_ ULONG WtsSessionId,
    _In_ ULONG Generation,
    _In_reads_bytes_(SidLength) const UCHAR* Sid,
    _In_ ULONG SidLength,
    _In_reads_bytes_(ARCEN_MICROPHONE_FRAME_BYTES) const UCHAR* Frame);

_IRQL_requires_max_(DISPATCH_LEVEL)
NTSTATUS ArcenMicrophoneRingAuthorizeReader(
    _Inout_ PARCEN_MICROPHONE_RING Ring,
    _In_ ULONG WtsSessionId,
    _Out_ ULONG* Generation);

_IRQL_requires_max_(DISPATCH_LEVEL)
NTSTATUS ArcenMicrophoneRingRead(
    _Inout_ PARCEN_MICROPHONE_RING Ring,
    _In_ ULONG WtsSessionId,
    _In_ ULONG Generation,
    _Out_writes_(ARCEN_MICROPHONE_FRAME_SAMPLES)
        SHORT Output[ARCEN_MICROPHONE_FRAME_SAMPLES]);

_IRQL_requires_max_(DISPATCH_LEVEL)
VOID ArcenMicrophoneRingClear(
    _Inout_ PARCEN_MICROPHONE_RING Ring,
    _In_ ULONG WtsSessionId,
    _In_ ULONG Generation);

_IRQL_requires_max_(DISPATCH_LEVEL)
VOID ArcenMicrophoneRingUnbind(
    _Inout_ PARCEN_MICROPHONE_RING Ring,
    _In_ ULONG WtsSessionId,
    _In_ ULONG Generation);

_IRQL_requires_max_(DISPATCH_LEVEL)
VOID ArcenMicrophoneRingStatus(
    _Inout_ PARCEN_MICROPHONE_RING Ring,
    _Out_ PARCEN_MICROPHONE_STATUS_RESPONSE Status);
