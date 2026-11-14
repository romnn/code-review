/* Drives the ledger through its C interface: the FFI smoke test `make ffi-test` runs. */
#include "ffi.h"

#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define COUNT(array) (sizeof(array) / sizeof((array)[0]))

static const ledger_entry ENTRIES[] = {
    {"assets:cash", 12500},
    {"assets:bank", -4000},
    {"expenses:food", 4000},
    {"income:salary", -12500},
};

static int check(ledger_status status, const char *what) {
    if (status != LEDGER_OK) {
        fprintf(stderr, "%s failed: %s\n", what, ledger_status_message(status));
        return 0;
    }
    return 1;
}

int main(int argc, char **argv) {
    const char *account = argc > 1 ? argv[1] : "assets:cash";
    if (strlen(account) >= LEDGER_ACCOUNT_MAX) {
        fprintf(stderr, "account name too long: %s\n", account);
        return EXIT_FAILURE;
    }

    ledger_t *ledger = ledger_new();
    if (ledger == NULL) {
        return EXIT_FAILURE;
    }
    size_t applied = 0;
    int ok = check(ledger_apply(ledger, ENTRIES, COUNT(ENTRIES), &applied), "apply");
    if (ok && applied != COUNT(ENTRIES)) {
        fprintf(stderr, "applied %zu of %zu entries\n", applied, COUNT(ENTRIES));
        ok = 0;
    }

    /* Every entry has its counter-entry, so the balances sum to zero. */
    int64_t sum = 0;
    for (size_t i = 0; i < COUNT(ENTRIES); i++) {
        sum += ledger_balance(ledger, ENTRIES[i].account);
    }
    printf("%s: %" PRId64 " cents (sum %" PRId64 ")\n", account,
           ledger_balance(ledger, account), sum);

    ledger_free(ledger);
    return ok && sum == 0 ? EXIT_SUCCESS : EXIT_FAILURE;
}
