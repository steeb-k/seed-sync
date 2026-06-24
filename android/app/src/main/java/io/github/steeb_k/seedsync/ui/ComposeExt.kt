package io.github.steeb_k.seedsync.ui

import androidx.compose.runtime.Composable
import androidx.compose.runtime.State
import androidx.compose.runtime.collectAsState
import kotlinx.coroutines.flow.StateFlow

/** Collect a [StateFlow] into Compose state, seeded with an explicit initial. */
@Composable
fun <T> StateFlow<T>.collectAsStateSafe(initial: T): State<T> = collectAsState(initial)
