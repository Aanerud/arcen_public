#include "arcen_iddcx_model.h"

#include <algorithm>

namespace arcen::iddcx {

namespace {

[[nodiscard]] bool AdapterLuidIsZero(
    const ARCEN_IDDCX_ADAPTER_LUID& luid) noexcept {
    return luid.LowPart == 0 && luid.HighPart == 0;
}

[[nodiscard]] bool ModeValid(const ARCEN_IDDCX_MODE& mode) noexcept {
    return mode.Width >= ARCEN_IDDCX_MIN_WIDTH &&
           mode.Width <= ARCEN_IDDCX_MAX_WIDTH &&
           mode.Height >= ARCEN_IDDCX_MIN_HEIGHT &&
           mode.Height <= ARCEN_IDDCX_MAX_HEIGHT &&
           mode.RefreshMillihz >= ARCEN_IDDCX_MIN_REFRESH_MILLIHZ &&
           mode.RefreshMillihz <= ARCEN_IDDCX_MAX_REFRESH_MILLIHZ;
}

}  // namespace

bool EdidChecksumValid(const uint8_t* bytes, const size_t length) noexcept {
    if (bytes == nullptr || length != ARCEN_IDDCX_EDID_BYTES) {
        return false;
    }
    static constexpr uint8_t Header[8] = {
        0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00,
    };
    if (!std::equal(std::begin(Header), std::end(Header), bytes)) {
        return false;
    }
    uint8_t sum = 0;
    for (size_t index = 0; index < length; ++index) {
        sum = static_cast<uint8_t>(sum + bytes[index]);
    }
    return sum == 0 && bytes[126] == 0;
}

ValidationResult ValidateApplyRequest(
    const ARCEN_IDDCX_APPLY_REQUEST& request) noexcept {
    if (request.Size != sizeof(request)) {
        return {ValidationError::InvalidSize, 0};
    }
    if (request.AbiVersion != ARCEN_IDDCX_ABI_VERSION) {
        return {ValidationError::InvalidAbi, 0};
    }
    if (request.Generation == 0) {
        return {ValidationError::InvalidGeneration, 0};
    }
    if (AdapterLuidIsZero(request.RenderAdapter)) {
        return {ValidationError::InvalidRenderAdapter, 0};
    }
    const uint32_t requiredFlags =
        ARCEN_IDDCX_APPLY_REPLACE_TOPOLOGY |
        ARCEN_IDDCX_APPLY_REQUIRE_RENDER_ADAPTER;
    if ((request.Flags & requiredFlags) != requiredFlags ||
        (request.Flags & ~requiredFlags) != 0) {
        return {ValidationError::InvalidFlags, 0};
    }
    if (request.MonitorCount == 0 ||
        request.MonitorCount > ARCEN_IDDCX_MAX_MONITORS) {
        return {ValidationError::InvalidMonitorCount, 0};
    }

    uint32_t connectors = 0;
    uint32_t primaryCount = 0;
    for (uint32_t index = 0; index < request.MonitorCount; ++index) {
        const auto& monitor = request.Monitors[index];
        if (monitor.ConnectorIndex >= ARCEN_IDDCX_MAX_MONITORS) {
            return {ValidationError::DuplicateConnector,
                    monitor.ConnectorIndex};
        }
        const uint32_t connectorBit = 1u << monitor.ConnectorIndex;
        if ((connectors & connectorBit) != 0) {
            return {ValidationError::DuplicateConnector,
                    monitor.ConnectorIndex};
        }
        connectors |= connectorBit;
        if ((monitor.Flags & ARCEN_IDDCX_MONITOR_PRIMARY) != 0) {
            ++primaryCount;
        }
        if ((monitor.Flags & ~ARCEN_IDDCX_MONITOR_PRIMARY) != 0) {
            return {ValidationError::InvalidFlags, monitor.ConnectorIndex};
        }
        if (monitor.ModeCount == 0 ||
            monitor.ModeCount > ARCEN_IDDCX_MAX_MODES_PER_MONITOR) {
            return {ValidationError::InvalidModeCount,
                    monitor.ConnectorIndex};
        }
        if (monitor.PreferredModeIndex >= monitor.ModeCount) {
            return {ValidationError::InvalidPreferredMode,
                    monitor.ConnectorIndex};
        }
        for (uint32_t mode = 0; mode < monitor.ModeCount; ++mode) {
            if (!ModeValid(monitor.Modes[mode])) {
                return {ValidationError::InvalidMode,
                        monitor.ConnectorIndex};
            }
        }
        if (monitor.RotationDegrees != 0 &&
            monitor.RotationDegrees != 90 &&
            monitor.RotationDegrees != 180 &&
            monitor.RotationDegrees != 270) {
            return {ValidationError::InvalidRotation,
                    monitor.ConnectorIndex};
        }
        if (!EdidChecksumValid(monitor.Edid,
                               ARCEN_IDDCX_EDID_BYTES)) {
            return {ValidationError::InvalidEdid,
                    monitor.ConnectorIndex};
        }
    }
    if (primaryCount != 1) {
        return {ValidationError::InvalidPrimaryCount, 0};
    }
    return {ValidationError::None, 0};
}

LifecycleModel::LifecycleModel() noexcept
    : adapter_(AdapterState::NotStarted),
      activeGeneration_(0),
      pendingGeneration_(0),
      pendingMonitorCount_(0),
      monitors_{} {
    monitors_.fill(MonitorState::Absent);
}

AdapterState LifecycleModel::Adapter() const noexcept {
    return adapter_;
}

uint32_t LifecycleModel::ActiveGeneration() const noexcept {
    return activeGeneration_;
}

uint32_t LifecycleModel::ActiveMonitorCount() const noexcept {
    return static_cast<uint32_t>(std::count(
        monitors_.begin(), monitors_.end(), MonitorState::Present));
}

MonitorState LifecycleModel::Monitor(
    const uint32_t connectorIndex) const noexcept {
    if (connectorIndex >= monitors_.size()) {
        return MonitorState::Failed;
    }
    return monitors_[connectorIndex];
}

bool LifecycleModel::BeginAdapterInitialization() noexcept {
    if (adapter_ != AdapterState::NotStarted) {
        return false;
    }
    adapter_ = AdapterState::Initializing;
    return true;
}

void LifecycleModel::FinishAdapterInitialization(
    const bool success) noexcept {
    adapter_ = success ? AdapterState::Ready : AdapterState::Failed;
}

bool LifecycleModel::BeginApply(const uint32_t generation,
                                const uint32_t monitorCount) noexcept {
    if (adapter_ != AdapterState::Ready || generation == 0 ||
        monitorCount == 0 || monitorCount > monitors_.size() ||
        pendingGeneration_ != 0) {
        return false;
    }
    pendingGeneration_ = generation;
    pendingMonitorCount_ = monitorCount;
    monitors_.fill(MonitorState::Absent);
    for (uint32_t index = 0; index < monitorCount; ++index) {
        monitors_[index] = MonitorState::Arriving;
    }
    return true;
}

bool LifecycleModel::MarkMonitorPresent(
    const uint32_t connectorIndex) noexcept {
    if (connectorIndex >= monitors_.size() ||
        monitors_[connectorIndex] != MonitorState::Arriving) {
        return false;
    }
    monitors_[connectorIndex] = MonitorState::Present;
    return true;
}

bool LifecycleModel::CommitApply() noexcept {
    if (pendingGeneration_ == 0 ||
        ActiveMonitorCount() != pendingMonitorCount_) {
        return false;
    }
    activeGeneration_ = pendingGeneration_;
    pendingGeneration_ = 0;
    pendingMonitorCount_ = 0;
    return true;
}

bool LifecycleModel::BeginRemove(const uint32_t generation,
                                 const bool allowAnyGeneration) noexcept {
    if (pendingGeneration_ != 0 ||
        (!allowAnyGeneration && generation != activeGeneration_)) {
        return false;
    }
    for (auto& monitor : monitors_) {
        if (monitor == MonitorState::Present) {
            monitor = MonitorState::Departing;
        }
    }
    return true;
}

bool LifecycleModel::MarkMonitorAbsent(
    const uint32_t connectorIndex) noexcept {
    if (connectorIndex >= monitors_.size() ||
        monitors_[connectorIndex] != MonitorState::Departing) {
        return false;
    }
    monitors_[connectorIndex] = MonitorState::Absent;
    return true;
}

bool LifecycleModel::CommitRemove() noexcept {
    if (std::any_of(monitors_.begin(), monitors_.end(),
                    [](const MonitorState state) {
                        return state != MonitorState::Absent;
                    })) {
        return false;
    }
    activeGeneration_ = 0;
    return true;
}

void LifecycleModel::FailApplyAndRestore(
    const uint32_t previousGeneration,
    const std::array<MonitorState,
                     ARCEN_IDDCX_MAX_MONITORS>& previous) noexcept {
    activeGeneration_ = previousGeneration;
    pendingGeneration_ = 0;
    pendingMonitorCount_ = 0;
    monitors_ = previous;
}

}  // namespace arcen::iddcx
