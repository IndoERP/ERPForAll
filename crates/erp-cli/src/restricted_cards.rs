// SPDX-License-Identifier: MIT OR Apache-2.0
//! Node-local, pinned WASI compiler for caller-authorized stored view cards.
use erp_addon_web::restricted_card::CardCompiler;
use erp_fields::{Error, Result};
use erp_sdk::BoxFuture;
use erp_ui_compiler_host::{
    ArchCompileRequest, ArtifactDigest, CancelToken, ExecutionLimits, WasmArtifact,
    WasmCompilerHost,
};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    ffi::OsString,
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::sync::Semaphore;

type Plans = VecDeque<([u8; 32], String)>;

struct Compiler {
    host: WasmCompilerHost,
    permits: Arc<Semaphore>,
    plans: Arc<Mutex<Plans>>,
}

impl CardCompiler for Compiler {
    fn compile(
        &self,
        arch: String,
        source: String,
        language: String,
        mut fields: Vec<String>,
    ) -> BoxFuture<'static, Result<String>> {
        let host = self.host.clone();
        let permits = Arc::clone(&self.permits);
        let plans = Arc::clone(&self.plans);
        Box::pin(async move {
            fields.sort_unstable();
            let request = ArchCompileRequest::card(arch, language, &fields, source)
                .map_err(|error| Error::validation(error.to_string()))?;
            // Frame boundaries distinguish every input, including the complete caller schema.
            let key: [u8; 32] = Sha256::digest(
                request
                    .encode()
                    .map_err(|error| Error::validation(error.to_string()))?,
            )
            .into();
            if let Some((_, plan)) = plans
                .lock()
                .map_err(|_| Error::internal("card cache unavailable"))?
                .iter()
                .find(|(hash, _)| *hash == key)
            {
                return Ok(plan.clone());
            }
            let permit = permits
                .try_acquire_owned()
                .map_err(|_| Error::validation("card compiler is busy; retry the view request"))?;
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let response = host
                    .compile_arch(&request, &CancelToken::new())
                    .map_err(|err| Error::validation(err.to_string()))?;
                let plan = String::from_utf8(response.into_bytes())
                    .map_err(|_| Error::internal("card compiler returned invalid UTF-8"))?;
                let mut cache = plans
                    .lock()
                    .map_err(|_| Error::internal("card cache unavailable"))?;
                if !cache.iter().any(|(hash, _)| *hash == key) {
                    let mut bytes =
                        plan.len() + cache.iter().map(|(_, entry)| entry.len()).sum::<usize>();
                    while cache.len() >= 64 || bytes > 4 * 1024 * 1024 {
                        let Some((_, evicted)) = cache.pop_front() else {
                            break;
                        };
                        bytes -= evicted.len();
                    }
                    cache.push_back((key, plan.clone()));
                }
                Ok(plan)
            })
            .await
            .map_err(|_| Error::internal("card compiler task failed"))?
        })
    }
}

pub(crate) fn install(path: Option<OsString>, digest: Option<OsString>) -> Result<()> {
    let (path, digest) = match (path, digest) {
        (None, None) => return Ok(()),
        (Some(path), Some(digest)) => (path, digest),
        _ => {
            return Err(Error::validation(
                "set both ERP_UI_CARD_GUEST and ERP_UI_CARD_GUEST_SHA256",
            ));
        }
    };
    let digest = digest
        .to_str()
        .ok_or_else(|| Error::validation("guest digest must be UTF-8"))?;
    let digest = ArtifactDigest::parse(digest)
        .map_err(|_| Error::validation("invalid card compiler SHA-256"))?;
    let artifact = WasmArtifact::load(Path::new(&path), &digest)
        .map_err(|err| Error::validation(err.to_string()))?;
    let limits = ExecutionLimits::default()
        .lowered_memory_bytes(64 * 1024 * 1024)
        .map_err(|err| Error::internal(err.to_string()))?;
    erp_addon_web::restricted_card::install_compiler(Arc::new(Compiler {
        host: WasmCompilerHost::new(&artifact, limits),
        permits: Arc::new(Semaphore::new(2)),
        plans: Arc::new(Mutex::new(VecDeque::new())),
    }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn optional_guest_requires_both_path_and_pin() {
        assert!(super::install(None, None).is_ok());
        assert!(super::install(Some("card.wasm".into()), None).is_err());
        assert!(super::install(None, Some("pin".into())).is_err());
    }
}
