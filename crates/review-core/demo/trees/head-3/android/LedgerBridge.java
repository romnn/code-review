package com.example.ledger;

import java.util.List;
import java.util.Objects;

/** Calls the native ledger through JNI, for balances while the device is offline. */
public final class LedgerBridge implements AutoCloseable {
    static {
        System.loadLibrary("ledger_ffi");
    }

    /** The longest account name, as native/ffi.h allows it. */
    private static final int ACCOUNT_MAX = 63;

    private long handle;

    public LedgerBridge() {
        handle = nativeNew();
        if (handle == 0L) {
            throw new IllegalStateException("could not allocate a ledger");
        }
    }

    /** One entry to apply: an account and an amount in cents. */
    public record Entry(String account, long cents) {
        public Entry {
            Objects.requireNonNull(account, "account");
            if (account.length() > ACCOUNT_MAX) {
                throw new IllegalArgumentException("account name too long: " + account);
            }
        }
    }

    /** Applies the entries, up to the ledger's batch limit, and returns how many it applied. */
    public int apply(List<Entry> entries) {
        String[] accounts = new String[entries.size()];
        long[] cents = new long[entries.size()];
        for (int i = 0; i < entries.size(); i++) {
            accounts[i] = entries.get(i).account();
            cents[i] = entries.get(i).cents();
        }
        return nativeApply(handle, accounts, cents);
    }

    public long balance(String account) {
        return nativeBalance(handle, Objects.requireNonNull(account));
    }

    @Override
    public void close() {
        if (handle != 0L) {
            nativeFree(handle);
            handle = 0L;
        }
    }

    private static native long nativeNew();

    private static native void nativeFree(long handle);

    private static native int nativeApply(long handle, String[] accounts, long[] cents);

    private static native long nativeBalance(long handle, String account);
}
