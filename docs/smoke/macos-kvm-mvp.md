# macOS KVM MVP Smoke Checks

## Permission Smoke

Run:

```sh
cargo run -p nexkvm -- doctor
```

Expected after Accessibility is granted:

- `macOS input accessibility: ready`
- `capture ready: true`
- `inject ready: true`

Expected before Accessibility is granted:

- `macOS input accessibility: permission-required`
- `capture ready: false`
- `inject ready: false`

## Release Signing Smoke

Run:

```sh
: "${APPLE_CODESIGN_IDENTITY:?set Developer ID Application identity from security find-identity}"
: "${APPLE_NOTARY_PROFILE:?set notarytool keychain profile}"
NEXKVM_RELEASE=1 ./scripts/package-macos.sh
```

Then validate:

```sh
codesign -dvvv --entitlements :- target/package/nexkvm.app
xcrun stapler validate target/package/nexkvm.app
spctl -a -vv target/package/nexkvm.app
```

Expected:

- `codesign` shows Developer ID signing and hardened runtime.
- `stapler validate` succeeds.
- `spctl` reports accepted source for the app bundle.
