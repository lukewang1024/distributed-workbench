//! Thin CLI over an Executor's pinned Pi extension. All calls use the local Controller.
use anyhow::{Result, bail};
use clap::Subcommand;
use serde_json::{Value, json};
use std::path::Path;
use workbench_protocol::Request;
use workbench_runtime::call_unix;

#[derive(Debug, Subcommand)]
pub enum ComputerUseCommand {
    /// List original plugin tool descriptions and JSON schemas.
    Tools {
        #[arg(long)]
        executor: String,
    },
    /// Acquire exclusive desktop control for a bounded session.
    Open {
        #[arg(long)]
        executor: String,
        #[arg(long)]
        owner: String,
        #[arg(long, default_value_t = 900000)]
        ttl_ms: u64,
    },
    Renew {
        #[arg(long)]
        executor: String,
        #[arg(long)]
        owner: String,
        #[arg(long)]
        token: String,
        #[arg(long, default_value_t = 900000)]
        ttl_ms: u64,
    },
    /// Invoke an original plugin tool; pass '-' to read JSON arguments from stdin.
    Call {
        #[arg(long)]
        executor: String,
        #[arg(long)]
        owner: String,
        #[arg(long)]
        token: String,
        tool: String,
        #[arg(default_value = "{}")]
        arguments: String,
    },
    /// Dispose plugin state/native children, then release desktop control.
    Close {
        #[arg(long)]
        executor: String,
        #[arg(long)]
        owner: String,
        #[arg(long)]
        token: String,
    },
}
fn rpc(socket: &Path, action: &str, params: Value) -> Result<Value> {
    let response = call_unix(socket, &Request::new(action, params))?;
    if !response.ok {
        let error = response
            .error
            .ok_or_else(|| anyhow::anyhow!("missing RPC error"))?;
        bail!("{}: {}", error.code, error.message);
    }
    Ok(response.result.unwrap_or(Value::Null))
}
fn invoke(
    socket: &Path,
    executor: &str,
    owner: &str,
    token: &str,
    tool: &str,
    arguments: Value,
) -> Result<Value> {
    if !arguments.is_object() {
        bail!("tool arguments must be a JSON object");
    }
    rpc(
        socket,
        "executor.call",
        json!({"executorId":executor,"action":"computer-use.call",
        "params":{"sessionId":owner,"tool":tool,"arguments":arguments},
        "leaseResource":format!("computer-use:{executor}"),"owner":owner,"token":token}),
    )
}
pub fn run(socket: &Path, command: ComputerUseCommand) -> Result<()> {
    let result = match command {
        ComputerUseCommand::Tools { executor } => rpc(
            socket,
            "executor.call",
            json!({"executorId":executor,"action":"computer-use.tools","params":{}}),
        ),
        ComputerUseCommand::Open {
            executor,
            owner,
            ttl_ms,
        } => rpc(
            socket,
            "lease.acquire",
            json!({"resource":format!("computer-use:{executor}"),"owner":owner,"ttlMs":ttl_ms}),
        ),
        ComputerUseCommand::Renew {
            executor,
            owner,
            token,
            ttl_ms,
        } => rpc(
            socket,
            "lease.renew",
            json!({"resource":format!("computer-use:{executor}"),"owner":owner,"token":token,"ttlMs":ttl_ms}),
        ),
        ComputerUseCommand::Call {
            executor,
            owner,
            token,
            tool,
            arguments,
        } => {
            let args = if arguments == "-" {
                serde_json::from_reader(std::io::stdin())?
            } else {
                serde_json::from_str(&arguments)?
            };
            invoke(socket, &executor, &owner, &token, &tool, args)
        }
        ComputerUseCommand::Close {
            executor,
            owner,
            token,
        } => {
            let closed = invoke(socket, &executor, &owner, &token, "close", json!({}));
            let released = rpc(
                socket,
                "lease.release",
                json!({"resource":format!("computer-use:{executor}"),"owner":owner,"token":token}),
            );
            closed.and(released)
        }
    }?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
