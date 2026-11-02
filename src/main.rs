use clap::{Parser, Subcommand};
use cooperalianca::tools;
use cooperalianca::{
    Client, Credentials, Error, Output, Result, Store, check_new_bills, marshal_jsonl, rows,
    unmarshal_jsonl,
};
use serde_json::{Value, json};
use std::{io::Read, path::PathBuf};
use zeroize::Zeroizing;

#[derive(Parser)]
#[command(
    name = "cooperalianca",
    version = env!("COOPERALIANCA_VERSION"),
    about = "Direct HTTP Cooperalianca Agência Virtual SDK for AI agents"
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
    Tools,
    Unmapped,
    /// Read strict JSON input from stdin and run an agent tool.
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
fn client(a: &Args) -> Result<Client> {
    if a.offline {
        return Client::offline(&a.base_url);
    }
    if let Some(dir) = &a.store_dir {
        Client::from_store(&a.base_url, Store::open(dir)?)
    } else {
        Client::new(&a.base_url)
    }
}
fn run(a: &Args) -> Result<Value> {
    match &a.command {
        Command::Tools => Ok(json!(tools::tools())),
        Command::ForgetSession => {
            let store = Store::open(
                a.store_dir
                    .as_ref()
                    .ok_or(Error::Validation("--store-dir is required"))?,
            )?;
            store.clear_session()?;
            Ok(json!({"forgotten":true}))
        }
        Command::Unmapped => Ok(json!(tools::unmapped_tools())),
        Command::SetupAuth {
            username,
            from_file,
        } => {
            let base = Client::normalize_base(&a.base_url)?;
            let store = Store::open(
                a.store_dir
                    .as_ref()
                    .ok_or(Error::Validation("--store-dir is required"))?,
            )?;
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
            if name != "check_new_bills" && !tools::tools().iter().any(|t| t.name == name) {
                return Err(Error::NotAvailable);
            }
            // Write previews and nothing else run without a client, credentials, or network.
            if !tools::needs_client(name, &value) {
                return tools::execute_offline(name, value);
            }
            let mut c = client(a)?;
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
            let mut c = client(a)?;
            let Output::Json(content) = c.call("open_invoices", &json!({"codigoUc": uc}))? else {
                return Err(Error::Parse);
            };
            let current = rows(&content);
            let new_bills = check_new_bills(&current, &previous);
            if *update_state {
                cooperalianca::store::write_private(state, &marshal_jsonl(&current)?)?;
            }
            Ok(json!({"new_bills":new_bills,"count":new_bills.len()}))
        }
    }
}
fn main() {
    let a = Args::parse();
    let value = match run(&a) {
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
