//! Call chrome: the sidebar's connected panel, the stage of participant tiles,
//! and the control bar under it.

mod controls;
mod panel;
mod stage;
mod tiles;

pub(in crate::screens::home) use controls::DeviceMenu;
