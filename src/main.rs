use clap::{
    Arg, ArgAction, ArgMatches, Command as Cli, CommandFactory, FromArgMatches, Subcommand,
};
use cooperalianca::catalog::{self, Kind, Op, Out};
use cooperalianca::tools;
use cooperalianca::{
    Client, Credentials, Error, Output, Result, Store, check_new_bills, marshal_jsonl,
    open_invoice_rows, unmarshal_jsonl,
};
use serde_json::{Map, Value, json};
use std::{io::Read, path::PathBuf};
use zeroize::Zeroizing;

#[derive(clap::Parser)]
#[command(
    name = "cooperalianca",
    version = env!("COOPERALIANCA_VERSION"),
    about = "Direct HTTP Cooperalianca Agência Virtual SDK for AI agents. Every portal operation is \
a subcommand with flags (see `tools`); `tool <name>` runs the same operations from JSON on stdin."
)]
struct Args {
    /// Refuse every portal HTTP call. Offline previews and local setup still work.
    #[arg(long, global = true)]
    offline: bool,
    #[arg(long,global=true,env="COOPERALIANCA_BASE_URL",default_value=cooperalianca::DEFAULT_BASE_URL)]
    base_url: String,
    #[arg(long, global = true, env = "COOPERALIANCA_STORE_DIR")]
    store_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// List every operation with its input schema and the CLI flags that run it.
    Tools,
    /// Portal functions that are deliberately not exposed, with the reason.
    Unmapped,
    SetupAuth {
        #[arg(long)]
        username: String,
        #[arg(long)]
        from_file: Option<PathBuf>,
    },
    /// Read tool input JSON on stdin and run one portal operation (see `tools`). Writes first
    /// return a preview; repeat with confirm, preview_id (and target/restated_target if destructive).
    /// PDF operations return size and SHA-256; --output also saves the PDF privately.
    Tool {
        name: String,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    CheckNewBills {
        #[arg(long)]
        uc: String,
        #[arg(long)]
        state: PathBuf,
        #[arg(long)]
        update_state: bool,
    },
    /// Delete the locally cached session token. Does not claim server-side revocation.
    ForgetSession,
}
/// Where to connect, shared by the fixed commands and the per-operation commands.
struct Conn {
    offline: bool,
    base_url: String,
    store_dir: Option<PathBuf>,
}
/// Another process may hold the store lock (a cron run, a second agent call). Wait for it, bounded.
fn open_store(dir: &PathBuf) -> Result<Store> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        match Store::open(dir) {
            Err(Error::Validation(m))
                if m.contains("store is busy") && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            other => return other,
        }
    }
}
impl Conn {
    fn client(&self) -> Result<Client> {
        if self.offline {
            return Client::offline(&self.base_url);
        }
        if let Some(dir) = &self.store_dir {
            Client::from_store(&self.base_url, open_store(dir)?)
        } else {
            Client::new(&self.base_url)
        }
    }
    fn store(&self) -> Result<Store> {
        open_store(
            self.store_dir
                .as_ref()
                .ok_or(Error::Validation("--store-dir is required"))?,
        )
    }
}
fn stdin_json() -> Result<Value> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        return Err(Error::TooLarge);
    }
    serde_json::from_slice(&bytes).map_err(|_| Error::Validation("stdin must contain valid JSON"))
}
fn leak(s: String) -> &'static str {
    // The CLI builds its command tree once per process, so these live for the whole run.
    Box::leak(s.into_boxed_str())
}
/// One subcommand per portal operation. Flags are the portal's own parameter names in kebab-case.
fn op_command(op: &'static Op) -> Cli {
    let mut c = Cli::new(leak(tools::kebab(op.name))).about(match op.kind {
        Kind::Read => op.help,
        Kind::Write => leak(format!(
            "{} Changes portal state: preview first, then --confirm.",
            op.help
        )),
        Kind::Destructive => leak(format!(
            "{} Destructive: preview first, then --confirm with --target and --restated-target.",
            op.help
        )),
    });
    for p in op.path_params.iter().chain(op.query) {
        c = c.arg(
            Arg::new(*p)
                .long(leak(tools::kebab(p)))
                .value_name("VALUE")
                .required(true),
        );
    }
    for p in op.optional_query {
        c = c.arg(Arg::new(*p).long(leak(tools::kebab(p))).value_name("VALUE"));
    }
    if op.body {
        c = c
            .arg(
                Arg::new("body")
                    .long("body")
                    .value_name("JSON")
                    .conflicts_with("body-file"),
            )
            .arg(
                Arg::new("body-file")
                    .long("body-file")
                    .value_name("PATH")
                    .value_parser(clap::value_parser!(PathBuf)),
            )
            .group(
                clap::ArgGroup::new("body-source")
                    .args(["body", "body-file"])
                    .required(true),
            );
    }
    if op.kind != Kind::Read {
        c = c
            .arg(
                Arg::new("confirm")
                    .long("confirm")
                    .action(ArgAction::SetTrue),
            )
            .arg(Arg::new("preview-id").long("preview-id").value_name("ID"));
    }
    if op.kind == Kind::Destructive {
        c = c
            .arg(Arg::new("target").long("target").value_name("TEXT"))
            .arg(
                Arg::new("restated-target")
                    .long("restated-target")
                    .value_name("TEXT"),
            );
    }
    if op.out == Out::Pdf {
        c = c.arg(
            Arg::new("output")
                .long("output")
                .value_name("PATH")
                .value_parser(clap::value_parser!(PathBuf)),
        );
    }
    c
}
fn op_input(op: &Op, m: &ArgMatches) -> Result<Value> {
    let mut o = Map::new();
    for p in op
        .path_params
        .iter()
        .chain(op.query)
        .chain(op.optional_query)
    {
        if let Some(v) = m.get_one::<String>(p) {
            o.insert((*p).into(), json!(v));
        }
    }
    if op.body {
        let raw = match (
            m.get_one::<String>("body"),
            m.get_one::<PathBuf>("body-file"),
        ) {
            (Some(b), _) => b.clone().into_bytes(),
            (None, Some(path)) => std::fs::read(path)?,
            _ => return Err(Error::Validation("--body or --body-file is required")),
        };
        if raw.len() > 1024 * 1024 {
            return Err(Error::TooLarge);
        }
        let body = serde_json::from_slice(&raw)
            .map_err(|_| Error::Validation("body must be valid JSON"))?;
        o.insert("body".into(), body);
    }
    if op.kind != Kind::Read {
        if m.get_flag("confirm") {
            o.insert("confirm".into(), json!(true));
        }
        if let Some(v) = m.get_one::<String>("preview-id") {
            o.insert("preview_id".into(), json!(v));
        }
    }
    if op.kind == Kind::Destructive {
        for (arg, key) in [("target", "target"), ("restated-target", "restated_target")] {
            if let Some(v) = m.get_one::<String>(arg) {
                o.insert(key.into(), json!(v));
            }
        }
    }
    Ok(Value::Object(o))
}
/// Runs one tool by name. Write previews run without a client, credentials, or network.
fn run_tool(conn: &Conn, name: &str, value: Value, output: Option<&PathBuf>) -> Result<Value> {
    if !tools::needs_client(name, &value) {
        return tools::execute_offline(name, value);
    }
    let mut c = conn.client()?;
    if let Some(path) = output {
        return match c.call(name, &value)? {
            Output::Pdf(bytes) => {
                cooperalianca::store::write_private(path, &bytes)?;
                Ok(json!(tools::pdf_info(&bytes)))
            }
            Output::Json(_) => Err(Error::Validation("--output applies to PDF operations")),
        };
    }
    c.execute_tool(name, value)
}
fn run(conn: &Conn, command: &Command) -> Result<Value> {
    match command {
        Command::Tools => Ok(json!(tools::tools())),
        Command::ForgetSession => {
            conn.store()?.clear_session()?;
            Ok(json!({"forgotten":true}))
        }
        Command::Unmapped => Ok(json!(tools::unmapped_tools())),
        Command::SetupAuth {
            username,
            from_file,
        } => {
            let base = Client::normalize_base(&conn.base_url)?;
            let store = conn.store()?;
            if let Some(path) = from_file {
                store.seed_from_file(base.as_str(), username, path)?;
            } else {
                let password = Zeroizing::new(rpassword::prompt_password("Password (no echo): ")?);
                store.enroll(
                    base.as_str(),
                    &Credentials {
                        username: username.clone(),
                        password: password.to_string(),
                    },
                )?;
            }
            Ok(json!({"enrolled":true,"source_deleted":from_file.is_some()}))
        }
        Command::Tool { name, output } => {
            let value = stdin_json()?;
            // Validate the name before opening credentials or making network calls.
            if name != "check_new_bills" && catalog::find(name).is_none() {
                return Err(Error::NotAvailable);
            }
            run_tool(conn, name, value, output.as_ref())
        }
        Command::CheckNewBills {
            uc,
            state,
            update_state,
        } => {
            let previous = match std::fs::read(state) {
                Ok(bytes) => unmarshal_jsonl(&bytes)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(e) => return Err(e.into()),
            };
            let mut c = conn.client()?;
            let Output::Json(content) = c.call("open_invoices", &json!({"codigoUc": uc}))? else {
                return Err(Error::Parse);
            };
            let current = open_invoice_rows(&content);
            let new_bills = check_new_bills(&current, &previous);
            if *update_state {
                cooperalianca::store::write_private(state, &marshal_jsonl(&current)?)?;
            }
            Ok(json!({"new_bills":new_bills,"count":new_bills.len()}))
        }
    }
}
fn main() {
    let mut cmd = Args::command();
    for op in catalog::OPS {
        cmd = cmd.subcommand(op_command(op));
    }
    let matches = cmd.get_matches();
    let conn = Conn {
        offline: matches.get_flag("offline"),
        base_url: matches
            .get_one::<String>("base_url")
            .cloned()
            .expect("base_url has a default"),
        store_dir: matches.get_one::<PathBuf>("store_dir").cloned(),
    };
    let result = match matches.subcommand() {
        Some((name, sub)) if catalog::find(&name.replace('-', "_")).is_some() => {
            let op = catalog::find(&name.replace('-', "_")).expect("checked above");
            op_input(op, sub).and_then(|input| {
                run_tool(
                    &conn,
                    op.name,
                    input,
                    sub.try_get_one::<PathBuf>("output").ok().flatten(),
                )
            })
        }
        _ => {
            let a = Args::from_arg_matches(&matches).expect("clap validated the arguments");
            run(&conn, &a.command)
        }
    };
    let value = match result {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", json!({"error":e.as_json()}));
            std::process::exit(1);
        }
    };
    // One JSON value on stdout per invocation; diagnostics never include secrets.
    if serde_json::to_writer(std::io::stdout().lock(), &value).is_err() {
        std::process::exit(1);
    }
    println!();
}
