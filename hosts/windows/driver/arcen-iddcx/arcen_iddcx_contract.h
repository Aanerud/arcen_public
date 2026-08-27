#pragma once

#include <stddef.h>
#include <stdint.h>

#define ARCEN_IDDCX_ABI_VERSION 1u
#define ARCEN_IDDCX_DRIVER_VERSION 0x00010000u
#define ARCEN_IDDCX_MAX_MONITORS 4u
#define ARCEN_IDDCX_MAX_MODES_PER_MONITOR 8u
#define ARCEN_IDDCX_EDID_BYTES 128u
#define ARCEN_IDDCX_MIN_WIDTH 320u
#define ARCEN_IDDCX_MAX_WIDTH 4095u
#define ARCEN_IDDCX_MIN_HEIGHT 240u
#define ARCEN_IDDCX_MAX_HEIGHT 4095u
#define ARCEN_IDDCX_MIN_REFRESH_MILLIHZ 24000u
#define ARCEN_IDDCX_MAX_REFRESH_MILLIHZ 120000u

#define ARCEN_IDDCX_CAP_DYNAMIC_MONITORS (1u << 0)
#define ARCEN_IDDCX_CAP_MONITOR_EDID (1u << 1)
#define ARCEN_IDDCX_CAP_EXACT_MODES (1u << 2)
#define ARCEN_IDDCX_CAP_RENDER_ADAPTER_AFFINITY (1u << 3)
#define ARCEN_IDDCX_CAP_ATOMIC_REPLACE (1u << 4)
#define ARCEN_IDDCX_CAP_ROLLBACK (1u << 5)
#define ARCEN_IDDCX_CAP_HANDLE_CLEANUP_ROLLBACK (1u << 6)
#define ARCEN_IDDCX_CAP_SWAPCHAIN_DRAIN (1u << 7)
#define ARCEN_IDDCX_CAP_CONSOLE_SESSION (1u << 8)
#define ARCEN_IDDCX_REQUIRED_CAPABILITIES                                      \
    (ARCEN_IDDCX_CAP_DYNAMIC_MONITORS | ARCEN_IDDCX_CAP_MONITOR_EDID |         \
     ARCEN_IDDCX_CAP_EXACT_MODES | ARCEN_IDDCX_CAP_RENDER_ADAPTER_AFFINITY |   \
     ARCEN_IDDCX_CAP_ATOMIC_REPLACE | ARCEN_IDDCX_CAP_ROLLBACK |               \
     ARCEN_IDDCX_CAP_HANDLE_CLEANUP_ROLLBACK |                                 \
     ARCEN_IDDCX_CAP_SWAPCHAIN_DRAIN | ARCEN_IDDCX_CAP_CONSOLE_SESSION)

#define ARCEN_IDDCX_APPLY_REPLACE_TOPOLOGY (1u << 0)
#define ARCEN_IDDCX_APPLY_REQUIRE_RENDER_ADAPTER (1u << 1)
#define ARCEN_IDDCX_MONITOR_PRIMARY (1u << 0)

#define ARCEN_IDDCX_ADAPTER_NOT_STARTED 0u
#define ARCEN_IDDCX_ADAPTER_INITIALIZING 1u
#define ARCEN_IDDCX_ADAPTER_READY 2u
#define ARCEN_IDDCX_ADAPTER_FAILED 3u

#define ARCEN_IDDCX_BINDING_ABSENT 0u
#define ARCEN_IDDCX_BINDING_ARRIVING 1u
#define ARCEN_IDDCX_BINDING_PRESENT 2u
#define ARCEN_IDDCX_BINDING_DEPARTING 3u
#define ARCEN_IDDCX_BINDING_FAILED 4u
#define ARCEN_IDDCX_BINDING_SWAPCHAIN_READY (1u << 0)
#define ARCEN_IDDCX_BINDING_RENDER_ADAPTER_MATCHED (1u << 1)

#define ARCEN_IDDCX_IOCTL_GET_CAPABILITIES 0x00226000u
#define ARCEN_IDDCX_IOCTL_APPLY_TOPOLOGY 0x0022e004u
#define ARCEN_IDDCX_IOCTL_REMOVE_TOPOLOGY 0x0022e008u
#define ARCEN_IDDCX_IOCTL_QUERY_STATUS 0x0022600cu

typedef struct ARCEN_IDDCX_ADAPTER_LUID {
    uint32_t LowPart;
    int32_t HighPart;
} ARCEN_IDDCX_ADAPTER_LUID;

typedef struct ARCEN_IDDCX_MODE {
    uint32_t Width;
    uint32_t Height;
    uint32_t RefreshMillihz;
} ARCEN_IDDCX_MODE;

typedef struct ARCEN_IDDCX_MONITOR_DESCRIPTOR {
    uint32_t ConnectorIndex;
    int32_t DesktopX;
    int32_t DesktopY;
    uint32_t RotationDegrees;
    uint32_t Flags;
    uint32_t ModeCount;
    uint32_t PreferredModeIndex;
    uint32_t PhysicalWidthMm;
    uint32_t PhysicalHeightMm;
    uint32_t SerialNumber;
    uint16_t ProductCode;
    uint16_t Reserved;
    ARCEN_IDDCX_MODE Modes[ARCEN_IDDCX_MAX_MODES_PER_MONITOR];
    uint8_t Edid[ARCEN_IDDCX_EDID_BYTES];
} ARCEN_IDDCX_MONITOR_DESCRIPTOR;

typedef struct ARCEN_IDDCX_APPLY_REQUEST {
    uint32_t Size;
    uint32_t AbiVersion;
    uint32_t Generation;
    uint32_t MonitorCount;
    ARCEN_IDDCX_ADAPTER_LUID RenderAdapter;
    uint32_t Flags;
    uint32_t Reserved;
    ARCEN_IDDCX_MONITOR_DESCRIPTOR Monitors[ARCEN_IDDCX_MAX_MONITORS];
} ARCEN_IDDCX_APPLY_REQUEST;

typedef struct ARCEN_IDDCX_MONITOR_BINDING {
    uint32_t ConnectorIndex;
    uint32_t State;
    ARCEN_IDDCX_ADAPTER_LUID OsAdapter;
    uint32_t OsTargetId;
    ARCEN_IDDCX_ADAPTER_LUID ActualRenderAdapter;
    uint32_t Flags;
} ARCEN_IDDCX_MONITOR_BINDING;

typedef struct ARCEN_IDDCX_TOPOLOGY_RESPONSE {
    uint32_t Size;
    uint32_t AbiVersion;
    uint32_t Generation;
    int32_t OperationStatus;
    uint32_t MonitorCount;
    int32_t RollbackStatus;
    uint32_t Reserved[2];
    ARCEN_IDDCX_MONITOR_BINDING Bindings[ARCEN_IDDCX_MAX_MONITORS];
} ARCEN_IDDCX_TOPOLOGY_RESPONSE;

typedef ARCEN_IDDCX_TOPOLOGY_RESPONSE ARCEN_IDDCX_STATUS_RESPONSE;

typedef struct ARCEN_IDDCX_REMOVE_REQUEST {
    uint32_t Size;
    uint32_t AbiVersion;
    uint32_t Generation;
    uint32_t Flags;
    uint32_t Reserved[2];
} ARCEN_IDDCX_REMOVE_REQUEST;

typedef struct ARCEN_IDDCX_CAPABILITIES {
    uint32_t Size;
    uint32_t AbiVersion;
    uint32_t DriverVersion;
    uint32_t Flags;
    uint32_t MaxMonitors;
    uint32_t MaxModesPerMonitor;
    uint32_t MinWidth;
    uint32_t MaxWidth;
    uint32_t MinHeight;
    uint32_t MaxHeight;
    uint32_t MinRefreshMillihz;
    uint32_t MaxRefreshMillihz;
    uint32_t AdapterState;
    uint32_t ActiveGeneration;
    uint32_t ActiveMonitorCount;
    uint32_t Reserved;
} ARCEN_IDDCX_CAPABILITIES;

#if defined(__cplusplus)
static_assert(sizeof(ARCEN_IDDCX_ADAPTER_LUID) == 8);
static_assert(sizeof(ARCEN_IDDCX_MODE) == 12);
static_assert(sizeof(ARCEN_IDDCX_MONITOR_DESCRIPTOR) == 268);
static_assert(offsetof(ARCEN_IDDCX_MONITOR_DESCRIPTOR, Modes) == 44);
static_assert(offsetof(ARCEN_IDDCX_MONITOR_DESCRIPTOR, Edid) == 140);
static_assert(sizeof(ARCEN_IDDCX_APPLY_REQUEST) == 1104);
static_assert(sizeof(ARCEN_IDDCX_MONITOR_BINDING) == 32);
static_assert(sizeof(ARCEN_IDDCX_TOPOLOGY_RESPONSE) == 160);
static_assert(sizeof(ARCEN_IDDCX_REMOVE_REQUEST) == 24);
static_assert(sizeof(ARCEN_IDDCX_CAPABILITIES) == 64);
#endif
