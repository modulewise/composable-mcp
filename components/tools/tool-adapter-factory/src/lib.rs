wit_bindgen::generate!({
    path: "wit",
    world: "tool-adapter-factory",
    generate_all,
});

struct Factory;

impl exports::composable::factory::factory::Guest for Factory {
    async fn build() -> Result<Vec<u8>, String> {
        let source = wasi::config::store::get("target")
            .map_err(|e| format!("reading config 'target': {e:?}"))?
            .ok_or_else(|| "no `config.target` set".to_string())?;
        let target = composable::factory::loader::load(source).await?;

        let function = wasi::config::store::get("function")
            .map_err(|e| format!("reading config 'function': {e:?}"))?;
        let description = wasi::config::store::get("description")
            .map_err(|e| format!("reading config 'description': {e:?}"))?;

        tool_adapter::build(target, function, description).map_err(|e| format!("{e:#}"))
    }
}

export!(Factory);
