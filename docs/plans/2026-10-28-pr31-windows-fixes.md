# Windows account and claim fixes

**Goal:** Fix the four reviewed issues without changing concurrent eligibility scanning or the serial background claim queue.

**Approach:** Stop automatic CAPTCHA callbacks synchronously when interaction is required, while allowing manual challenges to complete. Reject explicitly expired non-active snapshots before a switch can change the running client or its files. Preserve the saved per-account claim device ID in encrypted bundles, including legacy bundle compatibility. Keep ZCode's native telemetry and monitoring files unchanged during switching while retaining existing per-account IDs for queries and claims.

**Validation:** Use synthetic accounts and separate `ZCODE_SWITCH_HOME`/`ZCODE_SWITCH_DATA_ROOT` for all native switch and bundle checks. No live API, activation, or claim is needed.

1. Add failing CAPTCHA tests for auto-interaction races, manual interaction, and synchronous success; implement and rerun them.
2. Add isolated switch tests for expired targets, already-active token synchronization, and unchanged native telemetry/RUM state; implement and rerun them.
3. Add bundle round-trip tests for saved IDs and old bundles; implement and rerun them.
4. Update user-facing descriptions and run the JavaScript, Rust, i18n, build, and relevant offline UI checks.
