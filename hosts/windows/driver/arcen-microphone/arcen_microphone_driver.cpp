#define POOL_ZERO_DOWN_LEVEL_SUPPORT
#include <ntifs.h>
#include <wdmsec.h>
#include <initguid.h>
#include <portcls.h>
#include <ks.h>
#include <ksmedia.h>

#include "arcen_microphone_contract.h"
#include "arcen_microphone_guids.h"
#include "arcen_microphone_ring.h"

#define ARCEN_POOL_TAG 'mcrA'
#define ARCEN_MAX_WAVERT_BUFFER_BYTES \
    (ARCEN_MICROPHONE_RING_FRAMES * ARCEN_MICROPHONE_FRAME_BYTES)
#define ARCEN_FRAME_PERIOD_MS 20
#define ARCEN_FRAME_PERIOD_100NS (ARCEN_FRAME_PERIOD_MS * 10u * 1000u)
#define ARCEN_MAX_CATCHUP_FRAMES 4u
#define ARCEN_MAX_CAPTURE_STREAMS 16u

void* __cdecl operator new(size_t, void* storage) noexcept {
    return storage;
}

void __cdecl operator delete(void*, void*) noexcept {}

void __cdecl operator delete(void* storage) noexcept {
    if (storage != nullptr) {
        ExFreePoolWithTag(storage, ARCEN_POOL_TAG);
    }
}

void __cdecl operator delete(void* storage, size_t) noexcept {
    operator delete(storage);
}

namespace {

class WaveRtStream;

struct ARCEN_FILE_CONTEXT {
    ARCEN_MICROPHONE_BINDING Binding;
};

struct ARCEN_CONTROL_EXTENSION {
    IO_REMOVE_LOCK RemoveLock;
};

struct ARCEN_CAPTURE_IDENTITY {
    ULONG WtsSessionId;
};

struct ARCEN_CAPTURE_CREATE_CONTEXT {
    LIST_ENTRY Link;
    PKTHREAD Thread;
    ARCEN_CAPTURE_IDENTITY Identity;
};

struct ARCEN_DRIVER_CONTEXT {
    ARCEN_MICROPHONE_RING Ring;
    FAST_MUTEX ControlLock;
    PDEVICE_OBJECT AudioDevice;
    PDEVICE_OBJECT ControlDevice;
    PFILE_OBJECT Owner;
    UNICODE_STRING InterfaceName;
    volatile LONG Removed;
    volatile LONG PoweredD0;
    volatile LONG ActiveStreams;
    ULONG HighestGeneration;
    ULONG OpenHandles;
    KSPIN_LOCK CaptureCreateLock;
    LIST_ENTRY CaptureCreateContexts;
    KSPIN_LOCK StreamLock;
    WaveRtStream* Streams[ARCEN_MAX_CAPTURE_STREAMS];
};

ARCEN_DRIVER_CONTEXT g_Driver;
PDRIVER_DISPATCH g_PortClsCreate;
PDRIVER_DISPATCH g_PortClsCleanup;
PDRIVER_DISPATCH g_PortClsClose;
PDRIVER_DISPATCH g_PortClsDeviceControl;
PDRIVER_DISPATCH g_PortClsPnp;

WCHAR g_WaveName[] = L"ArcenMicrophoneWave";
WCHAR g_TopologyName[] = L"ArcenMicrophoneTopology";
UNICODE_STRING g_ControlDosName;

struct ARCEN_SERVICE_SID {
    SID Sid;
    ULONG AdditionalSubAuthorities[5];
};

const ARCEN_SERVICE_SID g_ArcenPierServiceSid = {
    {SID_REVISION, 6, SECURITY_NT_AUTHORITY,
     {SECURITY_SERVICE_ID_BASE_RID}},
    {2794664030u, 2322002993u, 548807306u, 4095822587u, 2900116599u}};
constexpr ULONG kSeGroupEnabled = 0x00000004u;

BOOLEAN IsEqualGuid(_In_ REFIID Left, _In_ REFIID Right) {
    return InlineIsEqualGUID(Left, Right) ? TRUE : FALSE;
}

BOOLEAN GenerationIsAfter(ULONG Candidate, ULONG Current) {
    return static_cast<LONG>(Candidate - Current) > 0;
}

NTSTATUS CompleteIrp(_Inout_ PIRP Irp, _In_ NTSTATUS Status, _In_ ULONG_PTR Information = 0) {
    Irp->IoStatus.Status = Status;
    Irp->IoStatus.Information = Information;
    IoCompleteRequest(Irp, IO_NO_INCREMENT);
    return Status;
}

template <typename T, typename... Args>
T* AllocateObject(Args&&... args) {
    void* storage = ExAllocatePoolZero(NonPagedPoolNx, sizeof(T), ARCEN_POOL_TAG);
    if (storage == nullptr) {
        return nullptr;
    }
    return new (storage) T(static_cast<Args&&>(args)...);
}

template <typename T>
void FreeObject(_In_ T* Object) {
    Object->~T();
    ExFreePoolWithTag(Object, ARCEN_POOL_TAG);
}

BOOLEAN RequestorHasServiceSid(_In_ PIRP Irp) {
    PEPROCESS process = IoGetRequestorProcess(Irp);
    if (process == nullptr) {
        return FALSE;
    }

    PACCESS_TOKEN token = PsReferencePrimaryToken(process);
    PTOKEN_GROUPS groups = nullptr;
    const NTSTATUS status = SeQueryInformationToken(
        token, TokenGroups, reinterpret_cast<PVOID*>(&groups));
    PsDereferencePrimaryToken(token);
    if (!NT_SUCCESS(status) || groups == nullptr) {
        return FALSE;
    }

    BOOLEAN found = FALSE;
    for (ULONG index = 0; index < groups->GroupCount; ++index) {
        const SID_AND_ATTRIBUTES& group = groups->Groups[index];
        if ((group.Attributes & kSeGroupEnabled) != 0 &&
            RtlEqualSid(
                group.Sid,
                const_cast<SID*>(&g_ArcenPierServiceSid.Sid))) {
            found = TRUE;
            break;
        }
    }
    ExFreePool(groups);
    return found;
}

NTSTATUS CaptureRequestorIdentity(
    _In_ PIRP Irp,
    _Out_ ARCEN_CAPTURE_IDENTITY* Identity) {
    if (Irp == nullptr || Identity == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    RtlSecureZeroMemory(Identity, sizeof(*Identity));

    ULONG sessionId = 0;
    NTSTATUS status = IoGetRequestorSessionId(Irp, &sessionId);
    if (!NT_SUCCESS(status) || sessionId == 0) {
        return NT_SUCCESS(status) ? STATUS_ACCESS_DENIED : status;
    }
    Identity->WtsSessionId = sessionId;
    return STATUS_SUCCESS;
}

void RegisterCaptureCreateContext(
    _Inout_ ARCEN_CAPTURE_CREATE_CONTEXT* Context) {
    Context->Thread = KeGetCurrentThread();
    KIRQL oldIrql;
    KeAcquireSpinLock(&g_Driver.CaptureCreateLock, &oldIrql);
    InsertHeadList(&g_Driver.CaptureCreateContexts, &Context->Link);
    KeReleaseSpinLock(&g_Driver.CaptureCreateLock, oldIrql);
}

void UnregisterCaptureCreateContext(
    _Inout_ ARCEN_CAPTURE_CREATE_CONTEXT* Context) {
    KIRQL oldIrql;
    KeAcquireSpinLock(&g_Driver.CaptureCreateLock, &oldIrql);
    RemoveEntryList(&Context->Link);
    KeReleaseSpinLock(&g_Driver.CaptureCreateLock, oldIrql);
    RtlSecureZeroMemory(Context, sizeof(*Context));
}

NTSTATUS CaptureIdentityForCurrentCreate(
    _Out_ ARCEN_CAPTURE_IDENTITY* Identity) {
    if (Identity == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    RtlSecureZeroMemory(Identity, sizeof(*Identity));
    PKTHREAD thread = KeGetCurrentThread();

    KIRQL oldIrql;
    KeAcquireSpinLock(&g_Driver.CaptureCreateLock, &oldIrql);
    NTSTATUS status = STATUS_ACCESS_DENIED;
    for (PLIST_ENTRY entry = g_Driver.CaptureCreateContexts.Flink;
         entry != &g_Driver.CaptureCreateContexts;
         entry = entry->Flink) {
        auto* context = CONTAINING_RECORD(
            entry, ARCEN_CAPTURE_CREATE_CONTEXT, Link);
        if (context->Thread == thread) {
            *Identity = context->Identity;
            status = STATUS_SUCCESS;
            break;
        }
    }
    KeReleaseSpinLock(&g_Driver.CaptureCreateLock, oldIrql);
    return status;
}

BOOLEAN IsFixedPcmFormat(_In_ const KSDATAFORMAT* Format) {
    if (Format == nullptr ||
        Format->FormatSize < sizeof(KSDATAFORMAT_WAVEFORMATEX) ||
        !IsEqualGuid(Format->MajorFormat, KSDATAFORMAT_TYPE_AUDIO) ||
        !IsEqualGuid(Format->SubFormat, KSDATAFORMAT_SUBTYPE_PCM) ||
        !IsEqualGuid(Format->Specifier, KSDATAFORMAT_SPECIFIER_WAVEFORMATEX)) {
        return FALSE;
    }
    const auto* wave = reinterpret_cast<const KSDATAFORMAT_WAVEFORMATEX*>(Format);
    return wave->WaveFormatEx.wFormatTag == WAVE_FORMAT_PCM &&
           wave->WaveFormatEx.nChannels == ARCEN_MICROPHONE_CHANNELS &&
           wave->WaveFormatEx.nSamplesPerSec == ARCEN_MICROPHONE_SAMPLE_RATE &&
           wave->WaveFormatEx.wBitsPerSample == ARCEN_MICROPHONE_BITS_PER_SAMPLE &&
           wave->WaveFormatEx.nBlockAlign == 2 &&
           wave->WaveFormatEx.nAvgBytesPerSec ==
               ARCEN_MICROPHONE_SAMPLE_RATE * 2 &&
           wave->WaveFormatEx.cbSize == 0;
}

void FillFixedPcmFormat(_Out_ KSDATAFORMAT_WAVEFORMATEX* Format) {
    RtlZeroMemory(Format, sizeof(*Format));
    Format->DataFormat.FormatSize = sizeof(*Format);
    Format->DataFormat.SampleSize = 2;
    Format->DataFormat.MajorFormat = KSDATAFORMAT_TYPE_AUDIO;
    Format->DataFormat.SubFormat = KSDATAFORMAT_SUBTYPE_PCM;
    Format->DataFormat.Specifier = KSDATAFORMAT_SPECIFIER_WAVEFORMATEX;
    Format->WaveFormatEx.wFormatTag = WAVE_FORMAT_PCM;
    Format->WaveFormatEx.nChannels = ARCEN_MICROPHONE_CHANNELS;
    Format->WaveFormatEx.nSamplesPerSec = ARCEN_MICROPHONE_SAMPLE_RATE;
    Format->WaveFormatEx.nAvgBytesPerSec = ARCEN_MICROPHONE_SAMPLE_RATE * 2;
    Format->WaveFormatEx.nBlockAlign = 2;
    Format->WaveFormatEx.wBitsPerSample = ARCEN_MICROPHONE_BITS_PER_SAMPLE;
}

NTSTATUS FixedDataRangeIntersection(
    _In_ ULONG PinId,
    _In_ PKSDATARANGE DataRange,
    _In_ PKSDATARANGE MatchingDataRange,
    _In_ ULONG OutputBufferLength,
    _Out_writes_bytes_to_opt_(OutputBufferLength, *ResultantFormatLength)
        PVOID ResultantFormat,
    _Out_ PULONG ResultantFormatLength) {
    if (PinId != 0 || DataRange == nullptr || MatchingDataRange == nullptr ||
        ResultantFormatLength == nullptr ||
        DataRange->FormatSize < sizeof(KSDATARANGE_AUDIO) ||
        MatchingDataRange->FormatSize < sizeof(KSDATARANGE_AUDIO)) {
        return STATUS_INVALID_PARAMETER;
    }
    const auto* proposed = reinterpret_cast<const KSDATARANGE_AUDIO*>(DataRange);
    const auto* supported =
        reinterpret_cast<const KSDATARANGE_AUDIO*>(MatchingDataRange);
    if (!IsEqualGuid(proposed->DataRange.MajorFormat, KSDATAFORMAT_TYPE_AUDIO) ||
        !IsEqualGuid(proposed->DataRange.SubFormat, KSDATAFORMAT_SUBTYPE_PCM) ||
        !IsEqualGuid(
            proposed->DataRange.Specifier,
            KSDATAFORMAT_SPECIFIER_WAVEFORMATEX) ||
        supported->MaximumChannels != 1 ||
        proposed->MaximumChannels < 1 ||
        proposed->MinimumBitsPerSample > ARCEN_MICROPHONE_BITS_PER_SAMPLE ||
        proposed->MaximumBitsPerSample < ARCEN_MICROPHONE_BITS_PER_SAMPLE ||
        proposed->MinimumSampleFrequency > ARCEN_MICROPHONE_SAMPLE_RATE ||
        proposed->MaximumSampleFrequency < ARCEN_MICROPHONE_SAMPLE_RATE) {
        return STATUS_NO_MATCH;
    }

    *ResultantFormatLength = sizeof(KSDATAFORMAT_WAVEFORMATEX);
    if (OutputBufferLength == 0) {
        return STATUS_BUFFER_OVERFLOW;
    }
    if (ResultantFormat == nullptr ||
        OutputBufferLength < sizeof(KSDATAFORMAT_WAVEFORMATEX)) {
        return STATUS_BUFFER_TOO_SMALL;
    }
    FillFixedPcmFormat(
        reinterpret_cast<KSDATAFORMAT_WAVEFORMATEX*>(ResultantFormat));
    return STATUS_SUCCESS;
}

KSDATARANGE_AUDIO g_WaveDataRange = {
    {
        sizeof(KSDATARANGE_AUDIO),
        0,
        2,
        0,
        STATICGUIDOF(KSDATAFORMAT_TYPE_AUDIO),
        STATICGUIDOF(KSDATAFORMAT_SUBTYPE_PCM),
        STATICGUIDOF(KSDATAFORMAT_SPECIFIER_WAVEFORMATEX),
    },
    1,
    ARCEN_MICROPHONE_BITS_PER_SAMPLE,
    ARCEN_MICROPHONE_BITS_PER_SAMPLE,
    ARCEN_MICROPHONE_SAMPLE_RATE,
    ARCEN_MICROPHONE_SAMPLE_RATE,
};

KSDATARANGE g_AnalogDataRange = {
    sizeof(KSDATARANGE),
    0,
    0,
    0,
    STATICGUIDOF(KSDATAFORMAT_TYPE_AUDIO),
    STATICGUIDOF(KSDATAFORMAT_SUBTYPE_ANALOG),
    STATICGUIDOF(KSDATAFORMAT_SPECIFIER_NONE),
};

PKSDATARANGE g_WaveRanges[] = {
    reinterpret_cast<PKSDATARANGE>(&g_WaveDataRange),
};
PKSDATARANGE g_AnalogRanges[] = {&g_AnalogDataRange};

KSPIN_INTERFACE g_StreamingInterfaces[] = {
    {
        STATICGUIDOF(KSINTERFACESETID_Standard),
        KSINTERFACE_STANDARD_STREAMING,
        0,
    },
};

KSPIN_MEDIUM g_StandardMediums[] = {
    {
        STATICGUIDOF(KSMEDIUMSETID_Standard),
        KSMEDIUM_TYPE_ANYINSTANCE,
        0,
    },
};

PCPIN_DESCRIPTOR g_WavePins[] = {
    {
        1,
        1,
        0,
        nullptr,
        {
            ARRAYSIZE(g_StreamingInterfaces),
            g_StreamingInterfaces,
            ARRAYSIZE(g_StandardMediums),
            g_StandardMediums,
            ARRAYSIZE(g_WaveRanges),
            g_WaveRanges,
            KSPIN_DATAFLOW_OUT,
            KSPIN_COMMUNICATION_SINK,
            &KSCATEGORY_AUDIO,
            &KSNODETYPE_MICROPHONE,
            0,
        },
    },
    {
        0,
        0,
        0,
        nullptr,
        {
            0,
            nullptr,
            0,
            nullptr,
            ARRAYSIZE(g_AnalogRanges),
            g_AnalogRanges,
            KSPIN_DATAFLOW_IN,
            KSPIN_COMMUNICATION_NONE,
            &KSCATEGORY_AUDIO,
            &KSNODETYPE_MICROPHONE,
            0,
        },
    },
};

PCCONNECTION_DESCRIPTOR g_WaveConnections[] = {
    {PCFILTER_NODE, 1, PCFILTER_NODE, 0},
};

GUID g_WaveCategories[] = {
    STATICGUIDOF(KSCATEGORY_AUDIO),
    STATICGUIDOF(KSCATEGORY_CAPTURE),
    STATICGUIDOF(KSCATEGORY_REALTIME),
};

PCFILTER_DESCRIPTOR g_WaveFilter = {
    0,
    nullptr,
    sizeof(PCPIN_DESCRIPTOR),
    ARRAYSIZE(g_WavePins),
    g_WavePins,
    0,
    0,
    nullptr,
    ARRAYSIZE(g_WaveConnections),
    g_WaveConnections,
    ARRAYSIZE(g_WaveCategories),
    g_WaveCategories,
};

PCPIN_DESCRIPTOR g_TopologyPins[] = {
    {
        0,
        0,
        0,
        nullptr,
        {
            0,
            nullptr,
            0,
            nullptr,
            ARRAYSIZE(g_AnalogRanges),
            g_AnalogRanges,
            KSPIN_DATAFLOW_OUT,
            KSPIN_COMMUNICATION_NONE,
            &KSNODETYPE_MICROPHONE,
            nullptr,
            0,
        },
    },
};

GUID g_TopologyCategories[] = {
    STATICGUIDOF(KSCATEGORY_AUDIO),
    STATICGUIDOF(KSCATEGORY_TOPOLOGY),
};

PCFILTER_DESCRIPTOR g_TopologyFilter = {
    0,
    nullptr,
    sizeof(PCPIN_DESCRIPTOR),
    ARRAYSIZE(g_TopologyPins),
    g_TopologyPins,
    0,
    0,
    nullptr,
    0,
    nullptr,
    ARRAYSIZE(g_TopologyCategories),
    g_TopologyCategories,
};

class MiniportTopology final : public IMiniportTopology {
public:
    MiniportTopology() : References_(1) {}

    STDMETHODIMP QueryInterface(REFIID Interface, PVOID* Object) override {
        if (Object == nullptr) {
            return STATUS_INVALID_PARAMETER;
        }
        *Object = nullptr;
        if (IsEqualGuid(Interface, IID_IUnknown) ||
            IsEqualGuid(Interface, IID_IMiniport) ||
            IsEqualGuid(Interface, IID_IMiniportTopology)) {
            *Object = static_cast<IMiniportTopology*>(this);
            AddRef();
            return STATUS_SUCCESS;
        }
        return STATUS_NOINTERFACE;
    }

    STDMETHODIMP_(ULONG) AddRef() override {
        return static_cast<ULONG>(InterlockedIncrement(&References_));
    }

    STDMETHODIMP_(ULONG) Release() override {
        const LONG remaining = InterlockedDecrement(&References_);
        if (remaining == 0) {
            FreeObject(this);
        }
        return static_cast<ULONG>(remaining);
    }

    STDMETHODIMP GetDescription(PPCFILTER_DESCRIPTOR* Description) override {
        if (Description == nullptr) {
            return STATUS_INVALID_PARAMETER;
        }
        *Description = &g_TopologyFilter;
        return STATUS_SUCCESS;
    }

    STDMETHODIMP DataRangeIntersection(
        ULONG,
        PKSDATARANGE,
        PKSDATARANGE,
        ULONG,
        PVOID,
        PULONG) override {
        return STATUS_NOT_SUPPORTED;
    }

    STDMETHODIMP Init(PUNKNOWN, PRESOURCELIST, PPORTTOPOLOGY) override {
        return STATUS_SUCCESS;
    }

private:
    volatile LONG References_;
};

class WaveRtStream final : public IMiniportWaveRTStreamNotification {
public:
    WaveRtStream(
        _In_ PPORTWAVERTSTREAM PortStream,
        _In_ const ARCEN_CAPTURE_IDENTITY& Identity)
        : References_(1),
          PortStream_(PortStream),
          AudioMdl_(nullptr),
          AudioBuffer_(nullptr),
          BufferBytes_(0),
          Position_(0),
          State_(KSSTATE_STOP),
          NotificationCount_(0),
          BytesSinceNotification_(0),
          Running_(0),
          EventCount_(0),
          ReaderGeneration_(0),
          ReaderIdentity_(Identity),
          Registered_(FALSE),
          LastServiceTime100ns_(0) {
        PortStream_->AddRef();
        KeInitializeTimerEx(&Timer_, NotificationTimer);
        KeInitializeDpc(&Dpc_, TimerDpc, this);
        KeInitializeSpinLock(&EventLock_);
        RtlZeroMemory(Events_, sizeof(Events_));
        InterlockedIncrement(&g_Driver.ActiveStreams);
    }

    ~WaveRtStream() {
        StopAndReset();
        FreeBuffer();
        RtlSecureZeroMemory(&ReaderIdentity_, sizeof(ReaderIdentity_));
        ReaderGeneration_ = 0;
        PortStream_->Release();
        InterlockedDecrement(&g_Driver.ActiveStreams);
    }

    NTSTATUS Register() {
        KIRQL oldIrql;
        KeAcquireSpinLock(&g_Driver.StreamLock, &oldIrql);
        NTSTATUS status = STATUS_INSUFFICIENT_RESOURCES;
        for (ULONG index = 0; index < ARRAYSIZE(g_Driver.Streams); ++index) {
            if (g_Driver.Streams[index] == nullptr) {
                g_Driver.Streams[index] = this;
                Registered_ = TRUE;
                status = STATUS_SUCCESS;
                break;
            }
        }
        KeReleaseSpinLock(&g_Driver.StreamLock, oldIrql);
        return status;
    }

    static void QuiesceAll() {
        WaveRtStream* streams[ARCEN_MAX_CAPTURE_STREAMS];
        ULONG count = 0;
        RtlZeroMemory(streams, sizeof(streams));

        KIRQL oldIrql;
        KeAcquireSpinLock(&g_Driver.StreamLock, &oldIrql);
        for (ULONG index = 0; index < ARRAYSIZE(g_Driver.Streams); ++index) {
            if (g_Driver.Streams[index] != nullptr) {
                g_Driver.Streams[index]->AddRef();
                streams[count++] = g_Driver.Streams[index];
            }
        }
        KeReleaseSpinLock(&g_Driver.StreamLock, oldIrql);

        for (ULONG index = 0; index < count; ++index) {
            streams[index]->StopAndReset();
            streams[index]->Release();
        }
    }

    STDMETHODIMP QueryInterface(REFIID Interface, PVOID* Object) override {
        if (Object == nullptr) {
            return STATUS_INVALID_PARAMETER;
        }
        *Object = nullptr;
        if (IsEqualGuid(Interface, IID_IUnknown) ||
            IsEqualGuid(Interface, IID_IMiniportWaveRTStream)) {
            *Object = static_cast<IMiniportWaveRTStream*>(this);
        } else if (IsEqualGuid(
                       Interface, IID_IMiniportWaveRTStreamNotification)) {
            *Object = static_cast<IMiniportWaveRTStreamNotification*>(this);
        } else {
            return STATUS_NOINTERFACE;
        }
        AddRef();
        return STATUS_SUCCESS;
    }

    STDMETHODIMP_(ULONG) AddRef() override {
        return static_cast<ULONG>(InterlockedIncrement(&References_));
    }

    STDMETHODIMP_(ULONG) Release() override {
        KIRQL oldIrql;
        KeAcquireSpinLock(&g_Driver.StreamLock, &oldIrql);
        const LONG remaining = InterlockedDecrement(&References_);
        if (remaining == 0 && Registered_) {
            for (ULONG index = 0; index < ARRAYSIZE(g_Driver.Streams); ++index) {
                if (g_Driver.Streams[index] == this) {
                    g_Driver.Streams[index] = nullptr;
                    Registered_ = FALSE;
                    break;
                }
            }
        }
        KeReleaseSpinLock(&g_Driver.StreamLock, oldIrql);
        if (remaining == 0) {
            FreeObject(this);
        }
        return static_cast<ULONG>(remaining);
    }

    STDMETHODIMP AllocateAudioBuffer(
        ULONG RequestedSize,
        PMDL* AudioBufferMdl,
        ULONG* ActualSize,
        ULONG* OffsetFromFirstPage,
        MEMORY_CACHING_TYPE* CacheType) override {
        return AllocateBuffer(
            0,
            RequestedSize,
            AudioBufferMdl,
            ActualSize,
            OffsetFromFirstPage,
            CacheType);
    }

    STDMETHODIMP_(VOID) FreeAudioBuffer(PMDL AudioBufferMdl, ULONG BufferSize)
        override {
        if (AudioBufferMdl == AudioMdl_ && BufferSize == BufferBytes_) {
            FreeBuffer();
        }
    }

    STDMETHODIMP GetClockRegister(KSRTAUDIO_HWREGISTER*) override {
        return STATUS_NOT_SUPPORTED;
    }

    STDMETHODIMP_(VOID) GetHWLatency(
        KSRTAUDIO_HWLATENCY* HardwareLatency) override {
        if (HardwareLatency != nullptr) {
            HardwareLatency->FifoSize = ARCEN_MICROPHONE_FRAME_BYTES;
            HardwareLatency->ChipsetDelay = 0;
            HardwareLatency->CodecDelay = 0;
        }
    }

    STDMETHODIMP GetPosition(KSAUDIO_POSITION* Position) override {
        if (Position == nullptr) {
            return STATUS_INVALID_PARAMETER;
        }
        const ULONG offset = static_cast<ULONG>(
            InterlockedCompareExchange(&Position_, 0, 0));
        Position->PlayOffset = offset;
        Position->WriteOffset = offset;
        return STATUS_SUCCESS;
    }

    STDMETHODIMP GetPositionRegister(KSRTAUDIO_HWREGISTER*) override {
        return STATUS_NOT_SUPPORTED;
    }

    STDMETHODIMP SetFormat(PKSDATAFORMAT DataFormat) override {
        if (State_ == KSSTATE_RUN || !IsFixedPcmFormat(DataFormat)) {
            return STATUS_INVALID_PARAMETER;
        }
        return STATUS_SUCCESS;
    }

    STDMETHODIMP SetState(KSSTATE State) override {
        if (State < KSSTATE_STOP || State > KSSTATE_RUN ||
            (State != State_ &&
             (State > State_ ? State - State_ : State_ - State) != 1)) {
            return STATUS_INVALID_DEVICE_STATE;
        }
        if (State == KSSTATE_RUN) {
            if (AudioBuffer_ == nullptr || BufferBytes_ == 0 ||
                InterlockedCompareExchange(&g_Driver.Removed, 0, 0) != 0 ||
                InterlockedCompareExchange(&g_Driver.PoweredD0, 0, 0) == 0) {
                return STATUS_DEVICE_NOT_READY;
            }
            ULONG generation = 0;
            const NTSTATUS authorize = ArcenMicrophoneRingAuthorizeReader(
                &g_Driver.Ring,
                ReaderIdentity_.WtsSessionId,
                &generation);
            if (!NT_SUCCESS(authorize) &&
                authorize != STATUS_DEVICE_NOT_READY) {
                return authorize;
            }
            ReaderGeneration_ = generation;
            LARGE_INTEGER due;
            due.QuadPart = -static_cast<LONGLONG>(ARCEN_FRAME_PERIOD_MS) *
                           10 * 1000;
            LastServiceTime100ns_ = KeQueryInterruptTimePrecise(nullptr);
            InterlockedExchange(&Running_, 1);
            KeSetTimerEx(&Timer_, due, ARCEN_FRAME_PERIOD_MS, &Dpc_);
        } else if (State == KSSTATE_PAUSE || State == KSSTATE_STOP) {
            StopTimer();
        }
        if (State == KSSTATE_STOP) {
            StopAndReset();
        }
        State_ = State;
        return STATUS_SUCCESS;
    }

    STDMETHODIMP AllocateBufferWithNotification(
        ULONG NotificationCount,
        ULONG RequestedSize,
        PMDL* AudioBufferMdl,
        ULONG* ActualSize,
        ULONG* OffsetFromFirstPage,
        MEMORY_CACHING_TYPE* CacheType) override {
        if (NotificationCount != 1 && NotificationCount != 2) {
            return STATUS_INVALID_PARAMETER;
        }
        return AllocateBuffer(
            NotificationCount,
            RequestedSize,
            AudioBufferMdl,
            ActualSize,
            OffsetFromFirstPage,
            CacheType);
    }

    STDMETHODIMP_(VOID) FreeBufferWithNotification(
        PMDL AudioBufferMdl,
        ULONG BufferSize) override {
        FreeAudioBuffer(AudioBufferMdl, BufferSize);
    }

    STDMETHODIMP RegisterNotificationEvent(PKEVENT NotificationEvent) override {
        if (NotificationEvent == nullptr) {
            return STATUS_INVALID_PARAMETER;
        }
        KIRQL oldIrql;
        KeAcquireSpinLock(&EventLock_, &oldIrql);
        for (ULONG index = 0; index < EventCount_; ++index) {
            if (Events_[index] == NotificationEvent) {
                KeReleaseSpinLock(&EventLock_, oldIrql);
                return STATUS_OBJECT_NAME_EXISTS;
            }
        }
        if (EventCount_ == ARRAYSIZE(Events_)) {
            KeReleaseSpinLock(&EventLock_, oldIrql);
            return STATUS_INSUFFICIENT_RESOURCES;
        }
        Events_[EventCount_++] = NotificationEvent;
        KeReleaseSpinLock(&EventLock_, oldIrql);
        return STATUS_SUCCESS;
    }

    STDMETHODIMP UnregisterNotificationEvent(PKEVENT NotificationEvent)
        override {
        if (NotificationEvent == nullptr) {
            return STATUS_INVALID_PARAMETER;
        }
        KIRQL oldIrql;
        KeAcquireSpinLock(&EventLock_, &oldIrql);
        for (ULONG index = 0; index < EventCount_; ++index) {
            if (Events_[index] == NotificationEvent) {
                Events_[index] = Events_[EventCount_ - 1];
                Events_[EventCount_ - 1] = nullptr;
                --EventCount_;
                KeReleaseSpinLock(&EventLock_, oldIrql);
                return STATUS_SUCCESS;
            }
        }
        KeReleaseSpinLock(&EventLock_, oldIrql);
        return STATUS_NOT_FOUND;
    }

private:
    NTSTATUS AllocateBuffer(
        ULONG NotificationCount,
        ULONG RequestedSize,
        PMDL* AudioBufferMdl,
        ULONG* ActualSize,
        ULONG* OffsetFromFirstPage,
        MEMORY_CACHING_TYPE* CacheType) {
        const ULONG alignment =
            NotificationCount == 2 ? 2 * ARCEN_MICROPHONE_FRAME_BYTES
                                   : ARCEN_MICROPHONE_FRAME_BYTES;
        if (AudioBufferMdl == nullptr || ActualSize == nullptr ||
            OffsetFromFirstPage == nullptr || CacheType == nullptr ||
            AudioMdl_ != nullptr || RequestedSize == 0 ||
            RequestedSize > ARCEN_MAX_WAVERT_BUFFER_BYTES ||
            RequestedSize > MAXULONG - (alignment - 1)) {
            return STATUS_INVALID_PARAMETER;
        }
        const ULONG aligned =
            ((RequestedSize + alignment - 1) / alignment) * alignment;
        if (aligned > ARCEN_MAX_WAVERT_BUFFER_BYTES) {
            return STATUS_INVALID_PARAMETER;
        }
        PHYSICAL_ADDRESS highAddress;
        highAddress.QuadPart = MAXLONGLONG;
        PMDL mdl = PortStream_->AllocatePagesForMdl(highAddress, aligned);
        if (mdl == nullptr || MmGetMdlByteCount(mdl) < aligned) {
            if (mdl != nullptr) {
                PortStream_->FreePagesFromMdl(mdl);
            }
            return STATUS_INSUFFICIENT_RESOURCES;
        }
        PVOID buffer = PortStream_->MapAllocatedPages(mdl, MmCached);
        if (buffer == nullptr) {
            PortStream_->FreePagesFromMdl(mdl);
            return STATUS_INSUFFICIENT_RESOURCES;
        }
        RtlSecureZeroMemory(buffer, aligned);
        AudioMdl_ = mdl;
        AudioBuffer_ = reinterpret_cast<PUCHAR>(buffer);
        BufferBytes_ = aligned;
        NotificationCount_ = NotificationCount;
        BytesSinceNotification_ = 0;
        InterlockedExchange(&Position_, 0);
        LastServiceTime100ns_ = 0;
        *AudioBufferMdl = mdl;
        *ActualSize = aligned;
        *OffsetFromFirstPage = 0;
        *CacheType = MmCached;
        return STATUS_SUCCESS;
    }

    void FreeBuffer() {
        StopAndReset();
        if (AudioMdl_ != nullptr) {
            if (AudioBuffer_ != nullptr) {
                RtlSecureZeroMemory(AudioBuffer_, BufferBytes_);
                PortStream_->UnmapAllocatedPages(AudioBuffer_, AudioMdl_);
            }
            PortStream_->FreePagesFromMdl(AudioMdl_);
        }
        AudioMdl_ = nullptr;
        AudioBuffer_ = nullptr;
        BufferBytes_ = 0;
        NotificationCount_ = 0;
    }

    void StopTimer() {
        InterlockedExchange(&Running_, 0);
        KeCancelTimer(&Timer_);
        KeFlushQueuedDpcs();
    }

    void StopAndReset() {
        StopTimer();
        if (AudioBuffer_ != nullptr) {
            RtlSecureZeroMemory(AudioBuffer_, BufferBytes_);
        }
        InterlockedExchange(&Position_, 0);
        BytesSinceNotification_ = 0;
        LastServiceTime100ns_ = 0;
        State_ = KSSTATE_STOP;
    }

    void OnTimer() {
        if (InterlockedCompareExchange(&Running_, 0, 0) == 0 ||
            InterlockedCompareExchange(&g_Driver.Removed, 0, 0) != 0 ||
            InterlockedCompareExchange(&g_Driver.PoweredD0, 0, 0) == 0 ||
            AudioBuffer_ == nullptr || BufferBytes_ == 0) {
            InterlockedExchange(&Running_, 0);
            KeCancelTimer(&Timer_);
            return;
        }

        const ULONGLONG now = KeQueryInterruptTimePrecise(nullptr);
        const ULONGLONG previous = LastServiceTime100ns_;
        if (previous == 0 || now <= previous) {
            LastServiceTime100ns_ = now;
            return;
        }
        const ULONGLONG framesDue =
            (now - previous) / ARCEN_FRAME_PERIOD_100NS;
        if (framesDue == 0) {
            return;
        }
        LastServiceTime100ns_ =
            previous + framesDue * ARCEN_FRAME_PERIOD_100NS;

        ULONG position = static_cast<ULONG>(
            InterlockedCompareExchange(&Position_, 0, 0));
        ULONGLONG framesToWrite = framesDue;
        if (framesDue > ARCEN_MAX_CATCHUP_FRAMES) {
            RtlSecureZeroMemory(AudioBuffer_, BufferBytes_);
            ArcenMicrophoneRingClear(
                &g_Driver.Ring,
                ReaderIdentity_.WtsSessionId,
                ReaderGeneration_);
            const ULONGLONG skipped = framesDue - ARCEN_MAX_CATCHUP_FRAMES;
            const ULONG bufferFrames =
                BufferBytes_ / ARCEN_MICROPHONE_FRAME_BYTES;
            position = (
                position +
                static_cast<ULONG>(skipped % bufferFrames) *
                    ARCEN_MICROPHONE_FRAME_BYTES
            ) % BufferBytes_;
            framesToWrite = ARCEN_MAX_CATCHUP_FRAMES;
        }

        for (ULONGLONG index = 0; index < framesToWrite; ++index) {
            SHORT frame[ARCEN_MICROPHONE_FRAME_SAMPLES];
            RtlSecureZeroMemory(frame, sizeof(frame));
            NTSTATUS readStatus = STATUS_DEVICE_NOT_READY;
            if (ReaderGeneration_ != 0) {
                readStatus = ArcenMicrophoneRingRead(
                    &g_Driver.Ring,
                    ReaderIdentity_.WtsSessionId,
                    ReaderGeneration_,
                    frame);
            }
            if (!NT_SUCCESS(readStatus)) {
                ULONG generation = 0;
                const NTSTATUS authorize =
                    ArcenMicrophoneRingAuthorizeReader(
                        &g_Driver.Ring,
                        ReaderIdentity_.WtsSessionId,
                        &generation);
                if (NT_SUCCESS(authorize)) {
                    ReaderGeneration_ = generation;
                    ArcenMicrophoneRingRead(
                        &g_Driver.Ring,
                        ReaderIdentity_.WtsSessionId,
                        ReaderGeneration_,
                        frame);
                } else if (authorize == STATUS_DEVICE_NOT_READY) {
                    ReaderGeneration_ = 0;
                }
            }
            const ULONG first =
                min(ARCEN_MICROPHONE_FRAME_BYTES, BufferBytes_ - position);
            RtlCopyMemory(
                AudioBuffer_ + position, frame, first);
            if (first < ARCEN_MICROPHONE_FRAME_BYTES) {
                RtlCopyMemory(
                    AudioBuffer_,
                    reinterpret_cast<PUCHAR>(frame) + first,
                    ARCEN_MICROPHONE_FRAME_BYTES - first);
            }
            RtlSecureZeroMemory(frame, sizeof(frame));
            position =
                (position + ARCEN_MICROPHONE_FRAME_BYTES) % BufferBytes_;
        }
        InterlockedExchange(&Position_, static_cast<LONG>(position));

        if (NotificationCount_ != 0) {
            const ULONG interval = BufferBytes_ / NotificationCount_;
            const ULONGLONG advanced =
                framesDue * ARCEN_MICROPHONE_FRAME_BYTES;
            const BOOLEAN notify =
                advanced >= interval ||
                BytesSinceNotification_ + advanced >= interval;
            BytesSinceNotification_ = static_cast<ULONG>(
                (BytesSinceNotification_ + (advanced % interval)) %
                interval);
            if (notify) {
                KIRQL oldIrql;
                KeAcquireSpinLock(&EventLock_, &oldIrql);
                for (ULONG index = 0; index < EventCount_; ++index) {
                    KeSetEvent(Events_[index], IO_SOUND_INCREMENT, FALSE);
                }
                KeReleaseSpinLock(&EventLock_, oldIrql);
            }
        }
    }

    static VOID TimerDpc(
        _In_ KDPC*,
        _In_opt_ PVOID DeferredContext,
        _In_opt_ PVOID,
        _In_opt_ PVOID) {
        static_cast<WaveRtStream*>(DeferredContext)->OnTimer();
    }

    volatile LONG References_;
    PPORTWAVERTSTREAM PortStream_;
    PMDL AudioMdl_;
    PUCHAR AudioBuffer_;
    ULONG BufferBytes_;
    volatile LONG Position_;
    KSSTATE State_;
    ULONG NotificationCount_;
    ULONG BytesSinceNotification_;
    volatile LONG Running_;
    KTIMER Timer_;
    KDPC Dpc_;
    KSPIN_LOCK EventLock_;
    PKEVENT Events_[2];
    ULONG EventCount_;
    ULONG ReaderGeneration_;
    ARCEN_CAPTURE_IDENTITY ReaderIdentity_;
    BOOLEAN Registered_;
    ULONGLONG LastServiceTime100ns_;
};

class MiniportWaveRt final : public IMiniportWaveRT {
public:
    MiniportWaveRt() : References_(1) {}

    STDMETHODIMP QueryInterface(REFIID Interface, PVOID* Object) override {
        if (Object == nullptr) {
            return STATUS_INVALID_PARAMETER;
        }
        *Object = nullptr;
        if (IsEqualGuid(Interface, IID_IUnknown) ||
            IsEqualGuid(Interface, IID_IMiniport) ||
            IsEqualGuid(Interface, IID_IMiniportWaveRT)) {
            *Object = static_cast<IMiniportWaveRT*>(this);
            AddRef();
            return STATUS_SUCCESS;
        }
        return STATUS_NOINTERFACE;
    }

    STDMETHODIMP_(ULONG) AddRef() override {
        return static_cast<ULONG>(InterlockedIncrement(&References_));
    }

    STDMETHODIMP_(ULONG) Release() override {
        const LONG remaining = InterlockedDecrement(&References_);
        if (remaining == 0) {
            FreeObject(this);
        }
        return static_cast<ULONG>(remaining);
    }

    STDMETHODIMP GetDescription(PPCFILTER_DESCRIPTOR* Description) override {
        if (Description == nullptr) {
            return STATUS_INVALID_PARAMETER;
        }
        *Description = &g_WaveFilter;
        return STATUS_SUCCESS;
    }

    STDMETHODIMP DataRangeIntersection(
        ULONG PinId,
        PKSDATARANGE DataRange,
        PKSDATARANGE MatchingDataRange,
        ULONG OutputBufferLength,
        PVOID ResultantFormat,
        PULONG ResultantFormatLength) override {
        return FixedDataRangeIntersection(
            PinId,
            DataRange,
            MatchingDataRange,
            OutputBufferLength,
            ResultantFormat,
            ResultantFormatLength);
    }

    STDMETHODIMP Init(PUNKNOWN, PRESOURCELIST, PPORTWAVERT) override {
        return STATUS_SUCCESS;
    }

    STDMETHODIMP NewStream(
        PMINIPORTWAVERTSTREAM* Stream,
        PPORTWAVERTSTREAM PortStream,
        ULONG Pin,
        BOOLEAN Capture,
        PKSDATAFORMAT DataFormat) override {
        if (Stream != nullptr) {
            *Stream = nullptr;
        }
        if (Stream == nullptr || PortStream == nullptr || Pin != 0 ||
            !Capture || !IsFixedPcmFormat(DataFormat) ||
            InterlockedCompareExchange(&g_Driver.Removed, 0, 0) != 0) {
            return STATUS_INVALID_PARAMETER;
        }
        ARCEN_CAPTURE_IDENTITY identity;
        const NTSTATUS identityStatus =
            CaptureIdentityForCurrentCreate(&identity);
        if (!NT_SUCCESS(identityStatus)) {
            return identityStatus;
        }
        auto* stream = AllocateObject<WaveRtStream>(PortStream, identity);
        RtlSecureZeroMemory(&identity, sizeof(identity));
        if (stream == nullptr) {
            return STATUS_INSUFFICIENT_RESOURCES;
        }
        const NTSTATUS registerStatus = stream->Register();
        if (!NT_SUCCESS(registerStatus)) {
            stream->Release();
            return registerStatus;
        }
        *Stream = static_cast<IMiniportWaveRTStream*>(stream);
        return STATUS_SUCCESS;
    }

    STDMETHODIMP GetDeviceDescription(
        PDEVICE_DESCRIPTION DeviceDescription) override {
        if (DeviceDescription == nullptr) {
            return STATUS_INVALID_PARAMETER;
        }
        RtlZeroMemory(DeviceDescription, sizeof(*DeviceDescription));
        DeviceDescription->Version = DEVICE_DESCRIPTION_VERSION;
        DeviceDescription->Master = FALSE;
        DeviceDescription->ScatterGather = TRUE;
        DeviceDescription->Dma32BitAddresses = TRUE;
        DeviceDescription->Dma64BitAddresses = TRUE;
        DeviceDescription->InterfaceType = Internal;
        DeviceDescription->MaximumLength = ARCEN_MAX_WAVERT_BUFFER_BYTES;
        return STATUS_SUCCESS;
    }

private:
    volatile LONG References_;
};

class AdapterPower final : public IAdapterPowerManagement {
public:
    AdapterPower() : References_(1) {}

    STDMETHODIMP QueryInterface(REFIID Interface, PVOID* Object) override {
        if (Object == nullptr) {
            return STATUS_INVALID_PARAMETER;
        }
        *Object = nullptr;
        if (IsEqualGuid(Interface, IID_IUnknown) ||
            IsEqualGuid(Interface, IID_IAdapterPowerManagement)) {
            *Object = static_cast<IAdapterPowerManagement*>(this);
            AddRef();
            return STATUS_SUCCESS;
        }
        return STATUS_NOINTERFACE;
    }

    STDMETHODIMP_(ULONG) AddRef() override {
        return static_cast<ULONG>(InterlockedIncrement(&References_));
    }

    STDMETHODIMP_(ULONG) Release() override {
        const LONG remaining = InterlockedDecrement(&References_);
        if (remaining == 0) {
            FreeObject(this);
        }
        return static_cast<ULONG>(remaining);
    }

    STDMETHODIMP_(VOID) PowerChangeState(POWER_STATE NewState) override {
        const BOOLEAN d0 = NewState.DeviceState == PowerDeviceD0;
        InterlockedExchange(&g_Driver.PoweredD0, d0 ? 1 : 0);
        if (!d0) {
            WaveRtStream::QuiesceAll();
            ARCEN_MICROPHONE_STATUS_RESPONSE status;
            ArcenMicrophoneRingStatus(&g_Driver.Ring, &status);
            if (status.State == ArcenMicrophoneStateBound) {
                ArcenMicrophoneRingClear(
                    &g_Driver.Ring, status.WtsSessionId, status.Generation);
            }
        }
    }

    STDMETHODIMP QueryPowerChangeState(POWER_STATE) override {
        return STATUS_SUCCESS;
    }

    STDMETHODIMP QueryDeviceCapabilities(PDEVICE_CAPABILITIES) override {
        return STATUS_SUCCESS;
    }

private:
    volatile LONG References_;
};

NTSTATUS CreateControlDevice(_In_ PDRIVER_OBJECT DriverObject) {
    if (g_Driver.ControlDevice != nullptr) {
        if (InterlockedCompareExchange(&g_Driver.Removed, 0, 0) != 0) {
            return STATUS_DELETE_PENDING;
        }
        if (g_Driver.InterfaceName.Buffer != nullptr) {
            const NTSTATUS status =
                IoSetDeviceInterfaceState(&g_Driver.InterfaceName, TRUE);
            if (!NT_SUCCESS(status)) {
                return status;
            }
        }
        return STATUS_SUCCESS;
    }
    UNICODE_STRING deviceName;
    UNICODE_STRING sddl;
    RtlInitUnicodeString(&deviceName, L"\\Device\\ArcenMicrophone");
    RtlInitUnicodeString(&g_ControlDosName, L"\\DosDevices\\ArcenMicrophone");
    RtlInitUnicodeString(
        &sddl,
        L"D:P(A;;GA;;;S-1-5-80-2794664030-2322002993-548807306-4095822587-2900116599)");

    NTSTATUS status = IoCreateDeviceSecure(
        DriverObject,
        sizeof(ARCEN_CONTROL_EXTENSION),
        &deviceName,
        ARCEN_MICROPHONE_DEVICE_TYPE,
        FILE_DEVICE_SECURE_OPEN,
        FALSE,
        &sddl,
        &GUID_DEVCLASS_ARCEN_MICROPHONE,
        &g_Driver.ControlDevice);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    auto* extension = static_cast<ARCEN_CONTROL_EXTENSION*>(
        g_Driver.ControlDevice->DeviceExtension);
    IoInitializeRemoveLock(
        &extension->RemoveLock, ARCEN_POOL_TAG, 0, 0);
    g_Driver.ControlDevice->Flags |= DO_BUFFERED_IO;

    status = IoCreateSymbolicLink(&g_ControlDosName, &deviceName);
    if (!NT_SUCCESS(status)) {
        IoDeleteDevice(g_Driver.ControlDevice);
        g_Driver.ControlDevice = nullptr;
        return status;
    }

    PDEVICE_OBJECT physicalDevice = nullptr;
    status = PcGetPhysicalDeviceObject(
        g_Driver.AudioDevice, &physicalDevice);
    if (NT_SUCCESS(status)) {
        status = IoRegisterDeviceInterface(
            physicalDevice,
            &GUID_DEVINTERFACE_ARCEN_MICROPHONE_CONTROL,
            nullptr,
            &g_Driver.InterfaceName);
    }
    if (NT_SUCCESS(status)) {
        status = IoSetDeviceInterfaceState(&g_Driver.InterfaceName, TRUE);
    }
    if (!NT_SUCCESS(status)) {
        IoDeleteSymbolicLink(&g_ControlDosName);
        IoDeleteDevice(g_Driver.ControlDevice);
        g_Driver.ControlDevice = nullptr;
        if (g_Driver.InterfaceName.Buffer != nullptr) {
            RtlFreeUnicodeString(&g_Driver.InterfaceName);
            RtlZeroMemory(
                &g_Driver.InterfaceName, sizeof(g_Driver.InterfaceName));
        }
        return status;
    }

    g_Driver.ControlDevice->Flags &= ~DO_DEVICE_INITIALIZING;
    return STATUS_SUCCESS;
}

void UnbindOwnerLocked() {
    if (g_Driver.Owner == nullptr) {
        return;
    }
    auto* context = static_cast<ARCEN_FILE_CONTEXT*>(
        g_Driver.Owner->FsContext);
    if (context != nullptr && context->Binding.Active) {
        ArcenMicrophoneRingUnbind(
            &g_Driver.Ring,
            context->Binding.WtsSessionId,
            context->Binding.Generation);
        RtlSecureZeroMemory(&context->Binding, sizeof(context->Binding));
    }
    g_Driver.Owner = nullptr;
}

void MarkRemoved() {
    if (InterlockedExchange(&g_Driver.Removed, 1) != 0) {
        return;
    }
    InterlockedExchange(&g_Driver.PoweredD0, 0);
    WaveRtStream::QuiesceAll();
    ExAcquireFastMutex(&g_Driver.ControlLock);
    UnbindOwnerLocked();
    ExReleaseFastMutex(&g_Driver.ControlLock);
    if (g_Driver.InterfaceName.Buffer != nullptr) {
        IoSetDeviceInterfaceState(&g_Driver.InterfaceName, FALSE);
    }
}

NTSTATUS DispatchControlCreate(_Inout_ PIRP Irp) {
    if (InterlockedCompareExchange(&g_Driver.Removed, 0, 0) != 0 ||
        !RequestorHasServiceSid(Irp)) {
        return CompleteIrp(Irp, STATUS_ACCESS_DENIED);
    }
    auto* stack = IoGetCurrentIrpStackLocation(Irp);
    if (stack->FileObject == nullptr || stack->FileObject->FsContext != nullptr) {
        return CompleteIrp(Irp, STATUS_INVALID_DEVICE_STATE);
    }
    auto* context = static_cast<ARCEN_FILE_CONTEXT*>(
        ExAllocatePoolZero(
            NonPagedPoolNx, sizeof(ARCEN_FILE_CONTEXT), ARCEN_POOL_TAG));
    if (context == nullptr) {
        return CompleteIrp(Irp, STATUS_INSUFFICIENT_RESOURCES);
    }
    RtlSecureZeroMemory(context, sizeof(*context));
    ExAcquireFastMutex(&g_Driver.ControlLock);
    if (++g_Driver.OpenHandles == 1) {
        g_Driver.HighestGeneration = 0;
    }
    stack->FileObject->FsContext = context;
    ExReleaseFastMutex(&g_Driver.ControlLock);
    return CompleteIrp(Irp, STATUS_SUCCESS);
}

NTSTATUS DispatchControlCleanup(_Inout_ PIRP Irp) {
    auto* stack = IoGetCurrentIrpStackLocation(Irp);
    if (stack->FileObject != nullptr) {
        ExAcquireFastMutex(&g_Driver.ControlLock);
        if (g_Driver.Owner == stack->FileObject) {
            UnbindOwnerLocked();
        }
        auto* context = static_cast<ARCEN_FILE_CONTEXT*>(
            stack->FileObject->FsContext);
        stack->FileObject->FsContext = nullptr;
        if (g_Driver.OpenHandles > 0 && --g_Driver.OpenHandles == 0) {
            g_Driver.HighestGeneration = 0;
        }
        ExReleaseFastMutex(&g_Driver.ControlLock);
        if (context != nullptr) {
            RtlSecureZeroMemory(context, sizeof(*context));
            ExFreePoolWithTag(context, ARCEN_POOL_TAG);
        }
    }
    return CompleteIrp(Irp, STATUS_SUCCESS);
}

NTSTATUS DispatchControlDeviceControl(_Inout_ PIRP Irp) {
    auto* stack = IoGetCurrentIrpStackLocation(Irp);
    if (stack->FileObject == nullptr) {
        return CompleteIrp(Irp, STATUS_DEVICE_REMOVED);
    }
    const ULONG inputLength =
        stack->Parameters.DeviceIoControl.InputBufferLength;
    const ULONG outputLength =
        stack->Parameters.DeviceIoControl.OutputBufferLength;
    PVOID buffer = Irp->AssociatedIrp.SystemBuffer;
    NTSTATUS status = STATUS_INVALID_DEVICE_REQUEST;
    ULONG_PTR information = 0;

    ExAcquireFastMutex(&g_Driver.ControlLock);
    auto* context = static_cast<ARCEN_FILE_CONTEXT*>(
        stack->FileObject->FsContext);
    if (context == nullptr ||
        InterlockedCompareExchange(&g_Driver.Removed, 0, 0) != 0) {
        status = STATUS_DEVICE_REMOVED;
    } else if (InterlockedCompareExchange(&g_Driver.PoweredD0, 0, 0) == 0) {
        status = STATUS_DEVICE_NOT_READY;
    } else {
        switch (stack->Parameters.DeviceIoControl.IoControlCode) {
        case IOCTL_ARCEN_MICROPHONE_BIND: {
            if (inputLength != sizeof(ARCEN_MICROPHONE_BIND_REQUEST) ||
                outputLength != 0 || buffer == nullptr) {
                status = STATUS_INFO_LENGTH_MISMATCH;
                break;
            }
            if (g_Driver.Owner != nullptr &&
                g_Driver.Owner != stack->FileObject) {
                status = STATUS_SHARING_VIOLATION;
                break;
            }
            const auto* request =
                static_cast<const ARCEN_MICROPHONE_BIND_REQUEST*>(buffer);
            if (g_Driver.HighestGeneration != 0 &&
                !GenerationIsAfter(
                    request->Generation, g_Driver.HighestGeneration)) {
                RtlSecureZeroMemory(
                    buffer, sizeof(ARCEN_MICROPHONE_BIND_REQUEST));
                status = STATUS_ACCESS_DENIED;
                break;
            }
            status = ArcenMicrophoneRingBind(&g_Driver.Ring, request);
            if (NT_SUCCESS(status)) {
                RtlSecureZeroMemory(
                    &context->Binding, sizeof(context->Binding));
                context->Binding.WtsSessionId = request->WtsSessionId;
                context->Binding.Generation = request->Generation;
                context->Binding.SidLength = request->SidLength;
                RtlCopyMemory(
                    context->Binding.Sid,
                    request->Sid,
                    request->SidLength);
                context->Binding.Active = TRUE;
                g_Driver.Owner = stack->FileObject;
                g_Driver.HighestGeneration = request->Generation;
            }
            RtlSecureZeroMemory(buffer, sizeof(ARCEN_MICROPHONE_BIND_REQUEST));
            break;
        }
        case IOCTL_ARCEN_MICROPHONE_FEED: {
            if (inputLength != sizeof(ARCEN_MICROPHONE_FEED_REQUEST) ||
                outputLength != 0 || buffer == nullptr) {
                status = STATUS_INFO_LENGTH_MISMATCH;
                break;
            }
            const auto* request =
                static_cast<const ARCEN_MICROPHONE_FEED_REQUEST*>(buffer);
            if (g_Driver.Owner != stack->FileObject ||
                !context->Binding.Active ||
                request->Version != ARCEN_MICROPHONE_CONTRACT_VERSION ||
                request->Generation != context->Binding.Generation ||
                request->FrameBytes != ARCEN_MICROPHONE_FRAME_BYTES ||
                request->Reserved != 0) {
                RtlSecureZeroMemory(
                    buffer, sizeof(ARCEN_MICROPHONE_FEED_REQUEST));
                status = STATUS_ACCESS_DENIED;
                break;
            }
            status = ArcenMicrophoneRingWrite(
                &g_Driver.Ring,
                context->Binding.WtsSessionId,
                context->Binding.Generation,
                context->Binding.Sid,
                context->Binding.SidLength,
                request->Frame);
            RtlSecureZeroMemory(
                buffer, sizeof(ARCEN_MICROPHONE_FEED_REQUEST));
            break;
        }
        case IOCTL_ARCEN_MICROPHONE_STOP: {
            if (inputLength != sizeof(ARCEN_MICROPHONE_STOP_REQUEST) ||
                outputLength != 0 || buffer == nullptr) {
                status = STATUS_INFO_LENGTH_MISMATCH;
                break;
            }
            const auto* request =
                static_cast<const ARCEN_MICROPHONE_STOP_REQUEST*>(buffer);
            if (request->Version != ARCEN_MICROPHONE_CONTRACT_VERSION ||
                g_Driver.Owner != stack->FileObject ||
                !context->Binding.Active ||
                request->Generation != context->Binding.Generation) {
                RtlSecureZeroMemory(
                    buffer, sizeof(ARCEN_MICROPHONE_STOP_REQUEST));
                status = STATUS_ACCESS_DENIED;
                break;
            }
            UnbindOwnerLocked();
            RtlSecureZeroMemory(
                buffer, sizeof(ARCEN_MICROPHONE_STOP_REQUEST));
            status = STATUS_SUCCESS;
            break;
        }
        case IOCTL_ARCEN_MICROPHONE_STATUS: {
            if (inputLength != 0 ||
                outputLength < sizeof(ARCEN_MICROPHONE_STATUS_RESPONSE) ||
                buffer == nullptr) {
                status = STATUS_INFO_LENGTH_MISMATCH;
                break;
            }
            auto* response =
                static_cast<ARCEN_MICROPHONE_STATUS_RESPONSE*>(buffer);
            ArcenMicrophoneRingStatus(&g_Driver.Ring, response);
            information = sizeof(*response);
            status = STATUS_SUCCESS;
            break;
        }
        default:
            status = STATUS_INVALID_DEVICE_REQUEST;
            break;
        }
    }
    ExReleaseFastMutex(&g_Driver.ControlLock);
    return CompleteIrp(Irp, status, information);
}

BOOLEAN IsControlDevice(_In_opt_ PDEVICE_OBJECT DeviceObject) {
    return DeviceObject != nullptr &&
           DeviceObject->DeviceType == ARCEN_MICROPHONE_DEVICE_TYPE;
}

ARCEN_CONTROL_EXTENSION* ControlExtension(
    _In_ PDEVICE_OBJECT DeviceObject) {
    return static_cast<ARCEN_CONTROL_EXTENSION*>(
        DeviceObject->DeviceExtension);
}

NTSTATUS DispatchCreate(_In_ PDEVICE_OBJECT DeviceObject, _Inout_ PIRP Irp) {
    if (!IsControlDevice(DeviceObject)) {
        ARCEN_CAPTURE_CREATE_CONTEXT context;
        RtlSecureZeroMemory(&context, sizeof(context));
        const NTSTATUS identityStatus =
            CaptureRequestorIdentity(Irp, &context.Identity);
        if (!NT_SUCCESS(identityStatus)) {
            return g_PortClsCreate(DeviceObject, Irp);
        }
        if (InterlockedCompareExchange(&g_Driver.PoweredD0, 0, 0) == 0) {
            return CompleteIrp(Irp, STATUS_DEVICE_NOT_READY);
        }
        RegisterCaptureCreateContext(&context);
        const NTSTATUS status = g_PortClsCreate(DeviceObject, Irp);
        UnregisterCaptureCreateContext(&context);
        return status;
    }
    if (DeviceObject != g_Driver.ControlDevice) {
        return CompleteIrp(Irp, STATUS_DELETE_PENDING);
    }
    auto* extension = ControlExtension(DeviceObject);
    const NTSTATUS lockStatus =
        IoAcquireRemoveLock(&extension->RemoveLock, Irp);
    if (!NT_SUCCESS(lockStatus)) {
        return CompleteIrp(Irp, lockStatus);
    }
    const NTSTATUS status = DispatchControlCreate(Irp);
    IoReleaseRemoveLock(&extension->RemoveLock, Irp);
    return status;
}

NTSTATUS DispatchCleanup(_In_ PDEVICE_OBJECT DeviceObject, _Inout_ PIRP Irp) {
    if (!IsControlDevice(DeviceObject)) {
        return g_PortClsCleanup(DeviceObject, Irp);
    }
    if (DeviceObject != g_Driver.ControlDevice ||
        InterlockedCompareExchange(&g_Driver.Removed, 0, 0) != 0) {
        return DispatchControlCleanup(Irp);
    }
    auto* extension = ControlExtension(DeviceObject);
    const NTSTATUS lockStatus =
        IoAcquireRemoveLock(&extension->RemoveLock, Irp);
    if (!NT_SUCCESS(lockStatus)) {
        if (InterlockedCompareExchange(&g_Driver.Removed, 0, 0) != 0) {
            return DispatchControlCleanup(Irp);
        }
        return CompleteIrp(Irp, lockStatus);
    }
    const NTSTATUS status = DispatchControlCleanup(Irp);
    IoReleaseRemoveLock(&extension->RemoveLock, Irp);
    return status;
}

NTSTATUS DispatchClose(_In_ PDEVICE_OBJECT DeviceObject, _Inout_ PIRP Irp) {
    if (!IsControlDevice(DeviceObject)) {
        return g_PortClsClose(DeviceObject, Irp);
    }
    return CompleteIrp(Irp, STATUS_SUCCESS);
}

NTSTATUS DispatchDeviceControl(
    _In_ PDEVICE_OBJECT DeviceObject,
    _Inout_ PIRP Irp) {
    if (!IsControlDevice(DeviceObject)) {
        return g_PortClsDeviceControl(DeviceObject, Irp);
    }
    if (DeviceObject != g_Driver.ControlDevice) {
        return CompleteIrp(Irp, STATUS_DELETE_PENDING);
    }
    auto* extension = ControlExtension(DeviceObject);
    const NTSTATUS lockStatus =
        IoAcquireRemoveLock(&extension->RemoveLock, Irp);
    if (!NT_SUCCESS(lockStatus)) {
        return CompleteIrp(Irp, lockStatus);
    }
    const NTSTATUS status = DispatchControlDeviceControl(Irp);
    IoReleaseRemoveLock(&extension->RemoveLock, Irp);
    return status;
}

NTSTATUS DispatchPnp(_In_ PDEVICE_OBJECT DeviceObject, _Inout_ PIRP Irp) {
    if (DeviceObject != g_Driver.AudioDevice) {
        return g_PortClsPnp(DeviceObject, Irp);
    }
    auto* stack = IoGetCurrentIrpStackLocation(Irp);
    if (stack->MinorFunction == IRP_MN_SURPRISE_REMOVAL) {
        MarkRemoved();
    } else if (stack->MinorFunction == IRP_MN_STOP_DEVICE) {
        InterlockedExchange(&g_Driver.PoweredD0, 0);
        WaveRtStream::QuiesceAll();
        ExAcquireFastMutex(&g_Driver.ControlLock);
        UnbindOwnerLocked();
        ExReleaseFastMutex(&g_Driver.ControlLock);
        if (g_Driver.InterfaceName.Buffer != nullptr) {
            IoSetDeviceInterfaceState(&g_Driver.InterfaceName, FALSE);
        }
    } else if (stack->MinorFunction == IRP_MN_REMOVE_DEVICE) {
        PDEVICE_OBJECT controlDevice = g_Driver.ControlDevice;
        auto* extension = controlDevice == nullptr
                              ? nullptr
                              : ControlExtension(controlDevice);
        const NTSTATUS lockStatus =
            extension == nullptr
                ? STATUS_DELETE_PENDING
                : IoAcquireRemoveLock(&extension->RemoveLock, Irp);
        MarkRemoved();
        if (NT_SUCCESS(lockStatus)) {
            IoReleaseRemoveLockAndWait(
                &extension->RemoveLock, Irp);
        }
        const NTSTATUS status = g_PortClsPnp(DeviceObject, Irp);
        IoDeleteSymbolicLink(&g_ControlDosName);
        if (g_Driver.InterfaceName.Buffer != nullptr) {
            RtlFreeUnicodeString(&g_Driver.InterfaceName);
            RtlZeroMemory(
                &g_Driver.InterfaceName, sizeof(g_Driver.InterfaceName));
        }
        if (g_Driver.ControlDevice != nullptr) {
            IoDeleteDevice(g_Driver.ControlDevice);
            g_Driver.ControlDevice = nullptr;
        }
        g_Driver.AudioDevice = nullptr;
        return status;
    }
    return g_PortClsPnp(DeviceObject, Irp);
}

NTSTATUS RegisterSubdevices(
    _In_ PDEVICE_OBJECT DeviceObject,
    _In_ PIRP Irp,
    _In_ PRESOURCELIST ResourceList,
    _In_ PUNKNOWN AdapterUnknown) {
    PPORT topologyPort = nullptr;
    PPORT wavePort = nullptr;
    MiniportTopology* topologyMiniport = nullptr;
    MiniportWaveRt* waveMiniport = nullptr;

    NTSTATUS status = PcNewPort(&topologyPort, CLSID_PortTopology);
    if (!NT_SUCCESS(status)) {
        goto Exit;
    }
    topologyMiniport = AllocateObject<MiniportTopology>();
    if (topologyMiniport == nullptr) {
        status = STATUS_INSUFFICIENT_RESOURCES;
        goto Exit;
    }
    status = topologyPort->Init(
        DeviceObject,
        Irp,
        static_cast<IMiniportTopology*>(topologyMiniport),
        AdapterUnknown,
        ResourceList);
    if (!NT_SUCCESS(status)) {
        goto Exit;
    }
    status = PcRegisterSubdevice(
        DeviceObject, g_TopologyName, topologyPort);
    if (!NT_SUCCESS(status)) {
        goto Exit;
    }

    status = PcNewPort(&wavePort, CLSID_PortWaveRT);
    if (!NT_SUCCESS(status)) {
        goto Exit;
    }
    waveMiniport = AllocateObject<MiniportWaveRt>();
    if (waveMiniport == nullptr) {
        status = STATUS_INSUFFICIENT_RESOURCES;
        goto Exit;
    }
    status = wavePort->Init(
        DeviceObject,
        Irp,
        static_cast<IMiniportWaveRT*>(waveMiniport),
        AdapterUnknown,
        ResourceList);
    if (!NT_SUCCESS(status)) {
        goto Exit;
    }
    status = PcRegisterSubdevice(DeviceObject, g_WaveName, wavePort);
    if (!NT_SUCCESS(status)) {
        goto Exit;
    }
    status = PcRegisterPhysicalConnection(
        DeviceObject, topologyPort, 0, wavePort, 1);

Exit:
    if (waveMiniport != nullptr) {
        waveMiniport->Release();
    }
    if (wavePort != nullptr) {
        wavePort->Release();
    }
    if (topologyMiniport != nullptr) {
        topologyMiniport->Release();
    }
    if (topologyPort != nullptr) {
        topologyPort->Release();
    }
    return status;
}

NTSTATUS StartDevice(
    _In_ PDEVICE_OBJECT DeviceObject,
    _In_ PIRP Irp,
    _In_ PRESOURCELIST ResourceList) {
    if (g_Driver.AudioDevice != nullptr &&
        g_Driver.AudioDevice != DeviceObject) {
        return STATUS_DEVICE_BUSY;
    }
    g_Driver.AudioDevice = DeviceObject;
    InterlockedExchange(&g_Driver.Removed, 0);
    InterlockedExchange(&g_Driver.PoweredD0, 1);

    auto* power = AllocateObject<AdapterPower>();
    if (power == nullptr) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    NTSTATUS status = RegisterSubdevices(
        DeviceObject,
        Irp,
        ResourceList,
        static_cast<IAdapterPowerManagement*>(power));
    if (NT_SUCCESS(status)) {
        status = PcRegisterAdapterPowerManagement(
            static_cast<IAdapterPowerManagement*>(power), DeviceObject);
    }
    power->Release();
    if (!NT_SUCCESS(status)) {
        return status;
    }
    return CreateControlDevice(DeviceObject->DriverObject);
}

NTSTATUS AddDevice(
    _In_ PDRIVER_OBJECT DriverObject,
    _In_ PDEVICE_OBJECT PhysicalDeviceObject) {
    return PcAddAdapterDevice(
        DriverObject,
        PhysicalDeviceObject,
        StartDevice,
        2,
        0);
}

void DriverUnload(_In_ PDRIVER_OBJECT) {
    MarkRemoved();
    IoDeleteSymbolicLink(&g_ControlDosName);
    if (g_Driver.InterfaceName.Buffer != nullptr) {
        RtlFreeUnicodeString(&g_Driver.InterfaceName);
    }
    if (g_Driver.ControlDevice != nullptr) {
        IoDeleteDevice(g_Driver.ControlDevice);
        g_Driver.ControlDevice = nullptr;
    }
}

}  // namespace

extern "C" DRIVER_INITIALIZE DriverEntry;

extern "C" NTSTATUS DriverEntry(
    _In_ PDRIVER_OBJECT DriverObject,
    _In_ PUNICODE_STRING RegistryPath) {
    ExInitializeDriverRuntime(0);
    RtlZeroMemory(&g_Driver, sizeof(g_Driver));
    ArcenMicrophoneRingInitialize(&g_Driver.Ring);
    ExInitializeFastMutex(&g_Driver.ControlLock);
    KeInitializeSpinLock(&g_Driver.CaptureCreateLock);
    InitializeListHead(&g_Driver.CaptureCreateContexts);
    KeInitializeSpinLock(&g_Driver.StreamLock);
    const NTSTATUS status =
        PcInitializeAdapterDriver(DriverObject, RegistryPath, AddDevice);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    g_PortClsCreate = DriverObject->MajorFunction[IRP_MJ_CREATE];
    g_PortClsCleanup = DriverObject->MajorFunction[IRP_MJ_CLEANUP];
    g_PortClsClose = DriverObject->MajorFunction[IRP_MJ_CLOSE];
    g_PortClsDeviceControl =
        DriverObject->MajorFunction[IRP_MJ_DEVICE_CONTROL];
    g_PortClsPnp = DriverObject->MajorFunction[IRP_MJ_PNP];
    DriverObject->MajorFunction[IRP_MJ_CREATE] = DispatchCreate;
    DriverObject->MajorFunction[IRP_MJ_CLEANUP] = DispatchCleanup;
    DriverObject->MajorFunction[IRP_MJ_CLOSE] = DispatchClose;
    DriverObject->MajorFunction[IRP_MJ_DEVICE_CONTROL] =
        DispatchDeviceControl;
    DriverObject->MajorFunction[IRP_MJ_PNP] = DispatchPnp;
    DriverObject->DriverUnload = DriverUnload;
    return STATUS_SUCCESS;
}
