# Plugins

The plugin system lets coklu grow through automation, custom sync handlers, AI clipboard actions, and device-specific workflows without coupling extensions directly to core subsystems.

## Architecture

The plugin crate exposes:

- `Plugin`: lifecycle and event-handling trait.
- `PluginContext`: host context passed to plugin hooks.
- `PluginManifest`: metadata, runtime, entrypoint, and requested capabilities.
- `PluginCapabilities`: least-privilege permission set.
- `PluginRegistry`: host-side load/unload/reload and event dispatch.
- `PluginRuntime`: async runtime backend trait.
- `PluginSandbox`: sandbox/resource policy.
- `MarketplaceCatalog`: installability and trust metadata.
- `HotReloadTracker`: debounced artifact reload state.

Plugins observe `core::Event` values rather than directly calling feature crates. This preserves decoupling and makes host permission checks enforceable.

## Runtime Kinds

Current runtime descriptors:

- `Native`: first-party trusted Rust/in-process plugins only.
- `Wasm`: intended third-party runtime using WASM/WASI-style isolation.
- `Lua`: scripting runtime behind the same brokered host-call boundary.

Feature flags currently act as API gates:

```toml
runtime-wasm = []
runtime-lua = []
```

Concrete engines such as Wasmtime or Lua bindings should land behind those features in a later phase.

## Manifest Shape

A plugin manifest includes:

- stable plugin id,
- name/version/description,
- runtime kind,
- entrypoint,
- required capabilities.

The host grants a capability set at load time. Loading fails if the grant does not satisfy the manifest.

## Capabilities

Capabilities are intentionally coarse but explicit:

- input read/inject,
- clipboard read/write,
- network send,
- storage,
- device metadata,
- audio control.

The registry filters event delivery based on the granted capability set. New sensitive event types should be mapped explicitly and should default to denial until reviewed.

## Sandboxing

Sandbox levels:

- `Strict`: marketplace-safe third-party default.
- `Brokered`: host-call mediated access with explicit permissions.
- `TrustedNative`: first-party/in-process only.

Resource limits model memory, CPU time, file access, and network access. Marketplace listings must satisfy policy before install.

## Hot Reload

`HotReloadTracker` watches artifact metadata and returns one of:

- track only,
- ignore because of debounce/no change,
- reload because the artifact changed and debounce elapsed.

This keeps file watching/platform-specific hot reload outside the pure model.

## Marketplace Policy

Marketplace listings include trust level, artifacts, runtime compatibility, required capabilities, and block status. Installability requires:

- listing not blocked,
- policy grant satisfies required capabilities,
- an artifact exists for the manifest runtime.

## Future Work

- Wire real WASM and Lua engines behind `PluginRuntime`.
- Add signed artifact verification.
- Add persistent plugin install state in storage.
- Add UI flows for capability review and marketplace trust.
- Add a stable host-call ABI for sandboxed runtimes.
