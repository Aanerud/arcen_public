#pragma once

#include "arcen_iddcx_contract.h"

#include <array>
#include <cstddef>
#include <cstdint>

namespace arcen::iddcx {

enum class ValidationError : uint32_t {
    None = 0,
    InvalidSize,
    InvalidAbi,
    InvalidGeneration,
    InvalidRenderAdapter,
    InvalidFlags,
    InvalidMonitorCount,
    DuplicateConnector,
    InvalidPrimaryCount,
    InvalidModeCount,
    InvalidPreferredMode,
    InvalidMode,
    InvalidRotation,
    InvalidEdid,
};

struct ValidationResult {
    ValidationError Error;
    uint32_t ConnectorIndex;

    [[nodiscard]] bool Ok() const noexcept {
        return Error == ValidationError::None;
    }
};

[[nodiscard]] bool EdidChecksumValid(const uint8_t* bytes, size_t length) noexcept;
[[nodiscard]] ValidationResult ValidateApplyRequest(
    const ARCEN_IDDCX_APPLY_REQUEST& request) noexcept;

enum class AdapterState : uint32_t {
    NotStarted = ARCEN_IDDCX_ADAPTER_NOT_STARTED,
    Initializing = ARCEN_IDDCX_ADAPTER_INITIALIZING,
    Ready = ARCEN_IDDCX_ADAPTER_READY,
    Failed = ARCEN_IDDCX_ADAPTER_FAILED,
};

enum class MonitorState : uint32_t {
    Absent = ARCEN_IDDCX_BINDING_ABSENT,
    Arriving = ARCEN_IDDCX_BINDING_ARRIVING,
    Present = ARCEN_IDDCX_BINDING_PRESENT,
    Departing = ARCEN_IDDCX_BINDING_DEPARTING,
    Failed = ARCEN_IDDCX_BINDING_FAILED,
};

class LifecycleModel final {
public:
    LifecycleModel() noexcept;

    [[nodiscard]] AdapterState Adapter() const noexcept;
    [[nodiscard]] uint32_t ActiveGeneration() const noexcept;
    [[nodiscard]] uint32_t ActiveMonitorCount() const noexcept;
    [[nodiscard]] MonitorState Monitor(uint32_t connectorIndex) const noexcept;

    bool BeginAdapterInitialization() noexcept;
    void FinishAdapterInitialization(bool success) noexcept;
    bool BeginApply(uint32_t generation, uint32_t monitorCount) noexcept;
    bool MarkMonitorPresent(uint32_t connectorIndex) noexcept;
    bool CommitApply() noexcept;
    bool BeginRemove(uint32_t generation, bool allowAnyGeneration) noexcept;
    bool MarkMonitorAbsent(uint32_t connectorIndex) noexcept;
    bool CommitRemove() noexcept;
    void FailApplyAndRestore(uint32_t previousGeneration,
                             const std::array<MonitorState,
                                              ARCEN_IDDCX_MAX_MONITORS>& previous) noexcept;

private:
    AdapterState adapter_;
    uint32_t activeGeneration_;
    uint32_t pendingGeneration_;
    uint32_t pendingMonitorCount_;
    std::array<MonitorState, ARCEN_IDDCX_MAX_MONITORS> monitors_;
};

}  // namespace arcen::iddcx
