//! The tool-adapter's `ReadVisitor`. Serializes a target value to JSON.

use anyhow::Result;

use composable_factory::world::{
    ImportedFunction, Interface, MapKey, ReadVisitor, Type, Value, ValueSpec,
};

/// Serializes a value into JSON.
pub struct JsonSerializer {
    /// The `serializer` interface.
    interface: Interface,
    /// The `serializer` resource handle.
    handle: Value,
    /// One entry per currently open map, innermost last. True for any map with
    /// string keys, indicating it should be written as a JSON object rather
    /// than an array of pairs. A map's value can be another map, so the entry
    /// being written is handled according to the key type of the last one.
    open_maps: Vec<bool>,
    /// Set between an entry's bracket and its key, so a string-keyed map's key
    /// is routed to `add-key` rather than `add-string`.
    expecting_key: bool,
}

impl JsonSerializer {
    pub fn new(interface: Interface) -> Result<Self> {
        let handle = interface
            .function("[constructor]serializer")?
            .call(&[])?
            .ok_or_else(|| anyhow::anyhow!("serializer constructor must return a handle"))?;
        Ok(JsonSerializer {
            interface,
            handle,
            open_maps: Vec::new(),
            expecting_key: false,
        })
    }

    pub fn into_json(self) -> Result<Value> {
        let h = self.handle.clone();
        let json = self
            .method("finish")?
            .call(&[h])?
            .ok_or_else(|| anyhow::anyhow!("serializer.finish must return a string"))?;
        self.handle.drop()?;
        Ok(json)
    }

    /// A `serializer` method for `[method]serializer.<name>`.
    fn method(&self, name: &str) -> Result<ImportedFunction> {
        self.interface
            .function(&format!("[method]serializer.{name}"))
    }

    /// Push one leaf value via the named `add-*` method.
    fn add(&self, name: &str, value: Value) -> Result<()> {
        let h = self.handle.clone();
        self.method(name)?.call(&[h, value])?;
        Ok(())
    }

    /// Whether the entry being written belongs to a string-keyed map.
    fn in_string_keyed_map(&self) -> bool {
        self.open_maps.last().copied().unwrap_or(false)
    }

    /// Call a method taking one string, which the WIT names `param`.
    fn call_with_str(&self, method: &str, param: &str, text: &str) -> Result<()> {
        let h = self.handle.clone();
        let target = self.method(method)?;
        let arg = target.param(param)?.value()?;
        arg.write(&ValueSpec::string(text))?;
        target.call(&[h, arg])?;
        Ok(())
    }
}

impl ReadVisitor for JsonSerializer {
    fn on_string(&mut self, value: Value) -> Result<()> {
        // A string-keyed map's key arrives here, but `begin_entry` flags it.
        let method = if self.expecting_key {
            self.expecting_key = false;
            "add-key"
        } else {
            "add-string"
        };
        self.add(method, value)
    }
    fn on_char(&mut self, value: Value) -> Result<()> {
        self.add("add-char", value)
    }
    fn on_bool(&mut self, value: Value) -> Result<()> {
        self.add("add-bool", value)
    }
    fn on_f32(&mut self, value: Value) -> Result<()> {
        self.add("add-f32", value)
    }
    fn on_f64(&mut self, value: Value) -> Result<()> {
        self.add("add-f64", value)
    }
    fn on_s8(&mut self, value: Value) -> Result<()> {
        self.add("add-s8", value)
    }
    fn on_s16(&mut self, value: Value) -> Result<()> {
        self.add("add-s16", value)
    }
    fn on_s32(&mut self, value: Value) -> Result<()> {
        self.add("add-s32", value)
    }
    fn on_s64(&mut self, value: Value) -> Result<()> {
        self.add("add-s64", value)
    }
    fn on_u8(&mut self, value: Value) -> Result<()> {
        self.add("add-u8", value)
    }
    fn on_u16(&mut self, value: Value) -> Result<()> {
        self.add("add-u16", value)
    }
    fn on_u32(&mut self, value: Value) -> Result<()> {
        self.add("add-u32", value)
    }
    fn on_u64(&mut self, value: Value) -> Result<()> {
        self.add("add-u64", value)
    }

    fn begin_record(&mut self) -> Result<()> {
        let h = self.handle.clone();
        self.method("begin-object")?.call(&[h])?;
        Ok(())
    }

    fn end_record(&mut self) -> Result<()> {
        let h = self.handle.clone();
        self.method("end-object")?.call(&[h])?;
        Ok(())
    }

    fn begin_field(&mut self, name: &str) -> Result<()> {
        // A record field's JSON key.
        let h = self.handle.clone();
        let add_key = self.method("add-key")?;
        let key = add_key.param("name")?.value()?;
        key.write(&ValueSpec::string(name))?;
        add_key.call(&[h, key])?;
        Ok(())
    }

    fn begin_list(&mut self) -> Result<()> {
        let h = self.handle.clone();
        self.method("begin-array")?.call(&[h])?;
        Ok(())
    }

    fn end_list(&mut self) -> Result<()> {
        let h = self.handle.clone();
        self.method("end-array")?.call(&[h])?;
        Ok(())
    }

    // A tuple serializes as a JSON array (positional, no names).
    fn begin_tuple(&mut self) -> Result<()> {
        let h = self.handle.clone();
        self.method("begin-array")?.call(&[h])?;
        Ok(())
    }

    fn end_tuple(&mut self) -> Result<()> {
        let h = self.handle.clone();
        self.method("end-array")?.call(&[h])?;
        Ok(())
    }

    // A map with string keys serializes as a JSON object. A map with any other
    // key type serializes as an array of [key, value] entry arrays.
    fn begin_map(&mut self, key: MapKey, _value: &Type) -> Result<()> {
        let string_keyed = key == MapKey::String;
        self.open_maps.push(string_keyed);
        let h = self.handle.clone();
        let open = if string_keyed {
            "begin-object"
        } else {
            "begin-array"
        };
        self.method(open)?.call(&[h])?;
        Ok(())
    }

    fn end_map(&mut self) -> Result<()> {
        let string_keyed = self.open_maps.pop().unwrap_or(false);
        let h = self.handle.clone();
        let close = if string_keyed {
            "end-object"
        } else {
            "end-array"
        };
        self.method(close)?.call(&[h])?;
        Ok(())
    }

    // An object's entry is written flat, as a field key and its value, so only
    // the array form (when the map key is not a string) brackets its entries.
    fn begin_entry(&mut self) -> Result<()> {
        if self.in_string_keyed_map() {
            return Ok(());
        }
        let h = self.handle.clone();
        self.method("begin-array")?.call(&[h])?;
        Ok(())
    }

    fn end_entry(&mut self) -> Result<()> {
        if self.in_string_keyed_map() {
            return Ok(());
        }
        let h = self.handle.clone();
        self.method("end-array")?.call(&[h])?;
        Ok(())
    }

    // An object's key is written with `add-key` rather than as a value, so the
    // string that follows is routed there. Every other key is a plain value.
    fn begin_key(&mut self) -> Result<()> {
        self.expecting_key = self.in_string_keyed_map();
        Ok(())
    }

    // An enum case name is the whole value, so it serializes as a bare string
    // rather than the tagged object a variant/option/result case needs.
    fn on_enum(&mut self, name: &str) -> Result<()> {
        self.call_with_str("add-enum", "case", name)
    }

    fn on_case(&mut self, name: &str) -> Result<()> {
        self.call_with_str("add-case", "case", name)
    }

    fn begin_case(&mut self, name: &str) -> Result<()> {
        self.call_with_str("begin-case", "case", name)
    }

    fn end_case(&mut self) -> Result<()> {
        let h = self.handle.clone();
        self.method("end-case")?.call(&[h])?;
        Ok(())
    }

    // A flags bitset serializes as a JSON array of the set flags' names.
    fn begin_flags(&mut self) -> Result<()> {
        let h = self.handle.clone();
        self.method("begin-flags")?.call(&[h])?;
        Ok(())
    }

    fn on_flag(&mut self, name: &str) -> Result<()> {
        let h = self.handle.clone();
        let add_flag = self.method("add-flag")?;
        let flag = add_flag.param("name")?.value()?;
        flag.write(&ValueSpec::string(name))?;
        add_flag.call(&[h, flag])?;
        Ok(())
    }

    fn end_flags(&mut self) -> Result<()> {
        let h = self.handle.clone();
        self.method("end-flags")?.call(&[h])?;
        Ok(())
    }
}
