# Audit and design notes

Audit date: 2026-11-02 (live verification added the same day). Subject: the public Cooperalianca Agência Virtual web
app and its JavaScript bundle. This is a fresh implementation modeled on
`condo-library`; it shares its encrypted store, offline mode, and release
pipeline. The portal client is new.

## What is evidenced

| Fact | Source |
| --- | --- |
| The portal is a single-page app; there is no HTML login form to parse | `GET /agenciavirtual/` returns a shell page loading `index.js` |
| The backend API root and an external token come from `/agenciavirtual/EmpConfig.js` | The bundle fetches that file at startup and reads `USE_BACKEND_URL_PROD` and `TOKEN_EXTERNO` |
| Requests carry `Authorization: Bearer <token>` and `use-token-externo` | The bundle's shared request helper |
| Login is `POST Auth` with `EmailInscricao` and `Senha`, answering `{Success, Content, Message}` | The bundle's auth module and one probe with a fake identity, which returned HTTP 400 `Credenciais inválidas.` |
| That probe was accepted without any reCAPTCHA header | The same probe |
| `Content` carries `Token`, `PossuiAuthDoisFatores`, `AceitePoliticaPrivacidade` | The bundle's auth module |
| Every operation in `src/catalog.rs`, with its HTTP method and parameter names | The bundle and its lazy-loaded chunks |
| `Titularidade` is 1 holder, 2 non-holder, 3 linked | An enum in the bundle |
| Invoice, contract, statement, income-tax, and generation reports are requested as binary blobs | `responseType: "blob"` in the bundle |

## Live verification

One enrolled account (the maintainer's own) was used on 2026-11-02. Verified:

- Login with the enrolled credentials succeeds with no reCAPTCHA token, and the
  session token is reused across processes.
- These reads succeeded: `profile`, `consumer_info`, `list_service_requests_home`,
  `open_invoices`, `last_invoice_summary`, `invoice_years`, `consumption_history`,
  and `list_shared_units` (empty). Observed shapes: `profile` carries the unit list
  in `RetPerfilUsuarioUcCarrosselDto`; `open_invoices` returns an object with the
  open invoices under `ListaFaturasImpressaoDTO` next to `UltimaFaturaPagaUc`;
  `invoice_years` returns `{Anos: [...]}`. `check_new_bills` uses that shape.
- `list_linked_units` returned an empty list for an account whose profile lists a
  unit, so use `profile` to find unit codes.
- `open_invoice_warnings` was rejected by the portal (`api`) for that account; the
  cause is not known.
- A first live attempt failed with HTTP 406 before any login: the portal refuses
  `Accept: application/json` on `EmpConfig.js`. The client now sends the web
  app's own `application/json, text/plain, */*`.

No write operation, PDF operation, or destructive operation has been run live.
No further failed-login probes are made: repeated failures can lock an account.

## What is not evidenced

- The shape of any request body, and of authenticated responses other than the
  reads listed above. Reads return `Content` unchanged and writes pass the body
  through. No typed models were invented.
- Server-side token lifetime, and whether `MinhaCoop/RenovarToken` could replace
  re-login. Its body is unknown, so the SDK reuses the token until a 401 and
  then logs in again once.
- Meaning of the unnamed positional path parameters, which the catalog names
  `arg1` and `arg2` as the web app passes them.
- Whether `Servicos/NiveisDeTensao` is a lookup or a request; it is gated as a
  write.
- That any operation not listed under live verification behaves as the bundle
  suggests. Tests run against a local mock portal with fake credentials.

## Design decisions

- **Same origin.** The backend named by `EmpConfig.js` must share the portal
  origin, and redirects may not leave it. Both values are read at runtime and
  never committed; the external token is sent only to that origin.
- **One login attempt, never retried.** A rejected password ends the attempt.
- **Writes are gated.** Every state-changing operation needs an unchanged
  preview and `confirm`; destructive ones also need the target restated.
  POST-based document generators and the billing simulation change nothing and
  are reads.
- **Not exposed.** Login itself, session-token endpoints, third-party sign-in,
  token material for external sites, privacy-policy acceptance, and two-factor
  code validation. See `cooperalianca unmapped`.
- **Fail closed.** Wrong response envelopes, malformed JSON, a missing
  `Content`, and non-PDF document bodies are errors, never empty results.
- **Not a security finding about the SDK:** the portal serves its external
  token in a public file. That is the operator's configuration and is only
  mentioned here so the operator can review it.
