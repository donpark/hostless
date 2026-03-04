#![cfg_attr(not(feature = "internal-testing"), allow(dead_code))]

#[cfg(feature = "internal-testing")]
pub mod auth;
#[cfg(not(feature = "internal-testing"))]
mod auth;

#[cfg(feature = "internal-testing")]
pub mod config;
#[cfg(not(feature = "internal-testing"))]
mod config;

#[cfg(feature = "internal-testing")]
pub mod process;
#[cfg(not(feature = "internal-testing"))]
mod process;

#[cfg(feature = "internal-testing")]
pub mod providers;
#[cfg(not(feature = "internal-testing"))]
mod providers;

#[cfg(feature = "internal-testing")]
pub mod server;
#[cfg(not(feature = "internal-testing"))]
mod server;

#[cfg(feature = "internal-testing")]
pub mod vault;
#[cfg(not(feature = "internal-testing"))]
mod vault;
