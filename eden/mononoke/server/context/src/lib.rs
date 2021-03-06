/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![deny(warnings)]
#![feature(never_type, result_flattening)]

pub use session_id::SessionId;

pub use crate::core::CoreContext;
#[cfg(fbcode_build)]
pub use crate::facebook::{is_external_sync, is_quicksand};
pub use crate::logging::{LoggingContainer, SamplingKey};
#[cfg(not(fbcode_build))]
pub use crate::oss::{is_external_sync, is_quicksand};
pub use crate::perf_counters::{PerfCounterType, PerfCounters};
pub use crate::session::{SessionClass, SessionContainer, SessionContainerBuilder};

mod core;
#[cfg(fbcode_build)]
mod facebook;
mod logging;
#[cfg(not(fbcode_build))]
mod oss;
mod perf_counters;
mod perf_counters_stack;
mod session;
