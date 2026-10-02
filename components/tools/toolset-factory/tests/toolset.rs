//! A generated toolset over two tools and a nested toolset, run in wasmtime
//! with host functions for its imports.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

use anyhow::{Context, Result};
use toolset_factory::Builder;
use wasmtime::component::{Component, Func, Instance, Linker, Val};
use wasmtime::{Config, Engine, Store};

const TOOLS: &str = "composable:tools/types@0.2.0";
const TOOLSET: &str = "composable:tools/toolset@0.2.0";

fn some(value: Val) -> Val {
    Val::Option(Some(Box::new(value)))
}

fn string(text: &str) -> Val {
    Val::String(text.to_string())
}

fn ok(value: Val) -> Val {
    Val::Result(Ok(Some(Box::new(value))))
}

/// A tool's metadata, with a description so copied fields can be checked.
fn metadata(name: &str, description: &str) -> Val {
    Val::Record(vec![
        ("name".to_string(), string(name)),
        ("title".to_string(), Val::Option(None)),
        ("description".to_string(), some(string(description))),
        ("input-schema".to_string(), string("{}")),
        ("output-schema".to_string(), Val::Option(None)),
        ("annotations".to_string(), Val::Option(None)),
        ("meta".to_string(), Val::List(Vec::new())),
    ])
}

/// A successful call's result, carrying one text block.
fn text_result(text: &str) -> Val {
    let block = Val::Variant(
        "text".to_string(),
        Some(Box::new(Val::Record(vec![
            ("text".to_string(), string(text)),
            ("annotations".to_string(), Val::Option(None)),
            ("meta".to_string(), Val::List(Vec::new())),
        ]))),
    );
    Val::Record(vec![
        ("content".to_string(), Val::List(vec![block])),
        ("is-error".to_string(), Val::Bool(false)),
        ("structured-content".to_string(), Val::Option(None)),
        ("meta".to_string(), Val::List(Vec::new())),
    ])
}

fn request(name: &str, arguments: &str) -> Val {
    Val::Record(vec![
        ("name".to_string(), string(name)),
        ("arguments".to_string(), string(arguments)),
        ("meta".to_string(), Val::List(Vec::new())),
    ])
}

/// The field `name` of a record value.
fn field<'v>(value: &'v Val, name: &str) -> &'v Val {
    let Val::Record(fields) = value else {
        panic!("not a record: {value:?}");
    };
    &fields.iter().find(|(key, _)| key == name).expect(name).1
}

/// The names the nested toolset was called with.
type Calls = Arc<Mutex<Vec<String>>>;

/// The host-provided functions the toolset imports. `alerts_fails` makes the
/// `alerts` tool's metadata return an error.
fn linker(engine: &Engine, calls: Calls, alerts_fails: bool) -> Result<Linker<()>> {
    let mut linker = Linker::<()>::new(engine);
    linker.instance(TOOLS)?;

    // `forecast` reports a name of its own, which the toolset replaces.
    let mut forecast = linker.instance("forecast")?;
    forecast.func_new_concurrent("metadata", |_, _, _, results| {
        Box::pin(async move {
            results[0] = ok(metadata("get-forecast", "The forecast"));
            Ok(())
        })
    })?;
    forecast.func_new_concurrent("call", |_, _, params, results| {
        Box::pin(async move {
            let Val::String(arguments) = &params[0] else {
                wasmtime::bail!("arguments are not a string");
            };
            results[0] = ok(text_result(&format!("rainy in {arguments}")));
            Ok(())
        })
    })?;

    let mut alerts = linker.instance("alerts")?;
    alerts.func_new_concurrent("metadata", move |_, _, _, results| {
        Box::pin(async move {
            results[0] = if alerts_fails {
                Val::Result(Err(Some(Box::new(string("down")))))
            } else {
                ok(metadata("alerts", "The alerts"))
            };
            Ok(())
        })
    })?;
    alerts.func_new_concurrent("call", |_, _, _, results| {
        Box::pin(async move {
            results[0] = ok(text_result("storm warning"));
            Ok(())
        })
    })?;

    let mut nested = linker.instance("nested")?;
    nested.func_new_concurrent("list", |_, _, _, results| {
        Box::pin(async move {
            results[0] = ok(Val::List(vec![
                metadata("x", "Tool x"),
                metadata("y", "Tool y"),
            ]));
            Ok(())
        })
    })?;
    nested.func_new_concurrent("call", move |_, _, params, results| {
        let calls = calls.clone();
        Box::pin(async move {
            let Val::String(name) = field(&params[0], "name") else {
                wasmtime::bail!("the request's name is not a string");
            };
            calls.lock().unwrap().push(name.clone());
            results[0] = ok(text_result(&format!("nested ran {name}")));
            Ok(())
        })
    })?;
    Ok(linker)
}

fn engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_component_model_implements(true);
    Ok(Engine::new(&config)?)
}

/// The generated toolset over `forecast`, `alerts` and `nested`.
fn toolset() -> Result<Vec<u8>> {
    let builder = Builder::new(
        vec!["forecast".to_string(), "alerts".to_string()],
        vec!["nested".to_string()],
    )?;
    composable_factory::build(&builder)
}

/// The toolset export's function `name`.
fn function(store: &mut Store<()>, instance: &Instance, name: &str) -> Result<Func> {
    let toolset = instance
        .get_export_index(&mut *store, None, TOOLSET)
        .context("the toolset export")?;
    instance
        .get_export_index(&mut *store, Some(&toolset), name)
        .and_then(|index| instance.get_func(&mut *store, index))
        .with_context(|| format!("the {name} function"))
}

#[test]
fn tools_are_listed_and_calls_are_routed() -> Result<()> {
    let engine = engine()?;
    let component = Component::new(&engine, toolset()?)?;
    let calls = Calls::default();
    let linker = linker(&engine, calls.clone(), false)?;

    let (listed, forecast, nested, unknown) = block_on(async {
        let mut store = Store::new(&engine, ());
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let list = function(&mut store, &instance, "list")?;
        let call = function(&mut store, &instance, "call")?;

        let mut listed = [Val::Bool(false)];
        list.call_async(&mut store, &[], &mut listed).await?;
        let mut forecast = [Val::Bool(false)];
        call.call_async(&mut store, &[request("forecast", "paris")], &mut forecast)
            .await?;
        let mut nested = [Val::Bool(false)];
        call.call_async(&mut store, &[request("nested_x", "{}")], &mut nested)
            .await?;
        let mut unknown = [Val::Bool(false)];
        call.call_async(&mut store, &[request("other", "{}")], &mut unknown)
            .await?;
        anyhow::Ok((listed, forecast, nested, unknown))
    })?;

    // A tool is listed by its import name, a nested toolset's tools with its
    // prefix, and every other field is copied.
    let expected = ok(Val::List(vec![
        metadata("forecast", "The forecast"),
        metadata("alerts", "The alerts"),
        metadata("nested_x", "Tool x"),
        metadata("nested_y", "Tool y"),
    ]));
    assert_eq!(listed[0], expected);

    assert_eq!(forecast[0], ok(text_result("rainy in paris")));

    // The nested toolset is called with the prefix stripped.
    assert_eq!(nested[0], ok(text_result("nested ran x")));
    assert_eq!(*calls.lock().unwrap(), ["x"]);

    let Val::Result(Err(Some(error))) = &unknown[0] else {
        panic!("an unknown tool is an error: {:?}", unknown[0]);
    };
    assert_eq!(field(error, "code"), &Val::S32(-32602));
    assert_eq!(field(error, "message"), &string("Unknown tool: other"));
    Ok(())
}

#[test]
fn a_failing_delegate_fails_the_whole_list() -> Result<()> {
    let engine = engine()?;
    let component = Component::new(&engine, toolset()?)?;
    let linker = linker(&engine, Calls::default(), true)?;

    let listed = block_on(async {
        let mut store = Store::new(&engine, ());
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let list = function(&mut store, &instance, "list")?;
        let mut listed = [Val::Bool(false)];
        list.call_async(&mut store, &[], &mut listed).await?;
        anyhow::Ok(listed)
    })?;

    assert_eq!(
        listed[0],
        Val::Result(Err(Some(Box::new(string("alerts: down")))))
    );
    Ok(())
}

#[test]
fn a_toolset_needs_at_least_one_import_and_unique_names() {
    assert!(Builder::new(Vec::new(), Vec::new()).is_err());
    let repeated = Builder::new(vec!["forecast".to_string()], vec!["forecast".to_string()]);
    assert!(repeated.is_err());
}

/// Run `future` to completion on this thread, parking it while the future is
/// not ready.
fn block_on<T>(future: impl Future<Output = T>) -> T {
    struct Unpark(std::thread::Thread);

    impl std::task::Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = Arc::new(Unpark(std::thread::current())).into();
    let mut cx = TaskContext::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        std::thread::park();
    }
}
