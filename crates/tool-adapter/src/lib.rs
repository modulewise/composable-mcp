//! Build a tool component by adapting an arbitrary target component.

use anyhow::Result;

pub mod deserializer;
pub mod factory;
pub mod serializer;

pub use factory::Factory;

/// Build the tool component.
pub fn build(
    target: Vec<u8>,
    function: Option<String>,
    description: Option<String>,
) -> Result<Vec<u8>> {
    composable_factory::build(&Factory::new(target, function, description))
}
