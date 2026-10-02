//! The factory component. Reads target tool and/or toolset names from config.

wit_bindgen::generate!({
    path: "wit",
    world: "toolset-factory",
    generate_all,
});

struct Factory;

impl exports::composable::factory::factory::Guest for Factory {
    async fn build() -> Result<Vec<u8>, String> {
        let builder = crate::Builder::new(names("tools")?, names("toolsets")?)
            .map_err(|e| format!("{e:#}"))?;
        composable_factory::build(&builder).map_err(|e| format!("{e:#}"))
    }
}

/// The import names for `key` as a comma-separated list, or none if absent.
fn names(key: &str) -> Result<Vec<String>, String> {
    let value =
        wasi::config::store::get(key).map_err(|e| format!("reading config '{key}': {e:?}"))?;
    Ok(value
        .map(|names| {
            names
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default())
}

export!(Factory);
