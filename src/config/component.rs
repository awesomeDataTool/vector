use serde::{Deserialize, Serialize};
use snafu::Snafu;
use std::marker::PhantomData;
use toml::Value;

use super::GenerateConfig;

#[derive(Debug, Snafu, Clone, PartialEq)]
pub enum ExampleError {
    #[snafu(display("unable to create an example for this component"))]
    MissingExample,
    #[snafu(display("type '{}' does not exist", type_str))]
    DoesNotExist { type_str: String },
}

/// Describes a component plugin storing its type name, an example config, and
/// other useful information about the plugin.
pub struct ComponentDescription<T: Sized> {
    pub type_str: &'static str,
    example_value: fn() -> Option<Value>,
    component_type: PhantomData<T>,
}

impl<T> ComponentDescription<T>
where
    T: 'static + Sized,
    inventory::iter<ComponentDescription<T>>:
        std::iter::IntoIterator<Item = &'static ComponentDescription<T>>,
{
    /// Creates a new component plugin description.
    pub fn new<'de, B: GenerateConfig>(type_str: &'static str) -> Self {
        ComponentDescription {
            type_str,
            example_value: || Some(B::generate_config()),
            component_type: PhantomData,
        }
    }

    /// Returns an example config for a plugin identified by its type.
    pub fn example(type_str: &str) -> Result<Value, ExampleError> {
        inventory::iter::<ComponentDescription<T>>
            .into_iter()
            .find(|t| t.type_str == type_str)
            .ok_or_else(|| ExampleError::DoesNotExist {
                type_str: type_str.to_owned(),
            })
            .and_then(|t| (t.example_value)().ok_or(ExampleError::MissingExample))
    }

    /// Returns a sorted Vec of all plugins registered of a type.
    pub fn types() -> Vec<&'static str> {
        let mut types = Vec::new();
        for definition in inventory::iter::<ComponentDescription<T>> {
            types.push(definition.type_str);
        }
        types.sort();
        types
    }
}
