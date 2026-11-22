# Plugin host components

This crate provides package validation and a bounded one-event subprocess
exchange. ZapFast applies requested drafts through its account-specific
application state, then returns the outcome as a JSON-RPC response.

`install` validates a ZIP package, checks its manifest, and extracts only
the executable for the selected platform. Callers must serialize installations
into the same root. Existing IDs are rejected. The host must never treat
this validation as a sandbox: a native executable has the user's operating
system permissions.

Installation writes into a temporary directory and publishes it with a
directory rename. Close the staged executable and manifest file handles before
renaming: Windows can reject a rename with access denied (OS error 5) while
files are still open, even when the same code works on Linux. Apply the same
precaution to future staged files. The
`installs_windows_executable_and_publishes_manifest` test exercises this
publication path; run it on Windows or under Wine as well as on Linux.
Wine does not replace a native Windows test. The preview 3 fix was also
confirmed by a user installing and running the example on native Windows.

`runtime::request_draft` runs one process per event and limits its execution
time and output size. Its return value is merely a proposal; it does not
write drafts. The application must recheck activation, account, chat,
unsaved editor text and attachments when applying it.

Run the standalone tests with:

```sh
cargo test --manifest-path plugins/host-core/Cargo.toml
```
