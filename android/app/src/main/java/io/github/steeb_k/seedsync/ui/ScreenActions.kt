package io.github.steeb_k.seedsync.ui

import uniffi.seed_mobile.ShareSummary

/** Callbacks the [AppScreen] invokes; implemented by [SeedAppHost]. */
class ScreenActions(
    val onCreateShare: () -> Unit,
    val onAddExisting: () -> Unit,
    val onOpenSettings: () -> Unit,
    val onEditDeviceName: (current: String) -> Unit,
    val onShowNodeAddr: () -> Unit,
    val onPauseAll: () -> Unit,
    val onResumeAll: () -> Unit,
    val onPause: (shareId: String) -> Unit,
    val onResume: (shareId: String) -> Unit,
    val onRevealKeys: (shareId: String) -> Unit,
    val onShowPeers: (shareId: String) -> Unit,
    val onRemove: (share: ShareSummary) -> Unit,
)
