// Every HashMap here is built with the default hasher; nothing in this crate ever
// swaps it, so generalizing every signature over BuildHasher is pure boilerplate.
#![allow(clippy::implicit_hasher)]

pub mod cli;
pub mod config;
pub mod orchestrate;
pub mod proxy;
pub mod secrets;
