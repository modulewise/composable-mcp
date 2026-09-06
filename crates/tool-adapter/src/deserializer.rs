//! The tool-adapter's `WriteVisitor`. Deserializes a call arg from JSON.

use anyhow::Result;

use composable_factory::world::{
    ImportedFunction, Interface, MapKey, Type, Value, ValueSpec, WriteVisitor,
};

/// Produces call arg values from JSON via a `deserializer` resource handle.
pub struct JsonDeserializer {
    /// The `deserializer` interface.
    interface: Interface,
    /// The `deserializer` resource handle.
    handle: Value,
    /// The key type of each map currently being read, innermost last. It
    /// indicates what form to expect: an object for string keys, an array of
    /// pairs otherwise. It also determines what accessor retrieves the key.
    open_maps: Vec<MapKey>,
    /// Which entry of the innermost map is being read, held across the whole
    /// entry, because an object reaches both its key and its value by index.
    entry_index: Option<Value>,
    /// Set while an object's key is being read, since the cursor cannot move
    /// onto a key. It is retrieved via `key-at` rather than a leaf accessor.
    reading_key: bool,
}

impl JsonDeserializer {
    /// Acquire the resource handle by calling the constructor.
    pub fn new(interface: Interface, json: Value) -> Result<Self> {
        let handle = interface
            .function("[constructor]deserializer")?
            .call(&[json])?
            .ok_or_else(|| anyhow::anyhow!("deserializer constructor must return a handle"))?;
        Ok(JsonDeserializer {
            interface,
            handle,
            open_maps: Vec::new(),
            entry_index: None,
            reading_key: false,
        })
    }

    /// Drop the resource handle.
    pub fn close(self) -> Result<()> {
        self.handle.drop()
    }

    /// A callable method for `[method]deserializer.<name>`.
    fn method(&self, name: &str) -> Result<ImportedFunction> {
        self.interface
            .function(&format!("[method]deserializer.{name}"))
    }

    /// Pull one leaf via the named method.
    fn get(&mut self, name: &str) -> Result<ValueSpec> {
        Ok(self.pull(name)?.into())
    }

    /// Pull the value at the cursor via the named accessor.
    fn pull(&self, name: &str) -> Result<Value> {
        let h = self.handle.clone();
        self.method(name)?
            .call(&[h])?
            .ok_or_else(|| anyhow::anyhow!("deserializer.{name} must return a value"))
    }

    /// Call a method taking the handle and an index, returning its result.
    fn call_with_index(&self, name: &str, index: &Value) -> Result<Value> {
        let h = self.handle.clone();
        self.method(name)?
            .call(&[h, index.clone()])?
            .ok_or_else(|| anyhow::anyhow!("deserializer.{name} must return a value"))
    }

    /// Descend into the element at a runtime index.
    fn enter_element(&self, index: &Value) -> Result<()> {
        let h = self.handle.clone();
        self.method("enter-element")?.call(&[h, index.clone()])?;
        Ok(())
    }

    /// Descend into the element at an index known while emitting.
    fn enter_element_at(&self, index: u32) -> Result<()> {
        let enter = self.method("enter-element")?;
        let position = enter.param("index")?.value()?;
        position.write(&ValueSpec::u32(index))?;
        let h = self.handle.clone();
        enter.call(&[h, position])?;
        Ok(())
    }

    /// The key type of the map whose entry is being read.
    fn entry_key(&self) -> Result<MapKey> {
        self.open_maps
            .last()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("a map entry outside any map"))
    }

    /// Which entry of the innermost map is being read.
    fn entry_index(&self) -> Result<Value> {
        self.entry_index
            .clone()
            .ok_or_else(|| anyhow::anyhow!("a map entry's member outside any entry"))
    }

    /// An object key index, if that is what's currently being read.
    fn reading_object_key(&self) -> Option<Value> {
        self.reading_key.then(|| self.entry_index.clone()).flatten()
    }

    /// Exit out of the current scope.
    fn exit(&mut self) -> Result<()> {
        let h = self.handle.clone();
        self.method("exit")?.call(&[h])?;
        Ok(())
    }
}

impl WriteVisitor for JsonDeserializer {
    fn begin_field(&mut self, name: &str) -> Result<()> {
        let h = self.handle.clone();
        let enter_field = self.method("enter-field")?;
        let key = enter_field.param("name")?.value()?;
        key.write(&ValueSpec::string(name))?;
        enter_field.call(&[h, key])?;
        Ok(())
    }

    fn end_field(&mut self) -> Result<()> {
        self.exit()
    }

    fn begin_element(&mut self, index: &Value) -> Result<()> {
        self.enter_element(index)
    }

    fn end_element(&mut self) -> Result<()> {
        self.exit()
    }

    // A map with string keys is read from a JSON object. A map with any other
    // key type is read from an array of [key, value] entry arrays.
    fn begin_map(&mut self, key: MapKey, _value: &Type) -> Result<()> {
        self.open_maps.push(key);
        Ok(())
    }

    fn end_map(&mut self) -> Result<()> {
        self.open_maps.pop();
        Ok(())
    }

    // An object's entry is reached by index, and its key sits beside its value
    // rather than within it, so `key-at` retrieves the key rather than
    // descending into the entry. Every other map is an array of [key, value]
    // arrays, so its entry is descended into like any other element.
    fn begin_entry(&mut self, index: &Value) -> Result<()> {
        self.entry_index = Some(index.clone());
        if self.entry_key()? != MapKey::String {
            self.enter_element(index)?;
        }
        Ok(())
    }

    fn end_entry(&mut self) -> Result<()> {
        let string_keyed = self.entry_key()? == MapKey::String;
        self.entry_index = None;
        if string_keyed {
            return Ok(());
        }
        self.exit()
    }

    fn begin_key(&mut self) -> Result<()> {
        // An object's key is retrieved via `key-at`, by entry index, so the
        // cursor is not moved and `on_string` handles the index lookup.
        if self.entry_key()? == MapKey::String {
            self.reading_key = true;
            return Ok(());
        }
        self.enter_element_at(0)
    }

    fn end_key(&mut self) -> Result<()> {
        if self.entry_key()? == MapKey::String {
            self.reading_key = false;
            return Ok(());
        }
        self.exit()
    }

    fn begin_value(&mut self) -> Result<()> {
        match self.entry_key()? {
            // The object's `index`th value, since its key was read in place.
            MapKey::String => {
                let index = self.entry_index()?;
                self.enter_element(&index)
            }
            _ => self.enter_element_at(1),
        }
    }

    fn end_value(&mut self) -> Result<()> {
        self.exit()
    }

    fn begin_payload(&mut self) -> Result<()> {
        let h = self.handle.clone();
        self.method("enter-payload")?.call(&[h])?;
        Ok(())
    }

    fn end_payload(&mut self) -> Result<()> {
        self.exit()
    }

    fn length(&mut self) -> Result<Value> {
        let h = self.handle.clone();
        self.method("length")?
            .call(&[h])?
            .ok_or_else(|| anyhow::anyhow!("deserializer.length must return a count"))
    }

    fn case_index(&mut self, names: &[&str]) -> Result<Value> {
        let case_index = self.method("case-index")?;
        let names_value = case_index.param("names")?.value()?;
        names_value.write(&ValueSpec::list(
            names.iter().map(|n| ValueSpec::string(*n)),
        ))?;
        let h = self.handle.clone();
        let found = case_index
            .call(&[h, names_value])?
            .ok_or_else(|| anyhow::anyhow!("deserializer.case-index must return an index"))?;

        found.assert_case("some")
    }

    fn on_flags(&mut self, declared: &[String]) -> Result<ValueSpec> {
        // Which flags are set is only known at runtime, so the deserializer
        // answers with the bits themselves: it matches the names against
        // `declared`, keeping that comparison out of the generated component.
        let flags = self.method("flag-bits")?;
        let names = flags.param("names")?.value()?;
        names.write(&ValueSpec::list(declared.iter().map(ValueSpec::string)))?;
        let h = self.handle.clone();
        let found = flags
            .call(&[h, names])?
            .ok_or_else(|| anyhow::anyhow!("deserializer.flag-bits must return the set bits"))?;
        // A set flag the type does not declare has no bit, so the built
        // component traps rather than silently dropping it.
        ValueSpec::flag_bits(found.assert_case("ok")?)
    }

    fn on_string(&mut self) -> Result<ValueSpec> {
        // An object's key is retrieved via `key-at`, by entry index, rather
        // than by a leaf accessor reading the cursor.
        if let Some(index) = self.reading_object_key() {
            return Ok(self.call_with_index("key-at", &index)?.into());
        }
        self.get("get-string")
    }
    fn on_char(&mut self) -> Result<ValueSpec> {
        self.get("get-char")
    }
    fn on_bool(&mut self) -> Result<ValueSpec> {
        self.get("get-bool")
    }
    fn on_f32(&mut self) -> Result<ValueSpec> {
        self.get("get-f32")
    }
    fn on_f64(&mut self) -> Result<ValueSpec> {
        self.get("get-f64")
    }
    fn on_s8(&mut self) -> Result<ValueSpec> {
        self.get("get-s8")
    }
    fn on_s16(&mut self) -> Result<ValueSpec> {
        self.get("get-s16")
    }
    fn on_s32(&mut self) -> Result<ValueSpec> {
        self.get("get-s32")
    }
    fn on_s64(&mut self) -> Result<ValueSpec> {
        self.get("get-s64")
    }
    fn on_u8(&mut self) -> Result<ValueSpec> {
        self.get("get-u8")
    }
    fn on_u16(&mut self) -> Result<ValueSpec> {
        self.get("get-u16")
    }
    fn on_u32(&mut self) -> Result<ValueSpec> {
        self.get("get-u32")
    }
    fn on_u64(&mut self) -> Result<ValueSpec> {
        self.get("get-u64")
    }
}
