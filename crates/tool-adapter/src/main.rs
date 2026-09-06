//! Tool Adapter CLI
//!
//! Usage:
//!   tool-adapter <target.wasm> <out.wasm> [function] [description]
//! e.g.
//!   tool-adapter calc.wasm calc-tool.wasm calc.add "Adds two ints"

use anyhow::{Context, Result, bail};

fn main() -> Result<()> {
    let mut argv = std::env::args().skip(1);
    let usage = "usage: tool-adapter <target.wasm> <out.wasm> [function] [description]";
    let target_path = argv.next().context(usage)?;
    let out_path = argv.next().context(usage)?;
    let function = argv.next().filter(|s| !s.is_empty());
    let description = argv.next().filter(|s| !s.is_empty());

    let target = std::fs::read(&target_path).with_context(|| format!("reading {target_path}"))?;

    match tool_adapter::build(target, function, description) {
        Ok(bytes) => {
            std::fs::write(&out_path, &bytes).with_context(|| format!("writing {out_path}"))?;
            eprintln!("wrote {out_path} ({} bytes)", bytes.len());
            Ok(())
        }
        Err(e) => bail!("build failed: {e:#}"),
    }
}
