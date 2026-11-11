//! HTTP boundary and tenant composition for the existing ERP services.
// The request future nests several async layers (request id, admission, dispatch, RPC); rustc's
// layout query for it needs a deeper limit than the default 128.
#![recursion_limit = "256"]
pub mod boundary;

pub mod config;
pub mod conn;
pub mod prefs;
pub mod proxy;
pub mod redirect;
pub mod startup_limits;

pub mod web;

pub mod assets;

pub mod client_error;
pub mod logging;
