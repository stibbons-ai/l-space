//! The L-Space App (shipped as Ook): a remote for the Bridge, and a player of its own (Local
//! playback).
//!
//! [`flow`] holds the App's state and actions apart from rendering, [`ui`] renders it,
//! [`local`] plays Items on the phone through a [`local::LocalPlayer`] (AVPlayer on iOS,
//! `ios`), [`storage`] is what the App keeps on the device, and [`i18n`] every string it shows
//! (except the Info.plist strings, which stay English). `client` is the HTTP client its calls
//! to the Bridge go through, plus the one request that follows redirects (to find where an
//! entered address leads).

mod client;
#[cfg(target_os = "ios")]
mod ios;

pub mod flow;
pub mod i18n;
pub mod local;
pub mod storage;
pub mod ui;
