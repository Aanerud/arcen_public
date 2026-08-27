#include "arcen_iddcx_model.h"

#include <array>
#include <cassert>
#include <cstdint>
#include <iostream>

namespace {

ARCEN_IDDCX_APPLY_REQUEST Request(const uint32_t monitorCount) {
    ARCEN_IDDCX_APPLY_REQUEST request{};
    request.Size = sizeof(request);
    request.AbiVersion = ARCEN_IDDCX_ABI_VERSION;
    request.Generation = 7;
    request.MonitorCount = monitorCount;
    request.RenderAdapter.LowPart = 42;
    request.Flags = ARCEN_IDDCX_APPLY_REPLACE_TOPOLOGY |
                    ARCEN_IDDCX_APPLY_REQUIRE_RENDER_ADAPTER;
    for (uint32_t index = 0; index < monitorCount; ++index) {
        auto& monitor = request.Monitors[index];
        monitor.ConnectorIndex = index;
        monitor.Flags = index == 0 ? ARCEN_IDDCX_MONITOR_PRIMARY : 0;
        monitor.ModeCount = 1;
        monitor.Modes[0] = {1920, 1080, 60000};
        monitor.Edid[0] = 0x00;
        for (size_t header = 1; header < 7; ++header) {
            monitor.Edid[header] = 0xff;
        }
        monitor.Edid[7] = 0x00;
        uint8_t checksum = 0;
        for (size_t byte = 0; byte < ARCEN_IDDCX_EDID_BYTES - 1;
             ++byte) {
            checksum = static_cast<uint8_t>(
                checksum + monitor.Edid[byte]);
        }
        monitor.Edid[ARCEN_IDDCX_EDID_BYTES - 1] =
            static_cast<uint8_t>(0u - checksum);
    }
    return request;
}

void ValidateOneThroughFour() {
    for (uint32_t count = 1; count <= ARCEN_IDDCX_MAX_MONITORS;
         ++count) {
        const auto request = Request(count);
        assert(arcen::iddcx::ValidateApplyRequest(request).Ok());
    }
}

void RejectsBrokenBoundaryContracts() {
    auto request = Request(2);
    request.Monitors[1].ConnectorIndex = 0;
    assert(arcen::iddcx::ValidateApplyRequest(request).Error ==
           arcen::iddcx::ValidationError::DuplicateConnector);

    request = Request(1);
    request.Monitors[0].Edid[10] ^= 1;
    assert(arcen::iddcx::ValidateApplyRequest(request).Error ==
           arcen::iddcx::ValidationError::InvalidEdid);

    request = Request(1);
    request.RenderAdapter = {};
    assert(arcen::iddcx::ValidateApplyRequest(request).Error ==
           arcen::iddcx::ValidationError::InvalidRenderAdapter);
}

void LifecycleRollsBackToPreviousSnapshot() {
    arcen::iddcx::LifecycleModel lifecycle;
    assert(lifecycle.BeginAdapterInitialization());
    lifecycle.FinishAdapterInitialization(true);
    assert(lifecycle.BeginApply(1, 2));
    assert(lifecycle.MarkMonitorPresent(0));
    assert(lifecycle.MarkMonitorPresent(1));
    assert(lifecycle.CommitApply());

    const std::array previous{
        arcen::iddcx::MonitorState::Present,
        arcen::iddcx::MonitorState::Present,
        arcen::iddcx::MonitorState::Absent,
        arcen::iddcx::MonitorState::Absent,
    };
    assert(lifecycle.BeginApply(2, 1));
    lifecycle.FailApplyAndRestore(1, previous);
    assert(lifecycle.ActiveGeneration() == 1);
    assert(lifecycle.ActiveMonitorCount() == 2);

    assert(lifecycle.BeginRemove(1, false));
    assert(lifecycle.MarkMonitorAbsent(0));
    assert(lifecycle.MarkMonitorAbsent(1));
    assert(lifecycle.CommitRemove());
    assert(lifecycle.ActiveGeneration() == 0);
}

}  // namespace

int main() {
    ValidateOneThroughFour();
    RejectsBrokenBoundaryContracts();
    LifecycleRollsBackToPreviousSnapshot();
    std::cout << "arcen-iddcx portable contract tests passed\n";
    return 0;
}
