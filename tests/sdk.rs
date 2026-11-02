use cooperalianca::catalog;
use cooperalianca::tools;
use cooperalianca::{Client, Credentials, Error, Output, Store, check_new_bills, rows};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};
use tiny_http::{Header, Response, Server};

const EXTERNAL: &str = "fakeexternal.token-1=";
struct Mock {
    base: String,
    logins: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    posts: Arc<Mutex<Vec<(String, String)>>>,
    _thread: thread::JoinHandle<()>,
}
fn mock(mode: &'static str) -> Mock {
    let server = Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_string();
    let base = format!("http://{addr}/agenciavirtual/");
    let logins = Arc::new(AtomicUsize::new(0));
    let issued = logins.clone();
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let posts = Arc::new(Mutex::new(Vec::new()));
    let seen_posts = posts.clone();
    let handle = thread::spawn(move || {
        let mut rejected_once = false;
        while let Ok(Some(mut req)) = server.recv_timeout(std::time::Duration::from_secs(2)) {
            count.fetch_add(1, Ordering::SeqCst);
            let path = req.url().to_owned();
            let method = req.method().as_str().to_owned();
            let headers: Vec<(String, String)> = req
                .headers()
                .iter()
                .map(|h| {
                    (
                        h.field.to_string().to_lowercase(),
                        h.value.as_str().to_owned(),
                    )
                })
                .collect();
            let header = |name: &str| {
                headers
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default()
            };
            let bearer = header("authorization");
            let external = header("use-token-externo");
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            let json = |v: Value| Response::from_string(v.to_string());
            let accept = header("accept");
            let mut response = if path.ends_with(".js") && !accept.contains("*/*") {
                // The real portal serves static files only when */* is acceptable.
                Response::from_string("").with_status_code(406)
            } else if path == "/agenciavirtual/EmpConfig.js" {
                let api = if mode == "hostile_config" {
                    "https://evil.invalid/api/".to_owned()
                } else {
                    format!("http://{addr}/agenciavirtual3bff/api/")
                };
                Response::from_string(format!(
                    "const Emp = {{ Config: {{\n  USE_BACKEND_URL_PROD: \"{api}\",\n  TOKEN_EXTERNO: \"{EXTERNAL}\"\n}} }};\nexport default Emp;"
                ))
            } else if path == "/agenciavirtual3bff/api/Auth" {
                assert_eq!(method, "POST");
                assert_eq!(external, EXTERNAL);
                assert!(body.contains("\"Senha\":\"fake-password\""));
                assert!(body.contains("\"EmailInscricao\":\"fake@example.test\""));
                let n = issued.fetch_add(1, Ordering::SeqCst) + 1;
                match mode {
                    "bad_password" => json(json!({"Success":false,"Message":"fake-password"}))
                        .with_status_code(400),
                    "two_factor" => json(
                        json!({"Success":true,"Content":{"Token":"t","PossuiAuthDoisFatores":true,"AceitePoliticaPrivacidade":"S"}}),
                    ),
                    "no_policy" => json(
                        json!({"Success":true,"Content":{"Token":"t","PossuiAuthDoisFatores":false,"AceitePoliticaPrivacidade":"N"}}),
                    ),
                    _ => json(
                        json!({"Success":true,"Content":{"Token":format!("jwt-{n}"),"PossuiAuthDoisFatores":false,"AceitePoliticaPrivacidade":"S"}}),
                    ),
                }
            } else {
                assert_eq!(external, EXTERNAL, "external token on every API call");
                let valid = format!("Bearer jwt-{}", issued.load(Ordering::SeqCst));
                if mode == "public" && !path.contains("Fatura") {
                    // Public reference data needs no token.
                } else if bearer != valid
                    || (mode == "expire_once" && !rejected_once && {
                        rejected_once = true;
                        true
                    })
                {
                    let mut r = json(json!({"Success":false,"Message":"Unauthorized"}));
                    r = r.with_status_code(401);
                    r.add_header(Header::from_bytes("Content-Type", "application/json").unwrap());
                    req.respond(r).unwrap();
                    continue;
                }
                if method == "POST" || method == "PUT" {
                    seen_posts
                        .lock()
                        .unwrap()
                        .push((path.clone(), body.clone()));
                }
                if (mode == "http_503" && method == "POST")
                    || (mode == "transient" && count.load(Ordering::SeqCst) <= 3 && method == "GET")
                {
                    Response::from_string("").with_status_code(503)
                } else if mode == "no_notices" && path.contains("Retiradas") {
                    json(
                        json!({"Success":false,"Message":"Sem avisos de corte para esse consumidor"}),
                    )
                } else if mode == "malformed" {
                    Response::from_string("garbage")
                } else if mode == "failed_envelope" {
                    json(json!({"Success":false,"Message":"nope"}))
                } else if mode == "missing_content" {
                    json(json!({"Success":true}))
                } else if path.contains("GerarLinkPdfFatura") {
                    assert!(path.contains("codigoUc=7") && path.contains("codigoFatura=a%26b%3D1"));
                    Response::from_string("%PDF-1.4 fake")
                } else if path.contains("RecuperarFaturasEmAberto") {
                    assert!(path.contains("codigoUc=7"));
                    Response::from_string(
                        r#"{"Success":true,"Content":{"ListaFaturasImpressaoDTO":[{"CodigoFatura":1,"Valor":9007199254740993.01,"Vencimento":"2026-11-10"}],"UltimaFaturaPagaUc":{"CodigoFatura":0}}}"#,
                    )
                } else if path.contains("Municipios") {
                    json(json!({"Success":true,"Content":[{"Codigo":1}]}))
                } else {
                    json(json!({"Success":true,"Content":null}))
                }
            };
            response.add_header(Header::from_bytes("Content-Type", "application/json").unwrap());
            req.respond(response).unwrap();
        }
    });
    Mock {
        base,
        logins,
        requests,
        posts,
        _thread: handle,
    }
}
fn creds() -> Credentials {
    Credentials {
        username: "fake@example.test".into(),
        password: "fake-password".into(),
    }
}
fn store(dir: &std::path::Path, base: &str) -> Store {
    if dir.exists() {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let s = Store::open(dir).unwrap();
    s.enroll(Client::normalize_base(base).unwrap().as_str(), &creds())
        .unwrap();
    s
}
fn json_out(c: &mut Client, name: &str, v: Value) -> Value {
    match c.call(name, &v).unwrap() {
        Output::Json(v) => v,
        Output::Pdf(_) => panic!("unexpected PDF"),
    }
}

#[test]
fn encrypted_session_survives_restart_without_password_login() {
    let m = mock("session");
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), &m.base);
    let mut c = Client::from_store(&m.base, s).unwrap();
    let rows = json_out(&mut c, "open_invoices", json!({"codigoUc":7}));
    assert_eq!(
        rows["ListaFaturasImpressaoDTO"][0]["Valor"].to_string(),
        "9007199254740993.01"
    );
    assert_eq!(m.logins.load(Ordering::SeqCst), 1);
    drop(c);
    let bytes = fs::read(dir.path().join("session.enc")).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("jwt-1"));
    // Removing credentials demonstrates that session reuse does not even load the password.
    fs::remove_file(dir.path().join("credentials.enc")).unwrap();
    let mut c = Client::from_store(&m.base, Store::open(dir.path()).unwrap()).unwrap();
    assert_eq!(
        json_out(&mut c, "open_invoices", json!({"codigoUc":7}))["ListaFaturasImpressaoDTO"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(m.logins.load(Ordering::SeqCst), 1);
    c.forget_session().unwrap();
    assert!(!dir.path().join("session.enc").exists());
}
#[test]
fn rejected_token_renews_once() {
    let m = mock("expire_once");
    let mut c = Client::with_credentials(&m.base, creds()).unwrap();
    assert_eq!(
        json_out(&mut c, "list_linked_units", json!({})),
        Value::Null
    );
    // First login on demand, then one renewal after the 401.
    assert_eq!(m.logins.load(Ordering::SeqCst), 2);
}
#[test]
fn rejected_password_is_never_retried_and_errors_are_redacted() {
    let m = mock("bad_password");
    let mut c = Client::with_credentials(&m.base, creds()).unwrap();
    let e = c.call("list_linked_units", &json!({})).err().unwrap();
    assert!(matches!(e, Error::Unauthenticated));
    assert_eq!(m.logins.load(Ordering::SeqCst), 1);
    assert!(!e.as_json().to_string().contains("fake-password"));
}
#[test]
fn two_factor_and_unaccepted_policy_are_explicit_errors_and_store_nothing() {
    for (mode, code) in [
        ("two_factor", "two_factor_required"),
        ("no_policy", "privacy_policy_not_accepted"),
    ] {
        let m = mock(mode);
        let dir = tempfile::tempdir().unwrap();
        let mut c = Client::from_store(&m.base, store(dir.path(), &m.base)).unwrap();
        let e = c.authenticate().unwrap_err();
        assert_eq!(e.code(), code);
        assert!(!dir.path().join("session.enc").exists());
        assert_eq!(m.logins.load(Ordering::SeqCst), 1);
    }
}
#[test]
fn hostile_backend_config_is_refused() {
    let m = mock("hostile_config");
    let mut c = Client::with_credentials(&m.base, creds()).unwrap();
    assert!(matches!(
        c.call("list_linked_units", &json!({})),
        Err(Error::Validation(_))
    ));
    assert_eq!(m.logins.load(Ordering::SeqCst), 0);
}
#[test]
fn public_reference_data_needs_no_credentials() {
    let m = mock("public");
    let mut c = Client::new(&m.base).unwrap();
    assert_eq!(
        json_out(&mut c, "list_municipalities", json!({}))[0]["Codigo"],
        1
    );
    assert_eq!(m.logins.load(Ordering::SeqCst), 0);
    // Protected data without a token or credentials is an authentication error, not an empty list.
    assert!(matches!(
        c.call("open_invoices", &json!({"codigoUc":7})),
        Err(Error::Unauthenticated)
    ));
}
#[test]
fn api_failures_are_never_empty_results() {
    for (mode, want) in [
        ("malformed", "parse"),
        ("failed_envelope", "api"),
        ("missing_content", "parse"),
    ] {
        let m = mock(mode);
        let mut c = Client::with_credentials(&m.base, creds()).unwrap();
        let e = c.call("list_linked_units", &json!({})).err().unwrap();
        assert_eq!(e.code(), want, "{mode}");
        if mode == "failed_envelope" {
            // A read's rejection carries the portal's own reason so an agent can act on it.
            assert!(e.to_string().contains("nope"), "{e}");
        }
    }
}
#[test]
fn transient_reads_retry_but_writes_are_sent_once() {
    let m = mock("transient");
    let mut c = Client::with_credentials(&m.base, creds()).unwrap();
    assert!(c.call("list_linked_units", &json!({})).is_ok());
    let m = mock("http_503");
    let mut c = Client::with_credentials(&m.base, creds()).unwrap();
    let input = json!({"body":{"x":1},"confirm":true});
    let preview = tools::plan(
        catalog::find("request_other_service").unwrap(),
        &json!({"body":{"x":1}}),
    )
    .unwrap()
    .preview_id;
    let mut input = input;
    input["preview_id"] = json!(preview);
    assert!(matches!(
        c.call("request_other_service", &input),
        Err(Error::Http(503))
    ));
    assert_eq!(m.posts.lock().unwrap().len(), 1);
}
#[test]
fn pdf_operations_validate_content() {
    let m = mock("session");
    let mut c = Client::with_credentials(&m.base, creds()).unwrap();
    let Output::Pdf(bytes) = c
        .call("invoice_pdf", &json!({"codigoUc":7,"codigoFatura":"a&b=1"}))
        .unwrap()
    else {
        panic!("expected PDF")
    };
    assert!(bytes.starts_with(b"%PDF-"));
    let info = c
        .execute_tool("invoice_pdf", json!({"codigoUc":7,"codigoFatura":"a&b=1"}))
        .unwrap();
    assert_eq!(info["size_bytes"], bytes.len());
    // A non-PDF body is a parse error, never accepted as a document.
    let m = mock("malformed");
    let mut c = Client::with_credentials(&m.base, creds()).unwrap();
    assert!(matches!(
        c.call("invoice_pdf", &json!({"codigoUc":7,"codigoFatura":1})),
        Err(Error::Parse)
    ));
}
#[test]
fn writes_need_unchanged_preview_and_confirmation() {
    let m = mock("session");
    let mut c = Client::with_credentials(&m.base, creds()).unwrap();
    let input = json!({"body":{"Descricao":"Poste"}});
    let preview = json_out(&mut c, "request_other_service", input.clone());
    assert_eq!(preview["sent"], false);
    assert_eq!(m.requests.load(Ordering::SeqCst), 0);
    let id = preview["preview_id"].as_str().unwrap().to_owned();
    // Changed payload, missing preview, and wrong preview are all refused before any request.
    for bad in [
        json!({"body":{"Descricao":"Outro"},"confirm":true,"preview_id":id}),
        json!({"body":{"Descricao":"Poste"},"confirm":true}),
        json!({"body":{"Descricao":"Poste"},"confirm":true,"preview_id":"nope"}),
    ] {
        assert!(matches!(
            c.call("request_other_service", &bad),
            Err(Error::Validation(_))
        ));
    }
    assert_eq!(m.requests.load(Ordering::SeqCst), 0);
    let done = json!({"body":{"Descricao":"Poste"},"confirm":true,"preview_id":id});
    assert_eq!(json_out(&mut c, "request_other_service", done), Value::Null);
    let posts = m.posts.lock().unwrap();
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].0,
        "/agenciavirtual3bff/api/Servicos/SolicitarOutrosServicos"
    );
    assert_eq!(posts[0].1, r#"{"Descricao":"Poste"}"#);
}
#[test]
fn destructive_writes_also_need_the_target_restated() {
    let m = mock("session");
    let mut c = Client::with_credentials(&m.base, creds()).unwrap();
    let op = catalog::find("unlink_unit").unwrap();
    let base = json!({"body":{"CodigoUc":7},"target":"unit 7"});
    let id = tools::plan(op, &base).unwrap().preview_id;
    let mismatch = json!({"body":{"CodigoUc":7},"target":"unit 7","restated_target":"unit 8","confirm":true,"preview_id":id});
    assert!(matches!(
        c.call("unlink_unit", &mismatch),
        Err(Error::Validation(_))
    ));
    assert!(matches!(
        c.call("unlink_unit", &json!({"body":{"CodigoUc":7}})),
        Err(Error::Validation(_))
    ));
    assert_eq!(m.requests.load(Ordering::SeqCst), 0);
    let ok = json!({"body":{"CodigoUc":7},"target":"unit 7","restated_target":"unit 7","confirm":true,"preview_id":id});
    assert!(c.call("unlink_unit", &ok).is_ok());
    assert_eq!(m.posts.lock().unwrap().len(), 1);
}
#[test]
fn strict_inputs_and_catalog_invariants() {
    let mut names = std::collections::HashSet::new();
    for op in catalog::OPS {
        assert!(names.insert(op.name), "duplicate {}", op.name);
        for p in op.path_params {
            assert!(op.path.contains(&format!("{{{p}}}")), "{}", op.name);
        }
        assert_eq!(
            op.path.matches('{').count(),
            op.path_params.len(),
            "{}",
            op.name
        );
        // Login and token handling are internal, never tools.
        assert!(
            !matches!(op.path, "Auth" | "Auth/Social" | "Auth/TokenMobile")
                && !op.path.starts_with("MinhaCoop/GerarToken")
        );
        assert!(!op.path.starts_with('/') && !op.path.contains(".."));
    }
    let list = tools::tools();
    assert_eq!(list.len(), catalog::OPS.len() + 1);
    let cases = [
        ("open_invoices", json!({})),
        ("open_invoices", json!({"codigoUc":7,"extra":1})),
        ("open_invoices", json!({"codigoUc":{"x":1}})),
        ("open_invoices", json!({"codigoUc":"  "})),
        ("open_invoices", json!({"codigoUc":"a\nb"})),
        ("request_other_service", json!({})),
        ("request_other_service", json!({"body":"text"})),
        ("list_municipalities", json!({"confirm":true})),
    ];
    let c = Client::offline("https://portal.test/agenciavirtual").unwrap();
    drop(c);
    for (name, input) in cases {
        assert!(
            tools::plan(catalog::find(name).unwrap(), &input).is_err(),
            "{name} {input}"
        );
    }
    // Path parameters are percent-encoded and cannot escape their segment.
    let p = tools::plan(
        catalog::find("validate_reset_code").unwrap(),
        &json!({"codigo":"1/../2","email":"a@b.test"}),
    )
    .unwrap();
    assert_eq!(
        p.path,
        "Usuarios/ValidarCodigoRedefinicaoSenha/1%2F..%2F2/a%40b.test"
    );
    assert!(matches!(
        tools::execute_offline("nope", json!({})),
        Err(Error::NotAvailable)
    ));
    assert!(matches!(
        tools::execute_offline("open_invoices", json!({"codigoUc":7})),
        Err(Error::NotAvailable)
    ));
}
#[test]
fn tool_schemas_validate_inputs_and_previews() {
    for t in tools::tools() {
        assert!(jsonschema::meta::is_valid(&t.input_schema), "{}", t.name);
        assert!(jsonschema::meta::is_valid(&t.output_schema), "{}", t.name);
    }
    let list = tools::tools();
    let find = |n: &str| list.iter().find(|t| t.name == n).unwrap();
    let open = jsonschema::validator_for(&find("open_invoices").input_schema).unwrap();
    assert!(open.is_valid(&json!({"codigoUc":7})));
    assert!(!open.is_valid(&json!({"codigoUc":7,"x":1})));
    assert!(!open.is_valid(&json!({})));
    let w = find("request_other_service");
    assert!(w.annotations.changes_state && !w.annotations.read_only && !w.annotations.moves_money);
    assert!(find("unlink_unit").annotations.destructive);
    assert!(find("open_invoices").annotations.read_only);
    let preview = tools::execute_offline("request_other_service", json!({"body":{"a":1}})).unwrap();
    assert!(
        jsonschema::validator_for(&w.output_schema)
            .unwrap()
            .is_valid(&preview)
    );
    assert!(
        tools::unmapped_tools()
            .iter()
            .any(|u| u["portal_function"] == "Auth")
    );
}
#[test]
fn jsonl_exact_numbers_and_deduplication() {
    let a: Value = serde_json::from_str(r#"{"Valor":9007199254740993.01,"Id":1}"#).unwrap();
    let b = json!({"Id":2});
    let bytes = cooperalianca::marshal_jsonl(&[a.clone(), b.clone()]).unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("9007199254740993.01"));
    let back = cooperalianca::unmarshal_jsonl(&bytes).unwrap();
    assert_eq!(back, vec![a.clone(), b.clone()]);
    assert_eq!(
        check_new_bills(&[a.clone(), b.clone(), b.clone()], &[a]),
        vec![b]
    );
    assert_eq!(rows(&Value::Null).len(), 0);
    assert_eq!(rows(&json!({"x":1})).len(), 1);
    assert_eq!(rows(&json!([1, 2])).len(), 2);
    assert!(cooperalianca::unmarshal_jsonl(b"not json\n").is_err());
}
#[test]
fn check_new_bills_tool_compares_rows() {
    let m = mock("session");
    let mut c = Client::with_credentials(&m.base, creds()).unwrap();
    let out = c
        .execute_tool("check_new_bills", json!({"codigoUc":7}))
        .unwrap();
    assert_eq!(out["count"], 1);
    let prev: Value = serde_json::from_str(
        r#"[{"CodigoFatura":1,"Valor":9007199254740993.01,"Vencimento":"2026-11-10"}]"#,
    )
    .unwrap();
    let out = c
        .execute_tool("check_new_bills", json!({"codigoUc":7,"previous":prev}))
        .unwrap();
    assert_eq!(out["count"], 0);
    assert!(
        c.execute_tool("check_new_bills", json!({"codigoUc":7,"previous":"x"}))
            .is_err()
    );
}
#[test]
fn urls_and_base_validation() {
    assert!(Client::normalize_base("http://example.test/x").is_err());
    assert!(Client::normalize_base("https://user:pw@example.test/x").is_err());
    assert!(Client::normalize_base("https://example.test/x?q=1").is_err());
    assert_eq!(
        Client::normalize_base(cooperalianca::DEFAULT_BASE_URL)
            .unwrap()
            .as_str(),
        "https://portal.cooperalianca.com.br/agenciavirtual/"
    );
    assert!(Client::normalize_base("http://127.0.0.1:1/x").is_ok());
}
#[test]
fn offline_mode_blocks_all_network_even_authentication() {
    let m = mock("session");
    let mut c = Client::offline(&m.base).unwrap();
    assert!(matches!(c.authenticate(), Err(Error::Offline)));
    for t in tools::tools() {
        let op = catalog::find(t.name);
        let input = match op {
            Some(op) if op.kind == catalog::Kind::Read => {
                let mut o = serde_json::Map::new();
                for p in op.path_params.iter().chain(op.query) {
                    o.insert((*p).into(), json!(1));
                }
                if op.body {
                    o.insert("body".into(), json!({}));
                }
                Value::Object(o)
            }
            _ => continue,
        };
        assert!(
            matches!(c.execute_tool(t.name, input), Err(Error::Offline)),
            "{}",
            t.name
        );
    }
    assert!(matches!(
        c.execute_tool("check_new_bills", json!({"codigoUc":1})),
        Err(Error::Offline)
    ));
    assert!(
        c.execute_tool("request_other_service", json!({"body":{}}))
            .is_ok()
    );
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cooperalianca"))
        .args(["--offline", "--base-url", &m.base, "tool", "open_invoices"])
        .stdin(std::process::Stdio::piped())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stderr).unwrap()["error"]["code"],
        "validation"
    );
    assert_eq!(m.requests.load(Ordering::SeqCst), 0);
}
#[test]
fn cli_end_to_end_local_only_with_fake_credentials() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let m = mock("session");
    let dir = tempfile::tempdir().unwrap();
    let store_dir = dir.path().join("store");
    let password = dir.path().join("password");
    fs::write(&password, b"fake-password\n").unwrap();
    fs::set_permissions(&password, fs::Permissions::from_mode(0o600)).unwrap();
    let run = |args: &[&str], stdin: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_cooperalianca"))
            .args(["--base-url", &m.base, "--store-dir"])
            .arg(&store_dir)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let out = run(
        &[
            "setup-auth",
            "--username",
            "fake@example.test",
            "--from-file",
            password.to_str().unwrap(),
        ],
        "",
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!password.exists());
    let out = run(&["tool", "open_invoices"], r#"{"codigoUc":7}"#);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["ListaFaturasImpressaoDTO"][0]["CodigoFatura"],
        1
    );
    let pdf = dir.path().join("fatura.pdf");
    let out = run(
        &["tool", "invoice_pdf", "--output", pdf.to_str().unwrap()],
        r#"{"codigoUc":7,"codigoFatura":"a&b=1"}"#,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(fs::read(&pdf).unwrap().starts_with(b"%PDF-"));
    assert_eq!(
        fs::metadata(&pdf).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // The same operations run from flags alone, with no stdin, for agents that cannot pipe input.
    let out = run(&["open-invoices", "--codigo-uc", "7"], "");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["ListaFaturasImpressaoDTO"][0]["CodigoFatura"],
        1
    );
    let flag_pdf = dir.path().join("flags.pdf");
    let out = run(
        &[
            "invoice-pdf",
            "--codigo-uc",
            "7",
            "--codigo-fatura",
            "a&b=1",
            "--output",
            flag_pdf.to_str().unwrap(),
        ],
        "",
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(fs::read(&flag_pdf).unwrap().starts_with(b"%PDF-"));
    let state = dir.path().join("open.jsonl");
    let out = run(
        &[
            "check-new-bills",
            "--uc",
            "7",
            "--state",
            state.to_str().unwrap(),
            "--update-state",
        ],
        "",
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["count"],
        1
    );
    let out = run(
        &[
            "check-new-bills",
            "--uc",
            "7",
            "--state",
            state.to_str().unwrap(),
        ],
        "",
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["count"],
        0
    );
    // Writes from flags: preview, then the identical call with its id and --confirm.
    let out = run(&["request-other-service", "--body", r#"{"b":2}"#], "");
    let preview: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(preview["sent"], false);
    assert!(m.posts.lock().unwrap().is_empty());
    let id = preview["preview_id"].as_str().unwrap();
    let out = run(
        &[
            "request-other-service",
            "--body",
            r#"{"b":3}"#,
            "--confirm",
            "--preview-id",
            id,
        ],
        "",
    );
    assert!(!out.status.success(), "a changed body must be refused");
    assert!(m.posts.lock().unwrap().is_empty());
    let out = run(
        &[
            "request-other-service",
            "--body",
            r#"{"b":2}"#,
            "--confirm",
            "--preview-id",
            id,
        ],
        "",
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(m.posts.lock().unwrap().len(), 1);
    m.posts.lock().unwrap().clear();
    // Writes from stdin: preview, then the identical call with its id and confirm.
    let out = run(&["tool", "request_other_service"], r#"{"body":{"a":1}}"#);
    let preview: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(preview["sent"], false);
    assert!(m.posts.lock().unwrap().is_empty());
    let confirmed = json!({"body":{"a":1},"confirm":true,"preview_id":preview["preview_id"]});
    assert!(
        run(&["tool", "request_other_service"], &confirmed.to_string())
            .status
            .success()
    );
    assert_eq!(m.posts.lock().unwrap().len(), 1);
    assert!(store_dir.join("session.enc").exists());
    assert!(run(&["forget-session"], "").status.success());
    assert!(!store_dir.join("session.enc").exists());
    let version = Command::new(env!("CARGO_BIN_EXE_cooperalianca"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("cooperalianca "));
    let unknown = run(&["tool", "not_a_tool"], "{}");
    assert_eq!(
        serde_json::from_slice::<Value>(&unknown.stderr).unwrap()["error"]["code"],
        "not_available"
    );
}

#[test]
fn encrypted_store_roundtrip_tampering_and_context_binding() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), "https://portal.test/");
    let base = "https://portal.test/";
    assert_eq!(s.credentials(base).unwrap().password, "fake-password");
    assert!(s.credentials("https://other.test/").is_err());
    let path = dir.path().join("credentials.enc");
    let mut bytes = fs::read(&path).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("fake-password"));
    let n = bytes.len();
    bytes[n - 1] ^= 1;
    fs::write(&path, bytes).unwrap();
    assert!(s.credentials(base).is_err());
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}
#[test]
fn store_reenrollment_and_process_lock() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), "https://portal.test/");
    let key = fs::read(dir.path().join("credentials.key")).unwrap();
    assert!(Store::open(dir.path()).is_err());
    s.enroll("https://portal.test/", &creds()).unwrap();
    assert_eq!(key, fs::read(dir.path().join("credentials.key")).unwrap());
    drop(s);
    // Tests run in parallel threads, and a concurrently spawned CLI process holds an inherited
    // copy of the lock descriptor until it execs, so the release can lag the drop briefly.
    let mut reopened = Store::open(dir.path());
    for _ in 0..200 {
        if reopened.is_ok() {
            break;
        }
        thread::sleep(std::time::Duration::from_millis(10));
        reopened = Store::open(dir.path());
    }
    assert!(reopened.is_ok());
}
#[test]
fn store_rejects_unsafe_files_and_missing_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private");
    let s = store(&path, "https://portal.test/");
    drop(s);
    fs::set_permissions(
        path.join("credentials.key"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(Store::open(&path).is_err());
    fs::remove_file(path.join("credentials.key")).unwrap();
    assert!(Store::open(&path).is_err());
    let linked = dir.path().join("linked");
    std::os::unix::fs::symlink(&path, &linked).unwrap();
    assert!(Store::open(linked).is_err());
}
#[test]
fn seed_import_deletes_source_only_after_encryption() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("store")).unwrap();
    let source = dir.path().join("password");
    fs::write(&source, b"fake-password\r\n").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
    s.seed_from_file("https://portal.test/", "fake@example.test", &source)
        .unwrap();
    assert!(!source.exists());
    assert_eq!(
        s.credentials("https://portal.test/").unwrap().password,
        "fake-password"
    );
}
#[test]
fn seed_refuses_public_git_symlink_and_hardlink_sources() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("store")).unwrap();
    let source = dir.path().join("password");
    fs::write(&source, b"fake-password").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(s.seed_from_file("x", "u", &source).is_err());
    assert!(source.exists());
    fs::set_permissions(&source, fs::Permissions::from_mode(0o600)).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&source, &link).unwrap();
    assert!(s.seed_from_file("x", "u", &link).is_err());
    fs::remove_file(&link).unwrap();
    fs::hard_link(&source, &link).unwrap();
    assert!(s.seed_from_file("x", "u", &source).is_err());
    fs::remove_file(&link).unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    assert!(s.seed_from_file("x", "u", &source).is_err());
    assert!(source.exists());
}
#[test]
fn output_replaces_symlink_without_following_and_state_remains_private() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    let output = dir.path().join("output");
    fs::write(&target, b"original").unwrap();
    std::os::unix::fs::symlink(&target, &output).unwrap();
    cooperalianca::store::write_private(&output, b"new output").unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"original");
    assert_eq!(fs::read(&output).unwrap(), b"new output");
    assert_eq!(
        fs::metadata(&output).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn every_operation_has_a_flag_command_and_stdin_tool_still_works() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let bin = env!("CARGO_BIN_EXE_cooperalianca");
    let listed: Vec<Value> =
        serde_json::from_slice(&Command::new(bin).arg("tools").output().unwrap().stdout).unwrap();
    assert_eq!(listed.len(), catalog::OPS.len() + 1);
    for t in &listed {
        let command = t["command"].as_str().unwrap();
        if command == "check-new-bills" {
            continue;
        }
        assert!(
            Command::new(bin)
                .args([command, "--help"])
                .output()
                .unwrap()
                .status
                .success(),
            "{command}"
        );
        // Each documented flag exists on the command.
        let help = String::from_utf8(
            Command::new(bin)
                .args([command, "--help"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        for flag in t["flags"].as_object().unwrap().values() {
            let flag = flag.as_str().unwrap().split_whitespace().next().unwrap();
            assert!(help.contains(flag), "{command} lacks {flag}");
        }
    }
    let mut child = Command::new(bin)
        .args(["--offline", "tool", "request_other_service"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"body":{"a":1}}"#)
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["sent"],
        false
    );
}

#[test]
fn rejection_messages_show_except_for_password_bodies() {
    let m = mock("failed_envelope");
    let mut c = Client::with_credentials(&m.base, creds()).unwrap();
    for (name, shown) in [("request_other_service", true), ("reset_password", false)] {
        let op = catalog::find(name).unwrap();
        let id = tools::plan(op, &json!({"body":{"x":1}}))
            .unwrap()
            .preview_id;
        let e = c
            .call(
                name,
                &json!({"body":{"x":1},"confirm":true,"preview_id":id}),
            )
            .err()
            .unwrap();
        assert_eq!(e.code(), "api");
        assert_eq!(e.to_string().contains("nope"), shown, "{name}: {e}");
    }
}
#[test]
fn portal_no_notices_rejection_is_an_empty_list() {
    let m = mock("no_notices");
    let mut c = Client::with_credentials(&m.base, creds()).unwrap();
    assert_eq!(json_out(&mut c, "list_removed_units", json!({})), json!([]));
    // Other rejections of the same operation are still errors.
    let m = mock("failed_envelope");
    let mut c = Client::with_credentials(&m.base, creds()).unwrap();
    assert!(c.call("list_removed_units", &json!({})).is_err());
}
#[test]
fn cli_waits_for_a_busy_store_instead_of_failing() {
    use std::process::{Command, Stdio};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let held = store(&path, "https://portal.test/");
    let child = Command::new(env!("CARGO_BIN_EXE_cooperalianca"))
        .arg("--store-dir")
        .arg(&path)
        .arg("forget-session")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    thread::sleep(std::time::Duration::from_millis(800));
    drop(held);
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
/// A server that promises keep-alive but answers one request per connection, and hangs up on any
/// request that arrives on a reused connection, as a server or proxy dropping idle connections does.
fn closing_server() -> (String, Arc<AtomicUsize>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let logins = Arc::new(AtomicUsize::new(0));
    let count = logins.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let count = count.clone();
            thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_lowercase();
                let want: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                while buf.len() < head_end + want {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let first = head.lines().next().unwrap_or("").to_owned();
                let body = if first.contains("empconfig.js") {
                    format!(
                        "const Emp = {{ Config: {{ USE_BACKEND_URL_PROD: \"http://{addr}/api/\", TOKEN_EXTERNO: \"{EXTERNAL}\" }} }};"
                    )
                } else if first.starts_with("post /api/auth") {
                    count.fetch_add(1, Ordering::SeqCst);
                    r#"{"Success":true,"Content":{"Token":"jwt-1","PossuiAuthDoisFatores":false,"AceitePoliticaPrivacidade":"S"}}"#.into()
                } else {
                    r#"{"Success":true,"Content":[{"Codigo":1}]}"#.into()
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
                    body.len()
                );
                // The race that bites pooled clients: the next request arrives on this connection just
                // as the server drops it. Hang up on it without answering.
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(400)));
                let _ = stream.read(&mut chunk);
            });
        }
    });
    (format!("http://{addr}/"), logins)
}
#[test]
fn first_call_survives_a_server_that_closes_idle_connections() {
    let (base, logins) = closing_server();
    for _ in 0..5 {
        let mut c = Client::with_credentials(&base, creds()).unwrap();
        // Config GET, then login POST, then the read: three requests on a server that drops each
        // connection after answering. None may fail, and the login is sent exactly once.
        assert_eq!(
            json_out(&mut c, "list_linked_units", json!({}))[0]["Codigo"],
            1
        );
    }
    assert_eq!(logins.load(Ordering::SeqCst), 5);
}
#[test]
fn transport_errors_say_why_without_leaking_anything() {
    // Nothing listens on port 1: a connect failure, which is categorized and retryable.
    let mut c = Client::with_credentials("http://127.0.0.1:1/", creds()).unwrap();
    let e = c.call("list_linked_units", &json!({})).err().unwrap();
    assert!(matches!(e, Error::Transport("connect")), "{e}");
    assert_eq!(e.as_json()["retryable"], true);
    let text = e.to_string();
    assert!(
        !text.contains("127.0.0.1") && !text.contains("fake"),
        "{text}"
    );
}
