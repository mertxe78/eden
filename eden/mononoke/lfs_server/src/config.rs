/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::{Context, Error};
use gotham_ext::middleware::PostRequestConfig;
use permission_checker::MononokeIdentity;
use serde::de::{Deserializer, Error as _};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use std::convert::{TryFrom, TryInto};
use std::default::Default;
use std::str::FromStr;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectPopularity {
    /// SCS counter category to use for blob popularity.
    pub category: String,
    /// How long (in seconds) to lookback
    pub window: u32,
    /// Objects whose sum of downloads exceeds the threshold during the window will not be
    /// consistently-routed. This ensures the full pool of servers can be used to serve very
    /// popular blobs.
    pub threshold: u64,
}

impl TryFrom<lfs_server_config::ObjectPopularity> for ObjectPopularity {
    type Error = Error;

    fn try_from(value: lfs_server_config::ObjectPopularity) -> Result<Self, Self::Error> {
        let window = value
            .window
            .try_into()
            .with_context(|| format!("Invalid window: {:?}", value.window))?;

        let threshold = value
            .threshold
            .try_into()
            .with_context(|| format!("Invalid threshold: {:?}", value.threshold))?;

        Ok(Self {
            category: value.category,
            window,
            threshold,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Limit {
    raw_limit: lfs_server_config::ThrottleLimit,
    client_identities: Vec<MononokeIdentity>,
}

impl TryFrom<lfs_server_config::ThrottleLimit> for Limit {
    type Error = Error;

    fn try_from(value: lfs_server_config::ThrottleLimit) -> Result<Self, Self::Error> {
        let client_identities = value
            .client_identities
            .iter()
            .map(|x| FromStr::from_str(&x))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            raw_limit: value,
            client_identities,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub raw_server_config: lfs_server_config::LfsServerConfig,
    throttle_limits: Vec<Limit>,
    object_popularity: Option<ObjectPopularity>,
}

impl TryFrom<lfs_server_config::LfsServerConfig> for ServerConfig {
    type Error = Error;

    fn try_from(value: lfs_server_config::LfsServerConfig) -> Result<Self, Error> {
        let throttle_limits = value
            .throttle_limits
            .iter()
            .cloned()
            .map(Limit::try_from)
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| "Invalid throttle limits")?;

        let object_popularity = value
            .object_popularity
            .as_ref()
            .map(|o| o.clone().try_into())
            .transpose()
            .with_context(|| "Invalid object popularity")?;

        Ok(Self {
            raw_server_config: value,
            throttle_limits,
            object_popularity,
        })
    }
}

impl<'de> Deserialize<'de> for ServerConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = lfs_server_config::LfsServerConfig::deserialize(deserializer)?;
        let config = Self::try_from(raw).map_err(|e| D::Error::custom(format!("{:?}", e)))?;
        Ok(config)
    }
}

impl Serialize for ServerConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        lfs_server_config::LfsServerConfig::serialize(&self.raw_server_config, serializer)
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        let raw_server_config = lfs_server_config::LfsServerConfig {
            track_bytes_sent: false,
            enable_consistent_routing: false,
            disable_hostname_logging: false,
            enforce_acl_check: false,
            throttle_limits: vec![],
            object_popularity: None,
            // TODO: Remove those once they're gone from Thrift configs.
            object_popularity_category: Default::default(),
            object_popularity_threshold: Default::default(),
        };

        Self {
            raw_server_config,
            throttle_limits: vec![],
            object_popularity: None,
        }
    }
}

impl ServerConfig {
    pub fn track_bytes_sent(&self) -> bool {
        self.raw_server_config.track_bytes_sent
    }
    pub fn enable_consistent_routing(&self) -> bool {
        self.raw_server_config.enable_consistent_routing
    }
    pub fn disable_hostname_logging(&self) -> bool {
        self.raw_server_config.disable_hostname_logging
    }
    pub fn throttle_limits(&self) -> Vec<Limit> {
        self.throttle_limits.clone()
    }
    pub fn enforce_acl_check(&self) -> bool {
        self.raw_server_config.enforce_acl_check
    }
    pub fn object_popularity(&self) -> Option<&ObjectPopularity> {
        self.object_popularity.as_ref()
    }
    #[cfg(test)]
    pub fn object_popularity_mut(&mut self) -> &mut Option<ObjectPopularity> {
        &mut self.object_popularity
    }
}

impl Limit {
    pub fn counter(&self) -> String {
        self.raw_limit.counter.clone()
    }
    pub fn limit(&self) -> i64 {
        self.raw_limit.limit
    }
    pub fn sleep_ms(&self) -> i64 {
        self.raw_limit.sleep_ms
    }
    pub fn max_jitter_ms(&self) -> i64 {
        self.raw_limit.max_jitter_ms
    }
    pub fn client_identities(&self) -> Vec<MononokeIdentity> {
        self.client_identities.clone()
    }
    pub fn probability_pct(&self) -> i64 {
        self.raw_limit.probability_pct
    }
}

impl PostRequestConfig for ServerConfig {
    fn resolve_hostname(&self) -> bool {
        !self.disable_hostname_logging()
    }
}
