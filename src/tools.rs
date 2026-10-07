//! The `composable:tools` interfaces as seen from a host: which components
//! export them, the functions to invoke, the values passed, and a toolset
//! that aggregates those components on the host side.

use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use composable_runtime::{Component, ComponentError, ComponentHost, Selector, Val};
use serde_json::Value;

/// The interface a component exports to be used as a tool. Matching
/// includes the version separator, since `tool` is a prefix of `toolset`.
pub const TOOL_EXPORT: &str = "composable:tools/tool@";

/// The interface a component exports to be used as a toolset.
pub const TOOLSET_EXPORT: &str = "composable:tools/toolset@";

/// The function key for `metadata` on `composable:tools/tool`.
pub const TOOL_METADATA: &str = "tool.metadata";

/// The function key for `call` on `composable:tools/tool`.
pub const TOOL_CALL: &str = "tool.call";

/// The function key for `list` on `composable:tools/toolset`.
pub const TOOLSET_LIST: &str = "toolset.list";

/// The function key for `call` on `composable:tools/toolset`.
pub const TOOLSET_CALL: &str = "toolset.call";

/// Joins a toolset's component name to the name of one of its tools.
const SEPARATOR: char = '_';

/// The MCP error code for a call to an unknown tool.
const UNKNOWN_TOOL: i32 = -32602;

/// Whether a component exports `composable:tools/tool`.
pub fn exports_tool(component: &Component) -> bool {
    exports(component, TOOL_EXPORT)
}

/// Whether a component exports `composable:tools/toolset`.
pub fn exports_toolset(component: &Component) -> bool {
    exports(component, TOOLSET_EXPORT)
}

/// A `list<meta-entry>` value, as an array of `[key, value]` pairs.
pub fn meta_entries(entries: impl IntoIterator<Item = (String, String)>) -> Value {
    Value::Array(
        entries
            .into_iter()
            .map(|(key, value)| Value::Array(vec![Value::String(key), Value::String(value)]))
            .collect(),
    )
}

/// A toolset that implements `list` and `call` operations for components that
/// export `composable:tools/tool` or `composable:tools/toolset`, excluding any
/// that are already configured as imports of a toolset component.
///
/// A tool is named by its component name. A toolset's tools are named by its
/// component name joined with each tool's own name with a `_` separator
/// between them so that no two names collide. A component exporting both
/// interfaces contributes both.
///
/// Values are `composable:tools` JSON. A WIT `err` value is returned as a
/// [`ComponentError`], whether it comes from a candidate or is created for an
/// unknown tool. Any other error is a host failure.
pub struct HostToolset {
    host: Arc<dyn ComponentHost>,
    tools: Vec<String>,
    toolsets: Vec<String>,
}

impl HostToolset {
    /// The candidates in `host`, limited to those `selector` matches if given.
    pub fn new(host: Arc<dyn ComponentHost>, selector: Option<&Selector>) -> Self {
        let mut tools = Vec::new();
        let mut toolsets = Vec::new();
        for component in host.list_components(selector) {
            if is_member(host.as_ref(), component) {
                continue;
            }
            if exports_tool(component) {
                tools.push(component.metadata.name.clone());
            }
            if exports_toolset(component) {
                toolsets.push(component.metadata.name.clone());
            }
        }
        tools.sort();
        toolsets.sort();
        Self {
            host,
            tools,
            toolsets,
        }
    }

    /// The `tool-metadata` of every tool.
    pub async fn list(&self) -> Result<Vec<Value>> {
        let mut listed = Vec::new();
        for component in &self.tools {
            let mut metadata = self.invoke(component, TOOL_METADATA, vec![]).await?;
            set_name(&mut metadata, component.clone())?;
            listed.push(metadata);
        }
        for component in &self.toolsets {
            let Value::Array(tools) = self.invoke(component, TOOLSET_LIST, vec![]).await? else {
                bail!("{component}: {TOOLSET_LIST} must return a list");
            };
            for mut metadata in tools {
                let name = metadata
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        anyhow!("{component}: tool-metadata requires a string \"name\"")
                    })?;
                let name = format!("{component}{SEPARATOR}{name}");
                set_name(&mut metadata, name)?;
                listed.push(metadata);
            }
        }
        Ok(listed)
    }

    /// Call the tool named by the `call-tool-request`, returning its
    /// `call-tool-result`.
    pub async fn call(&self, request: Value) -> Result<Value> {
        let Value::Object(mut request) = request else {
            bail!("call-tool-request must be an object");
        };
        let name = request
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("call-tool-request requires a string \"name\""))?
            .to_string();
        match name.split_once(SEPARATOR) {
            Some((component, tool)) if self.toolsets.iter().any(|t| t == component) => {
                request.insert("name".to_string(), Value::String(tool.to_string()));
                self.invoke(
                    component,
                    TOOLSET_CALL,
                    vec![Val::Json(Value::Object(request))],
                )
                .await
            }
            None if self.tools.contains(&name) => {
                let arguments = request
                    .remove("arguments")
                    .ok_or_else(|| anyhow!("call-tool-request requires \"arguments\""))?;
                let meta = request
                    .remove("meta")
                    .ok_or_else(|| anyhow!("call-tool-request requires \"meta\""))?;
                self.invoke(
                    &name,
                    TOOL_CALL,
                    vec![Val::Json(arguments), Val::Json(meta)],
                )
                .await
            }
            _ => Err(ComponentError {
                value: Some(Val::Json(serde_json::json!({
                    "code": UNKNOWN_TOOL,
                    "message": format!("Unknown tool: {name}"),
                    "data": null,
                }))),
            }
            .into()),
        }
    }

    async fn invoke(&self, component: &str, function: &str, args: Vec<Val>) -> Result<Value> {
        self.host
            .invoke(component, function, args, None)
            .await?
            .ok_or_else(|| anyhow!("{component}: {function} returned nothing"))?
            .into_json()
    }
}

fn exports(component: &Component, prefix: &str) -> bool {
    component.metadata.exports.iter().any(|export| {
        export
            .interface_name()
            .is_some_and(|interface| interface.as_str().starts_with(prefix))
    })
}

// Whether a toolset imports the component.
fn is_member(host: &dyn ComponentHost, component: &Component) -> bool {
    component
        .metadata
        .dependents
        .iter()
        .flatten()
        .any(|dependent| host.get_component(dependent).is_some_and(exports_toolset))
}

fn set_name(metadata: &mut Value, name: String) -> Result<()> {
    let object = metadata
        .as_object_mut()
        .ok_or_else(|| anyhow!("tool-metadata must be an object"))?;
    object.insert("name".to_string(), Value::String(name));
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;

    use composable_runtime::{
        ComponentInvoker, ComponentMetadata, Export, Interface, InterfaceName,
    };
    use serde_json::json;

    use super::*;

    // A testing host for components with no bytes. Each invocation returns the
    // value or `err` value set for its component and function, and is recorded
    // with its arguments.
    #[derive(Default)]
    struct TestHost {
        components: HashMap<String, Component>,
        returns: HashMap<(String, String), Result<Value, Value>>,
        invocations: Mutex<Vec<(String, String, Vec<Value>)>>,
    }

    impl TestHost {
        fn component(mut self, name: &str, exports: &[&str], dependents: &[&str]) -> Self {
            self.components
                .insert(name.to_string(), component(name, exports, dependents, &[]));
            self
        }

        fn labeled(mut self, name: &str, exports: &[&str], label: (&str, &str)) -> Self {
            self.components
                .insert(name.to_string(), component(name, exports, &[], &[label]));
            self
        }

        fn returns(mut self, component: &str, function: &str, value: Value) -> Self {
            self.returns
                .insert((component.to_string(), function.to_string()), Ok(value));
            self
        }

        fn fails(mut self, component: &str, function: &str, error: Value) -> Self {
            self.returns
                .insert((component.to_string(), function.to_string()), Err(error));
            self
        }
    }

    impl ComponentInvoker for TestHost {
        fn invoke<'a>(
            &'a self,
            component_name: &'a str,
            function_name: &'a str,
            args: Vec<Val>,
            _env: Option<HashMap<String, String>>,
        ) -> Pin<Box<dyn Future<Output = Result<Option<Val>>> + Send + 'a>> {
            Box::pin(async move {
                let args = args
                    .into_iter()
                    .map(Val::into_json)
                    .collect::<Result<Vec<_>>>()?;
                self.invocations.lock().unwrap().push((
                    component_name.to_string(),
                    function_name.to_string(),
                    args,
                ));
                let key = (component_name.to_string(), function_name.to_string());
                match self.returns.get(&key) {
                    Some(Ok(value)) => Ok(Some(Val::Json(value.clone()))),
                    Some(Err(error)) => Err(ComponentError {
                        value: Some(Val::Json(error.clone())),
                    }
                    .into()),
                    None => bail!("no return set for {component_name}: {function_name}"),
                }
            })
        }
    }

    impl ComponentHost for TestHost {
        fn get_component(&self, name: &str) -> Option<&Component> {
            self.components.get(name)
        }

        fn list_components(&self, selector: Option<&Selector>) -> Vec<&Component> {
            self.components
                .values()
                .filter(|c| selector.is_none_or(|s| s.matches(&c.metadata.to_selectable())))
                .collect()
        }
    }

    const TOOL: &str = "composable:tools/tool@0.2.0";
    const TOOLSET: &str = "composable:tools/toolset@0.2.0";

    fn component(
        name: &str,
        exports: &[&str],
        dependents: &[&str],
        labels: &[(&str, &str)],
    ) -> Component {
        Component {
            metadata: ComponentMetadata {
                name: name.to_string(),
                namespace: None,
                package: None,
                labels: labels
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                dependents: Some(dependents.iter().map(|d| d.to_string()).collect()),
                exports: exports
                    .iter()
                    .map(|interface| Export {
                        name: interface.to_string(),
                        interface: Some(Interface {
                            name: Some(InterfaceName::parse(interface).unwrap()),
                        }),
                    })
                    .collect(),
            },
            functions: HashMap::new(),
        }
    }

    fn toolset(host: TestHost) -> HostToolset {
        HostToolset::new(Arc::new(host), None)
    }

    fn metadata(name: &str) -> Value {
        json!({
            "name": name,
            "title": null,
            "description": null,
            "input-schema": "{\"type\":\"object\"}",
            "output-schema": null,
            "annotations": null,
            "meta": []
        })
    }

    fn request(name: &str) -> Value {
        json!({ "name": name, "arguments": "{\"a\":1}", "meta": [["k", "v"]] })
    }

    fn result(text: &str) -> Value {
        json!({
            "content": [{ "type": "text", "value": { "text": text, "annotations": null, "meta": [] } }],
            "is-error": false,
            "structured-content": null,
            "meta": []
        })
    }

    fn component_error(error: anyhow::Error) -> Value {
        match error.downcast::<ComponentError>().unwrap().value {
            Some(Val::Json(value)) => value,
            _ => panic!("expected a JSON err value"),
        }
    }

    #[test]
    fn meta_entries_are_pairs() {
        let value = meta_entries([("traceparent".to_string(), "00-abc".to_string())]);
        assert_eq!(value, json!([["traceparent", "00-abc"]]));
    }

    #[test]
    fn no_meta_entries_is_an_empty_list() {
        assert_eq!(meta_entries([]), json!([]));
    }

    #[test]
    fn exports_matching_includes_the_version_separator() {
        // `composable:tools/toolset` also starts with `composable:tools/tool`.
        assert!(TOOL.starts_with(TOOL_EXPORT));
        assert!(!TOOLSET.starts_with(TOOL_EXPORT));
        assert!(TOOLSET.starts_with(TOOLSET_EXPORT));
    }

    #[test]
    fn the_candidates_are_the_exporters_no_toolset_imports() {
        let toolset = toolset(
            TestHost::default()
                .component("greeter", &[TOOL], &["bundle"])
                .component("remote", &[TOOLSET], &["bundle"])
                .component("bundle", &[TOOLSET], &[])
                .component("adder", &[TOOL], &[])
                .component("plain", &["example:plain/api@1.0.0"], &[]),
        );
        assert_eq!(toolset.tools, ["adder"]);
        assert_eq!(toolset.toolsets, ["bundle"]);
    }

    #[test]
    fn an_exporter_imported_by_a_non_toolset_is_still_a_candidate() {
        let toolset = toolset(
            TestHost::default()
                .component("greeter", &[TOOL], &["agent"])
                .component("agent", &["example:agent/run@1.0.0"], &[]),
        );
        assert_eq!(toolset.tools, ["greeter"]);
    }

    #[test]
    fn a_component_exporting_both_interfaces_contributes_both() {
        let toolset = toolset(TestHost::default().component("both", &[TOOL, TOOLSET], &[]));
        assert_eq!(toolset.tools, ["both"]);
        assert_eq!(toolset.toolsets, ["both"]);
    }

    #[test]
    fn a_selector_filters_the_candidates() {
        let host = TestHost::default()
            .labeled("adder", &[TOOL], ("domain", "math"))
            .labeled("greeter", &[TOOL], ("domain", "text"));
        let selector = Selector::parse("labels.domain=math").unwrap();
        let toolset = HostToolset::new(Arc::new(host), Some(&selector));
        assert_eq!(toolset.tools, ["adder"]);
    }

    #[tokio::test]
    async fn list_names_a_tool_by_its_component_and_prefixes_a_toolsets_tools() {
        let toolset = toolset(
            TestHost::default()
                .component("greeter", &[TOOL], &[])
                .component("calculator", &[TOOLSET], &[])
                .returns("greeter", TOOL_METADATA, metadata("greeter.greet"))
                .returns(
                    "calculator",
                    TOOLSET_LIST,
                    json!([metadata("add"), metadata("multiply")]),
                ),
        );
        let names: Vec<_> = toolset
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|metadata| metadata["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["greeter", "calculator_add", "calculator_multiply"]);
    }

    #[tokio::test]
    async fn an_err_from_list_passes_through() {
        let toolset = toolset(
            TestHost::default()
                .component("calculator", &[TOOLSET], &[])
                .fails("calculator", TOOLSET_LIST, json!("unreachable")),
        );
        let error = toolset.list().await.unwrap_err();
        assert_eq!(component_error(error), json!("unreachable"));
    }

    #[tokio::test]
    async fn a_tool_is_called_with_the_request_arguments_and_meta() {
        let host = Arc::new(
            TestHost::default()
                .component("greeter", &[TOOL], &[])
                .returns("greeter", TOOL_CALL, result("hi")),
        );
        let toolset = HostToolset::new(host.clone(), None);
        assert_eq!(
            toolset.call(request("greeter")).await.unwrap(),
            result("hi")
        );
        assert_eq!(
            host.invocations.lock().unwrap()[0],
            (
                "greeter".to_string(),
                TOOL_CALL.to_string(),
                vec![json!("{\"a\":1}"), json!([["k", "v"]])]
            )
        );
    }

    #[tokio::test]
    async fn a_toolsets_tool_is_called_without_the_prefix() {
        let host = Arc::new(
            TestHost::default()
                .component("calculator", &[TOOLSET], &[])
                .returns("calculator", TOOLSET_CALL, result("42")),
        );
        let toolset = HostToolset::new(host.clone(), None);
        assert_eq!(
            toolset.call(request("calculator_multiply")).await.unwrap(),
            result("42")
        );
        assert_eq!(
            host.invocations.lock().unwrap()[0],
            (
                "calculator".to_string(),
                TOOLSET_CALL.to_string(),
                vec![request("multiply")]
            )
        );
    }

    #[tokio::test]
    async fn a_tool_error_passes_through_unchanged() {
        let error = json!({ "code": -32001, "message": "down", "data": null });
        let toolset = toolset(
            TestHost::default()
                .component("calculator", &[TOOLSET], &[])
                .fails("calculator", TOOLSET_CALL, error.clone()),
        );
        let returned = toolset.call(request("calculator_add")).await.unwrap_err();
        assert_eq!(component_error(returned), error);
    }

    #[tokio::test]
    async fn an_unknown_name_is_a_tool_error() {
        let toolset = toolset(
            TestHost::default()
                .component("greeter", &[TOOL], &[])
                .component("calculator", &[TOOLSET], &[]),
        );
        for name in ["missing", "missing_add", "greeter_greet", "calculator"] {
            let error = toolset.call(request(name)).await.unwrap_err();
            assert_eq!(
                component_error(error),
                json!({ "code": -32602, "message": format!("Unknown tool: {name}"), "data": null }),
            );
        }
    }
}
