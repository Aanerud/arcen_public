#include <array>
#include <cstdlib>
#include <iostream>

#include "arcen_microphone_ring.h"

namespace {

[[noreturn]] void Fail(const char* message) {
    std::cerr << message << '\n';
    std::exit(1);
}

void Require(bool condition, const char* message) {
    if (!condition) {
        Fail(message);
    }
}

ARCEN_MICROPHONE_BIND_REQUEST Binding(ULONG generation) {
    ARCEN_MICROPHONE_BIND_REQUEST request{};
    request.Version = ARCEN_MICROPHONE_CONTRACT_VERSION;
    request.WtsSessionId = 12;
    request.Generation = generation;
    const UCHAR sid[] = {
        1, 2, 0, 0, 0, 0, 0, 5,
        21, 0, 0, 0, 0xE8, 3, 0, 0,
    };
    request.SidLength = sizeof(sid);
    RtlCopyMemory(request.Sid, sid, sizeof(sid));
    return request;
}

}  // namespace

int main() {
    static_assert(IOCTL_ARCEN_MICROPHONE_BIND == 0x8000A000u);
    static_assert(IOCTL_ARCEN_MICROPHONE_FEED == 0x8000A004u);
    static_assert(IOCTL_ARCEN_MICROPHONE_STOP == 0x8000A008u);
    static_assert(IOCTL_ARCEN_MICROPHONE_STATUS == 0x8000600Cu);

    ARCEN_MICROPHONE_RING ring;
    ArcenMicrophoneRingInitialize(&ring);
    auto binding = Binding(7);
    Require(
        ArcenMicrophoneRingBind(&ring, &binding) == STATUS_SUCCESS,
        "valid binding rejected");

    auto stale = Binding(6);
    Require(
        ArcenMicrophoneRingBind(&ring, &stale) == STATUS_ACCESS_DENIED,
        "stale generation accepted");
    auto crossSession = Binding(8);
    crossSession.WtsSessionId = 13;
    Require(
        ArcenMicrophoneRingBind(&ring, &crossSession) ==
            STATUS_ACCESS_DENIED,
        "cross-session rebind accepted");

    ULONG readerGeneration = 0;
    Require(
        ArcenMicrophoneRingAuthorizeReader(
            &ring,
            binding.WtsSessionId,
            &readerGeneration) == STATUS_SUCCESS &&
            readerGeneration == binding.Generation,
        "authorized reader identity rejected");
    ULONG deniedGeneration = 0;
    Require(
        ArcenMicrophoneRingAuthorizeReader(
            &ring,
            crossSession.WtsSessionId,
            &deniedGeneration) == STATUS_ACCESS_DENIED &&
            deniedGeneration == 0,
        "cross-session reader identity accepted");

    std::array<SHORT, ARCEN_MICROPHONE_FRAME_SAMPLES> frame{};
    for (SHORT value = 1;
         value <= static_cast<SHORT>(ARCEN_MICROPHONE_RING_FRAMES + 2);
         ++value) {
        frame.fill(value);
        Require(
            ArcenMicrophoneRingWrite(
                &ring,
                binding.WtsSessionId,
                binding.Generation,
                binding.Sid,
                binding.SidLength,
                reinterpret_cast<const UCHAR*>(frame.data())) ==
                STATUS_SUCCESS,
            "authorized frame rejected");
    }

    ARCEN_MICROPHONE_STATUS_RESPONSE status;
    ArcenMicrophoneRingStatus(&ring, &status);
    Require(
        status.State == ArcenMicrophoneStateBound &&
            status.QueuedFrames == ARCEN_MICROPHONE_RING_FRAMES &&
            status.Overruns == 2,
        "bounded overrun status is wrong");

    std::array<SHORT, ARCEN_MICROPHONE_FRAME_SAMPLES> output{};
    output.fill(9);
    Require(
        ArcenMicrophoneRingRead(
            &ring,
            crossSession.WtsSessionId,
            readerGeneration,
            output.data()) == STATUS_ACCESS_DENIED,
        "cross-session reader was not rejected");
    for (SHORT sample : output) {
        Require(sample == 0, "cross-session reader did not receive silence");
    }
    ArcenMicrophoneRingStatus(&ring, &status);
    Require(
        status.QueuedFrames == ARCEN_MICROPHONE_RING_FRAMES,
        "cross-session reader drained queued audio");

    Require(
        ArcenMicrophoneRingRead(
            &ring,
            binding.WtsSessionId,
            readerGeneration,
            output.data()) == STATUS_SUCCESS,
        "authorized ring read failed");
    Require(output.front() == 3, "ring did not drop the oldest frames");
    for (ULONG index = 1; index < ARCEN_MICROPHONE_RING_FRAMES; ++index) {
        Require(
            ArcenMicrophoneRingRead(
                &ring,
                binding.WtsSessionId,
                readerGeneration,
                output.data()) == STATUS_SUCCESS,
            "authorized queued ring read failed");
    }
    output.fill(9);
    Require(
        ArcenMicrophoneRingRead(
            &ring,
            binding.WtsSessionId,
            readerGeneration,
            output.data()) == STATUS_SUCCESS,
        "authorized underrun read failed");
    for (SHORT sample : output) {
        Require(sample == 0, "underrun did not emit exact silence");
    }
    ArcenMicrophoneRingStatus(&ring, &status);
    Require(
        status.QueuedFrames == 0 && status.Underruns == 1,
        "underrun status is wrong");

    frame.fill(4);
    Require(
        ArcenMicrophoneRingWrite(
            &ring,
            binding.WtsSessionId,
            binding.Generation + 1,
            binding.Sid,
            binding.SidLength,
            reinterpret_cast<const UCHAR*>(frame.data())) ==
            STATUS_ACCESS_DENIED,
        "stale frame accepted");

    auto nextGeneration = Binding(8);
    Require(
        ArcenMicrophoneRingBind(&ring, &nextGeneration) == STATUS_SUCCESS,
        "new generation bind failed");
    frame.fill(7);
    Require(
        ArcenMicrophoneRingWrite(
            &ring,
            nextGeneration.WtsSessionId,
            nextGeneration.Generation,
            nextGeneration.Sid,
            nextGeneration.SidLength,
            reinterpret_cast<const UCHAR*>(frame.data())) == STATUS_SUCCESS,
        "new generation frame rejected");
    output.fill(9);
    Require(
        ArcenMicrophoneRingRead(
            &ring,
            binding.WtsSessionId,
            readerGeneration,
            output.data()) == STATUS_ACCESS_DENIED,
        "stale reader generation accepted");
    for (SHORT sample : output) {
        Require(sample == 0, "stale reader did not receive silence");
    }
    ArcenMicrophoneRingStatus(&ring, &status);
    Require(
        status.QueuedFrames == 1,
        "stale reader drained the new generation");
    ULONG refreshedGeneration = 0;
    Require(
        ArcenMicrophoneRingAuthorizeReader(
            &ring,
            nextGeneration.WtsSessionId,
            &refreshedGeneration) == STATUS_SUCCESS &&
            refreshedGeneration == nextGeneration.Generation,
        "same-session reader could not adopt the new generation");
    Require(
        ArcenMicrophoneRingRead(
            &ring,
            nextGeneration.WtsSessionId,
            refreshedGeneration,
            output.data()) == STATUS_SUCCESS &&
            output.front() == 7,
        "same-session reader did not resume on the new generation");
    ArcenMicrophoneRingUnbind(
        &ring, nextGeneration.WtsSessionId, nextGeneration.Generation);
    ArcenMicrophoneRingStatus(&ring, &status);
    Require(
        status.State == ArcenMicrophoneStateUnbound &&
            status.QueuedFrames == 0,
        "unbind did not clear state");
    std::cout << "Arcen microphone production ring tests passed.\n";
    return 0;
}
