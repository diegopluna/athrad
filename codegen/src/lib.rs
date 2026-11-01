//! Lower Athrad IR through Cranelift into an object for the host.

use anyhow::{Result, bail};

pub fn compile(module: &ir::Module, name: &str) -> Result<object::write::Object<'static>> {
    let _ = (module, name);
    bail!("codegen is not written yet")
}
