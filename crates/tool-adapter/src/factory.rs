//! A factory that creates a tool-adapter component.

use anyhow::{Context, Result};

use composable_factory::wit::{PackageSource, WorldSource};
use composable_factory::world::{ExportedFunction, ImportedFunction, Imports, Param, ValueSpec};
use composable_factory::{ComponentBuilder, World};

use crate::deserializer::JsonDeserializer;
use crate::serializer::JsonSerializer;

const MAPPER_WIT: &str = include_str!("../wit/mapper.wit");
const TOOL_WIT: &str = include_str!("../wit/tool.wit");

/// The tool-adapter factory.
pub struct Factory {
    /// The component to adapt.
    target: Vec<u8>,
    /// The function on the target to expose. If only one, that is the default.
    function: Option<String>,
    /// Optional tool description, surfaced in `metadata`.
    description: Option<String>,
}

impl Factory {
    pub fn new(target: Vec<u8>, function: Option<String>, description: Option<String>) -> Self {
        Factory {
            target,
            function,
            description,
        }
    }
}

impl ComponentBuilder for Factory {
    fn build_world(&self, world: &mut World) -> Result<()> {
        let target = WorldSource::from_component(&self.target)?;
        world.add_imports(target.exports())?;

        let mapper = PackageSource::from_text(MAPPER_WIT)?;
        world.add_imports(mapper.interface("serializer")?)?;
        world.add_imports(mapper.interface("deserializer")?)?;

        let tool = PackageSource::from_text(TOOL_WIT)?;
        world.add_exports(tool.interface("tool")?)
    }

    fn build_function(&self, function: &ExportedFunction, imports: &Imports) -> Result<()> {
        let name = function.name().to_string();
        match name.as_str() {
            "metadata" => self.build_metadata(function, imports),
            "call" => self.build_call(function, imports),
            other => anyhow::bail!("unexpected tool interface function '{other}'"),
        }
    }
}

impl Factory {
    /// The target function.
    fn target(&self, imports: &Imports) -> Result<(String, ImportedFunction)> {
        let candidates = self.candidates(imports)?;
        if candidates.is_empty() {
            anyhow::bail!("target exports no functions to adapt");
        }
        // A single-function target needs no selector.
        if candidates.len() == 1 {
            return Ok(candidates.into_iter().next().expect("just checked len"));
        }
        let selected = self.function.as_deref().with_context(|| {
            format!(
                "target exports {} functions; a `function` selector must choose one ({})",
                candidates.len(),
                candidates
                    .iter()
                    .map(|(q, _)| q.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        candidates
            .into_iter()
            .find(|(qualified, f)| qualified == selected || f.name() == selected)
            .with_context(|| format!("no target function matches \"{selected}\""))
    }

    fn candidates(&self, imports: &Imports) -> Result<Vec<(String, ImportedFunction)>> {
        let mut candidates = Vec::new();
        for interface in imports.interfaces() {
            let Some(name) = interface.name() else {
                continue;
            };
            if name == "serializer" || name == "deserializer" {
                continue;
            }
            for func in interface.functions()? {
                candidates.push((format!("{name}.{}", func.name()), func));
            }
        }
        // World-level exports of the target.
        for func in imports.functions()? {
            candidates.push((func.name().to_string(), func));
        }
        Ok(candidates)
    }

    /// Derive schemas from the target function.
    fn build_metadata(&self, function: &ExportedFunction, imports: &Imports) -> Result<()> {
        let (tool_name, target) = self.target(imports)?;
        let input_schema = composable_factory::schema::input_schema(&target.params()).to_string();
        let output_schema =
            composable_factory::schema::output_schema(target.result_type()).to_string();

        let spec = ValueSpec::ok(ValueSpec::record([
            ("name", ValueSpec::string(&tool_name)),
            ("title", ValueSpec::none()), // no title surfaced yet
            (
                "description",
                ValueSpec::optional_string(self.description.as_deref()),
            ),
            ("input-schema", ValueSpec::string(input_schema)),
            (
                "output-schema",
                ValueSpec::optional_string(Some(output_schema)),
            ),
            ("annotations", ValueSpec::none()), // no annotations surfaced yet
        ]));

        function
            .result()
            .context("the metadata export must have a result type")?
            .value()
            .write(&spec)
    }

    /// Deserialize the JSON input into the target function's params via the
    /// deserializer visitor, serialize its result to JSON via the serializer
    /// visitor, and write the JSON value as the function result.
    fn build_call(&self, function: &ExportedFunction, imports: &Imports) -> Result<()> {
        use composable_factory::world::WriteVisitor;

        let (_, target) = self.target(imports)?;
        target
            .result_type()
            .context("target function has no result")?; // no result should be allowed

        let input = function.param("input")?.receive()?;

        // Deserialize one arg per target param from the JSON.
        let mut deserializer = JsonDeserializer::new(imports.interface("deserializer")?, input)?;
        let mut args = Vec::new();
        for param in target.params() {
            deserializer.begin_field(param.name())?;
            let arg = param.value()?;
            arg.write_with(&mut deserializer)?;
            deserializer.end_field()?;
            args.push(arg);
        }

        // Call the target with the deserialized args.
        let result = target
            .call(&args)?
            .context("target function must return a result")?;

        // Release the deserializer.
        deserializer.close()?;

        // Serialize the target function's result to JSON.
        let mut serializer = JsonSerializer::new(imports.interface("serializer")?)?;
        result.read_with(&mut serializer)?;
        let json = serializer.into_json()?;

        // Write the JSON to the `ok` case of the tool's result.
        function
            .result()
            .context("the tool export must have a result type")?
            .value()
            .write(&ValueSpec::ok(json))
    }
}
