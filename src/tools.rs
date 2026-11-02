//! Agent-facing strict tool inputs, schemas, and explicit safety metadata, one tool per portal operation.
use crate::catalog::{self, Kind, Method, Op, Out};
use crate::client::{Output, required};
use crate::{Client, Error, Result, check_new_bills, rows};
use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

#[derive(Serialize, Debug)]
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub output_schema: Value,
    pub annotations: Annotations,
}
#[derive(Serialize, Debug)]
pub struct Annotations {
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
    pub moves_money: bool,
    pub changes_state: bool,
}
#[derive(Serialize)]
pub struct PdfInfo {
    pub size_bytes: usize,
    pub sha256: String,
    pub content_type: String,
}
fn string_or_int() -> Value {
    json!({"type":["string","integer"]})
}
fn op_schema(op: &Op) -> Value {
    let mut props = Map::new();
    let mut required = Vec::new();
    for p in op.path_params.iter().chain(op.query) {
        props.insert((*p).into(), string_or_int());
        required.push(json!(p));
    }
    for p in op.optional_query {
        props.insert((*p).into(), string_or_int());
    }
    if op.body {
        props.insert(
            "body".into(),
            json!({"type":["object","array"],"description":"JSON body exactly as the portal's web app sends it. Its shape is not documented."}),
        );
        required.push(json!("body"));
    }
    if op.kind != Kind::Read {
        props.insert("confirm".into(), json!({"type":"boolean"}));
        props.insert(
            "preview_id".into(),
            json!({"type":"string","description":"From the preview returned when confirm is absent or false."}),
        );
    }
    if op.kind == Kind::Destructive {
        props.insert(
            "target".into(),
            json!({"type":"string","description":"What is being removed, in plain words."}),
        );
        props.insert("restated_target".into(), json!({"type":"string","description":"Must equal target exactly when confirm is true."}));
    }
    json!({"type":"object","additionalProperties":false,"properties":props,"required":required})
}
fn tool_for(op: &'static Op) -> Tool {
    let changes = op.kind != Kind::Read;
    Tool {
        name: op.name,
        description: op.help,
        input_schema: op_schema(op),
        output_schema: if op.out == Out::Pdf {
            json!({"type":"object","properties":{"size_bytes":{"type":"integer"},"sha256":{"type":"string"},"content_type":{"type":"string"}},"required":["size_bytes","sha256","content_type"]})
        } else if changes {
            json!({"anyOf":[{"type":"object","properties":{"sent":{"const":false},"preview_id":{"type":"string"}},"required":["sent","preview_id"]},{}]})
        } else {
            json!({})
        },
        annotations: Annotations {
            read_only: !changes,
            destructive: op.kind == Kind::Destructive,
            idempotent: !changes,
            moves_money: false,
            changes_state: changes,
        },
    }
}
pub fn tools() -> Vec<Tool> {
    let mut all: Vec<Tool> = catalog::OPS.iter().map(tool_for).collect();
    all.push(Tool {
        name: "check_new_bills",
        description: "Fetch a unit's open invoices and return rows not present in previous.",
        input_schema: json!({"type":"object","additionalProperties":false,"properties":{"codigoUc":string_or_int(),"previous":{"type":"array"}},"required":["codigoUc"]}),
        output_schema: json!({"type":"object","properties":{"new_bills":{"type":"array"},"count":{"type":"integer"}},"required":["new_bills","count"]}),
        annotations: Annotations {
            read_only: true,
            destructive: false,
            idempotent: true,
            moves_money: false,
            changes_state: false,
        },
    });
    all
}
pub fn unmapped_tools() -> Vec<Value> {
    catalog::UNMAPPED
        .iter()
        .map(|(n, why)| json!({"portal_function":n,"reason":why}))
        .collect()
}
fn scalar(v: &Value) -> Result<String> {
    let s = match v {
        Value::String(s) => s.trim().to_owned(),
        Value::Number(n) if n.is_i64() || n.is_u64() => n.to_string(),
        _ => return Err(Error::Validation("parameters must be strings or integers")),
    };
    required(&s)?;
    if s.len() > 256 || s.chars().any(char::is_control) {
        return Err(Error::Validation(
            "parameter is too long or has control characters",
        ));
    }
    Ok(s)
}
fn encode_segment(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
/// A validated call: where it goes, what it sends, and what must be confirmed first.
pub struct Plan {
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: Option<Value>,
    pub preview_id: String,
    confirm: bool,
    given_preview: Option<String>,
    target: Option<(String, String)>,
}
pub fn plan(op: &Op, input: &Value) -> Result<Plan> {
    let obj = input
        .as_object()
        .ok_or(Error::Validation("tool input must be a JSON object"))?;
    let mut allowed: Vec<&str> = op
        .path_params
        .iter()
        .chain(op.query)
        .chain(op.optional_query)
        .copied()
        .collect();
    if op.body {
        allowed.push("body");
    }
    if op.kind != Kind::Read {
        allowed.extend(["confirm", "preview_id"]);
    }
    if op.kind == Kind::Destructive {
        allowed.extend(["target", "restated_target"]);
    }
    if obj.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err(Error::Validation("tool input does not match schema"));
    }
    let mut path = op.path.to_owned();
    for p in op.path_params {
        let v = scalar(
            obj.get(*p)
                .ok_or(Error::Validation("missing required parameter"))?,
        )?;
        path = path.replace(&format!("{{{p}}}"), &encode_segment(&v));
    }
    let mut query = Vec::new();
    for p in op.query {
        let v = scalar(
            obj.get(*p)
                .ok_or(Error::Validation("missing required parameter"))?,
        )?;
        query.push(((*p).to_owned(), v));
    }
    for p in op.optional_query {
        if let Some(v) = obj.get(*p) {
            query.push(((*p).to_owned(), scalar(v)?));
        }
    }
    let body = if op.body {
        let b = obj
            .get("body")
            .ok_or(Error::Validation("missing required parameter"))?;
        if !(b.is_object() || b.is_array()) {
            return Err(Error::Validation("body must be a JSON object or array"));
        }
        if serde_json::to_vec(b).map_err(|_| Error::Parse)?.len() > 1024 * 1024 {
            return Err(Error::TooLarge);
        }
        Some(b.clone())
    } else {
        None
    };
    let target = if op.kind == Kind::Destructive {
        let get = |k| obj.get(k).and_then(Value::as_str).map(str::to_owned);
        match (get("target"), get("restated_target")) {
            (Some(t), r) => {
                required(&t)?;
                Some((t, r.unwrap_or_default()))
            }
            _ => return Err(Error::Validation("target is required")),
        }
    } else {
        None
    };
    let preview_id = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&json!([
                op.name,
                path,
                query,
                body,
                target.as_ref().map(|t| &t.0)
            ]))
            .map_err(|_| Error::Parse)?
        )
    );
    Ok(Plan {
        path,
        query,
        body,
        preview_id,
        confirm: obj.get("confirm").and_then(Value::as_bool).unwrap_or(false),
        given_preview: obj
            .get("preview_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        target,
    })
}
impl Plan {
    /// Writes only proceed with an unchanged preview, `confirm`, and (if destructive) the target restated.
    fn ready(&self, op: &Op) -> Result<bool> {
        if op.kind == Kind::Read {
            return Ok(true);
        }
        if !self.confirm {
            return Ok(false);
        }
        if self.given_preview.as_deref() != Some(self.preview_id.as_str()) {
            return Err(Error::Validation("preview does not match payload"));
        }
        if let Some((t, r)) = &self.target
            && t != r
        {
            return Err(Error::Validation("target must match exactly"));
        }
        Ok(true)
    }
    fn preview(&self, op: &Op) -> Value {
        json!({
            "sent": false,
            "action": op.name,
            "method": match op.method { Method::Get => "GET", Method::Post => "POST", Method::Put => "PUT" },
            "path": self.path,
            "query": self.query,
            "body": self.body,
            "destructive": op.kind == Kind::Destructive,
            "preview_id": self.preview_id,
        })
    }
}
/// Offline tools never construct a client or access credentials: only write previews run here.
pub fn execute_offline(name: &str, v: Value) -> Result<Value> {
    let op = catalog::find(name).ok_or(Error::NotAvailable)?;
    if op.kind == Kind::Read {
        return Err(Error::NotAvailable);
    }
    let p = plan(op, &v)?;
    if p.ready(op)? {
        return Err(Error::NotAvailable);
    }
    Ok(p.preview(op))
}
pub fn needs_client(name: &str, v: &Value) -> bool {
    match catalog::find(name) {
        Some(op) if op.kind != Kind::Read => {
            v.get("confirm").and_then(Value::as_bool).unwrap_or(false)
        }
        _ => true,
    }
}
pub fn pdf_info(bytes: &[u8]) -> PdfInfo {
    PdfInfo {
        size_bytes: bytes.len(),
        sha256: format!("{:x}", Sha256::digest(bytes)),
        content_type: "application/pdf".into(),
    }
}
impl Client {
    /// Runs one catalog operation by name with its tool input.
    pub fn call(&mut self, name: &str, v: &Value) -> Result<Output> {
        let op = catalog::find(name).ok_or(Error::NotAvailable)?;
        let p = plan(op, v)?;
        if !p.ready(op)? {
            return Ok(Output::Json(p.preview(op)));
        }
        self.execute(op, &p.path, &p.query, p.body.as_ref())
    }
    pub fn execute_tool(&mut self, name: &str, v: Value) -> Result<Value> {
        if name == "check_new_bills" {
            let obj = v
                .as_object()
                .ok_or(Error::Validation("tool input must be a JSON object"))?;
            if obj.keys().any(|k| k != "codigoUc" && k != "previous") {
                return Err(Error::Validation("tool input does not match schema"));
            }
            let previous = match obj.get("previous") {
                Some(Value::Array(a)) => a.clone(),
                None => Vec::new(),
                _ => return Err(Error::Validation("previous must be an array")),
            };
            let uc = obj
                .get("codigoUc")
                .ok_or(Error::Validation("missing required parameter"))?;
            let Output::Json(content) = self.call("open_invoices", &json!({"codigoUc": uc}))?
            else {
                return Err(Error::Parse);
            };
            let new_bills = check_new_bills(&rows(&content), &previous);
            return Ok(json!({"count":new_bills.len(),"new_bills":new_bills}));
        }
        Ok(match self.call(name, &v)? {
            Output::Json(v) => v,
            Output::Pdf(b) => json!(pdf_info(&b)),
        })
    }
}
impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::Validation(_) => "validation",
            Self::Unauthenticated => "unauthenticated",
            Self::TwoFactor => "two_factor_required",
            Self::PrivacyPolicy => "privacy_policy_not_accepted",
            Self::Transport => "transport",
            Self::Http(_) => "http",
            Self::Api => "api",
            Self::Parse => "parse",
            Self::TooLarge => "response_too_large",
            Self::Io(_) => "io",
            Self::Crypto => "crypto",
            Self::NotAvailable => "not_available",
        }
    }
    pub fn as_json(&self) -> Value {
        json!({"code":self.code(),"message":self.to_string(),"retryable":matches!(self,Self::Transport|Self::Http(429|502|503|504))})
    }
}
