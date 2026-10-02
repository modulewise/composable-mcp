//! A factory for toolsets. The generated component has imports that match the
//! tool and toolset names provided, and it exports `composable:tools/toolset`.

#[cfg(target_arch = "wasm32")]
mod component;

use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};

use composable_factory::wit::PackageSource;
use composable_factory::world::{
    ExportedFunction, Imports, Kind, Value, ValueSpec, arm, exact, prefix,
};
use composable_factory::{ComponentBuilder, World};

/// The `composable:tools` package, whose `tool` and `toolset` interfaces the
/// generated toolset imports and exports.
const TOOLS_WIT: &str = include_str!("../../wit/package.wit");

/// Joins a nested toolset's import name to the names of its tools. It cannot
/// occur in an import name, so a prefix ends at its first occurrence.
const SEPARATOR: &str = "_";

/// The MCP error code for a call to an unknown tool.
const UNKNOWN_TOOL: i32 = -32602;

/// Builds an aggregating toolset component that delegates to the tools and
/// toolsets whose import names were provided.
pub struct Builder {
    tools: Vec<String>,
    toolsets: Vec<String>,
}

impl Builder {
    /// Builder for the toolset component.
    pub fn new(tools: Vec<String>, toolsets: Vec<String>) -> Result<Self> {
        if tools.is_empty() && toolsets.is_empty() {
            bail!("a toolset needs at least one tool or toolset");
        }
        let mut names = BTreeSet::new();
        for name in tools.iter().chain(&toolsets) {
            if !names.insert(name) {
                bail!("'{name}' is named more than once");
            }
        }
        Ok(Builder { tools, toolsets })
    }

    /// The members, tools first, each in config order.
    fn members(&self) -> Vec<Member<'_>> {
        let tools = self.tools.iter().map(|name| Member::Tool(name));
        let toolsets = self.toolsets.iter().map(|name| Member::Toolset(name));
        tools.chain(toolsets).collect()
    }
}

impl ComponentBuilder for Builder {
    fn build_world(&self, world: &mut World) -> Result<()> {
        let tools = PackageSource::from_text(TOOLS_WIT)?;
        for name in &self.tools {
            world.add_imports(tools.interface("tool")?.named(name)?)?;
        }
        for name in &self.toolsets {
            world.add_imports(tools.interface("toolset")?.named(name)?)?;
        }
        world.add_exports(tools.interface("toolset")?)
    }

    fn build_function(&self, function: &ExportedFunction, imports: &Imports) -> Result<()> {
        match function.name() {
            "list" => self.build_list(function, imports),
            "call" => self.build_call(function, imports),
            other => bail!("unexpected toolset function '{other}'"),
        }
    }
}

impl Builder {
    /// The `list()` result includes every tool, listed by its import name, and
    /// every tool of each nested toolset, prefixed by the toolset's import
    /// name and the separator. Those are the names `call()` routes on. If any
    /// tool's `metadata()` or any toolset's `list()` fails, the whole list
    /// fails with that member's name in the error.
    fn build_list(&self, function: &ExportedFunction, imports: &Imports) -> Result<()> {
        let result = function.result().context("list returns a result")?.value();
        list_members(&self.members(), Vec::new(), imports, &result)
    }

    /// Each `call()` request goes to the tool with its name, or to the nested
    /// toolset whose prefix it starts with, where the prefix will be removed
    /// before routing to a named tool within that toolset.
    fn build_call(&self, function: &ExportedFunction, imports: &Imports) -> Result<()> {
        let request = function.param("request")?.receive()?;
        let result = &function.result().context("call returns a result")?.value();
        let name = request.field("name")?;
        let arguments = &request.field("arguments")?;
        let meta = &request.field("meta")?;

        let mut arms = Vec::new();
        for tool in &self.tools {
            arms.push(exact(tool.as_str(), move || {
                let called = imports
                    .interface(tool)?
                    .function("call")?
                    .call(&[arguments.clone(), meta.clone()])?
                    .context("call returns a result")?;
                result.write(&ValueSpec::from(called))
            }));
        }
        for toolset in &self.toolsets {
            arms.push(prefix(format!("{toolset}{SEPARATOR}"), move |rest| {
                let call = imports.interface(toolset)?.function("call")?;
                let forwarded = call.param("request")?.value()?;
                forwarded.write(&ValueSpec::record([
                    ("name", ValueSpec::from(rest)),
                    ("arguments", arguments.into()),
                    ("meta", meta.into()),
                ]))?;
                let called = call.call(&[forwarded])?.context("call returns a result")?;
                result.write(&ValueSpec::from(called))
            }));
        }
        name.match_string(arms, || {
            result.write(&ValueSpec::err(ValueSpec::record([
                ("code", ValueSpec::s32(UNKNOWN_TOOL)),
                (
                    "message",
                    ValueSpec::concat([ValueSpec::string("Unknown tool: "), (&name).into()]),
                ),
                ("data", ValueSpec::none()),
            ])))
        })
    }
}

/// One member of the toolset with its import name.
enum Member<'a> {
    Tool(&'a str),
    Toolset(&'a str),
}

impl Member<'_> {
    fn name(&self) -> &str {
        match self {
            Member::Tool(name) | Member::Toolset(name) => name,
        }
    }
}

/// Ask each member for its tools in turn, each inside the previous one's `ok`
/// arm, since a payload is only valid inside its arm. The last writes every
/// member's tools, joined. An `err` writes that member's error instead.
fn list_members(
    members: &[Member<'_>],
    mut parts: Vec<ValueSpec>,
    imports: &Imports,
    result: &Value,
) -> Result<()> {
    let Some((member, rest)) = members.split_first() else {
        return result.write(&ValueSpec::ok(ValueSpec::concat(parts)));
    };
    let listed = match member {
        Member::Tool(name) => imports.interface(name)?.function("metadata")?.call(&[])?,
        Member::Toolset(name) => imports.interface(name)?.function("list")?.call(&[])?,
    }
    .with_context(|| format!("'{}' returns a result", member.name()))?;

    listed.dispatch(vec![
        arm("ok", move |payload| {
            let payload = payload.context("ok carries the member's tools")?;
            parts.push(match member {
                Member::Tool(name) => {
                    ValueSpec::list([renamed(&payload, ValueSpec::string(*name))?])
                }
                Member::Toolset(name) => {
                    let prefixed = payload.map(payload.ty(), |tool| {
                        let name = ValueSpec::concat([
                            ValueSpec::string(format!("{name}{SEPARATOR}")),
                            tool.field("name")?.into(),
                        ]);
                        renamed(&tool, name)
                    })?;
                    ValueSpec::from(prefixed)
                }
            });
            list_members(rest, parts, imports, result)
        }),
        arm("err", |payload| {
            let error = payload.context("err carries a message")?;
            result.write(&ValueSpec::err(ValueSpec::concat([
                ValueSpec::string(format!("{}: ", member.name())),
                error.into(),
            ])))
        }),
    ])
}

/// A tool's `metadata` with its `name` replaced by `name` (the import name).
/// Every other field is copied as-is.
fn renamed(metadata: &Value, name: ValueSpec) -> Result<ValueSpec> {
    let Kind::Record(fields) = metadata.ty().kind() else {
        bail!("tool metadata is not a record");
    };
    let mut name = Some(name);
    let fields = fields
        .iter()
        .map(|field| {
            let value = match field.name() {
                "name" => name.take().context("one name field")?,
                other => metadata.field(other)?.into(),
            };
            Ok((field.name().to_string(), value))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ValueSpec::record(fields))
}
