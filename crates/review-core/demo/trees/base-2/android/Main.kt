package com.example.ledger

import android.os.Bundle
import android.util.Log
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.lifecycle.lifecycleScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/** One account's balance, as the API returns it. */
data class Balance(val account: String, val cents: Long, val currency: String = "USD") {
    val isNegative: Boolean
        get() = cents < 0

    fun formatted(): String = "%s %.2f %s".format(account, cents / 100.0, currency)
}

sealed interface LoadState {
    data object Loading : LoadState
    data class Loaded(val balances: List<Balance>) : LoadState
    data class Failed(val error: Throwable) : LoadState
}

class MainActivity : ComponentActivity() {
    private val client by lazy { LedgerClient(baseUrl = BuildConfig.API_URL) }
    private val bridge = LedgerBridge()

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)
        render(LoadState.Loading)
        lifecycleScope.launch { render(load()) }
    }

    override fun onDestroy() {
        super.onDestroy()
    }

    private suspend fun load(): LoadState = try {
        val balances = withContext(Dispatchers.IO) { client.balances() }
        LoadState.Loaded(balances)
    } catch (error: Exception) {
        LoadState.Failed(error)
    }

    private fun render(state: LoadState) {
        when (state) {
            is LoadState.Loading -> Log.d(TAG, "loading balances")
            is LoadState.Loaded -> {
                val offlineCash = bridge.balance("assets:cash")
                val negative = state.balances.count { it.isNegative }
                Log.i(TAG, "${state.balances.size} balances, $negative negative, cash $offlineCash")
                state.balances.forEach { Log.v(TAG, it.formatted()) }
            }
            is LoadState.Failed -> Log.e(TAG, "load failed", state.error)
        }
    }

    companion object {
        private const val TAG = "Ledger"
    }
}
