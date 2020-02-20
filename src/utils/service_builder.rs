// Copyright (c) 2019, Arm Limited, All Rights Reserved
// SPDX-License-Identifier: Apache-2.0
//
// Licensed under the Apache License, Version 2.0 (the "License"); you may
// not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//          http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS, WITHOUT
// WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use crate::authenticators::simple_authenticator::SimpleAuthenticator;
use crate::back::{
    backend_handler::{BackEndHandler, BackEndHandlerBuilder},
    dispatcher::DispatcherBuilder,
};
use crate::front::listener::{ListenerConfig, ListenerType};
use crate::front::{
    domain_socket::DomainSocketListenerBuilder, front_end::FrontEndHandler,
    front_end::FrontEndHandlerBuilder, listener::Listen,
};
use crate::key_id_managers::on_disk_manager::{OnDiskKeyIDManagerBuilder, DEFAULT_MAPPINGS_PATH};
use crate::key_id_managers::{KeyIdManagerConfig, KeyIdManagerType, ManageKeyIDs};
use crate::providers::{core_provider::CoreProviderBuilder, Provide, ProviderConfig};
use log::{error, warn, LevelFilter};
use parsec_interface::operations_protobuf::ProtobufConverter;
use parsec_interface::requests::AuthType;
use parsec_interface::requests::{BodyType, ProviderID};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{Error, ErrorKind, Result};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;
use threadpool::{Builder as ThreadPoolBuilder, ThreadPool};

#[cfg(feature = "mbed-crypto-provider")]
use crate::providers::mbed_provider::MbedProviderBuilder;
#[cfg(feature = "pkcs11-provider")]
use crate::providers::pkcs11_provider::Pkcs11ProviderBuilder;
#[cfg(feature = "tpm-provider")]
use crate::providers::tpm_provider::TpmProviderBuilder;
#[cfg(any(
    feature = "mbed-crypto-provider",
    feature = "pkcs11-provider",
    feature = "tpm-provider"
))]
use {crate::providers::ProviderType, log::info};

const VERSION_MINOR: u8 = 0;
const VERSION_MAJOR: u8 = 1;

/// Default value for the limit on the request body size (in bytes) - equal to 1MB
const DEFAULT_BODY_LEN_LIMIT: usize = 1 << 19;

type KeyIdManager = Arc<RwLock<dyn ManageKeyIDs + Send + Sync>>;
type Provider = Box<dyn Provide + Send + Sync>;

#[derive(Copy, Clone, Deserialize, Debug)]
pub struct CoreSettings {
    pub thread_pool_size: Option<usize>,
    pub idle_listener_sleep_duration: Option<u64>,
    pub log_level: Option<LevelFilter>,
    pub log_timestamp: Option<bool>,
    pub body_len_limit: Option<usize>,
}

#[derive(Deserialize, Debug)]
pub struct ServiceConfig {
    pub core_settings: CoreSettings,
    pub listener: ListenerConfig,
    pub key_manager: Option<Vec<KeyIdManagerConfig>>,
    pub provider: Option<Vec<ProviderConfig>>,
}

#[derive(Copy, Clone, Debug)]
pub struct ServiceBuilder;

impl ServiceBuilder {
    pub fn build_service(config: &ServiceConfig) -> Result<FrontEndHandler> {
        let key_id_managers =
            build_key_id_managers(config.key_manager.as_ref().unwrap_or(&Vec::new()))?;

        let providers = build_providers(
            config.provider.as_ref().unwrap_or(&Vec::new()),
            key_id_managers,
        );

        if providers.is_empty() {
            error!("Parsec needs at least one provider to start. No valid provider could be created from the configuration.");
            return Err(Error::new(ErrorKind::InvalidData, "need one provider"));
        }

        let backend_handlers = build_backend_handlers(providers)?;

        let dispatcher = DispatcherBuilder::new()
            .with_backends(backend_handlers)
            .build()?;

        let simple_authenticator = Box::from(SimpleAuthenticator {});

        Ok(FrontEndHandlerBuilder::new()
            .with_dispatcher(dispatcher)
            .with_authenticator(AuthType::Simple, simple_authenticator)
            .with_body_len_limit(
                config
                    .core_settings
                    .body_len_limit
                    .unwrap_or(DEFAULT_BODY_LEN_LIMIT),
            )
            .build()?)
    }

    pub fn start_listener(config: ListenerConfig) -> Result<Box<dyn Listen>> {
        let listener = match config.listener_type {
            ListenerType::DomainSocket => DomainSocketListenerBuilder::new()
                .with_timeout(Duration::from_millis(config.timeout))
                .build(),
        }?;

        Ok(Box::new(listener))
    }

    pub fn build_threadpool(num_threads: Option<usize>) -> ThreadPool {
        let mut threadpool_builder = ThreadPoolBuilder::new();
        if let Some(num_threads) = num_threads {
            threadpool_builder = threadpool_builder.num_threads(num_threads);
        }
        threadpool_builder.build()
    }
}

fn build_backend_handlers(
    mut providers: HashMap<ProviderID, Provider>,
) -> Result<HashMap<ProviderID, BackEndHandler>> {
    let mut map = HashMap::new();

    let mut core_provider_builder =
        CoreProviderBuilder::new().with_version(VERSION_MINOR, VERSION_MAJOR);

    for (provider_id, provider) in providers.drain() {
        core_provider_builder = core_provider_builder.with_provider_info(provider.describe());

        let backend_handler = BackEndHandlerBuilder::new()
            .with_provider(provider)
            .with_converter(Box::from(ProtobufConverter {}))
            .with_provider_id(provider_id)
            .with_content_type(BodyType::Protobuf)
            .with_accept_type(BodyType::Protobuf)
            .with_version(VERSION_MINOR, VERSION_MAJOR)
            .build()?;
        let _ = map.insert(provider_id, backend_handler);
    }

    let core_provider_backend = BackEndHandlerBuilder::new()
        .with_provider(Box::from(core_provider_builder.build()?))
        .with_converter(Box::from(ProtobufConverter {}))
        .with_provider_id(ProviderID::CoreProvider)
        .with_content_type(BodyType::Protobuf)
        .with_accept_type(BodyType::Protobuf)
        .with_version(VERSION_MINOR, VERSION_MAJOR)
        .build()?;

    let _ = map.insert(ProviderID::CoreProvider, core_provider_backend);

    Ok(map)
}

fn build_providers(
    configs: &[ProviderConfig],
    key_id_managers: HashMap<String, KeyIdManager>,
) -> HashMap<ProviderID, Provider> {
    let mut map = HashMap::new();
    for config in configs {
        let provider_id = config.provider_type.to_provider_id();
        if map.contains_key(&provider_id) {
            warn!("Parsec currently only supports one instance of each provider type. Ignoring {} and continuing...", provider_id);
            continue;
        }

        let key_id_manager = match key_id_managers.get(&config.key_id_manager) {
            Some(key_id_manager) => key_id_manager,
            None => {
                error!(
                    "Key ID manager with specified name was not found ({})",
                    config.key_id_manager
                );
                continue;
            }
        };
        // The safety is checked by the fact that only one instance per provider type is enforced.
        let provider = match unsafe { get_provider(config, key_id_manager.clone()) } {
            Ok(provider) => provider,
            Err(e) => {
                error!("Provider {} can not be created ({}).", provider_id, e);
                continue;
            }
        };
        let _ = map.insert(provider_id, provider);
    }

    map
}

// This cfg_attr is used to allow the fact that key_id_manager is not used when there is no
// providers.
#[cfg_attr(
    not(all(
        feature = "mbed-crypto-provider",
        feature = "pkcs11-provider",
        feature = "tpm-provider"
    )),
    allow(unused_variables)
)]
unsafe fn get_provider(config: &ProviderConfig, key_id_manager: KeyIdManager) -> Result<Provider> {
    match config.provider_type {
        #[cfg(feature = "mbed-crypto-provider")]
        ProviderType::MbedProvider => {
            info!("Creating a Mbed Crypto Provider.");
            Ok(Box::from(
                MbedProviderBuilder::new()
                    .with_key_id_store(key_id_manager)
                    .build()?,
            ))
        }
        #[cfg(feature = "pkcs11-provider")]
        ProviderType::Pkcs11Provider => {
            info!("Creating a PKCS 11 Provider.");
            Ok(Box::from(
                Pkcs11ProviderBuilder::new()
                .with_key_id_store(key_id_manager)
                .with_pkcs11_library_path(config.library_path.clone().ok_or_else(|| {
                        error!("The PKCS 11 provider needs a library path in the configuration file.");
                        Error::new(ErrorKind::InvalidData, "missing PKCS 11 library path")
                })?)
                .with_slot_number(config.slot_number.ok_or_else(|| {
                        error!("The slot number of the device is needed to communicate with PKCS 11 library.");
                        Error::new(ErrorKind::InvalidData, "missing slot number")
                })?)
                .with_user_pin(config.user_pin.clone())
                .build()?,
                ))
        }
        #[cfg(feature = "tpm-provider")]
        ProviderType::TpmProvider => {
            info!("Creating a TPM Provider.");
            Ok(Box::from(
                TpmProviderBuilder::new()
                    .with_key_id_store(key_id_manager)
                    .with_tcti(config.tcti.as_ref().ok_or_else(|| {
                        error!("The TPM provider needs a TCTI in its configuration.");
                        Error::new(ErrorKind::InvalidData, "missing TCTI")
                    })?)
                    .with_owner_hierarchy_auth(config.owner_hierarchy_auth.as_ref().ok_or_else(|| {
                        error!("The TPM provider needs an Owner Hierarchy authentication value in its configuration.");
                        Error::new(ErrorKind::InvalidData, "missing owner hierarchy auth")
                    })?.to_owned())
                    .build()?,
            ))
        }
        #[cfg(not(all(
            feature = "mbed-crypto-provider",
            feature = "pkcs11-provider",
            feature = "tpm-provider"
        )))]
        _ => {
            error!(
                "Provider \"{:?}\" chosen in the configuration was not compiled in Parsec binary.",
                config.provider_type
            );
            Err(Error::new(ErrorKind::InvalidData, "provider not compiled"))
        }
    }
}

fn build_key_id_managers(configs: &[KeyIdManagerConfig]) -> Result<HashMap<String, KeyIdManager>> {
    let mut map = HashMap::new();
    for config in configs {
        let _ = map.insert(config.name.clone(), get_key_id_manager(config)?);
    }

    Ok(map)
}

fn get_key_id_manager(config: &KeyIdManagerConfig) -> Result<KeyIdManager> {
    let manager = match config.manager_type {
        KeyIdManagerType::OnDisk => {
            let store_path = if let Some(store_path) = &config.store_path {
                store_path.to_owned()
            } else {
                DEFAULT_MAPPINGS_PATH.to_string()
            };

            OnDiskKeyIDManagerBuilder::new()
                .with_mappings_dir_path(PathBuf::from(store_path))
                .build()?
        }
    };

    Ok(Arc::new(RwLock::new(manager)))
}