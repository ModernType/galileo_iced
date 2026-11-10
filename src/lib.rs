#![doc = "README.md"]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod pipeline;
mod state;
mod widget;

pub use galileo;

pub use state::{MapState, Projector, project, unproject};
pub use widget::{Click, MapWidget, map};
