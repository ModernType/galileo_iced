#![doc = "README.md"]
#![warn(missing_docs)]

mod pipeline;
mod state;
mod widget;

pub use galileo;

pub use state::{MapState, Projector, project, unproject};
pub use widget::{Click, MapWidget, map};
