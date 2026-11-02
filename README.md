# cooperalianca-library

Rust SDK and JSON CLI for the Cooperalianca Agência Virtual customer portal
(`portal.cooperalianca.com.br/agenciavirtual`). Designed for AI agents running
in a Linux/Unix sandbox, including Muse: direct HTTP only, strict tool inputs,
encrypted credentials and persistent sessions. No browser, browser driver,
browser extension, or browser fallback is used anywhere in the SDK.

It exposes every function the portal's web app performs, one tool per
operation. See [AUDIT.md](AUDIT.md) for how they were found and what is not
verified.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/imabee-glacier/cooperalianca-library/main/install/cooperalianca-library.sh | bash
```

## Build and check

Use current stable Rust on Linux or another Unix platform. Dependencies
are pinned by Cargo.lock. Run:

```sh
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo test --doc --locked
cargo build --release --locked
cargo audit --deny warnings
```

`cargo audit` requires `cargo install cargo-audit --locked`. CI runs these
same checks. Tests use local HTTP servers and fake credentials; they never
contact a live account. The executable is `target/release/cooperalianca`.

## Credentials and unattended operation

Enroll once with a password prompt that does not echo:

```sh
cooperalianca setup-auth --store-dir /absolute/private/path/cooperalianca-store \
  --username customer@example.test
```

The username is the e-mail or registration number the portal login accepts.
For sandbox enrollment, put **only the password** in a private UTF-8 file
outside any Git repository and the store directory, with mode `0600`.
A single trailing LF or CRLF is removed; password spaces are preserved.
Then run:

```sh
cooperalianca setup-auth --store-dir /absolute/private/path/cooperalianca-store \
  --username customer@example.test --from-file /absolute/private/path/password
```

The command reads the file without printing it, encrypts the credentials,
verifies the encrypted round trip, overwrites the source, then deletes it.
If enrollment fails before persistence, the source is retained. If source
cleanup fails after persistence, the command reports an error; the encrypted
credentials may already be enrolled. Overwriting and unlinking cannot
promise physical erasure from SSDs, copy-on-write filesystems, snapshots,
or backups.

Passwords are never command-line arguments or environment variables. Never
paste one into chat. Use a persistent private sandbox volume for the store if
it must survive a sandbox restart.

The store contains:

| File | Purpose |
| --- | --- |
| `credentials.key` | Random 256-bit local encryption key, mode 0600 |
| `credentials.enc` | Versioned AES-256-GCM encrypted username/password |
| `session.enc` | AES-256-GCM encrypted session token |
| `store.lock` | Exclusive process lock, preventing concurrent store updates |

The directory must be owned by the current user, mode `0700`, outside Git,
with no symlinks in its path. Missing directories are created privately.
Secret files must be owned, private regular files with no hard links or
symlinks. Credential re-enrollment retains the encryption key and clears the
cached session. Writes replace files atomically and fsync before reporting
success. Encryption authenticates the normalized portal URL and record
name, preventing reuse under a different portal or as a different record.

The key and ciphertext are on the same protected filesystem to support
unattended agents. Encryption protects ciphertext-only disclosure; anyone
who can read both can decrypt the secrets. Keep both out of artifacts,
backups accessible to others, and logs. Secret buffers owned by the SDK
are zeroized on drop where practical; copies made internally by HTTP/TLS
and serialization libraries are not claimed to be fully erasable.

## Enforced offline mode

`cooperalianca --offline` refuses every portal HTTP request, including
login. The SDK equivalent is `Client::offline(base_url)`. Tool discovery,
write previews, credential enrollment, and local session deletion can run
offline. Reads return the structured `offline` error: fresh account data
necessarily requires HTTP. The complete CLI workflow is tested against a
loopback mock portal with fake credentials, never the real account.

## Session lifetime

The client reads the portal's own `EmpConfig.js` for the backend API root
and the public external token the web app sends, and refuses a backend on
another origin. Login is `POST Auth` with the enrolled credentials; the
returned bearer token is saved encrypted and reused across processes.

On HTTP 401/403 a call loads the password only if needed and performs
**one** login attempt, then retries the call once. The login POST is never
retried. Only GETs retry transient 429/502/503/504; writes are sent once.
API rejection, malformed JSON, and a missing `Content` are errors rather
than empty results.

Accounts with two-factor authentication return `two_factor_required`, and
accounts that have not accepted the portal privacy policy return
`privacy_policy_not_accepted`. Neither is automated; do them in the portal.
After rejected credentials, fix enrollment instead of repeatedly scheduling
login attempts. The SDK cannot extend a server-side expiry, and there is no
keepalive or invented refresh endpoint.

To clear the local session token, including a corrupt cache:

```sh
cooperalianca forget-session --store-dir /absolute/private/path/cooperalianca-store
```

This clears local state; it does not claim to revoke sessions on the server.

## AI agent tools

Every portal operation is its own command with flags, so an agent can run it
without piping input. Flags are the portal's own parameter names in kebab-case.
`cooperalianca tools` lists every operation with its schema, command, and flags:

```sh
cooperalianca tools
cooperalianca unmapped
cooperalianca open-invoices --codigo-uc 123 \
  --store-dir /absolute/private/path/cooperalianca-store
cooperalianca list-invoices-by-year --codigo-uc 123 --titularidade 1 --ano 2025
```

The same operations run from JSON on stdin with `tool <name>`, which also
fits hosts that call tools programmatically. Both forms share one
implementation and one schema:

```sh
printf '%s\n' '{"codigoUc":123}' | cooperalianca tool open_invoices \
  --store-dir /absolute/private/path/cooperalianca-store
```

Every input is strict (unknown fields are rejected). Reads return the
portal's `Content` unchanged, because the portal's response shapes are not
documented. Numbers keep full precision.

Operations that change portal state take a body exactly as the web app sends
it (`--body '<json>'` or `--body-file <path>`), and work in two steps: run
without `--confirm` for a preview that sends nothing, then repeat the identical
command with `--confirm --preview-id <id>`. A changed payload is refused.
Cancellations and removals (`unlink-unit`, `cancel-agreement`,
`request-contract-termination`, `deactivate-account`) also need `--target` and
an identical `--restated-target`. The host remains responsible for obtaining
the account holder's consent.

```sh
cooperalianca request-other-service --body '{"Descricao":"example"}'
cooperalianca request-other-service --body '{"Descricao":"example"}' \
  --confirm --preview-id '<from the preview>'
```

Document operations return size and SHA-256; `--output` also saves the PDF
privately:

```sh
cooperalianca invoice-pdf --codigo-uc 123 --codigo-fatura 456 --output ./fatura.pdf
```

`check-new-bills` compares a unit's open invoices with the rows saved in a
state file and reports the ones not seen before (a changed row counts as new):

```sh
export COOPERALIANCA_STORE_DIR=/absolute/private/path/cooperalianca-store
cooperalianca check-new-bills --uc 123 --state ./open-invoices.jsonl --update-state
```

`COOPERALIANCA_BASE_URL` or `--base-url` selects the portal; the default is
the Cooperalianca Agência Virtual. The base URL contains no credentials.
HTTPS is required except for loopback tests.

Some portal rejections are normal answers rather than failures. The portal
reports "no cut-off notices" as an error, so `open-invoice-warnings` and
`list-removed-units` return `[]` for it. Every other rejection of a read or of
an ordinary write is an `api` error that carries the portal's own message;
operations whose body holds a password (`register-user`, `reset-password`) never
show it. A few portal operations need a Google reCAPTCHA token (`unmapped`
lists them); the SDK does not try to get around that.

Several commands may run at once, for example a cron job and an agent call:
the CLI waits up to 60 seconds for the store lock instead of failing.

Login handling, session token endpoints, third-party sign-in, two-factor
codes, and privacy-policy acceptance are not tools; `cooperalianca unmapped`
lists them with the reason. `pay_bill` and other payment flows are not part of
the portal's API surface here: `payment_code` and `payment_link` only read
what the portal shows.

## Library use

```rust,no_run
use cooperalianca::{Client, Output, Store};
use serde_json::json;

fn main() -> cooperalianca::Result<()> {
    let store = Store::open("/absolute/private/path/cooperalianca-store")?;
    let mut client = Client::from_store(cooperalianca::DEFAULT_BASE_URL, store)?;
    if let Output::Json(invoices) = client.call("open_invoices", &json!({"codigoUc": 123}))? {
        println!("{}", cooperalianca::rows(&invoices).len());
    }
    Ok(())
}
```
