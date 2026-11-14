// A C++ wrapper over the ledger's C interface, for the desktop importer.
#include "ffi.h"

#include <cstddef>
#include <cstdint>
#include <memory>
#include <span>
#include <stdexcept>
#include <string>
#include <string_view>
#include <vector>

namespace ledger {

class Error : public std::runtime_error {
public:
    explicit Error(ledger_status status)
        : std::runtime_error(ledger_status_message(status)), status_(status) {}

    [[nodiscard]] ledger_status status() const noexcept { return status_; }

private:
    ledger_status status_;
};

struct Entry {
    std::string account;
    std::int64_t cents = 0;
};

class Ledger {
public:
    Ledger() : handle_(ledger_new(), &ledger_free) {
        if (!handle_) {
            throw Error(LEDGER_ERR_MEMORY);
        }
    }

    // Applies the entries, up to the ledger's batch limit, and returns how many it applied.
    std::size_t apply(std::span<const Entry> entries) {
        std::vector<ledger_entry> raw;
        raw.reserve(entries.size());
        for (const auto& [account, cents] : entries) {
            if (account.size() >= LEDGER_ACCOUNT_MAX) {
                throw Error(LEDGER_ERR_ARGUMENT);
            }
            ledger_entry entry{};
            account.copy(entry.account, account.size());
            entry.amount_cents = cents;
            raw.push_back(entry);
        }
        std::size_t applied = 0;
        if (auto status = ledger_apply(handle_.get(), raw.data(), raw.size(), &applied);
            status != LEDGER_OK) {
            throw Error(status);
        }
        return applied;
    }

    [[nodiscard]] std::int64_t balance(std::string_view account) const {
        return ledger_balance(handle_.get(), std::string(account).c_str());
    }

private:
    std::unique_ptr<ledger_t, decltype(&ledger_free)> handle_;
};

template <typename Range>
constexpr std::int64_t total(const Range& entries) {
    std::int64_t sum = 0;
    for (const auto& entry : entries) {
        sum += entry.cents;
    }
    return sum;
}

}  // namespace ledger
