#include <initguid.h>

#include <windows.h>
#include <wdf.h>
#include <d3d11.h>
#include <dxgi1_2.h>
#include <iddcx/1.4/IddCx.h>

#include "arcen_iddcx_contract.h"
#include "arcen_iddcx_guids.h"
#include "arcen_iddcx_model.h"

#include <array>
#include <cstdint>
#include <cstring>
#include <new>

extern "C" DRIVER_INITIALIZE DriverEntry;
EVT_WDF_DRIVER_DEVICE_ADD ArcenIddCxDeviceAdd;
EVT_WDF_OBJECT_CONTEXT_CLEANUP ArcenIddCxDeviceCleanup;
EVT_WDF_DEVICE_D0_ENTRY ArcenIddCxDeviceD0Entry;
EVT_WDF_DEVICE_D0_EXIT ArcenIddCxDeviceD0Exit;
EVT_WDF_FILE_CLEANUP ArcenIddCxFileCleanup;

EVT_IDD_CX_DEVICE_IO_CONTROL ArcenIddCxDeviceIoControl;
EVT_IDD_CX_PARSE_MONITOR_DESCRIPTION ArcenIddCxParseMonitorDescription;
EVT_IDD_CX_ADAPTER_INIT_FINISHED ArcenIddCxAdapterInitFinished;
EVT_IDD_CX_ADAPTER_COMMIT_MODES ArcenIddCxAdapterCommitModes;
EVT_IDD_CX_MONITOR_GET_DEFAULT_DESCRIPTION_MODES
    ArcenIddCxMonitorGetDefaultDescriptionModes;
EVT_IDD_CX_MONITOR_QUERY_TARGET_MODES ArcenIddCxMonitorQueryTargetModes;
EVT_IDD_CX_MONITOR_ASSIGN_SWAPCHAIN ArcenIddCxMonitorAssignSwapChain;
EVT_IDD_CX_MONITOR_UNASSIGN_SWAPCHAIN ArcenIddCxMonitorUnassignSwapChain;

namespace {

constexpr wchar_t EndpointModel[] = L"Arcen Dynamic Virtual Display";
constexpr wchar_t EndpointManufacturer[] = L"Arcen";
constexpr DWORD SwapChainStartTimeoutMs = 5000;

[[nodiscard]] bool LuidEquals(const LUID& left,
                              const ARCEN_IDDCX_ADAPTER_LUID& right) noexcept {
    return left.LowPart == right.LowPart &&
           left.HighPart == right.HighPart;
}

[[nodiscard]] ARCEN_IDDCX_ADAPTER_LUID ToContractLuid(
    const LUID& luid) noexcept {
    return {luid.LowPart, luid.HighPart};
}

[[nodiscard]] LUID ToWindowsLuid(
    const ARCEN_IDDCX_ADAPTER_LUID& luid) noexcept {
    LUID value{};
    value.LowPart = luid.LowPart;
    value.HighPart = luid.HighPart;
    return value;
}

[[nodiscard]] DISPLAYCONFIG_ROTATION ToRotation(
    const uint32_t degrees) noexcept {
    switch (degrees) {
        case 90:
            return DISPLAYCONFIG_ROTATION_ROTATE90;
        case 180:
            return DISPLAYCONFIG_ROTATION_ROTATE180;
        case 270:
            return DISPLAYCONFIG_ROTATION_ROTATE270;
        default:
            return DISPLAYCONFIG_ROTATION_IDENTITY;
    }
}

[[nodiscard]] DISPLAYCONFIG_VIDEO_SIGNAL_INFO BuildSignalInfo(
    const ARCEN_IDDCX_MODE& mode,
    const UINT vsyncDivider) noexcept {
    const UINT horizontalBlank =
        ((mode.Width / 5u < 160u ? 160u : mode.Width / 5u) + 7u) &
        ~7u;
    const UINT verticalBlank = 45u;
    const UINT horizontalTotal = mode.Width + horizontalBlank;
    const UINT verticalTotal = mode.Height + verticalBlank;
    const UINT64 pixelRate =
        static_cast<UINT64>(horizontalTotal) *
        static_cast<UINT64>(verticalTotal) *
        static_cast<UINT64>(mode.RefreshMillihz) / 1000u;

    DISPLAYCONFIG_VIDEO_SIGNAL_INFO signal{};
    signal.pixelRate = pixelRate;
    signal.hSyncFreq.Numerator =
        static_cast<UINT32>(pixelRate > UINT32_MAX ? UINT32_MAX
                                                   : pixelRate);
    signal.hSyncFreq.Denominator = horizontalTotal;
    signal.vSyncFreq.Numerator = mode.RefreshMillihz;
    signal.vSyncFreq.Denominator = 1000u;
    signal.activeSize.cx = mode.Width;
    signal.activeSize.cy = mode.Height;
    signal.totalSize.cx = horizontalTotal;
    signal.totalSize.cy = verticalTotal;
    signal.videoStandard = 255;
    signal.scanLineOrdering =
        DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE;
    signal.AdditionalSignalInfo.vSyncFreqDivider = vsyncDivider;
    return signal;
}

class SwapChainPump final {
public:
    SwapChainPump(const IDDCX_SWAPCHAIN swapChain,
                  const HANDLE surfaceAvailable,
                  const LUID renderAdapter) noexcept
        : swapChain_(swapChain),
          surfaceAvailable_(surfaceAvailable),
          renderAdapter_(renderAdapter),
          stopEvent_(nullptr),
          readyEvent_(nullptr),
          thread_(nullptr),
          startStatus_(E_FAIL) {}

    ~SwapChainPump() {
        Stop();
    }

    [[nodiscard]] NTSTATUS Start() noexcept {
        stopEvent_ = CreateEventW(nullptr, TRUE, FALSE, nullptr);
        readyEvent_ = CreateEventW(nullptr, TRUE, FALSE, nullptr);
        if (stopEvent_ == nullptr || readyEvent_ == nullptr) {
            Stop();
            return STATUS_INSUFFICIENT_RESOURCES;
        }
        thread_ = CreateThread(nullptr, 0, ThreadEntry, this, 0, nullptr);
        if (thread_ == nullptr) {
            Stop();
            return STATUS_INSUFFICIENT_RESOURCES;
        }
        const DWORD wait =
            WaitForSingleObject(readyEvent_, SwapChainStartTimeoutMs);
        if (wait != WAIT_OBJECT_0 || FAILED(startStatus_)) {
            const NTSTATUS status =
                wait == WAIT_OBJECT_0
                    ? static_cast<NTSTATUS>(startStatus_)
                    : STATUS_IO_TIMEOUT;
            Stop();
            return status;
        }
        return STATUS_SUCCESS;
    }

    void Stop() noexcept {
        if (stopEvent_ != nullptr) {
            SetEvent(stopEvent_);
        }
        if (thread_ != nullptr) {
            WaitForSingleObject(thread_, INFINITE);
            CloseHandle(thread_);
            thread_ = nullptr;
        }
        if (readyEvent_ != nullptr) {
            CloseHandle(readyEvent_);
            readyEvent_ = nullptr;
        }
        if (stopEvent_ != nullptr) {
            CloseHandle(stopEvent_);
            stopEvent_ = nullptr;
        }
    }

private:
    static DWORD WINAPI ThreadEntry(void* context) noexcept {
        static_cast<SwapChainPump*>(context)->Run();
        return 0;
    }

    void Run() noexcept {
        IDXGIFactory1* factory = nullptr;
        IDXGIAdapter1* adapter = nullptr;
        ID3D11Device* device = nullptr;
        ID3D11DeviceContext* deviceContext = nullptr;
        IDXGIDevice* dxgiDevice = nullptr;

        HRESULT status =
            CreateDXGIFactory1(IID_PPV_ARGS(&factory));
        if (SUCCEEDED(status)) {
            for (UINT index = 0;; ++index) {
                IDXGIAdapter1* candidate = nullptr;
                if (factory->EnumAdapters1(index, &candidate) ==
                    DXGI_ERROR_NOT_FOUND) {
                    break;
                }
                DXGI_ADAPTER_DESC1 description{};
                const HRESULT descriptionStatus =
                    candidate->GetDesc1(&description);
                if (SUCCEEDED(descriptionStatus) &&
                    description.AdapterLuid.LowPart ==
                        renderAdapter_.LowPart &&
                    description.AdapterLuid.HighPart ==
                        renderAdapter_.HighPart) {
                    adapter = candidate;
                    break;
                }
                candidate->Release();
            }
            if (adapter == nullptr) {
                status = DXGI_ERROR_NOT_FOUND;
            }
        }
        if (SUCCEEDED(status)) {
            D3D_FEATURE_LEVEL featureLevel{};
            status = D3D11CreateDevice(
                adapter, D3D_DRIVER_TYPE_UNKNOWN, nullptr,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0,
                D3D11_SDK_VERSION, &device, &featureLevel,
                &deviceContext);
        }
        if (SUCCEEDED(status)) {
            status = device->QueryInterface(IID_PPV_ARGS(&dxgiDevice));
        }
        if (SUCCEEDED(status)) {
            IDARG_IN_SWAPCHAINSETDEVICE setDevice{};
            setDevice.pDevice = dxgiDevice;
            status = IddCxSwapChainSetDevice(swapChain_, &setDevice);
        }
        startStatus_ = status;
        SetEvent(readyEvent_);

        if (SUCCEEDED(status)) {
            const HANDLE events[2] = {stopEvent_, surfaceAvailable_};
            bool acquired = false;
            for (;;) {
                const DWORD wait =
                    WaitForMultipleObjects(2, events, FALSE, INFINITE);
                if (wait == WAIT_OBJECT_0) {
                    break;
                }
                if (wait != WAIT_OBJECT_0 + 1) {
                    break;
                }
                IDARG_OUT_RELEASEANDACQUIREBUFFER buffer{};
                const HRESULT acquire =
                    IddCxSwapChainReleaseAndAcquireBuffer(
                        swapChain_, &buffer);
                if (acquire == E_PENDING) {
                    continue;
                }
                if (FAILED(acquire)) {
                    break;
                }
                acquired = true;
                if (FAILED(IddCxSwapChainFinishedProcessingFrame(
                        swapChain_))) {
                    break;
                }
            }
            if (acquired) {
                IDARG_OUT_RELEASEANDACQUIREBUFFER release{};
                (void)IddCxSwapChainReleaseAndAcquireBuffer(
                    swapChain_, &release);
            }
        }

        if (dxgiDevice != nullptr) {
            dxgiDevice->Release();
        }
        if (deviceContext != nullptr) {
            deviceContext->Release();
        }
        if (device != nullptr) {
            device->Release();
        }
        if (adapter != nullptr) {
            adapter->Release();
        }
        if (factory != nullptr) {
            factory->Release();
        }
    }

    IDDCX_SWAPCHAIN swapChain_;
    HANDLE surfaceAvailable_;
    LUID renderAdapter_;
    HANDLE stopEvent_;
    HANDLE readyEvent_;
    HANDLE thread_;
    HRESULT startStatus_;
};

struct MonitorSlot {
    IDDCX_MONITOR MonitorObject;
    ARCEN_IDDCX_MONITOR_DESCRIPTOR Descriptor;
    ARCEN_IDDCX_MONITOR_BINDING Binding;
    SwapChainPump* Pump;

    MonitorSlot() noexcept
        : MonitorObject(nullptr),
          Descriptor{},
          Binding{},
          Pump(nullptr) {}
};

struct DeviceState {
    explicit DeviceState(const WDFDEVICE device) noexcept
        : Device(device),
          AdapterObject(nullptr),
          AdapterState(ARCEN_IDDCX_ADAPTER_NOT_STARTED),
          ActiveGeneration(0),
          ActiveMonitorCount(0),
          Owner(nullptr),
          ActiveRequest{},
          HasActiveRequest(false),
          Slots{} {
        InitializeCriticalSection(&Lock);
    }

    ~DeviceState() {
        DeleteCriticalSection(&Lock);
    }

    CRITICAL_SECTION Lock;
    WDFDEVICE Device;
    IDDCX_ADAPTER AdapterObject;
    uint32_t AdapterState;
    uint32_t ActiveGeneration;
    uint32_t ActiveMonitorCount;
    WDFFILEOBJECT Owner;
    ARCEN_IDDCX_APPLY_REQUEST ActiveRequest;
    bool HasActiveRequest;
    std::array<MonitorSlot, ARCEN_IDDCX_MAX_MONITORS> Slots;
};

struct DeviceContext {
    DeviceState* State;
};

struct MonitorContext {
    DeviceState* State;
    uint32_t ConnectorIndex;
};

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(DeviceContext,
                                   ArcenGetDeviceContext);
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(MonitorContext,
                                   ArcenGetMonitorContext);

DeviceState* volatile GlobalState = nullptr;

class DeviceLock final {
public:
    explicit DeviceLock(DeviceState* state) noexcept : state_(state) {
        EnterCriticalSection(&state_->Lock);
    }
    ~DeviceLock() {
        LeaveCriticalSection(&state_->Lock);
    }
    DeviceLock(const DeviceLock&) = delete;
    DeviceLock& operator=(const DeviceLock&) = delete;

private:
    DeviceState* state_;
};

[[nodiscard]] DeviceState* StateFromDevice(
    const WDFDEVICE device) noexcept {
    const auto context = ArcenGetDeviceContext(device);
    return context == nullptr ? nullptr : context->State;
}

[[nodiscard]] MonitorSlot* SlotFromMonitor(
    const IDDCX_MONITOR monitor) noexcept {
    const auto context = ArcenGetMonitorContext(monitor);
    if (context == nullptr || context->State == nullptr ||
        context->ConnectorIndex >= ARCEN_IDDCX_MAX_MONITORS) {
        return nullptr;
    }
    return &context->State->Slots[context->ConnectorIndex];
}

void ResetSlot(MonitorSlot& slot) noexcept {
    if (slot.Pump != nullptr) {
        slot.Pump->Stop();
        delete slot.Pump;
        slot.Pump = nullptr;
    }
    slot.MonitorObject = nullptr;
    slot.Descriptor = {};
    slot.Binding = {};
}

[[nodiscard]] NTSTATUS DepartAllLocked(DeviceState* state,
                                       const bool clearOwner) noexcept {
    NTSTATUS firstFailure = STATUS_SUCCESS;
    for (auto& slot : state->Slots) {
        if (slot.Pump != nullptr) {
            slot.Pump->Stop();
            delete slot.Pump;
            slot.Pump = nullptr;
        }
        if (slot.MonitorObject != nullptr) {
            slot.Binding.State = ARCEN_IDDCX_BINDING_DEPARTING;
            const NTSTATUS status =
                IddCxMonitorDeparture(slot.MonitorObject);
            if (!NT_SUCCESS(status) &&
                NT_SUCCESS(firstFailure)) {
                firstFailure = status;
            }
        }
        ResetSlot(slot);
    }
    state->ActiveGeneration = 0;
    state->ActiveMonitorCount = 0;
    state->ActiveRequest = {};
    state->HasActiveRequest = false;
    if (clearOwner) {
        state->Owner = nullptr;
    }
    return firstFailure;
}

[[nodiscard]] GUID ContainerIdFor(
    const ARCEN_IDDCX_MONITOR_DESCRIPTOR& descriptor) noexcept {
    GUID container = GUID_CONTAINER_ARCEN_IDDCX_BASE;
    container.Data1 ^= descriptor.SerialNumber;
    container.Data4[7] = static_cast<unsigned char>(
        container.Data4[7] ^ descriptor.ConnectorIndex);
    return container;
}

[[nodiscard]] NTSTATUS ArriveMonitorLocked(
    DeviceState* state,
    const ARCEN_IDDCX_MONITOR_DESCRIPTOR& descriptor) noexcept {
    if (descriptor.ConnectorIndex >= state->Slots.size()) {
        return STATUS_INVALID_PARAMETER;
    }
    auto& slot = state->Slots[descriptor.ConnectorIndex];
    if (slot.MonitorObject != nullptr) {
        return STATUS_OBJECT_NAME_COLLISION;
    }

    IDDCX_MONITOR_INFO monitorInfo{};
    monitorInfo.Size = sizeof(monitorInfo);
    monitorInfo.MonitorType =
        DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_WIRED;
    monitorInfo.ConnectorIndex = descriptor.ConnectorIndex;
    monitorInfo.MonitorDescription.Size =
        sizeof(monitorInfo.MonitorDescription);
    monitorInfo.MonitorDescription.Type =
        IDDCX_MONITOR_DESCRIPTION_TYPE_EDID;
    monitorInfo.MonitorDescription.DataSize =
        ARCEN_IDDCX_EDID_BYTES;
    monitorInfo.MonitorDescription.pData =
        const_cast<uint8_t*>(descriptor.Edid);
    monitorInfo.MonitorContainerId =
        ContainerIdFor(descriptor);

    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes,
                                            MonitorContext);
    IDARG_IN_MONITORCREATE input{};
    input.ObjectAttributes = &attributes;
    input.pMonitorInfo = &monitorInfo;
    IDARG_OUT_MONITORCREATE output{};
    NTSTATUS status = IddCxMonitorCreate(
        state->AdapterObject, &input, &output);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    const auto context =
        ArcenGetMonitorContext(output.MonitorObject);
    context->State = state;
    context->ConnectorIndex = descriptor.ConnectorIndex;
    slot.MonitorObject = output.MonitorObject;
    slot.Descriptor = descriptor;
    slot.Binding.ConnectorIndex = descriptor.ConnectorIndex;
    slot.Binding.State = ARCEN_IDDCX_BINDING_ARRIVING;

    IDARG_OUT_MONITORARRIVAL arrival{};
    status = IddCxMonitorArrival(slot.MonitorObject, &arrival);
    if (!NT_SUCCESS(status)) {
        slot.Binding.State = ARCEN_IDDCX_BINDING_FAILED;
        return status;
    }
    slot.Binding.State = ARCEN_IDDCX_BINDING_PRESENT;
    slot.Binding.OsAdapter =
        ToContractLuid(arrival.OsAdapterLuid);
    slot.Binding.OsTargetId = arrival.OsTargetId;
    return STATUS_SUCCESS;
}

[[nodiscard]] NTSTATUS ApplyDisplayConfigLocked(
    DeviceState* state,
    const ARCEN_IDDCX_APPLY_REQUEST& request) noexcept {
    std::array<IDDCX_DISPLAYCONFIGPATH,
               ARCEN_IDDCX_MAX_MONITORS>
        paths{};
    for (uint32_t index = 0; index < request.MonitorCount;
         ++index) {
        const auto& descriptor = request.Monitors[index];
        auto& path = paths[index];
        path.Size = sizeof(path);
        path.MonitorObject =
            state->Slots[descriptor.ConnectorIndex].MonitorObject;
        path.Position.x = descriptor.DesktopX;
        path.Position.y = descriptor.DesktopY;
        const auto& preferred =
            descriptor.Modes[descriptor.PreferredModeIndex];
        path.Resolution.cx = preferred.Width;
        path.Resolution.cy = preferred.Height;
        path.Rotation = ToRotation(descriptor.RotationDegrees);
        path.RefreshRate.Numerator =
            preferred.RefreshMillihz;
        path.RefreshRate.Denominator = 1000u;
        path.VSyncFreqDivider = 1u;
        path.MonitorScaleFactor = 100u;
        path.PhysicalWidthOverride =
            descriptor.PhysicalWidthMm;
        path.PhysicalHeightOverride =
            descriptor.PhysicalHeightMm;
    }
    IDARG_IN_ADAPTERDISPLAYCONFIGUPDATE update{};
    update.PathCount = request.MonitorCount;
    update.pPaths = paths.data();
    return IddCxAdapterDisplayConfigUpdate(
        state->AdapterObject, &update);
}

[[nodiscard]] NTSTATUS ApplyRequestLocked(
    DeviceState* state,
    const WDFFILEOBJECT owner,
    const ARCEN_IDDCX_APPLY_REQUEST& request,
    ARCEN_IDDCX_TOPOLOGY_RESPONSE* response) noexcept {
    if (state->AdapterState != ARCEN_IDDCX_ADAPTER_READY ||
        state->AdapterObject == nullptr) {
        return STATUS_DEVICE_NOT_READY;
    }
    if (state->Owner != nullptr && state->Owner != owner) {
        return STATUS_DEVICE_BUSY;
    }

    const bool hadPrevious = state->HasActiveRequest;
    const ARCEN_IDDCX_APPLY_REQUEST previous =
        state->ActiveRequest;
    const WDFFILEOBJECT previousOwner = state->Owner;
    const NTSTATUS departure =
        DepartAllLocked(state, false);
    if (!NT_SUCCESS(departure)) {
        return departure;
    }

    state->Owner = owner;
    state->ActiveRequest = request;
    state->HasActiveRequest = true;
    state->ActiveGeneration = request.Generation;
    state->ActiveMonitorCount = request.MonitorCount;
    IDARG_IN_ADAPTERSETRENDERADAPTER render{};
    render.PreferredRenderAdapter =
        ToWindowsLuid(request.RenderAdapter);
    IddCxAdapterSetRenderAdapter(state->AdapterObject,
                                &render);

    NTSTATUS status = STATUS_SUCCESS;
    for (uint32_t index = 0; index < request.MonitorCount;
         ++index) {
        status = ArriveMonitorLocked(state,
                                    request.Monitors[index]);
        if (!NT_SUCCESS(status)) {
            break;
        }
    }
    if (NT_SUCCESS(status)) {
        status = ApplyDisplayConfigLocked(state, request);
    }
    if (NT_SUCCESS(status)) {
        response->RollbackStatus = STATUS_SUCCESS;
        return STATUS_SUCCESS;
    }

    (void)DepartAllLocked(state, false);
    NTSTATUS rollbackStatus = STATUS_SUCCESS;
    if (hadPrevious) {
        state->Owner = previousOwner;
        state->ActiveRequest = previous;
        state->HasActiveRequest = true;
        state->ActiveGeneration = previous.Generation;
        state->ActiveMonitorCount = previous.MonitorCount;
        IDARG_IN_ADAPTERSETRENDERADAPTER restoreRender{};
        restoreRender.PreferredRenderAdapter =
            ToWindowsLuid(previous.RenderAdapter);
        IddCxAdapterSetRenderAdapter(state->AdapterObject,
                                    &restoreRender);
        for (uint32_t index = 0;
             index < previous.MonitorCount; ++index) {
            rollbackStatus = ArriveMonitorLocked(
                state, previous.Monitors[index]);
            if (!NT_SUCCESS(rollbackStatus)) {
                break;
            }
        }
        if (NT_SUCCESS(rollbackStatus)) {
            rollbackStatus =
                ApplyDisplayConfigLocked(state, previous);
        }
        if (!NT_SUCCESS(rollbackStatus)) {
            (void)DepartAllLocked(state, true);
        }
    } else {
        state->Owner = nullptr;
    }
    response->RollbackStatus = rollbackStatus;
    return status;
}

void FillResponseLocked(
    const DeviceState* state,
    ARCEN_IDDCX_TOPOLOGY_RESPONSE* response) noexcept {
    response->Size = sizeof(*response);
    response->AbiVersion = ARCEN_IDDCX_ABI_VERSION;
    response->Generation = state->ActiveGeneration;
    response->MonitorCount = state->ActiveMonitorCount;
    for (size_t index = 0; index < state->Slots.size();
         ++index) {
        response->Bindings[index] =
            state->Slots[index].Binding;
    }
}

[[nodiscard]] bool CopyDescriptorByEdid(
    const IDDCX_MONITOR_DESCRIPTION& description,
    ARCEN_IDDCX_MONITOR_DESCRIPTOR* descriptor) noexcept {
    if (descriptor == nullptr) {
        return false;
    }
    if (description.Type !=
            IDDCX_MONITOR_DESCRIPTION_TYPE_EDID ||
        description.DataSize != ARCEN_IDDCX_EDID_BYTES ||
        description.pData == nullptr) {
        return false;
    }
    DeviceState* state = GlobalState;
    if (state == nullptr) {
        return false;
    }
    DeviceLock lock(state);
    for (const auto& slot : state->Slots) {
        if (slot.MonitorObject != nullptr &&
            std::memcmp(slot.Descriptor.Edid,
                        description.pData,
                        ARCEN_IDDCX_EDID_BYTES) == 0) {
            *descriptor = slot.Descriptor;
            return true;
        }
    }
    return false;
}

[[nodiscard]] NTSTATUS CopyMonitorModes(
    const ARCEN_IDDCX_MONITOR_DESCRIPTOR& descriptor,
    const UINT inputCount,
    IDDCX_MONITOR_MODE* modes,
    UINT* outputCount,
    UINT* preferredIndex,
    const IDDCX_MONITOR_MODE_ORIGIN origin) noexcept {
    *outputCount = descriptor.ModeCount;
    *preferredIndex = descriptor.PreferredModeIndex;
    if (inputCount == 0 || modes == nullptr) {
        return STATUS_SUCCESS;
    }
    if (inputCount < descriptor.ModeCount) {
        *outputCount = 0;
        return STATUS_BUFFER_TOO_SMALL;
    }
    for (UINT index = 0; index < descriptor.ModeCount;
         ++index) {
        modes[index] = {};
        modes[index].Size = sizeof(modes[index]);
        modes[index].Origin = origin;
        modes[index].MonitorVideoSignalInfo =
            BuildSignalInfo(descriptor.Modes[index], 0);
    }
    return STATUS_SUCCESS;
}

[[nodiscard]] NTSTATUS CopyTargetModes(
    const ARCEN_IDDCX_MONITOR_DESCRIPTOR& descriptor,
    const UINT inputCount,
    IDDCX_TARGET_MODE* modes,
    UINT* outputCount) noexcept {
    *outputCount = descriptor.ModeCount;
    if (inputCount == 0 || modes == nullptr) {
        return STATUS_SUCCESS;
    }
    if (inputCount < descriptor.ModeCount) {
        *outputCount = 0;
        return STATUS_BUFFER_TOO_SMALL;
    }
    for (UINT index = 0; index < descriptor.ModeCount;
         ++index) {
        modes[index] = {};
        modes[index].Size = sizeof(modes[index]);
        const auto signal =
            BuildSignalInfo(descriptor.Modes[index], 1);
        modes[index].TargetVideoSignalInfo
            .targetVideoSignalInfo = signal;
        modes[index].RequiredBandwidth =
            signal.pixelRate;
    }
    return STATUS_SUCCESS;
}

void CompleteRequest(const WDFREQUEST request,
                     const NTSTATUS status,
                     const size_t information = 0) noexcept {
    WdfRequestCompleteWithInformation(request, status,
                                      information);
}

}  // namespace

extern "C" NTSTATUS DriverEntry(
    PDRIVER_OBJECT driverObject,
    PUNICODE_STRING registryPath) {
    WDF_DRIVER_CONFIG config;
    WDF_DRIVER_CONFIG_INIT(&config, ArcenIddCxDeviceAdd);
    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_OBJECT_ATTRIBUTES_INIT(&attributes);
    return WdfDriverCreate(driverObject, registryPath,
                           &attributes, &config,
                           WDF_NO_HANDLE);
}

NTSTATUS ArcenIddCxDeviceAdd(
    WDFDRIVER,
    PWDFDEVICE_INIT deviceInit) {
    IDD_CX_CLIENT_CONFIG clientConfig;
    IDD_CX_CLIENT_CONFIG_INIT(&clientConfig);
    clientConfig.EvtIddCxDeviceIoControl =
        ArcenIddCxDeviceIoControl;
    clientConfig.EvtIddCxParseMonitorDescription =
        ArcenIddCxParseMonitorDescription;
    clientConfig.EvtIddCxAdapterInitFinished =
        ArcenIddCxAdapterInitFinished;
    clientConfig.EvtIddCxAdapterCommitModes =
        ArcenIddCxAdapterCommitModes;
    clientConfig.EvtIddCxMonitorGetDefaultDescriptionModes =
        ArcenIddCxMonitorGetDefaultDescriptionModes;
    clientConfig.EvtIddCxMonitorQueryTargetModes =
        ArcenIddCxMonitorQueryTargetModes;
    clientConfig.EvtIddCxMonitorAssignSwapChain =
        ArcenIddCxMonitorAssignSwapChain;
    clientConfig.EvtIddCxMonitorUnassignSwapChain =
        ArcenIddCxMonitorUnassignSwapChain;

    NTSTATUS status =
        IddCxDeviceInitConfig(deviceInit, &clientConfig);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    WDF_PNPPOWER_EVENT_CALLBACKS powerCallbacks;
    WDF_PNPPOWER_EVENT_CALLBACKS_INIT(&powerCallbacks);
    powerCallbacks.EvtDeviceD0Entry =
        ArcenIddCxDeviceD0Entry;
    powerCallbacks.EvtDeviceD0Exit =
        ArcenIddCxDeviceD0Exit;
    WdfDeviceInitSetPnpPowerEventCallbacks(
        deviceInit, &powerCallbacks);

    WDF_FILEOBJECT_CONFIG fileConfig;
    WDF_FILEOBJECT_CONFIG_INIT(&fileConfig, nullptr, nullptr,
                               ArcenIddCxFileCleanup);
    WDF_OBJECT_ATTRIBUTES fileAttributes;
    WDF_OBJECT_ATTRIBUTES_INIT(&fileAttributes);
    WdfDeviceInitSetFileObjectConfig(deviceInit, &fileConfig,
                                     &fileAttributes);

    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes,
                                            DeviceContext);
    attributes.EvtCleanupCallback =
        ArcenIddCxDeviceCleanup;
    WDFDEVICE device = nullptr;
    status = WdfDeviceCreate(&deviceInit, &attributes,
                             &device);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    auto* state =
        new (std::nothrow) DeviceState(device);
    if (state == nullptr) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    ArcenGetDeviceContext(device)->State = state;
    GlobalState = state;

    DECLARE_CONST_UNICODE_STRING(
        symbolicLink,
        L"\\DosDevices\\Global\\ArcenIddCx");
    status = WdfDeviceCreateSymbolicLink(device,
                                         &symbolicLink);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    status = WdfDeviceCreateDeviceInterface(
        device, &GUID_DEVINTERFACE_ARCEN_IDDCX_CONTROL,
        nullptr);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    return IddCxDeviceInitialize(device);
}

void ArcenIddCxDeviceCleanup(
    WDFOBJECT object) {
    const WDFDEVICE device =
        static_cast<WDFDEVICE>(object);
    auto* context = ArcenGetDeviceContext(device);
    DeviceState* state =
        context == nullptr ? nullptr : context->State;
    if (state == nullptr) {
        return;
    }
    {
        DeviceLock lock(state);
        (void)DepartAllLocked(state, true);
    }
    if (GlobalState == state) {
        GlobalState = nullptr;
    }
    context->State = nullptr;
    delete state;
}

NTSTATUS ArcenIddCxDeviceD0Entry(
    WDFDEVICE device,
    WDF_POWER_DEVICE_STATE) {
    DeviceState* state = StateFromDevice(device);
    if (state == nullptr) {
        return STATUS_INVALID_DEVICE_STATE;
    }
    DeviceLock lock(state);
    if (state->AdapterState ==
        ARCEN_IDDCX_ADAPTER_READY) {
        return STATUS_SUCCESS;
    }
    if (state->AdapterState ==
        ARCEN_IDDCX_ADAPTER_INITIALIZING) {
        return STATUS_DEVICE_BUSY;
    }
    state->AdapterState =
        ARCEN_IDDCX_ADAPTER_INITIALIZING;

    IDDCX_ENDPOINT_VERSION hardwareVersion{};
    hardwareVersion.Size = sizeof(hardwareVersion);
    hardwareVersion.MajorVer = 1;
    IDDCX_ENDPOINT_VERSION firmwareVersion{};
    firmwareVersion.Size = sizeof(firmwareVersion);
    firmwareVersion.MajorVer = 1;

    IDDCX_ADAPTER_CAPS caps{};
    caps.Size = sizeof(caps);
    caps.Flags = IDDCX_ADAPTER_FLAGS_USE_SMALLEST_MODE;
    caps.MaxDisplayPipelineRate = UINT64_MAX;
    caps.MaxMonitorsSupported =
        ARCEN_IDDCX_MAX_MONITORS;
    caps.EndPointDiagnostics.Size =
        sizeof(caps.EndPointDiagnostics);
    caps.EndPointDiagnostics.TransmissionType =
        IDDCX_TRANSMISSION_TYPE_NETWORK_OTHER;
    caps.EndPointDiagnostics.pEndPointModelName =
        EndpointModel;
    caps.EndPointDiagnostics
        .pEndPointManufacturerName =
        EndpointManufacturer;
    caps.EndPointDiagnostics.pHardwareVersion =
        &hardwareVersion;
    caps.EndPointDiagnostics.pFirmwareVersion =
        &firmwareVersion;
    caps.EndPointDiagnostics.GammaSupport =
        IDDCX_FEATURE_IMPLEMENTATION_NONE;

    IDARG_IN_ADAPTER_INIT input{};
    input.WdfDevice = device;
    input.pCaps = &caps;
    input.ObjectAttributes = nullptr;
    IDARG_OUT_ADAPTER_INIT output{};
    const NTSTATUS status =
        IddCxAdapterInitAsync(&input, &output);
    if (NT_SUCCESS(status)) {
        state->AdapterObject = output.AdapterObject;
    } else {
        state->AdapterState =
            ARCEN_IDDCX_ADAPTER_FAILED;
    }
    return status;
}

NTSTATUS ArcenIddCxDeviceD0Exit(
    WDFDEVICE device,
    WDF_POWER_DEVICE_STATE) {
    DeviceState* state = StateFromDevice(device);
    if (state == nullptr) {
        return STATUS_SUCCESS;
    }
    DeviceLock lock(state);
    (void)DepartAllLocked(state, true);
    return STATUS_SUCCESS;
}

void ArcenIddCxFileCleanup(
    WDFFILEOBJECT fileObject) {
    const WDFDEVICE device =
        WdfFileObjectGetDevice(fileObject);
    DeviceState* state = StateFromDevice(device);
    if (state == nullptr) {
        return;
    }
    DeviceLock lock(state);
    if (state->Owner == fileObject) {
        (void)DepartAllLocked(state, true);
    }
}

void ArcenIddCxDeviceIoControl(
    WDFDEVICE device,
    WDFREQUEST request,
    size_t outputBufferLength,
    size_t inputBufferLength,
    ULONG ioControlCode) {
    DeviceState* state = StateFromDevice(device);
    if (state == nullptr) {
        CompleteRequest(request,
                        STATUS_INVALID_DEVICE_STATE);
        return;
    }

    if (ioControlCode ==
        ARCEN_IDDCX_IOCTL_GET_CAPABILITIES) {
        if (outputBufferLength <
            sizeof(ARCEN_IDDCX_CAPABILITIES)) {
            CompleteRequest(request,
                            STATUS_BUFFER_TOO_SMALL);
            return;
        }
        ARCEN_IDDCX_CAPABILITIES* output = nullptr;
        size_t length = 0;
        const NTSTATUS retrieve =
            WdfRequestRetrieveOutputBuffer(
                request, sizeof(*output),
                reinterpret_cast<void**>(&output), &length);
        if (!NT_SUCCESS(retrieve)) {
            CompleteRequest(request, retrieve);
            return;
        }
        DeviceLock lock(state);
        *output = {};
        output->Size = sizeof(*output);
        output->AbiVersion =
            ARCEN_IDDCX_ABI_VERSION;
        output->DriverVersion =
            ARCEN_IDDCX_DRIVER_VERSION;
        output->Flags =
            ARCEN_IDDCX_REQUIRED_CAPABILITIES;
        output->MaxMonitors =
            ARCEN_IDDCX_MAX_MONITORS;
        output->MaxModesPerMonitor =
            ARCEN_IDDCX_MAX_MODES_PER_MONITOR;
        output->MinWidth = ARCEN_IDDCX_MIN_WIDTH;
        output->MaxWidth = ARCEN_IDDCX_MAX_WIDTH;
        output->MinHeight = ARCEN_IDDCX_MIN_HEIGHT;
        output->MaxHeight = ARCEN_IDDCX_MAX_HEIGHT;
        output->MinRefreshMillihz =
            ARCEN_IDDCX_MIN_REFRESH_MILLIHZ;
        output->MaxRefreshMillihz =
            ARCEN_IDDCX_MAX_REFRESH_MILLIHZ;
        output->AdapterState = state->AdapterState;
        output->ActiveGeneration =
            state->ActiveGeneration;
        output->ActiveMonitorCount =
            state->ActiveMonitorCount;
        CompleteRequest(request, STATUS_SUCCESS,
                        sizeof(*output));
        return;
    }

    if (ioControlCode ==
        ARCEN_IDDCX_IOCTL_QUERY_STATUS) {
        if (outputBufferLength <
            sizeof(ARCEN_IDDCX_STATUS_RESPONSE)) {
            CompleteRequest(request,
                            STATUS_BUFFER_TOO_SMALL);
            return;
        }
        ARCEN_IDDCX_STATUS_RESPONSE* output = nullptr;
        size_t length = 0;
        const NTSTATUS retrieve =
            WdfRequestRetrieveOutputBuffer(
                request, sizeof(*output),
                reinterpret_cast<void**>(&output), &length);
        if (!NT_SUCCESS(retrieve)) {
            CompleteRequest(request, retrieve);
            return;
        }
        *output = {};
        DeviceLock lock(state);
        FillResponseLocked(state, output);
        output->OperationStatus = STATUS_SUCCESS;
        output->RollbackStatus = STATUS_SUCCESS;
        CompleteRequest(request, STATUS_SUCCESS,
                        sizeof(*output));
        return;
    }

    if (ioControlCode ==
        ARCEN_IDDCX_IOCTL_APPLY_TOPOLOGY) {
        if (inputBufferLength !=
                sizeof(ARCEN_IDDCX_APPLY_REQUEST) ||
            outputBufferLength <
                sizeof(ARCEN_IDDCX_TOPOLOGY_RESPONSE)) {
            CompleteRequest(request,
                            STATUS_INFO_LENGTH_MISMATCH);
            return;
        }
        ARCEN_IDDCX_APPLY_REQUEST* input = nullptr;
        ARCEN_IDDCX_TOPOLOGY_RESPONSE* output = nullptr;
        size_t length = 0;
        NTSTATUS retrieve =
            WdfRequestRetrieveInputBuffer(
                request, sizeof(*input),
                reinterpret_cast<void**>(&input), &length);
        if (!NT_SUCCESS(retrieve)) {
            CompleteRequest(request, retrieve);
            return;
        }
        retrieve = WdfRequestRetrieveOutputBuffer(
            request, sizeof(*output),
            reinterpret_cast<void**>(&output), &length);
        if (!NT_SUCCESS(retrieve)) {
            CompleteRequest(request, retrieve);
            return;
        }
        const auto validation =
            arcen::iddcx::ValidateApplyRequest(*input);
        if (!validation.Ok()) {
            CompleteRequest(request,
                            STATUS_INVALID_PARAMETER);
            return;
        }
        *output = {};
        DeviceLock lock(state);
        const NTSTATUS operation = ApplyRequestLocked(
            state, WdfRequestGetFileObject(request),
            *input, output);
        FillResponseLocked(state, output);
        output->OperationStatus = operation;
        CompleteRequest(request, STATUS_SUCCESS,
                        sizeof(*output));
        return;
    }

    if (ioControlCode ==
        ARCEN_IDDCX_IOCTL_REMOVE_TOPOLOGY) {
        if (inputBufferLength !=
                sizeof(ARCEN_IDDCX_REMOVE_REQUEST) ||
            outputBufferLength <
                sizeof(ARCEN_IDDCX_TOPOLOGY_RESPONSE)) {
            CompleteRequest(request,
                            STATUS_INFO_LENGTH_MISMATCH);
            return;
        }
        ARCEN_IDDCX_REMOVE_REQUEST* input = nullptr;
        ARCEN_IDDCX_TOPOLOGY_RESPONSE* output = nullptr;
        size_t length = 0;
        NTSTATUS retrieve =
            WdfRequestRetrieveInputBuffer(
                request, sizeof(*input),
                reinterpret_cast<void**>(&input), &length);
        if (!NT_SUCCESS(retrieve)) {
            CompleteRequest(request, retrieve);
            return;
        }
        retrieve = WdfRequestRetrieveOutputBuffer(
            request, sizeof(*output),
            reinterpret_cast<void**>(&output), &length);
        if (!NT_SUCCESS(retrieve)) {
            CompleteRequest(request, retrieve);
            return;
        }
        if (input->Size != sizeof(*input) ||
            input->AbiVersion !=
                ARCEN_IDDCX_ABI_VERSION) {
            CompleteRequest(request,
                            STATUS_INVALID_PARAMETER);
            return;
        }
        *output = {};
        DeviceLock lock(state);
        NTSTATUS operation = STATUS_SUCCESS;
        if (state->Owner !=
            WdfRequestGetFileObject(request)) {
            operation = STATUS_ACCESS_DENIED;
        } else if (input->Generation != 0 &&
                   input->Generation !=
                       state->ActiveGeneration) {
            operation = STATUS_REVISION_MISMATCH;
        } else {
            operation = DepartAllLocked(state, true);
        }
        FillResponseLocked(state, output);
        output->OperationStatus = operation;
        output->RollbackStatus = operation;
        CompleteRequest(request, STATUS_SUCCESS,
                        sizeof(*output));
        return;
    }

    CompleteRequest(request,
                    STATUS_INVALID_DEVICE_REQUEST);
}

NTSTATUS ArcenIddCxParseMonitorDescription(
    const IDARG_IN_PARSEMONITORDESCRIPTION* input,
    IDARG_OUT_PARSEMONITORDESCRIPTION* output) {
    if (input == nullptr || output == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    ARCEN_IDDCX_MONITOR_DESCRIPTOR descriptor{};
    if (!CopyDescriptorByEdid(input->MonitorDescription,
                              &descriptor)) {
        return STATUS_INVALID_PARAMETER;
    }
    return CopyMonitorModes(
        descriptor, input->MonitorModeBufferInputCount,
        input->pMonitorModes,
        &output->MonitorModeBufferOutputCount,
        &output->PreferredMonitorModeIdx,
        IDDCX_MONITOR_MODE_ORIGIN_MONITORDESCRIPTOR);
}

NTSTATUS ArcenIddCxAdapterInitFinished(
    IDDCX_ADAPTER adapter,
    const IDARG_IN_ADAPTER_INIT_FINISHED* input) {
    DeviceState* state = GlobalState;
    if (state == nullptr || input == nullptr) {
        return STATUS_INVALID_DEVICE_STATE;
    }
    DeviceLock lock(state);
    state->AdapterObject = adapter;
    state->AdapterState =
        NT_SUCCESS(input->AdapterInitStatus)
            ? ARCEN_IDDCX_ADAPTER_READY
            : ARCEN_IDDCX_ADAPTER_FAILED;
    return STATUS_SUCCESS;
}

NTSTATUS ArcenIddCxAdapterCommitModes(
    IDDCX_ADAPTER,
    const IDARG_IN_COMMITMODES*) {
    return STATUS_SUCCESS;
}

NTSTATUS ArcenIddCxMonitorGetDefaultDescriptionModes(
    IDDCX_MONITOR monitor,
    const IDARG_IN_GETDEFAULTDESCRIPTIONMODES* input,
    IDARG_OUT_GETDEFAULTDESCRIPTIONMODES* output) {
    MonitorSlot* slot = SlotFromMonitor(monitor);
    if (slot == nullptr || input == nullptr ||
        output == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    return CopyMonitorModes(
        slot->Descriptor,
        input->DefaultMonitorModeBufferInputCount,
        input->pDefaultMonitorModes,
        &output->DefaultMonitorModeBufferOutputCount,
        &output->PreferredMonitorModeIdx,
        IDDCX_MONITOR_MODE_ORIGIN_DRIVER);
}

NTSTATUS ArcenIddCxMonitorQueryTargetModes(
    IDDCX_MONITOR monitor,
    const IDARG_IN_QUERYTARGETMODES* input,
    IDARG_OUT_QUERYTARGETMODES* output) {
    MonitorSlot* slot = SlotFromMonitor(monitor);
    if (slot == nullptr || input == nullptr ||
        output == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    return CopyTargetModes(
        slot->Descriptor,
        input->TargetModeBufferInputCount,
        input->pTargetModes,
        &output->TargetModeBufferOutputCount);
}

NTSTATUS ArcenIddCxMonitorAssignSwapChain(
    IDDCX_MONITOR monitor,
    const IDARG_IN_SETSWAPCHAIN* input) {
    MonitorSlot* slot = SlotFromMonitor(monitor);
    const auto context = ArcenGetMonitorContext(monitor);
    if (slot == nullptr || context == nullptr ||
        context->State == nullptr || input == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    DeviceState* state = context->State;
    DeviceLock lock(state);
    if (!LuidEquals(input->RenderAdapterLuid,
                    state->ActiveRequest.RenderAdapter)) {
        slot->Binding.ActualRenderAdapter =
            ToContractLuid(input->RenderAdapterLuid);
        slot->Binding.State =
            ARCEN_IDDCX_BINDING_FAILED;
        return STATUS_DEVICE_CONFIGURATION_ERROR;
    }
    if (slot->Pump != nullptr) {
        slot->Pump->Stop();
        delete slot->Pump;
        slot->Pump = nullptr;
    }
    auto* pump = new (std::nothrow) SwapChainPump(
        input->hSwapChain, input->hNextSurfaceAvailable,
        input->RenderAdapterLuid);
    if (pump == nullptr) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    const NTSTATUS status = pump->Start();
    if (!NT_SUCCESS(status)) {
        delete pump;
        slot->Binding.State =
            ARCEN_IDDCX_BINDING_FAILED;
        return status;
    }
    slot->Pump = pump;
    slot->Binding.ActualRenderAdapter =
        ToContractLuid(input->RenderAdapterLuid);
    slot->Binding.Flags =
        ARCEN_IDDCX_BINDING_SWAPCHAIN_READY |
        ARCEN_IDDCX_BINDING_RENDER_ADAPTER_MATCHED;
    slot->Binding.State =
        ARCEN_IDDCX_BINDING_PRESENT;
    return STATUS_SUCCESS;
}

NTSTATUS ArcenIddCxMonitorUnassignSwapChain(
    IDDCX_MONITOR monitor) {
    MonitorSlot* slot = SlotFromMonitor(monitor);
    const auto context = ArcenGetMonitorContext(monitor);
    if (slot == nullptr || context == nullptr ||
        context->State == nullptr) {
        return STATUS_INVALID_PARAMETER;
    }
    DeviceLock lock(context->State);
    if (slot->Pump != nullptr) {
        slot->Pump->Stop();
        delete slot->Pump;
        slot->Pump = nullptr;
    }
    slot->Binding.Flags = 0;
    slot->Binding.ActualRenderAdapter = {};
    return STATUS_SUCCESS;
}
