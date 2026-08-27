#include "arcen_microphone_ring.h"

namespace {

_IRQL_requires_max_(DISPATCH_LEVEL)
BOOLEAN BindingMatches(
    _In_ const ARCEN_MICROPHONE_BINDING* Binding,
    _In_ ULONG WtsSessionId,
    _In_ ULONG Generation,
    _In_reads_bytes_(SidLength) const UCHAR* Sid,
    _In_ ULONG SidLength) {
    return Binding->Active && Binding->WtsSessionId == WtsSessionId &&
           Binding->Generation == Generation && Binding->SidLength == SidLength &&
           RtlEqualMemory(Binding->Sid, Sid, SidLength);
}

_IRQL_requires_max_(DISPATCH_LEVEL)
BOOLEAN ReaderIdentityMatches(
    _In_ const ARCEN_MICROPHONE_BINDING* Binding,
    _In_ ULONG WtsSessionId) {
    return Binding->Active && Binding->WtsSessionId == WtsSessionId;
}

_IRQL_requires_max_(DISPATCH_LEVEL)
VOID ClearFrames(_Inout_ PARCEN_MICROPHONE_RING Ring) {
    RtlSecureZeroMemory(Ring->Frames, sizeof(Ring->Frames));
    Ring->ReadIndex = 0;
    Ring->Length = 0;
}

BOOLEAN GenerationIsAfter(ULONG Candidate, ULONG Current) {
    return static_cast<LONG>(Candidate - Current) > 0;
}

}  // namespace

VOID ArcenMicrophoneRingInitialize(PARCEN_MICROPHONE_RING Ring) {
    RtlSecureZeroMemory(Ring, sizeof(*Ring));
    KeInitializeSpinLock(&Ring->Lock);
}

NTSTATUS ArcenMicrophoneRingBind(
    PARCEN_MICROPHONE_RING Ring,
    const ARCEN_MICROPHONE_BIND_REQUEST* Request) {
    if (Request == nullptr ||
        Request->Version != ARCEN_MICROPHONE_CONTRACT_VERSION ||
        Request->WtsSessionId == 0 || Request->Generation == 0 ||
        Request->SidLength < 8 ||
        Request->SidLength > ARCEN_MICROPHONE_MAX_SID_BYTES ||
        !RtlValidSid(const_cast<SID*>(
            reinterpret_cast<const SID*>(Request->Sid))) ||
        RtlLengthSid(const_cast<SID*>(
            reinterpret_cast<const SID*>(Request->Sid))) != Request->SidLength) {
        return STATUS_INVALID_PARAMETER;
    }

    KIRQL oldIrql;
    KeAcquireSpinLock(&Ring->Lock, &oldIrql);
    if (Ring->Binding.Active) {
        const BOOLEAN sameIdentity =
            Ring->Binding.WtsSessionId == Request->WtsSessionId &&
            Ring->Binding.SidLength == Request->SidLength &&
            RtlEqualMemory(
                Ring->Binding.Sid, Request->Sid, Request->SidLength);
        if (!sameIdentity ||
            (Request->Generation != Ring->Binding.Generation &&
             !GenerationIsAfter(
                 Request->Generation, Ring->Binding.Generation))) {
            KeReleaseSpinLock(&Ring->Lock, oldIrql);
            return STATUS_ACCESS_DENIED;
        }
    }
    ClearFrames(Ring);
    Ring->Overruns = 0;
    Ring->Underruns = 0;
    RtlSecureZeroMemory(&Ring->Binding, sizeof(Ring->Binding));
    Ring->Binding.WtsSessionId = Request->WtsSessionId;
    Ring->Binding.Generation = Request->Generation;
    Ring->Binding.SidLength = Request->SidLength;
    RtlCopyMemory(Ring->Binding.Sid, Request->Sid, Request->SidLength);
    Ring->Binding.Active = TRUE;
    KeReleaseSpinLock(&Ring->Lock, oldIrql);
    return STATUS_SUCCESS;
}

NTSTATUS ArcenMicrophoneRingWrite(
    PARCEN_MICROPHONE_RING Ring,
    ULONG WtsSessionId,
    ULONG Generation,
    const UCHAR* Sid,
    ULONG SidLength,
    const UCHAR* Frame) {
    if (Sid == nullptr || Frame == nullptr ||
        SidLength > ARCEN_MICROPHONE_MAX_SID_BYTES) {
        return STATUS_INVALID_PARAMETER;
    }

    KIRQL oldIrql;
    KeAcquireSpinLock(&Ring->Lock, &oldIrql);
    if (!BindingMatches(
            &Ring->Binding, WtsSessionId, Generation, Sid, SidLength)) {
        KeReleaseSpinLock(&Ring->Lock, oldIrql);
        return STATUS_ACCESS_DENIED;
    }
    if (Ring->Length == ARCEN_MICROPHONE_RING_FRAMES) {
        RtlSecureZeroMemory(
            Ring->Frames[Ring->ReadIndex], ARCEN_MICROPHONE_FRAME_BYTES);
        Ring->ReadIndex =
            (Ring->ReadIndex + 1) % ARCEN_MICROPHONE_RING_FRAMES;
        --Ring->Length;
        ++Ring->Overruns;
    }
    const ULONG write =
        (Ring->ReadIndex + Ring->Length) % ARCEN_MICROPHONE_RING_FRAMES;
    RtlCopyMemory(Ring->Frames[write], Frame, ARCEN_MICROPHONE_FRAME_BYTES);
    ++Ring->Length;
    KeReleaseSpinLock(&Ring->Lock, oldIrql);
    return STATUS_SUCCESS;
}

NTSTATUS ArcenMicrophoneRingAuthorizeReader(
    PARCEN_MICROPHONE_RING Ring,
    ULONG WtsSessionId,
    ULONG* Generation) {
    if (Generation == nullptr || WtsSessionId == 0) {
        return STATUS_INVALID_PARAMETER;
    }
    *Generation = 0;

    KIRQL oldIrql;
    KeAcquireSpinLock(&Ring->Lock, &oldIrql);
    NTSTATUS status = STATUS_SUCCESS;
    if (!Ring->Binding.Active) {
        status = STATUS_DEVICE_NOT_READY;
    } else if (!ReaderIdentityMatches(&Ring->Binding, WtsSessionId)) {
        status = STATUS_ACCESS_DENIED;
    } else {
        *Generation = Ring->Binding.Generation;
    }
    KeReleaseSpinLock(&Ring->Lock, oldIrql);
    return status;
}

NTSTATUS ArcenMicrophoneRingRead(
    PARCEN_MICROPHONE_RING Ring,
    ULONG WtsSessionId,
    ULONG Generation,
    SHORT Output[ARCEN_MICROPHONE_FRAME_SAMPLES]) {
    if (Output == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    RtlSecureZeroMemory(Output, ARCEN_MICROPHONE_FRAME_BYTES);
    if (WtsSessionId == 0 || Generation == 0) {
        return STATUS_INVALID_PARAMETER;
    }

    KIRQL oldIrql;
    KeAcquireSpinLock(&Ring->Lock, &oldIrql);
    NTSTATUS status = STATUS_SUCCESS;
    if (!Ring->Binding.Active ||
        Ring->Binding.WtsSessionId != WtsSessionId ||
        Ring->Binding.Generation != Generation) {
        status = Ring->Binding.Active ? STATUS_ACCESS_DENIED
                                      : STATUS_DEVICE_NOT_READY;
    } else if (Ring->Length == 0) {
        ++Ring->Underruns;
    } else {
        RtlCopyMemory(
            Output, Ring->Frames[Ring->ReadIndex],
            ARCEN_MICROPHONE_FRAME_BYTES);
        RtlSecureZeroMemory(
            Ring->Frames[Ring->ReadIndex], ARCEN_MICROPHONE_FRAME_BYTES);
        Ring->ReadIndex =
            (Ring->ReadIndex + 1) % ARCEN_MICROPHONE_RING_FRAMES;
        --Ring->Length;
    }
    KeReleaseSpinLock(&Ring->Lock, oldIrql);
    return status;
}

VOID ArcenMicrophoneRingClear(
    PARCEN_MICROPHONE_RING Ring,
    ULONG WtsSessionId,
    ULONG Generation) {
    KIRQL oldIrql;
    KeAcquireSpinLock(&Ring->Lock, &oldIrql);
    if (Ring->Binding.Active &&
        Ring->Binding.WtsSessionId == WtsSessionId &&
        Ring->Binding.Generation == Generation) {
        ClearFrames(Ring);
    }
    KeReleaseSpinLock(&Ring->Lock, oldIrql);
}

VOID ArcenMicrophoneRingUnbind(
    PARCEN_MICROPHONE_RING Ring,
    ULONG WtsSessionId,
    ULONG Generation) {
    KIRQL oldIrql;
    KeAcquireSpinLock(&Ring->Lock, &oldIrql);
    if (Ring->Binding.Active &&
        Ring->Binding.WtsSessionId == WtsSessionId &&
        Ring->Binding.Generation == Generation) {
        ClearFrames(Ring);
        RtlSecureZeroMemory(&Ring->Binding, sizeof(Ring->Binding));
    }
    KeReleaseSpinLock(&Ring->Lock, oldIrql);
}

VOID ArcenMicrophoneRingStatus(
    PARCEN_MICROPHONE_RING Ring,
    PARCEN_MICROPHONE_STATUS_RESPONSE Status) {
    KIRQL oldIrql;
    KeAcquireSpinLock(&Ring->Lock, &oldIrql);
    RtlSecureZeroMemory(Status, sizeof(*Status));
    Status->Version = ARCEN_MICROPHONE_CONTRACT_VERSION;
    Status->State = Ring->Binding.Active
                        ? ArcenMicrophoneStateBound
                        : ArcenMicrophoneStateUnbound;
    Status->WtsSessionId = Ring->Binding.WtsSessionId;
    Status->Generation = Ring->Binding.Generation;
    Status->QueuedFrames = Ring->Length;
    Status->Overruns = Ring->Overruns;
    Status->Underruns = Ring->Underruns;
    KeReleaseSpinLock(&Ring->Lock, oldIrql);
}
