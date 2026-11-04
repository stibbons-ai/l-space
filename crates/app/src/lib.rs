//! The L-Space App (shipped as Ook): a remote for the Bridge.
//!
//! [`flow`] holds the App's state and actions apart from rendering, [`ui`] renders it,
//! [`storage`] is what the App keeps on the device, and [`i18n`] every string it shows.

pub mod flow;
pub mod i18n;
pub mod storage;
pub mod ui;
