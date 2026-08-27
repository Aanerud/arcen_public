#pragma once

#if defined(ARCEN_MICROPHONE_PORTABLE_TEST)
#include "arcen_microphone_test_shim.h"
#else
#include <ntifs.h>
#endif

#define ARCEN_MICROPHONE_CONTRACT_VERSION 1u
#define ARCEN_MICROPHONE_SAMPLE_RATE 48000u
#define ARCEN_MICROPHONE_CHANNELS 1u
#define ARCEN_MICROPHONE_BITS_PER_SAMPLE 16u
#define ARCEN_MICROPHONE_FRAME_SAMPLES 960u
#define ARCEN_MICROPHONE_FRAME_BYTES 1920u
#define ARCEN_MICROPHONE_RING_FRAMES 10u
#define ARCEN_MICROPHONE_MAX_SID_BYTES 68u
#define ARCEN_MICROPHONE_DEVICE_TYPE 0x8000u

#define IOCTL_ARCEN_MICROPHONE_BIND \
    CTL_CODE(ARCEN_MICROPHONE_DEVICE_TYPE, 0x800, METHOD_BUFFERED, FILE_WRITE_DATA)
#define IOCTL_ARCEN_MICROPHONE_FEED \
    CTL_CODE(ARCEN_MICROPHONE_DEVICE_TYPE, 0x801, METHOD_BUFFERED, FILE_WRITE_DATA)
#define IOCTL_ARCEN_MICROPHONE_STOP \
    CTL_CODE(ARCEN_MICROPHONE_DEVICE_TYPE, 0x802, METHOD_BUFFERED, FILE_WRITE_DATA)
#define IOCTL_ARCEN_MICROPHONE_STATUS \
    CTL_CODE(ARCEN_MICROPHONE_DEVICE_TYPE, 0x803, METHOD_BUFFERED, FILE_READ_DATA)

typedef enum _ARCEN_MICROPHONE_STATE {
    ArcenMicrophoneStateUnbound = 0,
    ArcenMicrophoneStateBound = 1,
    ArcenMicrophoneStateRemoved = 2
} ARCEN_MICROPHONE_STATE;

typedef struct _ARCEN_MICROPHONE_BIND_REQUEST {
    ULONG Version;
    ULONG WtsSessionId;
    ULONG Generation;
    ULONG SidLength;
    UCHAR Sid[ARCEN_MICROPHONE_MAX_SID_BYTES];
} ARCEN_MICROPHONE_BIND_REQUEST, *PARCEN_MICROPHONE_BIND_REQUEST;

typedef struct _ARCEN_MICROPHONE_FEED_REQUEST {
    ULONG Version;
    ULONG Generation;
    ULONG FrameBytes;
    ULONG Reserved;
    UCHAR Frame[ARCEN_MICROPHONE_FRAME_BYTES];
} ARCEN_MICROPHONE_FEED_REQUEST, *PARCEN_MICROPHONE_FEED_REQUEST;

typedef struct _ARCEN_MICROPHONE_STOP_REQUEST {
    ULONG Version;
    ULONG Generation;
} ARCEN_MICROPHONE_STOP_REQUEST, *PARCEN_MICROPHONE_STOP_REQUEST;

typedef struct _ARCEN_MICROPHONE_STATUS_RESPONSE {
    ULONG Version;
    ULONG State;
    ULONG WtsSessionId;
    ULONG Generation;
    ULONG QueuedFrames;
    ULONG Overruns;
    ULONG Underruns;
    ULONG Reserved;
} ARCEN_MICROPHONE_STATUS_RESPONSE, *PARCEN_MICROPHONE_STATUS_RESPONSE;

static_assert(sizeof(ARCEN_MICROPHONE_BIND_REQUEST) == 84,
              "Rust and kernel bind layouts must remain identical");
static_assert(sizeof(ARCEN_MICROPHONE_FEED_REQUEST) == 1936,
              "Rust and kernel feed layouts must remain identical");
static_assert(sizeof(ARCEN_MICROPHONE_STOP_REQUEST) == 8,
              "Rust and kernel stop layouts must remain identical");
static_assert(sizeof(ARCEN_MICROPHONE_STATUS_RESPONSE) == 32,
              "Rust and kernel status layouts must remain identical");
