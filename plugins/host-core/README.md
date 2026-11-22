# Experimental plugin host components

This crate is **not connected to ZapFast**. Its package validation and
one-event subprocess experiment are prototypes, not an installable plugin
manager or a complete JSON-RPC implementation. In particular, the caller
must still apply a draft safely and return the actual JSON-RPC response.

`install` validates a ZIP package, checks its manifest, and extracts only
the executable for the selected platform. Callers must serialize installations
into the same root. Existing IDs are rejected. The host must never treat
this validation as a sandbox: a native executable has the user's operating
system permissions.

`runtime::request_draft` runs one process per event and limits its execution
time and output size. Its return value is merely a proposal; it does not
write drafts. The application must recheck activation, account, chat,
unsaved editor text and attachments when applying it.

Run the standalone tests with:

```sh
cargo test --manifest-path plugins/host-core/Cargo.toml
```
