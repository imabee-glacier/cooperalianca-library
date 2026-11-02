# Audit and design notes

Audit date: 2026-11-02. Subject: the public Cooperalianca Agência Virtual web
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

No credentialed request has been made. The single probe used a fake identity
and no further login probes are made: repeated failures can lock an account or
trigger a challenge.

## What is not evidenced

- That a successful login works without a reCAPTCHA token. The web app attaches
  one when its widget has produced it, and only the failed-credential path was
  observed without it.
- The shape of any request body or authenticated response. Reads return
  `Content` unchanged and writes pass the body through. No typed models were
  invented.
- Server-side token lifetime, and whether `MinhaCoop/RenovarToken` could replace
  re-login. Its body is unknown, so the SDK reuses the token until a 401 and
  then logs in again once.
- Meaning of the unnamed positional path parameters, which the catalog names
  `arg1` and `arg2` as the web app passes them.
- Whether `Servicos/NiveisDeTensao` is a lookup or a request; it is gated as a
  write.
- That every operation behaves live as the bundle suggests. All tests run
  against a local mock portal with fake credentials.

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
