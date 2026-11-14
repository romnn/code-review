/* The ledger's C interface, for the Android app and other native callers. */
#ifndef LEDGER_FFI_H
#define LEDGER_FFI_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* The longest account name, its terminating NUL included. */
#define LEDGER_ACCOUNT_MAX 64

/* An opaque ledger; create it with ledger_new and release it with ledger_free. */
typedef struct ledger_handle ledger_t;

typedef enum {
    LEDGER_OK = 0,
    LEDGER_ERR_ARGUMENT = 1,
    LEDGER_ERR_LIMIT = 2,
    LEDGER_ERR_MEMORY = 3,
} ledger_status;

typedef struct {
    char account[LEDGER_ACCOUNT_MAX];
    int64_t amount_cents;
} ledger_entry;

ledger_t *ledger_new(void);
void ledger_free(ledger_t *ledger);

/* Applies up to LIMIT entries and stores how many it applied in *applied. */
ledger_status ledger_apply(ledger_t *ledger, const ledger_entry *entries, size_t count,
                           size_t *applied);

/* The balance of account in cents, 0 for an unknown account. */
int64_t ledger_balance(const ledger_t *ledger, const char *account);

/* A static, human-readable description of status. */
const char *ledger_status_message(ledger_status status);

#ifdef __cplusplus
}
#endif

#endif /* LEDGER_FFI_H */
